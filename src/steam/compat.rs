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

/// The compatibility tool set for `appid`, if any.
pub fn get_game_proton_version(appid: i64) -> Result<Option<String>> {
    Ok(get_compat_overrides()?.remove(&appid).map(|o| o.dest).filter(|d| !d.is_empty()))
}

/// appid -> tool name for every app with an override, from one read of config.vdf.
pub fn get_all_proton_version_mappings() -> Result<BTreeMap<i64, String>> {
    Ok(get_compat_overrides()?.into_iter().filter(|(_, o)| !o.dest.is_empty()).map(|(id, o)| (id, o.dest)).collect())
}
