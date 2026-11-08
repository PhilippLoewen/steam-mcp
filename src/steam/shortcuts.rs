//! Non-Steam game shortcuts in userdata/{id}/config/shortcuts.vdf (binary VDF).

use std::fs;

use anyhow::Result;

use crate::vdf::{VdfExt, VdfObject, VdfValue, js_entries, parse_binary_vdf, parse_int, serialize_binary_vdf};

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

/// All shortcuts; empty when the file is missing, unreadable or empty.
pub fn read_shortcuts() -> Result<Vec<ShortcutEntry>> {
    let path = get_user_data_dir()?.join("config/shortcuts.vdf");
    let Ok(buf) = fs::read(&path) else { return Ok(Vec::new()) };
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    let parsed = parse_binary_vdf(&buf)?;
    let shortcuts = match parsed.get("shortcuts").or_else(|| parsed.get("Shortcuts")) {
        Some(VdfValue::Obj(o)) => o,
        _ => &parsed,
    };

    let mut entries = Vec::new();
    for (_, value) in js_entries(shortcuts) {
        let VdfValue::Obj(obj) = value else { continue };
        let tags = obj
            .obj_of(&["tags", "Tags"])
            .map(|t| js_entries(t).into_iter().filter_map(|(_, v)| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        entries.push(ShortcutEntry {
            appid: obj.str_of(&["appid", "AppId", "appID"]).map_or(Some(0), parse_int).unwrap_or(0),
            app_name: obj.str_or_empty(&["appname", "AppName", "app_name"]),
            exe: obj.str_or_empty(&["exe", "Exe"]),
            start_dir: obj.str_or_empty(&["StartDir", "startdir"]),
            launch_options: obj.str_or_empty(&["LaunchOptions", "launchoptions"]),
            last_play_time: obj.str_of(&["LastPlayTime", "lastplaytime"]).map_or(Some(0), parse_int).unwrap_or(0),
            tags,
        });
    }
    Ok(entries)
}

/// Rewrite shortcuts.vdf with `shortcuts`, backing the old file up to shortcuts.vdf.bak.
pub fn write_shortcuts(shortcuts: &[ShortcutEntry]) -> Result<()> {
    let config_dir = get_user_data_dir()?.join("config");
    let path = config_dir.join("shortcuts.vdf");
    fs::create_dir_all(&config_dir)?;
    if path.exists() {
        fs::copy(&path, config_dir.join("shortcuts.vdf.bak"))?;
    }

    let mut entries = VdfObject::new();
    for (i, s) in shortcuts.iter().enumerate() {
        let mut entry = VdfObject::new();
        entry.insert("appid".into(), s.appid.to_string().into());
        entry.insert("AppName".into(), s.app_name.clone().into());
        entry.insert("Exe".into(), s.exe.clone().into());
        entry.insert("StartDir".into(), s.start_dir.clone().into());
        entry.insert("LaunchOptions".into(), s.launch_options.clone().into());
        entry.insert("LastPlayTime".into(), s.last_play_time.to_string().into());
        if !s.tags.is_empty() {
            let tags: VdfObject =
                s.tags.iter().enumerate().map(|(t, tag)| (t.to_string(), VdfValue::from(tag.clone()))).collect();
            entry.insert("tags".into(), tags.into());
        }
        entries.insert(i.to_string(), entry.into());
    }
    let mut root = VdfObject::new();
    root.insert("shortcuts".into(), entries.into());
    fs::write(&path, serialize_binary_vdf(&root))?;
    Ok(())
}
