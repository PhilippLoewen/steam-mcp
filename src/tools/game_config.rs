//! game_config: launch options, launch presets, MangoHud configs and Steam client settings.

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::steam::paths::is_steam_running;
use crate::steam::userdata::{get_app_launch_options, get_local_config, set_app_launch_options};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{home_dir, io_error, js_num, num, read_text};
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

const STEAM_RUNNING_WARNING: &str = "WARNING: Steam is currently running!\nModifying localconfig.vdf while Steam is running will likely result in your changes being overwritten when Steam exits.\nPlease close Steam first, then try again.\n\nLaunch options were NOT modified.";

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
        handler: |args| boxed(handle(args)),
    }
}

async fn handle(args: Args) -> ToolResult {
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
    if is_steam_running() {
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

    if is_steam_running() {
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

fn mangohud(args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    let mango_action = args.str("mangohud_action").unwrap_or_else(|| "show".to_string());
    let mango_dir = home_dir().join(".config").join("MangoHud");
    let global_path = mango_dir.join("MangoHud.conf");
    let appid = truthy_appid(args);
    let game = appid.map(|id| {
        (format!("game-specific (appid: {})", js_num(id)), mango_dir.join(format!("wine-{}.conf", js_num(id))))
    });
    let (scope, config_path) = game.clone().unwrap_or_else(|| ("global".to_string(), global_path.clone()));

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
                match trimmed.find('=') {
                    None => new_settings.insert(trimmed.to_string(), String::new()),
                    Some(i) => {
                        new_settings.insert(trimmed[..i].trim().to_string(), trimmed[i + 1..].trim().to_string())
                    }
                };
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

            fs::create_dir_all(&mango_dir).map_err(|e| io_error(&e, "mkdir", &mango_dir))?;
            let content = lines.join("\n");
            fs::write(&config_path, &content).map_err(|e| io_error(&e, "open", &config_path))?;

            Ok(ToolResult::json(&json!({
                "action": "set",
                "scope": scope,
                "path": config_path.to_string_lossy(),
                "updatedSettings": js_object(&new_settings),
                "config": content,
            })))
        }
        _ => {
            fs::create_dir_all(&mango_dir).map_err(|e| io_error(&e, "mkdir", &mango_dir))?;
            fs::write(&config_path, format!("{MANGOHUD_DEFAULTS}\n"))
                .map_err(|e| io_error(&e, "open", &config_path))?;
            Ok(ToolResult::json(&json!({
                "action": "reset",
                "scope": scope,
                "path": config_path.to_string_lossy(),
                "config": MANGOHUD_DEFAULTS,
                "message": "MangoHud config reset to sensible defaults.",
            })))
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
