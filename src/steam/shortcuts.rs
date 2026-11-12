//! Non-Steam game shortcuts in userdata/{id}/config/shortcuts.vdf (binary VDF).
//!
//! Edits change the parsed file in place: entries and fields this module
//! doesn't know (icon, IsHidden, AllowOverlay, ...) are written back untouched.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use crate::util::{backup_file, io_error, write_atomic};
use crate::vdf::{
    BinObject, BinValue, is_array_index, js_entries, parse_binary_vdf, parse_binary_vdf_strict, parse_int,
    serialize_binary_vdf,
};

use super::paths::get_user_data_dir;

#[derive(Debug, Clone)]
pub struct ShortcutEntry {
    pub appid: i64,
    pub app_name: String,
    pub exe: String,
    pub start_dir: String,
    pub launch_options: String,
    pub last_play_time: i64,
    pub tags: Vec<String>,
}

fn shortcuts_path() -> Result<PathBuf> {
    Ok(get_user_data_dir()?.join("config/shortcuts.vdf"))
}

/// All shortcuts; empty when the file is missing, unreadable or empty.
pub fn read_shortcuts() -> Result<Vec<ShortcutEntry>> {
    let Ok(buf) = fs::read(shortcuts_path()?) else { return Ok(Vec::new()) };
    let root = parse_binary_vdf(&buf)?;
    let list = match root.get("shortcuts").or_else(|| root.get("Shortcuts")) {
        Some(BinValue::Obj(o)) => o,
        _ => &root,
    };
    Ok(entries(list).into_iter().map(|(_, entry)| entry).collect())
}

/// Append `shortcuts` to shortcuts.vdf.
pub fn add_shortcuts(shortcuts: &[ShortcutEntry]) -> Result<()> {
    add_shortcuts_in(&shortcuts_path()?, shortcuts)
}

/// Remove the first shortcut `matches` accepts, if any, and return it.
pub fn remove_shortcut(matches: impl Fn(&ShortcutEntry) -> bool) -> Result<Option<ShortcutEntry>> {
    remove_shortcut_in(&shortcuts_path()?, matches)
}

/// The text of the first of `keys` present; None when absent or an object.
fn text_of(obj: &BinObject, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| obj.get(*k)).and_then(BinValue::text)
}

fn int_of(obj: &BinObject, keys: &[&str]) -> i64 {
    match keys.iter().find_map(|k| obj.get(*k)) {
        Some(BinValue::Uint(n)) => i64::from(*n),
        Some(BinValue::Str(s)) => parse_int(s).unwrap_or(0),
        _ => 0,
    }
}

fn to_entry(obj: &BinObject) -> ShortcutEntry {
    let tags = keys_obj(obj, &["tags", "Tags"])
        .map(|t| js_entries(t).into_iter().filter_map(|(_, v)| v.text()).collect())
        .unwrap_or_default();
    ShortcutEntry {
        appid: int_of(obj, &["appid", "AppId", "appID"]),
        app_name: text_of(obj, &["appname", "AppName", "app_name"]).unwrap_or_default(),
        exe: text_of(obj, &["exe", "Exe"]).unwrap_or_default(),
        start_dir: text_of(obj, &["StartDir", "startdir"]).unwrap_or_default(),
        launch_options: text_of(obj, &["LaunchOptions", "launchoptions"]).unwrap_or_default(),
        last_play_time: int_of(obj, &["LastPlayTime", "lastplaytime"]),
        tags,
    }
}

fn keys_obj<'a>(obj: &'a BinObject, keys: &[&str]) -> Option<&'a BinObject> {
    keys.iter().find_map(|k| obj.get(*k)).and_then(BinValue::as_obj)
}

/// The shortcut entries of `list` in Steam's order, with their keys.
fn entries(list: &BinObject) -> Vec<(&str, ShortcutEntry)> {
    js_entries(list).into_iter().filter_map(|(k, v)| v.as_obj().map(|o| (k, to_entry(o)))).collect()
}

/// A new entry as Steam writes one: ids and times as uint32, the rest as strings.
fn to_bin(s: &ShortcutEntry) -> Result<BinObject> {
    let uint = |n: i64, what: &str| u32::try_from(n).map_err(|_| anyhow!("{what} {n} doesn't fit a uint32"));
    let mut entry = BinObject::new();
    entry.insert("appid".into(), uint(s.appid, "appid")?.into());
    entry.insert("AppName".into(), s.app_name.as_str().into());
    entry.insert("Exe".into(), s.exe.as_str().into());
    entry.insert("StartDir".into(), s.start_dir.as_str().into());
    entry.insert("LaunchOptions".into(), s.launch_options.as_str().into());
    entry.insert("LastPlayTime".into(), uint(s.last_play_time, "LastPlayTime")?.into());
    if !s.tags.is_empty() {
        let tags: BinObject =
            s.tags.iter().enumerate().map(|(i, tag)| (i.to_string(), BinValue::from(tag.as_str()))).collect();
        entry.insert("tags".into(), tags.into());
    }
    Ok(entry)
}

/// The parsed file at `path`; an empty document when it doesn't exist or is
/// empty. Any other read or parse failure is an error, so a file that can't
/// be understood is never replaced.
fn load(path: &Path) -> Result<BinObject> {
    match fs::read(path) {
        Ok(buf) => parse_binary_vdf_strict(&buf)
            .map_err(|e| anyhow!("{} can't be edited safely ({e}); nothing was written", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(BinObject::new()),
        Err(e) => Err(io_error(&e, "open", path)),
    }
}

/// The shortcuts list inside `root`, created in an empty document.
fn list_mut<'a>(root: &'a mut BinObject, path: &Path) -> Result<&'a mut BinObject> {
    if root.is_empty() {
        root.insert("shortcuts".into(), BinObject::new().into());
    }
    let key = ["shortcuts", "Shortcuts"].into_iter().find(|k| matches!(root.get(*k), Some(BinValue::Obj(_))));
    let Some(key) = key else {
        bail!("{} has no shortcuts list; nothing was written", path.display());
    };
    Ok(root.get_mut(key).and_then(BinValue::as_obj_mut).expect("checked object"))
}

fn save(path: &Path, root: &BinObject) -> Result<()> {
    let bytes = serialize_binary_vdf(root)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| io_error(&e, "mkdir", dir))?;
    }
    backup_file(path).map_err(|e| io_error(&e, "backup", path))?;
    write_atomic(path, &bytes).map_err(|e| io_error(&e, "open", path))?;
    Ok(())
}

fn add_shortcuts_in(path: &Path, shortcuts: &[ShortcutEntry]) -> Result<()> {
    let mut root = load(path)?;
    let list = list_mut(&mut root, path)?;
    let next =
        list.keys().filter(|k| is_array_index(k)).filter_map(|k| k.parse::<u64>().ok()).max().map_or(0, |n| n + 1);
    for (index, s) in (next..).zip(shortcuts) {
        list.insert(index.to_string(), to_bin(s)?.into());
    }
    save(path, &root)
}

fn remove_shortcut_in(path: &Path, matches: impl Fn(&ShortcutEntry) -> bool) -> Result<Option<ShortcutEntry>> {
    let mut root = load(path)?;
    let list = list_mut(&mut root, path)?;
    let Some((key, removed)) = entries(list).into_iter().find(|(_, e)| matches(e)).map(|(k, e)| (k.to_string(), e))
    else {
        return Ok(None);
    };

    // Steam numbers the entries 0..n; close the gap, keeping their order.
    let mut indexed = Vec::new();
    let mut named = Vec::new();
    for (k, v) in std::mem::take(list) {
        if k == key {
            continue;
        }
        if is_array_index(&k) {
            indexed.push((k.parse::<u64>().expect("index key"), v));
        } else {
            named.push((k, v));
        }
    }
    indexed.sort_by_key(|(n, _)| *n);
    list.extend(indexed.into_iter().enumerate().map(|(i, (_, v))| (i.to_string(), v)));
    list.extend(named);

    save(path, &root)?;
    Ok(Some(removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ShortcutEntry {
        ShortcutEntry {
            appid: 3_000_000_000,
            app_name: name.into(),
            exe: "/usr/bin/game".into(),
            start_dir: "/usr/bin".into(),
            launch_options: String::new(),
            last_play_time: 0,
            tags: vec![],
        }
    }

    /// A file as Steam writes it, with fields this module doesn't model.
    fn steam_file(names: &[&str]) -> BinObject {
        let list: BinObject = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let mut e = BinObject::new();
                e.insert("appid".into(), BinValue::Uint(2_000_000_000 + i as u32));
                e.insert("AppName".into(), BinValue::from(*name));
                e.insert("Exe".into(), "\"/opt/game\"".into());
                e.insert("icon".into(), "/opt/game.png".into());
                e.insert("IsHidden".into(), BinValue::Uint(0));
                e.insert("AllowOverlay".into(), BinValue::Uint(1));
                e.insert("FlatpakAppID".into(), "".into());
                (i.to_string(), BinValue::Obj(e))
            })
            .collect();
        let mut root = BinObject::new();
        root.insert("shortcuts".into(), list.into());
        root
    }

    fn write_file(dir: &Path, root: &BinObject) -> PathBuf {
        let path = dir.join("shortcuts.vdf");
        fs::write(&path, serialize_binary_vdf(root).unwrap()).unwrap();
        path
    }

    fn read_root(path: &Path) -> BinObject {
        parse_binary_vdf(&fs::read(path).unwrap()).unwrap()
    }

    /// shortcuts.vdf bytes built by hand, not by our serializer: one entry
    /// with fields this module doesn't model and a digit-only name.
    fn handmade_file() -> Vec<u8> {
        [
            &b"\x00shortcuts\x00"[..],
            b"\x000\x00",
            b"\x02appid\x00\x01\x02\x03\x84",
            b"\x01AppName\x002048\x00",
            b"\x01Exe\x00\"/opt/game\"\x00",
            b"\x01icon\x00/opt/game.png\x00",
            b"\x02IsHidden\x00\x00\x00\x00\x00",
            b"\x02AllowOverlay\x00\x01\x00\x00\x00",
            b"\x00tags\x00\x010\x00Favorite\x00\x08",
            b"\x08",
            b"\x08\x08",
        ]
        .concat()
    }

    #[test]
    fn add_appends_without_changing_a_byte_of_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shortcuts.vdf");
        let original = handmade_file();
        fs::write(&path, &original).unwrap();

        add_shortcuts_in(&path, &[entry("New")]).unwrap();

        let after = fs::read(&path).unwrap();
        // Everything up to the end tags of the list and the root is unchanged.
        assert!(after.starts_with(&original[..original.len() - 2]));
        assert!(after.ends_with(b"\x08\x08\x08"));
    }

    #[test]
    fn edits_refuse_what_they_cannot_write_back_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shortcuts.vdf");
        let original = handmade_file();
        // "2048" becomes "\xe9048": not UTF-8.
        let latin1 = original.iter().map(|&b| if b == b'2' { 0xe9 } else { b }).collect();
        let duplicate = [&original[..original.len() - 2], b"\x000\x00\x08", b"\x08\x08"].concat();
        let trailing = [&original[..], b"junk"].concat();
        let unclosed_root = original[..original.len() - 1].to_vec();
        for bytes in [latin1, duplicate, trailing, unclosed_root] {
            fs::write(&path, &bytes).unwrap();
            let err = add_shortcuts_in(&path, &[entry("New")]).unwrap_err();
            assert!(err.to_string().contains("nothing was written"), "{err}");
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn remove_keeps_keys_that_are_not_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let mut root = steam_file(&["One", "Two"]);
        let list = root["shortcuts"].as_obj_mut().unwrap();
        let extra = list["1"].clone();
        list.insert("007".into(), extra.clone());
        let path = write_file(dir.path(), &root);

        remove_shortcut_in(&path, |e| e.app_name == "One").unwrap().unwrap();

        let root = read_root(&path);
        let list = root["shortcuts"].as_obj().unwrap();
        assert_eq!(list.keys().collect::<Vec<_>>(), ["0", "007"]);
        assert_eq!(list["007"], extra);
    }

    #[test]
    fn add_keeps_existing_entries_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let original = steam_file(&["One", "2048"]);
        let path = write_file(dir.path(), &original);

        add_shortcuts_in(&path, &[entry("New")]).unwrap();

        let root = read_root(&path);
        let list = root["shortcuts"].as_obj().unwrap();
        assert_eq!(list["0"], original["shortcuts"].as_obj().unwrap()["0"]);
        // A numeric-looking name stays a string.
        assert_eq!(list["1"], original["shortcuts"].as_obj().unwrap()["1"]);
        let added = list["2"].as_obj().unwrap();
        assert_eq!(added["appid"], BinValue::Uint(3_000_000_000));
        assert_eq!(added["AppName"], BinValue::from("New"));
    }

    #[test]
    fn remove_renumbers_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let original = steam_file(&["One", "Two", "Three"]);
        let path = write_file(dir.path(), &original);

        let removed = remove_shortcut_in(&path, |e| e.app_name == "Two").unwrap().unwrap();
        assert_eq!(removed.app_name, "Two");

        let root = read_root(&path);
        let list = root["shortcuts"].as_obj().unwrap();
        let before = original["shortcuts"].as_obj().unwrap();
        assert_eq!(list.keys().collect::<Vec<_>>(), ["0", "1"]);
        assert_eq!(list["0"], before["0"]);
        assert_eq!(list["1"], before["2"]);
    }

    #[test]
    fn remove_without_a_match_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), &steam_file(&["One"]));
        let before = fs::read(&path).unwrap();
        assert!(remove_shortcut_in(&path, |e| e.app_name == "Nope").unwrap().is_none());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn add_creates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/shortcuts.vdf");
        add_shortcuts_in(&path, &[entry("New")]).unwrap();
        let root = read_root(&path);
        assert_eq!(root["shortcuts"].as_obj().unwrap()["0"].as_obj().unwrap()["AppName"], BinValue::from("New"));
    }

    #[test]
    fn a_truncated_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), &steam_file(&["One", "Two"]));
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 20);
        fs::write(&path, &bytes).unwrap();

        let err = add_shortcuts_in(&path, &[entry("New")]).unwrap_err();
        assert!(err.to_string().contains("nothing was written"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn nul_in_a_field_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), &steam_file(&["One"]));
        let before = fs::read(&path).unwrap();
        let mut bad = entry("A\0\x08");
        bad.exe = "/x".into();
        assert!(add_shortcuts_in(&path, &[bad]).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn edits_leave_a_backup_of_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(dir.path(), &steam_file(&["One"]));
        let original = fs::read(&path).unwrap();
        add_shortcuts_in(&path, &[entry("A")]).unwrap();
        add_shortcuts_in(&path, &[entry("B")]).unwrap();
        let backups: Vec<_> =
            crate::util::read_dir_names(dir.path()).unwrap().into_iter().filter(|n| n.ends_with(".bak")).collect();
        assert_eq!(backups.len(), 2);
        assert_eq!(fs::read(dir.path().join(&backups[0])).unwrap(), original);
    }
}
