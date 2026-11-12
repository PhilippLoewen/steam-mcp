//! storage: disk usage, cleanup, shader cache, cloud saves, backups.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::output;
use crate::steam::compat::{get_all_proton_version_mappings, get_installed_proton_versions, is_runtime, tool_ids};
use crate::steam::library::{LibraryFolder, get_libraries};
use crate::steam::manifests::{AppManifest, read_all_manifests};
use crate::steam::paths::{get_steam_dir, get_user_data_dir};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, Write, blocking};
use crate::util::{dir_size, format_bytes, io_error, js_num, locale_cmp, num, read_text, sorted_entries};
use crate::vdf::{VdfExt, VdfValue, js_entries, parse_int, parse_vdf};

const ACTIONS: &[&str] =
    &["disk_report", "orphaned", "cleanup", "shader_cache", "shader_stats", "saves_list", "saves_stats", "backup"];

pub fn tool() -> Tool {
    Tool {
        name: "storage",
        description: "Disk usage, cleanup, shader cache, cloud saves, backups",
        params: vec![
            Param::new("action", ParamType::Enum(ACTIONS)).required(),
            Param::new("appid", ParamType::Number),
            Param::new("sort_by", ParamType::Enum(&["name", "size"])),
            Param::new("limit", ParamType::Number),
        ],
        handler: |args| blocking(move || handle(&args)),
        writes: &[Write::Action("backup")],
        destructive: false,
        open_world: false,
    }
}

fn handle(args: &Args) -> ToolResult {
    match args.str("action").as_deref() {
        Some("disk_report") => ToolResult::or_error(disk_report(), "Error generating disk usage report"),
        Some("orphaned") => ToolResult::or_error(orphaned(), "Error finding orphaned data"),
        Some("cleanup") => ToolResult::or_error(cleanup(), "Error generating cleanup recommendations"),
        Some("shader_cache") => ToolResult::or_error(shader_cache(args), "Error getting shader cache"),
        Some("shader_stats") => ToolResult::or_error(shader_stats(), "Error getting shader cache stats"),
        Some("saves_list") => ToolResult::or_error(saves_list(args), "Error listing cloud saves"),
        Some("saves_stats") => ToolResult::or_error(saves_stats(), "Error getting cloud save stats"),
        Some("backup") => ToolResult::or_error(backup(args), "Error backing up saves"),
        _ => ToolResult::error("Unknown action"),
    }
}

struct AppidDir {
    appid: i64,
    path: PathBuf,
    size: i64,
}

/// Numeric subdirectories of `dir` (as JS `parseInt` sees them) with their sizes.
fn list_appid_dirs(dir: &Path) -> Vec<AppidDir> {
    let Ok(entries) = sorted_entries(dir) else { return Vec::new() };
    entries
        .into_iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let appid = parse_int(&e.file_name().to_string_lossy())?;
            let path = e.path();
            Some(AppidDir { appid, size: dir_size(&path) as i64, path })
        })
        .collect()
}

/// Total size and count of regular files under `dir`.
fn walk_dir_stats(dir: &Path) -> (i64, i64) {
    let Ok(entries) = fs::read_dir(dir) else { return (0, 0) };
    let (mut size, mut count) = (0, 0);
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let (s, c) = walk_dir_stats(&entry.path());
            size += s;
            count += c;
        } else if ft.is_file()
            && let Ok(meta) = fs::metadata(entry.path())
        {
            size += meta.len() as i64;
            count += 1;
        }
    }
    (size, count)
}

fn name_map(manifests: &[AppManifest]) -> HashMap<i64, String> {
    manifests.iter().map(|m| (m.appid, m.name.clone())).collect()
}

fn name_of(names: &HashMap<i64, String>, appid: i64) -> String {
    names.get(&appid).cloned().unwrap_or_else(|| format!("Unknown ({appid})"))
}

fn mounted(libraries: &[LibraryFolder]) -> impl Iterator<Item = PathBuf> + '_ {
    libraries.iter().filter(|l| l.mounted).map(|l| l.path.join("steamapps"))
}

fn fmt(bytes: i64) -> String {
    format_bytes(bytes as f64)
}

fn path_str(path: &Path) -> String {
    path.display().to_string()
}

/// JS `array.slice(0, end)` for a numeric `end`.
fn js_slice_end(len: usize, end: f64) -> usize {
    let len_f = len as f64;
    let end = end.trunc();
    let end = if end < 0.0 { (len_f + end).max(0.0) } else { end.min(len_f) };
    end as usize
}

fn disk_report() -> Result<ToolResult> {
    let manifests = read_all_manifests()?;
    let libraries = get_libraries()?;
    let names = name_map(&manifests);

    #[derive(Default)]
    struct Usage {
        name: String,
        install: i64,
        compatdata: i64,
        shadercache: i64,
        workshop: i64,
        total: i64,
    }

    fn ensure<'a>(games: &'a mut IndexMap<i64, Usage>, names: &HashMap<i64, String>, appid: i64) -> &'a mut Usage {
        games.entry(appid).or_insert_with(|| Usage { name: name_of(names, appid), ..Default::default() })
    }

    let mut games: IndexMap<i64, Usage> = IndexMap::new();
    let (mut total_install, mut total_compat, mut total_shader, mut total_workshop) = (0i64, 0i64, 0i64, 0i64);
    for m in &manifests {
        ensure(&mut games, &names, m.appid).install = m.size_on_disk;
        total_install = total_install.saturating_add(m.size_on_disk);
    }
    for steamapps in mounted(&libraries) {
        for d in list_appid_dirs(&steamapps.join("compatdata")) {
            let u = ensure(&mut games, &names, d.appid);
            u.compatdata = u.compatdata.saturating_add(d.size);
            total_compat = total_compat.saturating_add(d.size);
        }
        for d in list_appid_dirs(&steamapps.join("shadercache")) {
            let u = ensure(&mut games, &names, d.appid);
            u.shadercache = u.shadercache.saturating_add(d.size);
            total_shader = total_shader.saturating_add(d.size);
        }
        for d in list_appid_dirs(&steamapps.join("workshop").join("content")) {
            let u = ensure(&mut games, &names, d.appid);
            u.workshop = u.workshop.saturating_add(d.size);
            total_workshop = total_workshop.saturating_add(d.size);
        }
    }
    for u in games.values_mut() {
        u.total = saturating_sum(&[u.install, u.compatdata, u.shadercache, u.workshop]);
    }

    let mut sorted: Vec<(i64, Usage)> = games.into_iter().collect();
    sorted.sort_by_key(|a| Reverse(a.1.total));
    let per_game: Vec<Value> = sorted
        .iter()
        .map(|(appid, u)| {
            json!({
                "appid": appid,
                "name": u.name,
                "install": fmt(u.install),
                "compatdata": fmt(u.compatdata),
                "shadercache": fmt(u.shadercache),
                "workshop": fmt(u.workshop),
                "total": fmt(u.total),
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "totalFootprint": fmt(saturating_sum(&[total_install, total_compat, total_shader, total_workshop])),
        "breakdown": {
            "installs": fmt(total_install),
            "compatdata": fmt(total_compat),
            "shadercache": fmt(total_shader),
            "workshop": fmt(total_workshop),
        },
        "gameCount": per_game.len(),
        "perGameBreakdown": per_game,
    })))
}

struct Orphan {
    appid: i64,
    category: &'static str,
    path: PathBuf,
    size: i64,
}

impl Orphan {
    fn to_json(&self) -> Value {
        json!({
            "appid": self.appid,
            "category": self.category,
            "path": path_str(&self.path),
            "size": fmt(self.size),
        })
    }
}

fn find_orphans(libraries: &[LibraryFolder], installed: &HashSet<i64>, with_workshop: bool) -> Vec<Orphan> {
    let mut orphans = Vec::new();
    for steamapps in mounted(libraries) {
        let mut categories =
            vec![("compatdata", steamapps.join("compatdata")), ("shadercache", steamapps.join("shadercache"))];
        if with_workshop {
            categories.push(("workshop", steamapps.join("workshop").join("content")));
        }
        for (category, dir) in categories {
            orphans.extend(
                list_appid_dirs(&dir).into_iter().filter(|d| !installed.contains(&d.appid)).map(|d| Orphan {
                    appid: d.appid,
                    category,
                    path: d.path,
                    size: d.size,
                }),
            );
        }
    }
    orphans.sort_by_key(|a| Reverse(a.size));
    orphans
}

fn orphaned() -> Result<ToolResult> {
    let manifests = read_all_manifests()?;
    let libraries = get_libraries()?;
    let installed: HashSet<i64> = manifests.iter().map(|m| m.appid).collect();
    let orphans = find_orphans(&libraries, &installed, true);
    let total: i64 = orphans.iter().map(|o| o.size).sum();

    Ok(ToolResult::json(&json!({
        "orphanedCount": orphans.len(),
        "totalSize": fmt(total),
        "orphaned": orphans.iter().map(Orphan::to_json).collect::<Vec<_>>(),
    })))
}

fn cleanup() -> Result<ToolResult> {
    let manifests = read_all_manifests()?;
    let libraries = get_libraries()?;
    let installed: HashSet<i64> = manifests.iter().map(|m| m.appid).collect();
    let names = name_map(&manifests);

    let orphans = find_orphans(&libraries, &installed, false);

    let mut shader_caches: Vec<(String, AppidDir)> = mounted(&libraries)
        .flat_map(|steamapps| list_appid_dirs(&steamapps.join("shadercache")))
        .filter(|d| installed.contains(&d.appid))
        .map(|d| (name_of(&names, d.appid), d))
        .collect();
    shader_caches.sort_by_key(|a| Reverse(a.1.size));

    let mut never_played: Vec<&AppManifest> = manifests.iter().filter(|m| m.last_played == 0).collect();
    never_played.sort_by_key(|a| Reverse(a.size_on_disk));
    let never_played: Vec<Value> =
        never_played.iter().map(|m| json!({ "appid": m.appid, "name": m.name, "size": fmt(m.size_on_disk) })).collect();

    let mappings = get_all_proton_version_mappings()?;
    // Tools picked for an installed game, plus the default for all games (appid 0).
    let used: HashSet<&str> =
        manifests.iter().map(|m| m.appid).chain([0]).filter_map(|id| mappings.get(&id).map(String::as_str)).collect();
    let unused_proton: Vec<Value> = get_installed_proton_versions()?
        .iter()
        .filter(|v| !is_runtime(v) && !tool_ids(v).iter().any(|id| used.contains(id.as_str())))
        .map(|v| json!({ "name": v.name, "path": path_str(&v.path), "size": format_bytes(v.size as f64) }))
        .collect();

    Ok(ToolResult::json(&json!({
        "orphanedData": {
            "description": "Data directories for uninstalled games. Shader caches (shadercache) can be deleted safely. \
                Proton prefixes (compatdata) can hold the only copy of saves and settings for games without \
                Steam Cloud: back them up or check them before deleting.",
            "count": orphans.len(),
            "totalSize": fmt(orphans.iter().map(|o| o.size).sum()),
            "items": orphans.iter().take(20).map(Orphan::to_json).collect::<Vec<_>>(),
        },
        "largestShaderCaches": {
            "description": "Shader caches for installed games. Can be deleted; Steam will rebuild them on next launch.",
            "top10": shader_caches
                .iter()
                .take(10)
                .map(|(name, d)| json!({
                    "appid": d.appid,
                    "name": name,
                    "path": path_str(&d.path),
                    "size": fmt(d.size),
                }))
                .collect::<Vec<_>>(),
        },
        "neverPlayedGames": {
            "description": "Installed games that have never been played. Consider uninstalling to reclaim space.",
            "count": never_played.len(),
            "games": never_played,
        },
        "unusedProtonVersions": {
            "description": "Proton versions not used by any installed game. Consider removing.",
            "versions": unused_proton,
        },
    })))
}

fn shader_cache(args: &Args) -> Result<ToolResult> {
    let libraries = get_libraries()?;
    let Some(appid) = args.f64("appid") else {
        return Ok(ToolResult::error("appid is required for shader_cache action."));
    };

    let mut total = 0;
    let mut paths = Vec::new();
    for l in libraries.iter().filter(|l| l.mounted) {
        let dir = l.path.join("steamapps").join("shadercache").join(js_num(appid));
        if fs::metadata(&dir).is_ok_and(|m| m.is_dir()) {
            total += dir_size(&dir);
            paths.push(path_str(&dir));
        }
    }

    if paths.is_empty() {
        return Ok(ToolResult::text(format!("No shader cache found for appid {}.", js_num(appid))));
    }

    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "size": format_bytes(total as f64),
        "sizeBytes": total,
        "paths": paths,
    })))
}

/// String values of config.vdf's ShaderCacheManager section, if present.
fn gpu_driver_info() -> Option<Map<String, Value>> {
    let config = parse_vdf(&read_text(&get_steam_dir().ok()?.join("config").join("config.vdf")).ok()?);
    let shader_cache = config
        .obj_of(&["InstallConfigStore"])?
        .obj_of(&["Software"])?
        .obj_of(&["Valve", "valve"])?
        .obj_of(&["Steam", "steam"])?
        .obj_of(&["ShaderCacheManager"])?;
    Some(
        js_entries(shader_cache)
            .into_iter()
            .filter_map(|(k, v)| match v {
                VdfValue::Str(s) => Some((k.to_string(), Value::from(s.as_str()))),
                VdfValue::Obj(_) => None,
            })
            .collect(),
    )
}

fn shader_stats() -> Result<ToolResult> {
    let libraries = get_libraries()?;
    let manifests = read_all_manifests()?;
    let names = name_map(&manifests);

    let mut caches: IndexMap<i64, i64> = IndexMap::new();
    for steamapps in mounted(&libraries) {
        for d in list_appid_dirs(&steamapps.join("shadercache")) {
            *caches.entry(d.appid).or_default() += d.size;
        }
    }
    let mut list: Vec<(i64, i64)> = caches.into_iter().collect();
    list.sort_by_key(|a| Reverse(a.1));
    let total: i64 = list.iter().map(|(_, s)| s).sum();

    let mut output = Map::new();
    output.insert("totalSize".into(), fmt(total).into());
    output.insert("cacheCount".into(), list.len().into());
    output.insert(
        "top10BySize".into(),
        list.iter()
            .take(10)
            .map(|(appid, size)| json!({ "appid": appid, "name": name_of(&names, *appid), "size": fmt(*size) }))
            .collect::<Vec<_>>()
            .into(),
    );
    if let Some(info) = gpu_driver_info().filter(|i| !i.is_empty()) {
        output.insert("gpuDriverInfo".into(), info.into());
    }
    Ok(ToolResult::json(&output.into()))
}

struct SaveInfo {
    appid: i64,
    name: String,
    size: i64,
    file_count: i64,
}

impl SaveInfo {
    fn to_json(&self) -> Value {
        json!({ "appid": self.appid, "name": self.name, "size": fmt(self.size), "fileCount": self.file_count })
    }
}

fn collect_saves() -> Result<Vec<SaveInfo>> {
    let userdata = get_user_data_dir()?;
    let manifests = read_all_manifests()?;
    let names = name_map(&manifests);

    let Ok(entries) = sorted_entries(&userdata) else { return Ok(Vec::new()) };
    Ok(entries
        .into_iter()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let appid = parse_int(&e.file_name().to_string_lossy())?;
            let (size, file_count) = walk_dir_stats(&e.path());
            (file_count > 0).then(|| SaveInfo { appid, name: name_of(&names, appid), size, file_count })
        })
        .collect())
}

fn saves_list(args: &Args) -> Result<ToolResult> {
    let mut saves = collect_saves()?;
    if args.str("sort_by").as_deref() == Some("name") {
        saves.sort_by(|a, b| locale_cmp(&a.name, &b.name));
    } else {
        saves.sort_by_key(|a| Reverse(a.size));
    }

    let limited = &saves[..js_slice_end(saves.len(), args.f64("limit").unwrap_or(50.0))];
    Ok(ToolResult::json(&json!({
        "totalGames": saves.len(),
        "totalSize": fmt(saves.iter().map(|s| s.size).sum()),
        "returned": limited.len(),
        "saves": limited.iter().map(SaveInfo::to_json).collect::<Vec<_>>(),
    })))
}

fn saves_stats() -> Result<ToolResult> {
    let mut saves = collect_saves()?;
    saves.sort_by_key(|a| Reverse(a.size));

    Ok(ToolResult::json(&json!({
        "totalGames": saves.len(),
        "totalSize": fmt(saves.iter().map(|s| s.size).sum()),
        "totalFiles": saves.iter().map(|s| s.file_count).sum::<i64>(),
        "top10BySize": saves.iter().take(10).map(SaveInfo::to_json).collect::<Vec<_>>(),
    })))
}

fn cp_error(code: &str, message: String, path: &Path) -> anyhow::Error {
    anyhow!("Invalid src or dest: cp returned {code} ({message}) {}", path.display())
}

fn cp_einval(message: String, path: &Path) -> anyhow::Error {
    cp_error("EINVAL", message, path)
}

/// Copy `src` to `dest`, which must not exist yet: nothing is merged into or
/// replaced, so two source names that collide on the destination's filesystem
/// fail instead of overwriting each other. Symlinks are recreated with
/// absolute targets (copied as files where symlinks aren't available).
fn copy_recursive(src: &Path, dest: &Path) -> Result<()> {
    let src_abs = std::path::absolute(src).unwrap_or_else(|_| src.to_path_buf());
    let dest_abs = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());
    if src_abs == dest_abs {
        return Err(cp_einval("src and dest cannot be the same".to_string(), dest));
    }
    let src_meta = fs::metadata(src).map_err(|e| io_error(&e, "stat", src))?;
    if src_meta.is_dir() && dest_abs.starts_with(&src_abs) {
        return Err(cp_einval(
            format!("cannot copy {} to a subdirectory of self {}", src.display(), dest.display()),
            dest,
        ));
    }
    copy_entry(src, dest)
}

fn copy_entry(src: &Path, dest: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(src).map_err(|e| io_error(&e, "lstat", src))?;
    let ft = meta.file_type();
    if ft.is_dir() {
        fs::create_dir(dest).map_err(|e| io_error(&e, "mkdir", dest))?;
        for entry in fs::read_dir(src).map_err(|e| io_error(&e, "opendir", src))? {
            let entry = entry.map_err(|e| io_error(&e, "readdir", src))?;
            copy_entry(&entry.path(), &dest.join(entry.file_name()))?;
        }
        fs::set_permissions(dest, meta.permissions()).map_err(|e| io_error(&e, "chmod", dest))?;
    } else if ft.is_symlink() {
        let target = fs::read_link(src).map_err(|e| io_error(&e, "readlink", src))?;
        let target =
            if target.is_absolute() { target } else { src.parent().map(|p| p.join(&target)).unwrap_or(target) };
        symlink(&target, dest)?;
    } else if !ft.is_file() {
        // A FIFO or device file: opening one could block forever, and it holds no save data.
        return Err(cp_einval(format!("{} is not a regular file, folder or symlink", src.display()), src));
    } else {
        let copy_error = |e: std::io::Error| anyhow!("{} -> '{}'", io_error(&e, "copyfile", src), dest.display());
        let mut from = fs::File::open(src).map_err(copy_error)?;
        // create_new: fails on an existing entry, symlink or not, instead of writing through it.
        let mut to = fs::OpenOptions::new().write(true).create_new(true).open(dest).map_err(copy_error)?;
        std::io::copy(&mut from, &mut to).map_err(copy_error)?;
        to.set_permissions(meta.permissions()).map_err(copy_error)?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).map_err(|e| io_error(&e, "symlink", link))
}

/// Without symlinks, the file the link points at is copied in its place.
#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    let copy = || -> std::io::Result<()> {
        let mut from = fs::File::open(target)?;
        let mut to = fs::OpenOptions::new().write(true).create_new(true).open(link)?;
        std::io::copy(&mut from, &mut to).map(|_| ())
    };
    copy().map_err(|e| io_error(&e, "symlink", link))
}

/// Sizes come from manifests and the disk; add them without wrapping on nonsense values.
fn saturating_sum(values: &[i64]) -> i64 {
    values.iter().fold(0, |sum, v| sum.saturating_add(*v))
}

/// Whether either folder is inside the other (or they are the same).
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Copy local cloud saves into a new, timestamped folder in the output folder.
fn backup(args: &Args) -> Result<ToolResult> {
    let userdata = get_user_data_dir()?;
    let out_dir = output::output_dir()?;
    // Compared resolved, so `..` and symlinks can't hide an overlap. When
    // userdata can't be resolved, the copy below fails on it anyway.
    if let Ok(real_userdata) = fs::canonicalize(&userdata)
        && overlaps(&out_dir, &real_userdata)
    {
        return Ok(ToolResult::error(format!(
            "The output folder {} overlaps the Steam userdata folder {}; set {} to a folder outside it.",
            out_dir.display(),
            real_userdata.display(),
            output::OUTPUT_DIR_ENV
        )));
    }
    read_all_manifests()?;

    let appid = args.f64("appid").map(js_num);
    if let Some(id) = &appid
        && !userdata.join(id).exists()
    {
        return Ok(ToolResult::error(format!("No cloud save data found for appid {id} in {}", userdata.display())));
    }
    let name = match &appid {
        Some(id) => format!("saves-{}-{id}", output::timestamp()),
        None => format!("saves-{}", output::timestamp()),
    };
    let dest_dir = output::create_unique_folder(&out_dir, &name)?;

    let mut backed_up = 0;
    let mut total = 0;
    let result = (|| -> Result<()> {
        if let Some(id) = &appid {
            let dest = dest_dir.join(id);
            copy_recursive(&userdata.join(id), &dest)?;
            total += dir_size(&dest);
            backed_up = 1;
            return Ok(());
        }
        for entry in fs::read_dir(&userdata).map_err(|e| io_error(&e, "scandir", &userdata))? {
            let entry = entry.map_err(|e| io_error(&e, "scandir", &userdata))?;
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            if parse_int(&name.to_string_lossy()).is_none() {
                continue;
            }
            let dest = dest_dir.join(&name);
            copy_recursive(&entry.path(), &dest)?;
            total += dir_size(&dest);
            backed_up += 1;
        }
        Ok(())
    })();
    if let Err(e) = result {
        return Ok(ToolResult::error(format!(
            "Backup from {} stopped: {e}. {} holds a partial copy.",
            userdata.display(),
            dest_dir.display()
        )));
    }

    Ok(ToolResult::json(&json!({
        "gamesBackedUp": backed_up,
        "totalSize": format_bytes(total as f64),
        "destination": dest_dir.to_string_lossy(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_add_up_without_wrapping() {
        assert_eq!(saturating_sum(&[i64::MAX, 10, 20]), i64::MAX);
        assert_eq!(saturating_sum(&[1, 2, 3]), 6);
    }

    #[test]
    fn overlap_is_containment_either_way() {
        assert!(overlaps(Path::new("/a/b"), Path::new("/a")));
        assert!(overlaps(Path::new("/a"), Path::new("/a/b")));
        assert!(overlaps(Path::new("/a"), Path::new("/a")));
        assert!(!overlaps(Path::new("/a/b"), Path::new("/a/bc")));
    }

    #[test]
    fn copy_never_merges_or_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("remote")).unwrap();
        fs::write(src.join("remote/save.dat"), "save").unwrap();

        let dest = tmp.path().join("dest");
        copy_recursive(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("remote/save.dat")).unwrap(), "save");

        // An existing destination is refused, not merged into.
        assert!(copy_recursive(&src, &dest).is_err());

        // A file already in the way is refused, not truncated.
        let in_the_way = tmp.path().join("other.dat");
        fs::write(&in_the_way, "keep").unwrap();
        assert!(copy_entry(&src.join("remote/save.dat"), &in_the_way).is_err());
        assert_eq!(fs::read_to_string(&in_the_way).unwrap(), "keep");
    }

    #[test]
    #[cfg(unix)]
    fn copy_refuses_fifos_instead_of_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(copy_entry(&fifo, &tmp.path().join("copy")).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn copy_never_writes_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("X"), "save").unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, "precious").unwrap();

        let dest = tmp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        std::os::unix::fs::symlink(&victim, dest.join("X")).unwrap();
        assert!(copy_entry(&src.join("X"), &dest.join("X")).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
    }
}
