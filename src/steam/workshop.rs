//! appworkshop_*.acf files: installed Workshop items per app.

use indexmap::IndexMap;
use std::path::Path;

use anyhow::Result;

use crate::util::{read_dir_names, read_text};
use crate::vdf::{VdfExt, VdfObject, VdfValue, is_array_index, parse_int, parse_vdf};

use super::paths::get_library_folders;

#[derive(Debug, Clone)]
pub struct WorkshopItem {
    pub published_file_id: String,
    pub size: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone)]
pub struct WorkshopManifest {
    pub appid: i64,
    pub size_on_disk: i64,
    pub items: Vec<WorkshopItem>,
}

fn int(obj: &VdfObject, keys: &[&str]) -> i64 {
    obj.str_of(keys).map_or(Some(0), parse_int).unwrap_or(0)
}

/// Parse one appworkshop_*.acf, merging WorkshopItemsInstalled and WorkshopItemDetails.
pub fn read_workshop_manifest(path: &Path) -> Result<WorkshopManifest> {
    let parsed = parse_vdf(&read_text(path)?);
    let state = match parsed.get("AppWorkshop").or_else(|| parsed.get("appworkshop")) {
        Some(VdfValue::Obj(o)) => o,
        _ => &parsed,
    };
    let appid = state.str_of(&["appid"]).and_then(parse_int).unwrap_or(0);
    let size_on_disk = int(state, &["SizeOnDisk", "sizeondisk"]);
    let installed = state.obj_of(&["WorkshopItemsInstalled", "workshopitemsinstalled"]);
    let details = state.obj_of(&["WorkshopItemDetails", "workshopitemdetails"]);

    // Set insertion order over both sections, each in Object.keys order.
    let mut ids: Vec<&str> = Vec::new();
    for section in [installed, details].into_iter().flatten() {
        let mut keys: Vec<&str> = section.keys().map(String::as_str).collect();
        keys.sort_by_key(|k| if is_array_index(k) { (0, k.parse::<u64>().unwrap_or(0)) } else { (1, 0) });
        for k in keys {
            if !ids.contains(&k) {
                ids.push(k);
            }
        }
    }

    let items = ids
        .into_iter()
        .map(|id| {
            let mut size = 0;
            let mut time_updated = 0;
            if let Some(obj) = installed.and_then(|i| i.get(id)).and_then(VdfValue::as_obj) {
                size = int(obj, &["size", "Size"]);
                time_updated = int(obj, &["timeupdated", "TimeUpdated"]);
            }
            if let Some(obj) = details.and_then(|d| d.get(id)).and_then(VdfValue::as_obj) {
                let truthy = |k: &str| obj.get(k).is_some_and(|v| v.as_str() != Some(""));
                if truthy("timetouched") || truthy("timeupdated") {
                    time_updated = int(obj, &["timeupdated", "timetouched"]);
                }
            }
            WorkshopItem { published_file_id: id.to_string(), size, time_updated }
        })
        .collect();

    Ok(WorkshopManifest { appid, size_on_disk, items })
}

/// Workshop data for every app across all libraries, keyed by appid in discovery order.
pub fn read_all_workshop_data() -> Result<IndexMap<i64, WorkshopManifest>> {
    let mut result = IndexMap::new();
    for folder in get_library_folders()? {
        let dir = folder.join("steamapps/workshop");
        let Ok(entries) = read_dir_names(&dir) else { continue };
        for entry in entries {
            if !entry.starts_with("appworkshop_") || !entry.ends_with(".acf") {
                continue;
            }
            let path = dir.join(&entry);
            match read_workshop_manifest(&path) {
                Ok(data) => {
                    result.insert(data.appid, data);
                }
                Err(err) => eprintln!("Failed to parse workshop manifest {}: {err}", path.display()),
            }
        }
    }
    Ok(result)
}
