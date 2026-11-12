//! game_config: launch options, launch presets, MangoHud configs and Steam client settings.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::steam::launch_options::{check_launch_options, check_mangohud_setting};
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

const STEAM_RUNNING_WARNING: &str = "WARNING: Steam is currently running (or that couldn't be checked)!\nModifying localconfig.vdf while Steam is running will likely result in your changes being overwritten when Steam exits.\nPlease close Steam first, then try again.\n\nLaunch options were NOT modified.";

const MANGOHUD_DEFAULTS: &str =
    "fps\ngpu_stats\ngpu_temp\ncpu_stats\ncpu_temp\nram\nvram\nframe_timing\nposition=top-left";

pub fn tool() -> Tool {
    Tool {
        name: "game_config",
        description: "Launch options, presets, MangoHud, Steam settings",
        params: vec![
            Param::new("action", ParamType::Enum(ACTIONS)).required(),
            Param::new("appid", ParamType::Number),
            Param::new("options", ParamType::String),
            Param::new("preset", ParamType::String),
            Param::new("mangohud_action", ParamType::Enum(MANGOHUD_ACTIONS)),
            Param::new("settings", ParamType::String),
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
    let result = match action.as_str() {
        "get_launch_opts" => get_launch_opts(&args),
        "set_launch_opts" => set_launch_opts(&args),
        "presets" => presets(&args),
        "mangohud" => mangohud(&args),
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

fn launch_options_of(appid: f64) -> Result<Option<String>> {
    if appid.fract() != 0.0 {
        get_local_config()?;
        return Ok(None);
    }
    get_app_launch_options(appid as i64)
}

fn get_launch_opts(args: &Args) -> Result<ToolResult> {
    let Some(appid) = truthy_appid(args) else {
        return Ok(ToolResult::error("An appid is required for get_launch_opts."));
    };
    let options = launch_options_of(appid)?;
    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "launchOptions": options.clone().unwrap_or_default(),
        "hasLaunchOptions": options.is_some_and(|o| !o.is_empty()),
    })))
}

fn set_launch_opts(args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    let Some(appid) = truthy_appid(args) else {
        return Ok(ToolResult::error("An appid is required for set_launch_opts."));
    };
    let Some(options) = args.str("options") else {
        return Ok(ToolResult::error("The \"options\" parameter is required for set_launch_opts."));
    };
    if let Err(why) = check_launch_options(&options) {
        return Ok(ToolResult::error(format!(
            "Launch options were NOT modified: {why}.\nAccepted are game arguments, or \
             [VAR=value …] [wrapper …] %command% [game arguments …] with listed variables and wrappers. \
             For anything else, set the launch options in Steam's game properties yourself."
        )));
    }
    if steam_may_be_running() {
        return Ok(ToolResult::error(STEAM_RUNNING_WARNING));
    }

    set_app_launch_options(integral_appid(appid)?, &options)?;

    let shown = if options.is_empty() { "(cleared)" } else { options.as_str() };
    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "launchOptions": options,
        "success": true,
        "message": format!("Launch options for appid {} set to: {shown}", js_num(appid)),
    })))
}

fn presets(args: &Args) -> Result<ToolResult> {
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

    if steam_may_be_running() {
        return Ok(ToolResult::error(STEAM_RUNNING_WARNING));
    }

    let previous = launch_options_of(appid)?.unwrap_or_default();
    let Some(preset) = preset else {
        bail!("Cannot read properties of undefined (reading 'includes')");
    };
    let new_options = merge_launch_options(&previous, preset.options);

    set_app_launch_options(integral_appid(appid)?, &new_options)?;

    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "preset": preset_name,
        "description": preset.description,
        "previousLaunchOptions": if previous.is_empty() { "(none)" } else { previous.as_str() },
        "newLaunchOptions": new_options,
        "success": true,
    })))
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

fn mangohud(args: &Args) -> Result<ToolResult> {
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
            if steam_may_be_running() {
                return Ok(ToolResult::error(format!(
                    "{STEAM_RUNNING_WARNING}\nA per-game MangoHud config needs the game's launch options \
                     changed, so the MangoHud config was NOT modified either."
                )));
            }
            let current = get_app_launch_options(id)?.unwrap_or_default();
            let updated = match with_mangohud_config_file(&current, &path) {
                Ok(updated) => updated,
                Err(why) => return Ok(ToolResult::error(format!("MangoHud config NOT modified: {why}."))),
            };
            Some((id, current, updated))
        }
        _ => None,
    };
    let write_config = |content: &str| -> Result<()> {
        let dir = config_path.parent().unwrap_or(&mango_dir);
        fs::create_dir_all(dir).map_err(|e| io_error(&e, "mkdir", dir))?;
        backup_file(&config_path).map_err(|e| io_error(&e, "backup", &config_path))?;
        write_atomic(&config_path, content.as_bytes()).map_err(|e| io_error(&e, "open", &config_path))?;
        if let Some((id, current, updated)) = &launch_update
            && updated != current
        {
            set_app_launch_options(*id, updated)?;
        }
        Ok(())
    };
    let launch_json = |out: &mut Map<String, Value>| {
        if let Some((_, _, updated)) = &launch_update {
            out.insert("launchOptions".into(), updated.clone().into());
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
    use super::*;

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
