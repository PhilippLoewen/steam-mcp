//! Proton / compatibility tool configuration.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::util::{dir_size, read_text, sorted_entries};
use crate::vdf::{VdfExt, js_entries, parse_int, parse_vdf};

use super::paths::{get_library_folders, get_steam_dir};

#[derive(Debug, Clone)]
pub struct CompatOverride {
    /// Tool name, e.g. "proton_9" or "GE-Proton10-33".
    pub dest: String,
}

#[derive(Debug, Clone)]
pub struct ProtonVersion {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

/// CompatToolMapping from config/config.vdf, keyed by appid (ascending, like a JS record).
pub fn get_compat_overrides() -> Result<BTreeMap<i64, CompatOverride>> {
    let path = get_steam_dir()?.join("config/config.vdf");
    let parsed = parse_vdf(&read_text(&path)?);
    let Some(mapping) = parsed.navigate(&["InstallConfigStore", "Software", "Valve", "Steam", "CompatToolMapping"])
    else {
        return Ok(BTreeMap::new());
    };

    let mut result = BTreeMap::new();
    for (key, value) in js_entries(mapping) {
        let (Some(appid), Some(obj)) = (parse_int(key), value.as_obj()) else { continue };
        result.insert(appid, CompatOverride { dest: obj.str_or_empty(&["name", "Name"]) });
    }
    Ok(result)
}

/// Size of steamapps/compatdata/{appid} in `library_path`, 0 when absent.
pub fn get_compat_data_size(library_path: &Path, appid: i64) -> u64 {
    let dir = library_path.join("steamapps/compatdata").join(appid.to_string());
    if dir.exists() { dir_size(&dir) } else { 0 }
}

fn subdirs(dir: &Path, filter: impl Fn(&str) -> bool) -> Vec<ProtonVersion> {
    let Ok(entries) = sorted_entries(dir) else { return Vec::new() };
    entries
        .into_iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            filter(&name).then(|| {
                let path = e.path();
                ProtonVersion { size: dir_size(&path), name, path }
            })
        })
        .collect()
}

/// Installed compatibility tools: compatibilitytools.d/* and every library's common/Proton*.
pub fn get_installed_proton_versions() -> Result<Vec<ProtonVersion>> {
    let steam = get_steam_dir()?;
    let mut results = subdirs(&steam.join("compatibilitytools.d"), |_| true);
    for folder in get_library_folders()? {
        results.extend(subdirs(&folder.join("steamapps/common"), |n| n.starts_with("Proton")));
    }
    Ok(results)
}

/// The names Steam's CompatToolMapping uses for `version`. A custom tool
/// lists them in its compatibilitytool.vdf; an official one is named after
/// its folder (`Proton 9.0` → `proton_9`, `Proton 6.3` → `proton_63`,
/// `Proton - Experimental` → `proton_experimental`). The folder name itself
/// is always included.
pub fn tool_ids(version: &ProtonVersion) -> Vec<String> {
    let mut ids = vec![version.name.clone()];
    if let Ok(text) = read_text(&version.path.join("compatibilitytool.vdf"))
        && let Some(tools) = parse_vdf(&text).navigate(&["compatibilitytools", "compat_tools"])
    {
        ids.extend(tools.keys().cloned());
    }
    ids.extend(official_tool_id(&version.name));
    ids
}

fn official_tool_id(folder: &str) -> Option<String> {
    let rest = folder.strip_prefix("Proton")?.trim().trim_start_matches("- ").trim();
    let (major, minor) = rest.split_once('.').unwrap_or((rest, "0"));
    if !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()) && minor.bytes().all(|b| b.is_ascii_digit()) {
        return Some(if minor == "0" || minor.is_empty() {
            format!("proton_{major}")
        } else {
            format!("proton_{major}{minor}")
        });
    }
    (!rest.is_empty()).then(|| format!("proton_{}", rest.to_ascii_lowercase().replace([' ', '-'], "_")))
}

/// Proton runtimes games load implicitly (EasyAntiCheat, BattlEye); never
/// picked as a game's tool, so never "unused".
pub fn is_runtime(version: &ProtonVersion) -> bool {
    version.name.contains("Runtime")
}

/// The compatibility tool set for `appid`, if any.
pub fn get_game_proton_version(appid: i64) -> Result<Option<String>> {
    Ok(get_compat_overrides()?.remove(&appid).map(|o| o.dest).filter(|d| !d.is_empty()))
}

/// appid -> tool name for every app with an override, from one read of config.vdf.
pub fn get_all_proton_version_mappings() -> Result<BTreeMap<i64, String>> {
    Ok(get_compat_overrides()?.into_iter().filter(|(_, o)| !o.dest.is_empty()).map(|(id, o)| (id, o.dest)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_official_tools_like_steam() {
        for (folder, id) in [
            ("Proton 9.0", "proton_9"),
            ("Proton 8.0", "proton_8"),
            ("Proton 6.3", "proton_63"),
            ("Proton 5.13", "proton_513"),
            ("Proton 3.7", "proton_37"),
            ("Proton - Experimental", "proton_experimental"),
            ("Proton Hotfix", "proton_hotfix"),
        ] {
            assert_eq!(official_tool_id(folder).as_deref(), Some(id), "{folder}");
        }
        assert_eq!(official_tool_id("GE-Proton9-20"), None);
    }

    #[test]
    fn reads_custom_tool_names() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("GE-Proton9-20");
        std::fs::create_dir(&tool).unwrap();
        std::fs::write(
            tool.join("compatibilitytool.vdf"),
            "\"compatibilitytools\"\n{\n\t\"compat_tools\"\n\t{\n\t\t\"GE-Proton9-20-custom\"\n\t\t{\n\t\t\t\"display_name\" \"GE\"\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        let version = ProtonVersion { name: "GE-Proton9-20".into(), path: tool, size: 0 };
        assert_eq!(tool_ids(&version), ["GE-Proton9-20", "GE-Proton9-20-custom"]);
    }
}
