//! appmanifest_*.acf files: one per installed app.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;

use crate::util::{FileCache, read_dir_names};
use crate::vdf::{VdfExt, VdfObject, VdfValue, parse_int, parse_vdf};

use super::paths::get_library_folders;

#[derive(Debug, Clone)]
pub struct AppManifest {
    pub appid: i64,
    pub name: String,
    pub installdir: String,
    pub size_on_disk: i64,
    pub buildid: String,
    pub last_updated: i64,
    pub last_played: i64,
    pub state_flags: i64,
    /// The library folder (parent of steamapps/).
    pub library_path: PathBuf,
}

static CACHE: LazyLock<FileCache<Option<AppManifest>>> = LazyLock::new(FileCache::new);

/// The AppState section of a parsed manifest, or the whole document when it has none.
pub fn app_state(parsed: &VdfObject) -> &VdfObject {
    match parsed.get("AppState").or_else(|| parsed.get("appstate")) {
        Some(VdfValue::Obj(o)) => o,
        _ => parsed,
    }
}

/// Parse one appmanifest_*.acf.
pub fn read_manifest(path: &Path) -> Result<AppManifest> {
    let library_path = path.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    let manifest = CACHE
        .get(path, |content| {
            let parsed = parse_vdf(content);
            let state = app_state(&parsed);
            // A manifest without an appid is corrupt; reading it as 0 would match
            // Steam's default compat-tool entry, so it is rejected below.
            let appid = state.str_of(&["appid"]).and_then(parse_int)?;
            Some(AppManifest {
                appid,
                name: state.str_or_empty(&["name"]),
                installdir: state.str_or_empty(&["installdir"]),
                size_on_disk: state.int_of(&["SizeOnDisk", "sizeondisk"], 0),
                buildid: state.str_or_empty(&["buildid"]),
                last_updated: state.int_of(&["LastUpdated", "lastupdated"], 0),
                last_played: state.int_of(&["LastPlayed", "lastplayed"], 0),
                state_flags: state.int_of(&["StateFlags", "stateflags"], 0),
                library_path: library_path.clone(),
            })
        })
        .map_err(|e| crate::util::io_error(&e, "open", path))?;
    manifest.ok_or_else(|| anyhow::anyhow!("no appid in {}", path.display()))
}

/// Every manifest across all library folders. Missing or unmounted folders are skipped.
pub fn read_all_manifests() -> Result<Vec<AppManifest>> {
    let mut manifests = Vec::new();
    for folder in get_library_folders()? {
        let steamapps = folder.join("steamapps");
        if !steamapps.exists() {
            eprintln!("Library folder not accessible, skipping: {}", steamapps.display());
            continue;
        }
        let entries = match read_dir_names(&steamapps) {
            Ok(e) => e,
            Err(err) => {
                eprintln!("Failed to read library folder {}: {err}", steamapps.display());
                continue;
            }
        };
        for entry in entries {
            if !entry.starts_with("appmanifest_") || !entry.ends_with(".acf") {
                continue;
            }
            let path = steamapps.join(&entry);
            match read_manifest(&path) {
                Ok(m) => manifests.push(m),
                Err(err) => eprintln!("Failed to parse manifest {}: {err}", path.display()),
            }
        }
    }
    Ok(manifests)
}
