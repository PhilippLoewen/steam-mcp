//! game_config: launch options, launch presets, MangoHud configs and Steam client settings.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::steam::launch_options::{check_launch_options, check_mangohud_setting};
use crate::steam::millennium::{
    BridgeLaunchOptions, BridgeOptions, BridgeOutcome, LaunchOptionsRequest, bridge_launch_options,
};
use crate::steam::paths::steam_may_be_running;
use crate::steam::userdata::{get_app_launch_options, get_local_config, set_app_launch_options};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, Write, blocking};
use crate::util::{backup_file, home_dir, io_error, js_num, num, read_text, write_atomic};
use crate::vdf::{VdfObject, VdfValue, is_array_index, js_entries};

const ACTIONS: &[&str] = &["get_launch_opts", "set_launch_opts", "presets", "mangohud", "steam_settings"];
const MANGOHUD_ACTIONS: &[&str] = &["show", "set", "reset"];

struct Preset {
    name: &'static str,
    description: &'static str,
    options: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset {
        name: "pipewire-audio",
        description: "Force PipeWire audio backend (fixes audio issues on PipeWire systems)",
        options: "-sdlaudiodriver pipewire",
    },
    Preset { name: "force-vulkan", description: "Force Vulkan renderer", options: "-force-vulkan" },
    Preset { name: "force-dx11", description: "Force DirectX 11 (via DXVK for Proton games)", options: "-force-d3d11" },
    Preset {
        name: "disable-fullscreen-optimizations",
        description: "Disable fullscreen optimizations (can fix stuttering)",
        options: "-window-mode exclusive",
    },
    Preset { name: "skip-intro", description: "Skip intro videos (common for Source/Unity games)", options: "-novid" },
    Preset { name: "mangohud", description: "Enable MangoHud performance overlay", options: "mangohud %command%" },
    Preset {
        name: "gamemode",
        description: "Enable Feral GameMode for performance optimization",
        options: "gamemoderun %command%",
    },
    Preset {
        name: "mangohud-gamemode",
        description: "Enable both MangoHud overlay and GameMode",
        options: "mangohud gamemoderun %command%",
    },
    Preset {
        name: "prime-nvidia",
        description: "Force NVIDIA GPU on hybrid graphics laptops",
        options: "__NV_PRIME_RENDER_OFFLOAD=1 __GLX_VENDOR_LIBRARY_NAME=nvidia %command%",
    },
    Preset {
        name: "disable-esync",
        description: "Disable Esync (can fix hangs/crashes in some Proton games)",
        options: "PROTON_NO_ESYNC=1 %command%",
    },
    Preset {
        name: "disable-fsync",
        description: "Disable Fsync (can fix issues on kernels without fsync support)",
        options: "PROTON_NO_FSYNC=1 %command%",
    },
    Preset { name: "proton-log", description: "Enable Proton debug logging", options: "PROTON_LOG=1 %command%" },
    Preset {
        name: "force-proton",
        description: "Force Proton for a native Linux game (useful if native port is broken)",
        options: "STEAM_LINUX_RUNTIME_LOG=1 %command%",
    },
];

/// Names a JS object literal inherits from Object.prototype, so `PRESETS[name]` is truthy
/// but has no `options`.
const OBJECT_PROTOTYPE_KEYS: &[&str] = &[
    "constructor",
    "__defineGetter__",
    "__defineSetter__",
    "hasOwnProperty",
    "__lookupGetter__",
    "__lookupSetter__",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "valueOf",
    "__proto__",
    "toLocaleString",
];

/// The refusal for launch options edits while Steam runs and the Millennium
/// bridge can't take them; `bridge_status` says why it can't.
fn steam_running_warning(bridge_status: &str) -> String {
    format!(
        "WARNING: Steam is currently running (or that couldn't be checked)!\n\
         Modifying localconfig.vdf while Steam is running will likely result in your changes being overwritten when Steam exits.\n\
         Please close Steam first, then try again, or install the steam-mcp-bridge Millennium plugin\n\
         (millennium-plugin/ in the steam-mcp repo) to change launch options while Steam runs.\n\
         Millennium bridge: {bridge_status}.\n\n\
         Launch options were NOT modified."
    )
}

/// Everything a launch options edit touches outside its own logic, so tests can stand in for Steam.
trait LaunchEnv {
    fn steam_running(&self) -> bool;
    fn bridge(&self, request: &LaunchOptionsRequest) -> Result<BridgeOutcome<BridgeLaunchOptions>>;
    /// From localconfig.vdf.
    fn read(&self, appid: i64) -> Result<Option<String>>;
    /// To localconfig.vdf.
    fn write(&self, appid: i64, options: &str) -> Result<()>;
}

/// The real localconfig.vdf and Millennium socket.
struct LiveEnv;

impl LaunchEnv for LiveEnv {
    fn steam_running(&self) -> bool {
        steam_may_be_running()
    }

    fn bridge(&self, request: &LaunchOptionsRequest) -> Result<BridgeOutcome<BridgeLaunchOptions>> {
        // game_config's file edits are synchronous, so unlike library it runs on
        // tokio's blocking pool, where blocking on the runtime is allowed.
        tokio::runtime::Handle::current().block_on(bridge_launch_options(request, &BridgeOptions::default()))
    }

    fn read(&self, appid: i64) -> Result<Option<String>> {
        get_app_launch_options(appid)
    }

    fn write(&self, appid: i64, options: &str) -> Result<()> {
        set_app_launch_options(appid, options)
    }
}

/// The launch options an edit starts from, and whether the edit has to go
/// through the bridge. Err holds the refusal when Steam runs and the bridge
/// can't take the edit.
fn current_for_edit(env: &impl LaunchEnv, appid: f64) -> Result<Result<(String, bool), String>> {
    if !env.steam_running() {
        return Ok(Ok((launch_options_of(env, appid)?.unwrap_or_default(), false)));
    }
    match env.bridge(&LaunchOptionsRequest::Get { appid: integral_appid(appid)? })? {
        BridgeOutcome::Done(current) => Ok(Ok((current.launch_options, true))),
        BridgeOutcome::Unavailable(why) => Ok(Err(steam_running_warning(&why))),
    }
}

/// Write launch options read by [`current_for_edit`]. Through the bridge, the
/// write is refused if Steam's launch options are no longer `previous`.
fn write_edit(env: &impl LaunchEnv, appid: i64, options: &str, previous: &str, bridged: bool) -> Result<()> {
    if !bridged {
        return env.write(appid, options);
    }
    let request =
        LaunchOptionsRequest::Set { appid, options: options.to_string(), expected: Some(previous.to_string()) };
    match env.bridge(&request)? {
        BridgeOutcome::Done(_) => Ok(()),
        BridgeOutcome::Unavailable(why) => {
            bail!(
                "the Millennium bridge stopped answering before the launch options were set ({why}); they were NOT modified"
            )
        }
    }
}

fn insert_via(out: &mut Map<String, Value>, bridged: bool) {
    if bridged {
        out.insert("via".into(), "millennium".into());
    }
}

const MANGOHUD_DEFAULTS: &str =
    "fps\ngpu_stats\ngpu_temp\ncpu_stats\ncpu_temp\nram\nvram\nframe_timing\nposition=top-left";

pub fn tool() -> Tool {
    Tool {
        name: "game_config",
        description: "Launch options, presets, MangoHud, Steam settings",
        params: vec![
            Param::new("action", ParamType::Enum(ACTIONS)).required(),
            Param::new("appid", ParamType::Number).describe(
                "The game (for \"get_launch_opts\", \"set_launch_opts\", applying a preset, and a game's MangoHud config)",
            ),
            Param::new("options", ParamType::String)
                .describe("The complete new launch options (for \"set_launch_opts\" only; \"\" clears them)"),
            Param::new("preset", ParamType::String).describe(
                "Preset name to merge into the game's launch options (for \"presets\" only, with appid; \
                 omit it to list the presets)",
            ),
            Param::new("mangohud_action", ParamType::Enum(MANGOHUD_ACTIONS))
                .describe("For \"mangohud\": show (default), set, or reset"),
            Param::new("settings", ParamType::String)
                .describe("Comma-separated key=value pairs (for \"mangohud\" with mangohud_action \"set\")"),
        ],
        handler: |args| blocking(move || handle(args)),
        writes: &[
            Write::Action("set_launch_opts"),
            Write::Param("preset"),
            Write::Value("mangohud_action", "set"),
            Write::Value("mangohud_action", "reset"),
        ],
        destructive: true,
        open_world: false,
    }
}

fn handle(args: Args) -> ToolResult {
    let action = args.str("action").unwrap_or_default();
    let env = LiveEnv;
    let result = match action.as_str() {
        "get_launch_opts" => get_launch_opts(&env, &args),
        "set_launch_opts" => set_launch_opts(&env, &args),
        "presets" => presets(&env, &args),
        "mangohud" => mangohud(&env, &args),
        "steam_settings" => steam_settings(),
        _ => bail_unknown(),
    };
    ToolResult::or_error(result, &format!("Error in game_config ({action})"))
}

fn bail_unknown() -> Result<ToolResult> {
    bail!("unknown action")
}

/// The appid when truthy in JS (present and non-zero).
fn truthy_appid(args: &Args) -> Option<f64> {
    args.f64("appid").filter(|&id| id != 0.0)
}

/// An appid as the integer key the localconfig.vdf helpers take.
fn integral_appid(appid: f64) -> Result<i64> {
    if appid.fract() != 0.0 {
        bail!("appid must be an integer");
    }
    Ok(appid as i64)
}

fn launch_options_of(env: &impl LaunchEnv, appid: f64) -> Result<Option<String>> {
    if appid.fract() != 0.0 {
        get_local_config()?;
        return Ok(None);
    }
    env.read(appid as i64)
}

fn get_launch_opts(env: &impl LaunchEnv, args: &Args) -> Result<ToolResult> {
    let Some(appid) = truthy_appid(args) else {
        return Ok(ToolResult::error("An appid is required for get_launch_opts."));
    };
    // localconfig.vdf lags behind a running Steam, so ask Steam when the bridge can.
    let mut stale = None;
    if env.steam_running() && appid.fract() == 0.0 {
        match env.bridge(&LaunchOptionsRequest::Get { appid: appid as i64 })? {
            BridgeOutcome::Done(current) => {
                return Ok(ToolResult::json(&json!({
                    "appid": num(appid),
                    "launchOptions": current.launch_options,
                    "hasLaunchOptions": !current.launch_options.is_empty(),
                    "via": "millennium",
                })));
            }
            BridgeOutcome::Unavailable(why) => stale = Some(why),
        }
    }
    let options = launch_options_of(env, appid)?;
    let mut out = Map::new();
    out.insert("appid".into(), num(appid));
    out.insert("launchOptions".into(), options.clone().unwrap_or_default().into());
    out.insert("hasLaunchOptions".into(), options.is_some_and(|o| !o.is_empty()).into());
    if let Some(why) = stale {
        out.insert(
            "note".into(),
            format!(
                "Steam is running, but the Millennium bridge can't be used ({why}); \
                 these are from localconfig.vdf, which may lag behind Steam."
            )
            .into(),
        );
    }
    Ok(ToolResult::json(&Value::Object(out)))
}

fn set_launch_opts(env: &impl LaunchEnv, args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    // A preset passed here used to be ignored silently, and a preset name in
    // "options" is a valid game argument, so say where presets go, first.
    if args.str("preset").is_some_and(|p| !p.is_empty()) {
        return Ok(ToolResult::error(
            "set_launch_opts doesn't apply presets; launch options were NOT modified. Apply a preset with \
             action \"presets\", appid and preset (without options).",
        ));
    }
    let Some(appid) = truthy_appid(args) else {
        return Ok(ToolResult::error("An appid is required for set_launch_opts."));
    };
    let Some(options) = args.str("options") else {
        return Ok(ToolResult::error(
            "The \"options\" parameter is required for set_launch_opts. To apply a preset, use action \"presets\".",
        ));
    };
    if let Err(why) = check_launch_options(&options) {
        return Ok(ToolResult::error(format!(
            "Launch options were NOT modified: {why}.\nAccepted are game arguments, or \
             [VAR=value …] [wrapper …] %command% [game arguments …] with listed variables and wrappers. \
             For anything else, set the launch options in Steam's game properties yourself."
        )));
    }
    let shown = if options.is_empty() { "(cleared)" } else { options.as_str() };
    let message = format!("Launch options for appid {} set to: {shown}", js_num(appid));

    if env.steam_running() {
        let request =
            LaunchOptionsRequest::Set { appid: integral_appid(appid)?, options: options.clone(), expected: None };
        return Ok(match env.bridge(&request)? {
            BridgeOutcome::Unavailable(why) => ToolResult::error(steam_running_warning(&why)),
            BridgeOutcome::Done(set) => {
                let mut out = Map::new();
                out.insert("appid".into(), num(appid));
                out.insert("launchOptions".into(), options.into());
                out.insert("success".into(), true.into());
                out.insert("changed".into(), set.changed.into());
                out.insert("via".into(), "millennium".into());
                if set.verified_by_readback {
                    out.insert("verifiedByReadback".into(), true.into());
                }
                out.insert("message".into(), message.into());
                ToolResult::json(&Value::Object(out))
            }
        });
    }

    env.write(integral_appid(appid)?, &options)?;

    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "launchOptions": options,
        "success": true,
        "message": message,
    })))
}

fn presets(env: &impl LaunchEnv, args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    let Some(preset_name) = args.str("preset").filter(|p| !p.is_empty()) else {
        let list: Vec<Value> = PRESETS
            .iter()
            .map(|p| json!({ "name": p.name, "description": p.description, "options": p.options }))
            .collect();
        return Ok(ToolResult::json(&json!({ "presets": list })));
    };

    let Some(appid) = truthy_appid(args) else {
        return Ok(ToolResult::error("An appid is required when applying a preset."));
    };

    let preset = PRESETS.iter().find(|p| p.name == preset_name);
    let inherited = OBJECT_PROTOTYPE_KEYS.contains(&preset_name.as_str());
    if preset.is_none() && !inherited {
        let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        return Ok(ToolResult::error(format!(
            "Unknown preset \"{preset_name}\". Available presets: {}",
            names.join(", ")
        )));
    }

    let (previous, bridged) = match current_for_edit(env, appid)? {
        Ok(current) => current,
        Err(refusal) => return Ok(ToolResult::error(refusal)),
    };
    let Some(preset) = preset else {
        bail!("Cannot read properties of undefined (reading 'includes')");
    };
    let new_options = merge_launch_options(&previous, preset.options);

    write_edit(env, integral_appid(appid)?, &new_options, &previous, bridged)?;

    let mut out = Map::new();
    out.insert("appid".into(), num(appid));
    out.insert("preset".into(), preset_name.into());
    out.insert("description".into(), preset.description.into());
    out.insert("previousLaunchOptions".into(), (if previous.is_empty() { "(none)" } else { previous.as_str() }).into());
    out.insert("newLaunchOptions".into(), new_options.into());
    out.insert("success".into(), true.into());
    insert_via(&mut out, bridged);
    Ok(ToolResult::json(&Value::Object(out)))
}

/// Merge a preset into existing launch options. Flag presets are appended unless
/// present; `%command%` presets replace options without `%command%`, otherwise their
/// wrappers go before the existing `%command%` and suffixes after it.
fn merge_launch_options(current: &str, preset: &str) -> String {
    if !preset.contains("%command%") {
        if current.contains(preset.trim()) {
            return current.to_string();
        }
        return if current.is_empty() { preset.to_string() } else { format!("{current} {preset}") };
    }

    if current.is_empty() || !current.contains("%command%") {
        return preset.to_string();
    }

    let split = |s: &str| {
        let mut parts = s.split("%command%");
        let prefix = parts.next().unwrap_or("").trim().to_string();
        let suffix = parts.next().unwrap_or("").trim().to_string();
        (prefix, suffix)
    };
    let (preset_prefix, preset_suffix) = split(preset);
    let (current_prefix, current_suffix) = split(current);

    let current_tokens: Vec<&str> = current_prefix.split_whitespace().collect();
    let preset_tokens: Vec<&str> =
        if preset_prefix.is_empty() { vec![""] } else { preset_prefix.split_whitespace().collect() };
    let merged_prefix = preset_tokens
        .into_iter()
        .filter(|t| !current_tokens.contains(t))
        .chain([current_prefix.as_str()])
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let merged_suffix = [current_suffix.as_str(), preset_suffix.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if merged_suffix.is_empty() {
        format!("{merged_prefix} %command%")
    } else {
        format!("{merged_prefix} %command% {merged_suffix}")
    }
}

fn mangohud_config(scope: &str, path: &Path) -> Result<Value> {
    let exists = path.exists();
    let content = if exists { read_text(path)? } else { String::new() };
    Ok(json!({ "scope": scope, "path": path.to_string_lossy(), "exists": exists, "content": content }))
}

fn setting_line(key: &str, value: &str) -> String {
    if value.is_empty() { key.to_string() } else { format!("{key}={value}") }
}

/// `Object.fromEntries(map)`: array-index keys first, ascending, then insertion order.
fn js_object(entries: &IndexMap<String, String>) -> Map<String, Value> {
    let mut index: Vec<(u64, &String, &String)> = Vec::new();
    let mut rest = Vec::new();
    for (k, v) in entries {
        if is_array_index(k) {
            index.push((k.parse().expect("checked"), k, v));
        } else {
            rest.push((k, v));
        }
    }
    index.sort_by_key(|(n, _, _)| *n);
    index.into_iter().map(|(_, k, v)| (k, v)).chain(rest).map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()
}

/// The per-game config steam-mcp keeps for `appid`. MangoHud finds per-game
/// configs only by program name, so the game's launch options point it here.
fn mangohud_game_config(mango_dir: &Path, appid: i64) -> PathBuf {
    mango_dir.join("steam-mcp").join(format!("{appid}.conf"))
}

/// `current` launch options made to load MangoHud's config from `path`: an
/// existing MANGOHUD_CONFIGFILE= value is replaced, else `env
/// MANGOHUD_CONFIGFILE=<path>` goes right before %command%, where it reaches
/// the game behind any wrappers or shell syntax the user has. Game arguments
/// without %command% become `env … %command% <arguments>`.
/// An existing value is only replaced when it is a plain path ending at
/// whitespace or the end; a quoted or escaped one, or one followed directly by
/// shell syntax, is refused rather than guessed at.
fn with_mangohud_config_file(current: &str, path: &str) -> Result<String, String> {
    const VAR: &str = "MANGOHUD_CONFIGFILE=";
    let assignment = format!("{VAR}{path}");
    if let Some(start) = current.find(VAR).filter(|&i| i == 0 || current[..i].ends_with([' ', '\t'])) {
        let value_start = start + VAR.len();
        let value_end = current[value_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || "_./-".contains(c)))
            .map_or(current.len(), |n| value_start + n);
        if !current[value_end..].is_empty() && !current[value_end..].starts_with([' ', '\t']) {
            return Err("the launch options already set MANGOHUD_CONFIGFILE in a form steam-mcp can't edit \
                        safely (quoted, escaped or followed by shell syntax); change it in Steam yourself"
                .into());
        }
        return Ok(format!("{}{assignment}{}", &current[..start], &current[value_end..]));
    }
    Ok(match current.find("%command%") {
        Some(at) => format!("{}env {assignment} {}", &current[..at], &current[at..]),
        None => format!("env {assignment} %command% {}", current.trim()).trim_end().to_string(),
    })
}

/// Whether launch options turn MangoHud on (the wrapper or MANGOHUD=1).
fn mangohud_enabled(options: &str) -> bool {
    options.split_whitespace().any(|w| w == "mangohud" || w == "MANGOHUD=1")
}

fn mangohud(env: &impl LaunchEnv, args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    let mango_action = args.str("mangohud_action").unwrap_or_else(|| "show".to_string());
    let mango_dir = home_dir().join(".config").join("MangoHud");
    let global_path = mango_dir.join("MangoHud.conf");
    let appid = match truthy_appid(args) {
        Some(id) => Some(integral_appid(id)?),
        None => None,
    };
    let game = appid.map(|id| (format!("game-specific (appid: {id})"), mangohud_game_config(&mango_dir, id)));
    let (scope, config_path) = game.clone().unwrap_or_else(|| ("global".to_string(), global_path.clone()));

    // A per-game edit also points the game's launch options at its config.
    let launch_update = match (appid, mango_action.as_str()) {
        (Some(id), "set" | "reset") => {
            let path = config_path.to_string_lossy().into_owned();
            if !path.chars().all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c)) {
                return Ok(ToolResult::error(format!(
                    "MangoHud config NOT modified: the per-game config path {path:?} has characters that \
                     launch options can't carry unquoted. Set MANGOHUD_CONFIGFILE in Steam yourself."
                )));
            }
            let (current, bridged) = match current_for_edit(env, id as f64)? {
                Ok(current) => current,
                Err(refusal) => {
                    return Ok(ToolResult::error(format!(
                        "{refusal}\nA per-game MangoHud config needs the game's launch options \
                         changed, so the MangoHud config was NOT modified either."
                    )));
                }
            };
            let updated = match with_mangohud_config_file(&current, &path) {
                Ok(updated) => updated,
                Err(why) => return Ok(ToolResult::error(format!("MangoHud config NOT modified: {why}."))),
            };
            Some((id, current, updated, bridged))
        }
        _ => None,
    };
    let write_config = |content: &str| -> Result<()> {
        let dir = config_path.parent().unwrap_or(&mango_dir);
        fs::create_dir_all(dir).map_err(|e| io_error(&e, "mkdir", dir))?;
        backup_file(&config_path).map_err(|e| io_error(&e, "backup", &config_path))?;
        write_atomic(&config_path, content.as_bytes()).map_err(|e| io_error(&e, "open", &config_path))?;
        if let Some((id, current, updated, bridged)) = &launch_update
            && updated != current
        {
            write_edit(env, *id, updated, current, *bridged).map_err(|e| {
                anyhow!(
                    "{e} (the MangoHud config {} was written, but the game's launch options may not point at it; \
                     check them before retrying)",
                    config_path.display()
                )
            })?;
        }
        Ok(())
    };
    let launch_json = |out: &mut Map<String, Value>| {
        if let Some((_, _, updated, bridged)) = &launch_update {
            out.insert("launchOptions".into(), updated.clone().into());
            insert_via(out, *bridged);
            if !mangohud_enabled(updated) {
                out.insert(
                    "note".into(),
                    "MangoHud isn't turned on for this game yet; apply the mangohud preset to see the overlay.".into(),
                );
            }
        }
    };

    match mango_action.as_str() {
        "show" => {
            let mut configs = vec![mangohud_config("global", &global_path)?];
            if let Some((scope, path)) = &game {
                configs.push(mangohud_config(scope, path)?);
            }
            Ok(ToolResult::json(&json!({ "configs": configs })))
        }
        "set" => {
            let Some(settings) = args.str("settings").filter(|s| !s.is_empty()) else {
                return Ok(ToolResult::error(
                    "The \"settings\" parameter is required for mangohud_action=set. Provide comma-separated key=value pairs.",
                ));
            };

            let mut new_settings: IndexMap<String, String> = IndexMap::new();
            for pair in settings.split(',') {
                let trimmed = pair.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let (key, value) = match trimmed.find('=') {
                    None => (trimmed, ""),
                    Some(i) => (trimmed[..i].trim(), trimmed[i + 1..].trim()),
                };
                if let Err(why) = check_mangohud_setting(key, value) {
                    return Ok(ToolResult::error(format!("MangoHud config NOT modified: {why}.")));
                }
                new_settings.insert(key.to_string(), value.to_string());
            }

            let existing = if config_path.exists() { Some(read_text(&config_path)?) } else { None };
            let existing_lines: Vec<&str> = existing.as_deref().map(|e| e.split('\n').collect()).unwrap_or_default();

            let mut updated: Vec<&str> = Vec::new();
            let mut lines: Vec<String> = Vec::new();
            for line in existing_lines {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    lines.push(line.to_string());
                    continue;
                }
                let key = trimmed.find('=').map_or(trimmed, |i| trimmed[..i].trim());
                match new_settings.get_key_value(key) {
                    Some((k, value)) => {
                        lines.push(setting_line(key, value));
                        updated.push(k);
                    }
                    None => lines.push(line.to_string()),
                }
            }
            for (key, value) in &new_settings {
                if !updated.contains(&key.as_str()) {
                    lines.push(setting_line(key, value));
                }
            }

            let content = lines.join("\n");
            write_config(&content)?;

            let mut out = Map::new();
            out.insert("action".into(), "set".into());
            out.insert("scope".into(), scope.into());
            out.insert("path".into(), config_path.to_string_lossy().into());
            out.insert("updatedSettings".into(), js_object(&new_settings).into());
            out.insert("config".into(), content.into());
            launch_json(&mut out);
            Ok(ToolResult::json(&Value::Object(out)))
        }
        _ => {
            write_config(&format!("{MANGOHUD_DEFAULTS}\n"))?;
            let mut out = Map::new();
            out.insert("action".into(), "reset".into());
            out.insert("scope".into(), scope.into());
            out.insert("path".into(), config_path.to_string_lossy().into());
            out.insert("config".into(), MANGOHUD_DEFAULTS.into());
            out.insert("message".into(), "MangoHud config reset to sensible defaults.".into());
            launch_json(&mut out);
            Ok(ToolResult::json(&Value::Object(out)))
        }
    }
}

/// A VDF value as JSON.stringify writes the parsed JS object.
fn vdf_json(value: &VdfValue) -> Value {
    match value {
        VdfValue::Str(s) => Value::String(s.clone()),
        VdfValue::Obj(o) => {
            Value::Object(js_entries(o).into_iter().map(|(k, v)| (k.to_string(), vdf_json(v))).collect())
        }
    }
}

fn fps_corner_position(corner: &str) -> &'static str {
    match corner {
        "1" => "Top-left",
        "2" => "Top-right",
        "3" => "Bottom-right",
        "4" => "Bottom-left",
        _ => "Off",
    }
}

fn matching_entries(system: &VdfObject, matches: impl Fn(&str) -> bool) -> Map<String, Value> {
    js_entries(system)
        .into_iter()
        .filter(|(k, _)| matches(&k.to_lowercase()))
        .map(|(k, v)| (k.to_string(), vdf_json(v)))
        .collect()
}

fn steam_settings() -> Result<ToolResult> {
    let config = get_local_config()?;
    let root = config.get("UserLocalConfigStore").or_else(|| config.get("userlocalconfigstore"));
    let Some(VdfValue::Obj(root)) = root else {
        return Ok(ToolResult::error(
            "Could not read Steam settings: UserLocalConfigStore not found in localconfig.vdf",
        ));
    };

    let mut settings = Map::new();
    match root.get("system").or_else(|| root.get("System")) {
        Some(VdfValue::Obj(system)) => {
            let (value, position) = match system.get("InGameOverlayShowFPSCorner") {
                None => (Value::from("0"), "Off"),
                Some(VdfValue::Str(s)) => (Value::from(s.as_str()), fps_corner_position(s)),
                Some(other) => (vdf_json(other), "Off"),
            };
            settings.insert("fpsOverlay".into(), json!({ "value": value, "position": position }));
            settings.insert(
                "enableOverlay".into(),
                (system.get("EnableGameOverlay") != Some(&VdfValue::Str("0".into()))).into(),
            );
            settings.insert(
                "enableScreenshots".into(),
                (system.get("EnableScreenshots") != Some(&VdfValue::Str("0".into()))).into(),
            );

            let streaming = matching_entries(system, |k| k.contains("streaming") || k.contains("remoteplay"));
            if !streaming.is_empty() {
                settings.insert("streaming".into(), streaming.into());
            }
            let controller = matching_entries(system, |k| k.contains("controller"));
            if !controller.is_empty() {
                settings.insert("controller".into(), controller.into());
            }
        }
        _ => {
            settings.insert(
                "note".into(),
                "System section not found in localconfig.vdf. Settings may be stored elsewhere.".into(),
            );
        }
    }

    Ok(ToolResult::json(&Value::Object(settings)))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};

    use super::*;

    /// Stands in for Steam: whether it runs, the bridge's answers in order, and localconfig.vdf.
    #[derive(Default)]
    struct FakeEnv {
        running: bool,
        answers: RefCell<VecDeque<Result<BridgeOutcome<BridgeLaunchOptions>>>>,
        requests: RefCell<Vec<LaunchOptionsRequest>>,
        file: RefCell<HashMap<i64, String>>,
    }

    impl FakeEnv {
        fn running(answers: Vec<Result<BridgeOutcome<BridgeLaunchOptions>>>) -> Self {
            Self { running: true, answers: RefCell::new(answers.into()), ..Default::default() }
        }
    }

    impl LaunchEnv for FakeEnv {
        fn steam_running(&self) -> bool {
            self.running
        }

        fn bridge(&self, request: &LaunchOptionsRequest) -> Result<BridgeOutcome<BridgeLaunchOptions>> {
            self.requests.borrow_mut().push(request.clone());
            self.answers.borrow_mut().pop_front().expect("an answer for every bridge request")
        }

        fn read(&self, appid: i64) -> Result<Option<String>> {
            Ok(self.file.borrow().get(&appid).cloned())
        }

        fn write(&self, appid: i64, options: &str) -> Result<()> {
            self.file.borrow_mut().insert(appid, options.to_string());
            Ok(())
        }
    }

    fn args(v: Value) -> Args {
        Args(v.as_object().unwrap().clone())
    }

    fn done(options: &str, changed: bool) -> Result<BridgeOutcome<BridgeLaunchOptions>> {
        Ok(BridgeOutcome::Done(BridgeLaunchOptions {
            launch_options: options.into(),
            changed,
            verified_by_readback: false,
        }))
    }

    fn unavailable() -> Result<BridgeOutcome<BridgeLaunchOptions>> {
        Ok(BridgeOutcome::Unavailable("Millennium is not running (no socket at /tmp/x.sock)".into()))
    }

    fn json_of(result: &ToolResult) -> Value {
        serde_json::from_str(&result.text).unwrap()
    }

    #[test]
    fn set_while_steam_runs_goes_through_the_bridge() {
        let env = FakeEnv::running(vec![done("-novid", true)]);
        let result = set_launch_opts(&env, &args(json!({ "appid": 10, "options": "-novid" }))).unwrap();
        let out = json_of(&result);
        assert_eq!((out["via"].as_str(), out["changed"].as_bool()), (Some("millennium"), Some(true)));
        assert_eq!(
            *env.requests.borrow(),
            [LaunchOptionsRequest::Set { appid: 10, options: "-novid".into(), expected: None }]
        );
        assert!(env.file.borrow().is_empty());
    }

    #[test]
    fn set_while_steam_runs_without_the_bridge_is_refused() {
        let env = FakeEnv::running(vec![unavailable()]);
        let result = set_launch_opts(&env, &args(json!({ "appid": 10, "options": "-novid" }))).unwrap();
        assert!(result.is_error);
        assert!(result.text.contains("Millennium bridge: Millennium is not running"), "{}", result.text);
        assert!(result.text.ends_with("Launch options were NOT modified."));
        assert!(env.file.borrow().is_empty());
    }

    #[test]
    fn set_refuses_a_preset_instead_of_ignoring_it() {
        let env = FakeEnv::running(vec![]);
        for call in [
            json!({ "appid": 10, "preset": "skip-intro" }),
            json!({ "appid": 10, "options": "-x", "preset": "force-vulkan" }),
            json!({ "preset": "skip-intro" }),
        ] {
            let result = set_launch_opts(&env, &args(call)).unwrap();
            assert!(result.is_error && result.text.contains("action \"presets\""), "{}", result.text);
        }
        assert!(env.requests.borrow().is_empty());
        let closed = FakeEnv::default();
        assert!(set_launch_opts(&closed, &args(json!({ "appid": 10, "preset": "skip-intro" }))).unwrap().is_error);
        assert!(closed.file.borrow().is_empty());
    }

    #[test]
    fn set_checks_options_before_asking_steam() {
        let env = FakeEnv::running(vec![]);
        let result = set_launch_opts(&env, &args(json!({ "appid": 10, "options": "rm -rf ~; %command%" }))).unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);
        assert!(env.requests.borrow().is_empty());
    }

    #[test]
    fn set_while_steam_is_closed_writes_the_file() {
        let env = FakeEnv::default();
        let out = json_of(&set_launch_opts(&env, &args(json!({ "appid": 10, "options": "-novid" }))).unwrap());
        assert!(out.get("via").is_none());
        assert_eq!(env.file.borrow()[&10], "-novid");
    }

    #[test]
    fn preset_while_steam_runs_merges_into_steams_options() {
        let env = FakeEnv::running(vec![done("-high", false), done("-high -novid", true)]);
        let result = presets(&env, &args(json!({ "appid": 10, "preset": "skip-intro" }))).unwrap();
        let out = json_of(&result);
        assert_eq!(out["newLaunchOptions"], "-high -novid");
        assert_eq!(out["previousLaunchOptions"], "-high");
        assert_eq!(out["via"], "millennium");
        assert_eq!(
            *env.requests.borrow(),
            [
                LaunchOptionsRequest::Get { appid: 10 },
                LaunchOptionsRequest::Set { appid: 10, options: "-high -novid".into(), expected: Some("-high".into()) },
            ]
        );
    }

    #[test]
    fn preset_while_steam_runs_without_the_bridge_is_refused() {
        let env = FakeEnv::running(vec![unavailable()]);
        let result = presets(&env, &args(json!({ "appid": 10, "preset": "skip-intro" }))).unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);
        assert_eq!(env.requests.borrow().len(), 1);
    }

    #[test]
    fn a_bridge_error_is_reported_not_retried_on_the_file() {
        let env = FakeEnv::running(vec![
            done("-high", false),
            Err(anyhow!("The launch options of appid 10 changed in Steam since steam-mcp read them")),
        ]);
        let err = presets(&env, &args(json!({ "appid": 10, "preset": "skip-intro" }))).unwrap_err();
        assert!(err.to_string().contains("changed in Steam"), "{err}");
        assert!(env.file.borrow().is_empty());
    }

    #[test]
    fn get_while_steam_runs_asks_steam_and_falls_back_to_the_file() {
        let env = FakeEnv::running(vec![done("-novid", false)]);
        env.file.borrow_mut().insert(10, "stale".into());
        let out = json_of(&get_launch_opts(&env, &args(json!({ "appid": 10 }))).unwrap());
        assert_eq!((out["launchOptions"].as_str(), out["via"].as_str()), (Some("-novid"), Some("millennium")));

        let env = FakeEnv::running(vec![unavailable()]);
        env.file.borrow_mut().insert(10, "from-file".into());
        let out = json_of(&get_launch_opts(&env, &args(json!({ "appid": 10 }))).unwrap());
        assert_eq!((out["launchOptions"].as_str(), out.get("via")), (Some("from-file"), None));
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("Millennium is not running") && note.contains("may lag"), "{note}");

        let env = FakeEnv::default();
        let out = json_of(&get_launch_opts(&env, &args(json!({ "appid": 10 }))).unwrap());
        assert!(out.get("note").is_none());
    }

    #[test]
    fn a_write_confirmed_by_reading_back_says_so() {
        let verified =
            BridgeLaunchOptions { launch_options: "-novid".into(), changed: true, verified_by_readback: true };
        let env = FakeEnv::running(vec![Ok(BridgeOutcome::Done(verified))]);
        let out = json_of(&set_launch_opts(&env, &args(json!({ "appid": 10, "options": "-novid" }))).unwrap());
        assert_eq!(out["verifiedByReadback"], true);
    }

    #[test]
    fn presets_pass_the_launch_option_check() {
        for preset in PRESETS {
            assert_eq!(check_launch_options(preset.options), Ok(()), "{}", preset.name);
        }
    }

    #[test]
    fn points_launch_options_at_the_mangohud_config() {
        let p = "/home/u/.config/MangoHud/steam-mcp/10.conf";
        let cases = [
            ("", format!("env MANGOHUD_CONFIGFILE={p} %command%")),
            ("-novid -high", format!("env MANGOHUD_CONFIGFILE={p} %command% -novid -high")),
            ("%command%", format!("env MANGOHUD_CONFIGFILE={p} %command%")),
            ("mangohud %command% -dx12", format!("mangohud env MANGOHUD_CONFIGFILE={p} %command% -dx12")),
            (
                "gamescope -f -- gamemoderun %command%",
                format!("gamescope -f -- gamemoderun env MANGOHUD_CONFIGFILE={p} %command%"),
            ),
            ("cd /x; %command%", format!("cd /x; env MANGOHUD_CONFIGFILE={p} %command%")),
            (
                "mangohud env MANGOHUD_CONFIGFILE=/old.conf %command%",
                format!("mangohud env MANGOHUD_CONFIGFILE={p} %command%"),
            ),
            ("MANGOHUD_CONFIGFILE=/old.conf\t%command%", format!("MANGOHUD_CONFIGFILE={p}\t%command%")),
        ];
        for (current, expected) in cases {
            assert_eq!(with_mangohud_config_file(current, p), Ok(expected), "{current:?}");
        }
        // Applying it twice changes nothing more.
        let once = with_mangohud_config_file("mangohud %command%", p).unwrap();
        assert_eq!(with_mangohud_config_file(&once, p), Ok(once));
        // Values it can't delimit safely are refused, not cut.
        for current in [
            "env MANGOHUD_CONFIGFILE=\"/home/u/old config.conf\" %command%",
            "MANGOHUD_CONFIGFILE=/old.conf;%command%",
            "MANGOHUD_CONFIGFILE=/old\\ x.conf %command%",
            "MANGOHUD_CONFIGFILE=$HOME/x.conf %command%",
        ] {
            assert!(with_mangohud_config_file(current, p).is_err(), "{current:?}");
        }
        // The merged result still takes presets.
        assert_eq!(
            merge_launch_options(&with_mangohud_config_file("", p).unwrap(), "gamemoderun %command%"),
            format!("gamemoderun env MANGOHUD_CONFIGFILE={p} %command%")
        );
    }

    #[test]
    fn detects_mangohud_in_launch_options() {
        assert!(mangohud_enabled("mangohud %command%"));
        assert!(mangohud_enabled("MANGOHUD=1 %command%"));
        assert!(!mangohud_enabled("env MANGOHUD_CONFIGFILE=/x.conf %command%"));
    }

    #[test]
    fn merges_launch_options_like_ts() {
        assert_eq!(merge_launch_options("", "-novid"), "-novid");
        assert_eq!(merge_launch_options("-novid", "-novid"), "-novid");
        assert_eq!(merge_launch_options("-high", "-novid"), "-high -novid");
        assert_eq!(merge_launch_options("-high", "mangohud %command%"), "mangohud %command%");
        assert_eq!(
            merge_launch_options("gamemoderun %command% -high", "mangohud %command%"),
            "mangohud gamemoderun %command% -high"
        );
        assert_eq!(
            merge_launch_options("mangohud %command%", "mangohud gamemoderun %command%"),
            "gamemoderun mangohud %command%"
        );
    }

    #[test]
    fn orders_updated_settings_like_object_from_entries() {
        let mut m = IndexMap::new();
        m.insert("fps".to_string(), String::new());
        m.insert("10".to_string(), "a".to_string());
        m.insert("2".to_string(), "b".to_string());
        let keys: Vec<String> = js_object(&m).keys().cloned().collect();
        assert_eq!(keys, ["2", "10", "fps"]);
    }
}
