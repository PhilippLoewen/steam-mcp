//! steam_status: client status, download queue and download progress.

use anyhow::Result;
use serde_json::{Value, json};

use crate::steam::manifests::read_all_manifests;
use crate::steam::paths::{SteamState, get_library_folders, get_user_config, steam_state};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{format_bytes, read_dir_names, to_fixed};
use crate::vdf::{VdfValue, parse_int, parse_vdf};

pub fn tool() -> Tool {
    Tool {
        name: "steam_status",
        description: "Steam client status, download queue, or download progress",
        params: vec![
            Param::new("action", ParamType::Enum(&["status", "queue", "progress"])).default("status"),
            Param::new("appid", ParamType::Number),
        ],
        writes: &[],
        destructive: false,
        open_world: false,
        handler: |args| {
            boxed(async move {
                match args.str("action").as_deref() {
                    Some("queue") => ToolResult::or_error(queue(), "Error checking download queue"),
                    Some("progress") => ToolResult::or_error(progress(&args), "Error checking download progress"),
                    _ => status(),
                }
            })
        },
    }
}

/// AppManifest StateFlags bits.
const STATE_FLAGS: &[(i64, &str)] = &[
    (1, "Invalid"),
    (2, "Uninstalled"),
    (4, "FullyInstalled"),
    (16, "Encrypted"),
    (32, "Locked"),
    (64, "FilesMissing"),
    (128, "FilesCorrupt"),
    (256, "UpdateRunning"),
    (512, "UpdateRequired"),
    (1024, "UpdateStarted"),
    (2048, "Uninstalling"),
    (4096, "BackupRunning"),
    (8192, "Reconfiguring"),
    (16384, "Validating"),
    (32768, "AddingFiles"),
    (65536, "Preallocating"),
    (131072, "Downloading"),
    (262144, "Staging"),
    (524288, "Committing"),
    (1048576, "UpdateStopping"),
];

fn decode_state_flags(flags: i64) -> Vec<&'static str> {
    // JS bitwise ops work on the low 32 bits.
    let flags = flags as i32 as i64;
    let names: Vec<&str> = STATE_FLAGS.iter().filter(|(bit, _)| flags & bit != 0).map(|(_, n)| *n).collect();
    if names.is_empty() { vec!["Unknown"] } else { names }
}

fn status() -> ToolResult {
    let user = get_user_config()
        .ok()
        .map(|c| json!({ "accountName": c.account_name, "personaName": c.persona_name, "steamId64": c.steam_id64 }));
    let installed = read_all_manifests().map(|m| m.len()).unwrap_or(0);
    ToolResult::json(&json!({
        // null when the process list couldn't be read.
        "steamRunning": match steam_state() {
            SteamState::Running => Value::Bool(true),
            SteamState::NotRunning => Value::Bool(false),
            SteamState::Unknown => Value::Null,
        },
        "currentUser": user,
        "installedGames": installed,
    }))
}

fn queue() -> Result<ToolResult> {
    let manifests = read_all_manifests()?;
    let games: Vec<Value> = manifests
        .iter()
        .filter(|m| m.state_flags != 4)
        .map(|m| {
            json!({
                "appid": m.appid,
                "name": m.name,
                "stateFlags": m.state_flags,
                "states": decode_state_flags(m.state_flags),
                "libraryPath": m.library_path,
            })
        })
        .collect();
    Ok(ToolResult::json(&json!({
        "totalInstalled": manifests.len(),
        "nonNormalCount": games.len(),
        "games": games,
    })))
}

fn pct(done: i64, total: i64) -> String {
    format!("{}%", to_fixed(done as f64 / total as f64 * 100.0, 1))
}

fn progress(args: &Args) -> Result<ToolResult> {
    let filter = args.f64("appid");
    let mut results = Vec::new();

    for folder in get_library_folders()? {
        let steamapps = folder.join("steamapps");
        if !steamapps.exists() {
            continue;
        }
        let Ok(entries) = read_dir_names(&steamapps) else { continue };
        for entry in entries {
            if !entry.starts_with("appmanifest_") || !entry.ends_with(".acf") {
                continue;
            }
            // Read fresh, not cached: download byte counts change constantly.
            let Ok(content) = std::fs::read_to_string(steamapps.join(&entry)) else { continue };
            let parsed = parse_vdf(&content);
            let state = parsed.get("AppState").and_then(VdfValue::as_obj).unwrap_or(&parsed);
            let int = |key: &str| state.get(key).and_then(VdfValue::as_str).and_then(parse_int).unwrap_or(0);
            let appid = int("appid");
            let flags = int("StateFlags");

            match filter {
                Some(want) if appid as f64 != want => continue,
                None if flags == 4 => continue,
                _ => {}
            }

            let (to_dl, dl, to_stage, staged) =
                (int("BytesToDownload"), int("BytesDownloaded"), int("BytesToStage"), int("BytesStaged"));
            let dl_pct = if to_dl > 0 { pct(dl, to_dl) } else { "N/A".to_string() };
            let stage_pct = if to_stage > 0 { pct(staged, to_stage) } else { "N/A".to_string() };
            let total = to_dl + to_stage;
            let overall = if total > 0 {
                pct(dl + staged, total)
            } else if flags == 4 {
                "100%".to_string()
            } else {
                "N/A".to_string()
            };
            let name = match state.get("name") {
                Some(VdfValue::Str(s)) => s.clone(),
                Some(VdfValue::Obj(_)) => "[object Object]".to_string(),
                None => format!("Unknown ({appid})"),
            };

            results.push(json!({
                "appid": appid,
                "name": name,
                "stateFlags": flags,
                "states": decode_state_flags(flags),
                "bytesToDownload": to_dl,
                "bytesDownloaded": dl,
                "bytesToStage": to_stage,
                "bytesStaged": staged,
                "downloadProgress": format!("{} / {} ({dl_pct})", format_bytes(dl as f64), format_bytes(to_dl as f64)),
                "stageProgress": format!("{} / {} ({stage_pct})", format_bytes(staged as f64), format_bytes(to_stage as f64)),
                "overallProgress": overall,
                "libraryPath": folder,
            }));
        }
    }

    if results.is_empty() {
        return Ok(ToolResult::text(match filter {
            Some(id) => format!("No manifest found for appid {}, or it is fully installed.", crate::util::js_num(id)),
            None => "No active downloads or updates.".to_string(),
        }));
    }
    Ok(ToolResult::json(&Value::Array(results)))
}
