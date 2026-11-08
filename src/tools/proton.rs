//! proton: Proton/Wine compatibility data, ProtonDB ratings, prefixes and crash logs.

use std::collections::HashMap;
use std::fs::{self, DirEntry};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use indexmap::IndexSet;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::steam::api::HTTP;
use crate::steam::compat::{
    get_all_proton_version_mappings, get_compat_data_size, get_compat_overrides, get_game_proton_version,
    get_installed_proton_versions,
};
use crate::steam::library::get_libraries;
use crate::steam::manifests::read_all_manifests;
use crate::steam::paths::{get_library_folders, get_steam_dir};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{count_files, dir_size, format_bytes, js_num, locale_cmp, num};
use crate::vdf::parse_int;

const ACTIONS: &[&str] = &["list_games", "info", "versions", "db_rating", "recommend", "prefix", "crash_logs"];

pub fn tool() -> Tool {
    Tool {
        name: "proton",
        description: "Proton/Wine compat: list games, info, versions, DB rating, recommend, prefix, logs",
        params: vec![
            Param::new("action", ParamType::Enum(ACTIONS)).required(),
            Param::new("appid", ParamType::Number),
            Param::new("lines", ParamType::Number),
        ],
        handler: |args| boxed(handle(args)),
    }
}

/// A JS-number appid: `text` is `String(appid)`, `int` is set when it can match a real (integral) appid.
#[derive(Clone)]
struct AppId {
    value: f64,
    text: String,
    int: Option<i64>,
}

impl AppId {
    fn new(value: f64) -> Self {
        Self { value, text: js_num(value), int: (value.fract() == 0.0).then_some(value as i64) }
    }

    fn json(&self) -> Value {
        num(self.value)
    }
}

async fn handle(args: Args) -> ToolResult {
    let action = args.str("action").unwrap_or_default();
    let appid = args.f64("appid").map(AppId::new);
    if action != "list_games" && action != "versions" && appid.is_none() {
        return ToolResult::error(format!("appid is required for action \"{action}\""));
    }
    let prefix = format!("Error in proton/{action}");
    match (action.as_str(), appid) {
        ("list_games", _) => ToolResult::or_error(list_games(), &prefix),
        ("versions", _) => ToolResult::or_error(versions(), &prefix),
        ("info", Some(id)) => ToolResult::or_error(info(&id), &prefix),
        ("db_rating", Some(id)) => ToolResult::or_error(db_rating(&id).await, &prefix),
        ("recommend", Some(id)) => ToolResult::or_error(recommend(&id).await, &prefix),
        ("prefix", Some(id)) => ToolResult::or_error(wine_prefix(&id), &prefix),
        ("crash_logs", Some(id)) => ToolResult::or_error(crash_logs(&id, args.f64("lines").unwrap_or(50.0)), &prefix),
        _ => ToolResult::error(format!("{prefix}: unknown action")),
    }
}

/// Directory entries in Node `readdirSync` order (libuv sorts them by name bytes).
fn sorted_entries(dir: &Path) -> std::io::Result<Vec<DirEntry>> {
    let mut entries: Vec<DirEntry> = fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(DirEntry::file_name);
    Ok(entries)
}

fn is_dir(entry: &DirEntry) -> bool {
    entry.file_type().is_ok_and(|t| t.is_dir())
}

fn is_file(entry: &DirEntry) -> bool {
    entry.file_type().is_ok_and(|t| t.is_file())
}

/// Node `path.extname`, lowercased.
fn extname_lower(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && !name[..i].bytes().all(|b| b == b'.') => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

fn find_files(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = sorted_entries(dir) else { return found };
    for entry in entries {
        let path = entry.path();
        if is_dir(&entry) {
            found.extend(find_files(&path, extensions));
        } else if is_file(&entry) {
            let ext = extname_lower(&entry.file_name().to_string_lossy());
            if extensions.contains(&ext.as_str()) {
                found.push(path);
            }
        }
    }
    found
}

/// The start index of JS `array.slice(start)` for an array of `len` items.
fn js_slice_start(len: usize, start: f64) -> usize {
    let start = start.trunc();
    if start < 0.0 { (len as f64 + start).max(0.0) as usize } else { start.min(len as f64) as usize }
}

fn read_tail(path: &Path, n: f64) -> String {
    let Ok(bytes) = fs::read(path) else { return "(unable to read file)".to_string() };
    let content = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = content.split('\n').collect();
    let start = (lines.len() as f64 - n).max(0.0);
    lines[js_slice_start(lines.len(), start)..].join("\n")
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}

fn list_games() -> Result<ToolResult> {
    let manifests = read_all_manifests()?;
    let libraries = get_libraries()?;
    let names: HashMap<i64, &str> = manifests.iter().map(|m| (m.appid, m.name.as_str())).collect();

    let override_appids: Vec<i64> = get_compat_overrides().map(|o| o.into_keys().collect()).unwrap_or_default();

    let mut compatdata_appids: IndexSet<i64> = IndexSet::new();
    for lib in libraries.iter().filter(|l| l.mounted) {
        let Ok(entries) = sorted_entries(&lib.path.join("steamapps/compatdata")) else { continue };
        for entry in entries.iter().filter(|e| is_dir(e)) {
            if let Some(id) = parse_int(&entry.file_name().to_string_lossy()) {
                compatdata_appids.insert(id);
            }
        }
    }

    let proton_appids: IndexSet<i64> =
        override_appids.into_iter().chain(compatdata_appids).filter(|id| names.contains_key(id)).collect();

    let mappings = get_all_proton_version_mappings()?;
    let mut results: Vec<(String, Value)> = proton_appids
        .into_iter()
        .map(|appid| {
            let prefix_size =
                manifests.iter().find(|m| m.appid == appid).map_or(0, |m| get_compat_data_size(&m.library_path, appid));
            let name = names.get(&appid).map_or_else(|| format!("Unknown ({appid})"), |n| n.to_string());
            let entry = json!({
                "appid": appid,
                "name": name,
                "protonVersion": mappings.get(&appid).map_or("Unknown", String::as_str),
                "prefixSize": format_bytes(prefix_size as f64),
            });
            (name, entry)
        })
        .collect();
    results.sort_by(|a, b| locale_cmp(&a.0, &b.0));
    let games: Vec<Value> = results.into_iter().map(|(_, v)| v).collect();

    Ok(ToolResult::json(&json!({ "count": games.len(), "games": games })))
}

fn info(appid: &AppId) -> Result<ToolResult> {
    let version = match appid.int {
        Some(id) => get_game_proton_version(id)?,
        None => {
            get_compat_overrides()?;
            None
        }
    };
    let Some(version) = version else {
        return Ok(ToolResult::text(format!(
            "No Proton/compatibility data found for appid {}. This game may be native Linux or not installed.",
            appid.text
        )));
    };

    let manifests = read_all_manifests()?;
    let (prefix_path, prefix_size) = match manifests.iter().find(|m| Some(m.appid) == appid.int) {
        Some(m) => (
            path_str(&m.library_path.join("steamapps/compatdata").join(&appid.text)),
            get_compat_data_size(&m.library_path, m.appid),
        ),
        None => (String::new(), 0),
    };

    Ok(ToolResult::json(&json!({
        "appid": appid.json(),
        "protonVersion": version,
        "prefixPath": prefix_path,
        "prefixSize": format_bytes(prefix_size as f64),
        "hasCompatData": prefix_size > 0,
    })))
}

fn versions() -> Result<ToolResult> {
    let manifests = read_all_manifests()?;
    let mappings = get_all_proton_version_mappings()?;
    let mut usage: HashMap<&str, Vec<Value>> = HashMap::new();
    for m in &manifests {
        if let Some(ver) = mappings.get(&m.appid) {
            usage.entry(ver.as_str()).or_default().push(json!({ "appid": m.appid, "name": m.name }));
        }
    }

    let mut versions: Vec<(String, Value)> = get_installed_proton_versions()?
        .into_iter()
        .map(|v| {
            let entry = json!({
                "name": v.name,
                "path": path_str(&v.path),
                "size": format_bytes(v.size as f64),
                "gamesUsing": usage.get(v.name.as_str()).cloned().unwrap_or_default(),
            });
            (v.name, entry)
        })
        .collect();
    versions.sort_by(|a, b| locale_cmp(&a.0, &b.0));
    let versions: Vec<Value> = versions.into_iter().map(|(_, v)| v).collect();

    Ok(ToolResult::json(&json!({ "count": versions.len(), "versions": versions })))
}

fn protondb_url(appid: &AppId) -> String {
    format!("https://www.protondb.com/api/v1/reports/summaries/{}.json", appid.text)
}

async fn db_rating(appid: &AppId) -> Result<ToolResult> {
    let response = HTTP.get(protondb_url(appid)).send().await?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(ToolResult::text(format!(
            "No ProtonDB data found for appid {}. This game may not have been reported on ProtonDB.",
            appid.text
        )));
    }
    if !status.is_success() {
        return Ok(ToolResult::error(format!(
            "ProtonDB API returned status {} for appid {}.",
            status.as_u16(),
            appid.text
        )));
    }
    let data: Value = response.json().await?;
    if data.is_null() {
        bail!("Cannot read properties of null (reading 'tier')");
    }

    let mut out = Map::new();
    out.insert("appid".into(), appid.json());
    for (key, field) in [
        ("tier", "tier"),
        ("bestReportedTier", "bestReportedTier"),
        ("trendingTier", "trendingTier"),
        ("confidence", "confidence"),
        ("totalReports", "total"),
    ] {
        if let Some(v) = data.get(field) {
            out.insert(key.into(), v.clone());
        }
    }
    Ok(ToolResult::json(&Value::Object(out)))
}

/// A value as JS string concatenation renders it; `None` is `undefined`.
fn js_concat(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.as_f64().map_or_else(|| n.to_string(), js_num),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Array(a)) => {
            a.iter().map(|v| if v.is_null() { String::new() } else { js_concat(Some(v)) }).collect::<Vec<_>>().join(",")
        }
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// The ProtonDB tier, or None when the summary could not be fetched.
async fn fetch_tier(appid: &AppId) -> Option<Option<Value>> {
    let response = HTTP.get(protondb_url(appid)).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data: Value = response.json().await.ok()?;
    if data.is_null() {
        return None;
    }
    Some(data.get("tier").cloned())
}

async fn recommend(appid: &AppId) -> Result<ToolResult> {
    let fetched = fetch_tier(appid).await;
    let available = fetched.is_some();
    let tier: Option<Value> = fetched.unwrap_or_else(|| Some("unknown".into()));

    let current_version = appid
        .int
        .and_then(|id| get_game_proton_version(id).ok().flatten())
        .unwrap_or_else(|| "Not configured".to_string());

    let installed: Vec<String> = get_installed_proton_versions()?.into_iter().map(|v| v.name).collect();

    let tier_str = tier.as_ref().and_then(Value::as_str);
    let recommendation = if !available {
        "Could not fetch ProtonDB data. Check https://www.protondb.com/ manually for compatibility reports.".to_string()
    } else if tier_str == Some("native") {
        "This game has a native Linux build. Proton is not needed unless the native version has issues.".to_string()
    } else if matches!(tier_str, Some("platinum" | "gold")) {
        "This game runs well with Proton. Your current configuration should be fine. If you experience minor issues, try updating to the latest stable Proton version.".to_string()
    } else if matches!(tier_str, Some("silver" | "bronze")) {
        let has_ge = installed.iter().any(|v| {
            let v = v.to_lowercase();
            v.contains("ge-proton") || v.contains("ge_proton")
        });
        let advice = if has_ge {
            "Try switching to one of your installed GE-Proton versions, which often include extra patches for problematic games."
        } else {
            "Consider installing GE-Proton (a community build with extra patches) for better compatibility. Download from https://github.com/GloriousEggroll/proton-ge-custom/releases and place in ~/.steam/root/compatibilitytools.d/"
        };
        format!("This game has mixed compatibility reports. {advice}")
    } else if tier_str == Some("borked") {
        format!(
            "WARNING: This game is reported as borked on ProtonDB, meaning it does not work or has severe issues. Check https://www.protondb.com/app/{} for workarounds or wait for future Proton updates.",
            appid.text
        )
    } else {
        format!(
            "ProtonDB tier is \"{}\". Check https://www.protondb.com/app/{} for details.",
            js_concat(tier.as_ref()),
            appid.text
        )
    };

    let mut out = Map::new();
    out.insert("appid".into(), appid.json());
    if let Some(t) = tier {
        out.insert("protonDbTier".into(), t);
    }
    out.insert("currentProtonVersion".into(), current_version.into());
    out.insert("installedVersions".into(), json!(installed));
    out.insert("recommendation".into(), recommendation.into());
    Ok(ToolResult::json(&Value::Object(out)))
}

fn find_compat_dir(appid: &AppId) -> Result<Option<PathBuf>> {
    Ok(get_library_folders()?
        .into_iter()
        .map(|lib| lib.join("steamapps/compatdata").join(&appid.text))
        .find(|candidate| candidate.exists()))
}

fn windows_version(system_reg: &Path) -> String {
    let Ok(bytes) = fs::read(system_reg) else { return "Unknown".to_string() };
    let content = String::from_utf8_lossy(&bytes);
    let capture = |key: &str| {
        Regex::new(&format!(r#""{key}"="([^"]+)""#)).expect("valid regex").captures(&content).map(|c| c[1].to_string())
    };
    match capture("ProductName") {
        Some(product) => match capture("CSDVersion") {
            Some(csd) => format!("{product} {csd}"),
            None => product,
        },
        None => "Unknown".to_string(),
    }
}

fn wine_prefix(appid: &AppId) -> Result<ToolResult> {
    let Some(compat_dir) = find_compat_dir(appid)? else {
        return Ok(ToolResult::text(format!(
            "No compatibility data (Wine prefix) found for appid {}. The game may be native Linux or not installed.",
            appid.text
        )));
    };

    let pfx = compat_dir.join("pfx");
    let has_pfx = pfx.exists();
    let has_drive_c = has_pfx && pfx.join("drive_c").exists();
    let has_system_reg = has_pfx && pfx.join("system.reg").exists();
    let windows_version = if has_system_reg { windows_version(&pfx.join("system.reg")) } else { "Unknown".to_string() };

    let prefix_size = dir_size(&compat_dir);
    let file_count = count_files(&compat_dir);

    let steam_user = pfx.join("drive_c/users/steamuser");
    let user_dirs: Vec<String> = if steam_user.exists() {
        sorted_entries(&steam_user)
            .map(|entries| {
                entries.iter().filter(|e| is_dir(e)).map(|e| e.file_name().to_string_lossy().into_owned()).collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(ToolResult::json(&json!({
        "appid": appid.json(),
        "prefixPath": path_str(&compat_dir),
        "prefixSize": format_bytes(prefix_size as f64),
        "prefixSizeBytes": prefix_size,
        "windowsVersion": windows_version,
        "hasPfx": has_pfx,
        "hasDriveC": has_drive_c,
        "hasSystemReg": has_system_reg,
        "fileCount": file_count,
        "userDirs": user_dirs,
    })))
}

fn crash_entry(path: &Path, size: u64, kind: &str) -> Value {
    json!({ "path": path_str(path), "size": format_bytes(size as f64), "type": kind })
}

fn log_entry(path: &Path, content: String) -> Value {
    json!({ "path": path_str(path), "content": content })
}

fn crash_logs(appid: &AppId, lines: f64) -> Result<ToolResult> {
    let compat_dir = find_compat_dir(appid)?;
    let mut crash_files: Vec<Value> = Vec::new();
    let mut log_contents: Vec<Value> = Vec::new();

    let push_crash = |files: &mut Vec<Value>, path: &Path, size: u64, kind: &str| {
        files.push(crash_entry(path, size, kind));
    };

    if let Some(pfx) = compat_dir.map(|d| d.join("pfx")).filter(|p| p.exists()) {
        for path in find_files(&pfx, &[".dmp", ".mdmp"]) {
            if let Some(size) = file_size(&path) {
                push_crash(&mut crash_files, &path, size, "crash_dump");
            }
        }

        for path in find_files(&pfx, &[".log"]) {
            if let Some(size) = file_size(&path) {
                push_crash(&mut crash_files, &path, size, "log");
                if size > 0 {
                    log_contents.push(log_entry(&path, read_tail(&path, lines)));
                }
            }
        }

        let local_app_data = pfx.join("drive_c/users/steamuser/AppData/Local");
        let crash_dumps = local_app_data.join("CrashDumps");
        if crash_dumps.exists()
            && let Ok(entries) = sorted_entries(&crash_dumps)
        {
            for entry in entries.iter().filter(|e| is_file(e)) {
                let path = entry.path();
                if let Some(size) = file_size(&path) {
                    push_crash(&mut crash_files, &path, size, "crash_dump_dir");
                }
            }
        }

        if local_app_data.exists() {
            for path in find_files(&local_app_data, &[".log"]) {
                let key = path_str(&path);
                if crash_files.iter().any(|f| f["path"].as_str() == Some(key.as_str())) {
                    continue;
                }
                if let Some(size) = file_size(&path) {
                    push_crash(&mut crash_files, &path, size, "appdata_log");
                    if size > 0 {
                        log_contents.push(log_entry(&path, read_tail(&path, lines)));
                    }
                }
            }
        }
    }

    if let Ok(steam_dir) = get_steam_dir() {
        let logs_dir = steam_dir.join("logs");
        if logs_dir.exists()
            && let Ok(entries) = sorted_entries(&logs_dir)
        {
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !(name.ends_with(".log") || name.ends_with(".txt")) {
                    continue;
                }
                let path = entry.path();
                let Ok(bytes) = fs::read(&path) else { continue };
                let content = String::from_utf8_lossy(&bytes);
                if !content.contains(&appid.text) {
                    continue;
                }
                let Some(size) = file_size(&path) else { continue };
                push_crash(&mut crash_files, &path, size, "steam_log");
                let matching: Vec<&str> = content.split('\n').filter(|l| l.contains(&appid.text)).collect();
                let relevant = matching[js_slice_start(matching.len(), -lines)..].join("\n");
                if !relevant.is_empty() {
                    log_contents.push(log_entry(&path, relevant));
                }
            }
        }
    }

    Ok(ToolResult::json(&json!({
        "appid": appid.json(),
        "totalFilesFound": crash_files.len(),
        "crashFiles": crash_files,
        "logContents": log_contents,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_like_js() {
        assert_eq!(js_slice_start(10, -3.0), 7);
        assert_eq!(js_slice_start(10, -0.0), 0);
        assert_eq!(js_slice_start(10, -20.0), 0);
        assert_eq!(js_slice_start(10, 4.7), 4);
        assert_eq!(js_slice_start(10, 15.0), 10);
    }

    #[test]
    fn extname_matches_node() {
        assert_eq!(extname_lower("crash.DMP"), ".dmp");
        assert_eq!(extname_lower(".log"), "");
        assert_eq!(extname_lower("noext"), "");
        assert_eq!(extname_lower("a.b.log"), ".log");
    }
}
