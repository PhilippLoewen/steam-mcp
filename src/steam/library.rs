//! Library folders with their sizes and free space.

use std::path::PathBuf;

use anyhow::Result;

use crate::util::free_space;
use crate::vdf::{VdfValue, is_array_index, js_entries, parse_int};

use super::paths::read_library_folders_vdf;

#[derive(Debug, Clone)]
pub struct LibraryFolder {
    pub path: PathBuf,
    pub label: String,
    /// totalsize from libraryfolders.vdf.
    pub total_size: i64,
    /// Free bytes on the folder's filesystem.
    pub free_space: u64,
    pub appids: Vec<i64>,
    pub mounted: bool,
}

/// libraryfolders.vdf entries enriched with free space and mount state.
pub fn get_libraries() -> Result<Vec<LibraryFolder>> {
    let folders = read_library_folders_vdf()?;
    let mut results = Vec::new();
    for (key, value) in js_entries(&folders) {
        if !key.bytes().all(|b| b.is_ascii_digit()) || key.is_empty() {
            continue;
        }
        let VdfValue::Obj(entry) = value else { continue };
        let Some(path) = entry.get("path").and_then(VdfValue::as_str).filter(|p| !p.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(path);

        let appids = entry
            .get("apps")
            .and_then(VdfValue::as_obj)
            .map(|apps| {
                // Object.keys order: integer keys ascending.
                let mut ids: Vec<(bool, i64)> =
                    apps.keys().filter_map(|k| parse_int(k).map(|id| (is_array_index(k), id))).collect();
                ids.sort_by_key(|(idx, id)| if *idx { (0, *id) } else { (1, 0) });
                ids.into_iter().map(|(_, id)| id).collect()
            })
            .unwrap_or_default();

        let mounted = path.exists();
        let free = if mounted { free_space(&path).unwrap_or(0) } else { 0 };
        let total_size = entry.get("totalsize").and_then(VdfValue::as_str).and_then(parse_int).unwrap_or(0);
        let label = entry.get("label").and_then(VdfValue::as_str).unwrap_or("").to_string();

        results.push(LibraryFolder { path, label, total_size, free_space: free, appids, mounted });
    }
    Ok(results)
}
