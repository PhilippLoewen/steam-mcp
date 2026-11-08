//! userdata/{id}/config/localconfig.vdf: launch options and playtime.

use std::fs;

use anyhow::Result;

use crate::util::read_text;
use crate::vdf::{VdfExt, VdfObject, VdfValue, js_entries, parse_int, parse_vdf, serialize_vdf};

use super::paths::get_user_data_dir;

#[derive(Debug, Clone, Copy)]
pub struct Playtime {
    pub appid: i64,
    /// Minutes.
    pub playtime: i64,
    /// Unix seconds.
    pub last_played: i64,
}

/// The parsed localconfig.vdf of the active account.
pub fn get_local_config() -> Result<VdfObject> {
    let path = get_user_data_dir()?.join("config/localconfig.vdf");
    Ok(parse_vdf(&read_text(&path)?))
}

/// UserLocalConfigStore > Software > Valve > Steam > apps.
fn apps_section(config: &VdfObject) -> Option<&VdfObject> {
    config.navigate(&["UserLocalConfigStore", "Software", "Valve", "Steam", "apps"])
}

/// Launch options for `appid`, if set.
pub fn get_app_launch_options(appid: i64) -> Result<Option<String>> {
    let config = get_local_config()?;
    Ok(apps_section(&config)
        .and_then(|apps| apps.get(&appid.to_string()))
        .and_then(VdfValue::as_obj)
        .and_then(|app| app.str_of(&["LaunchOptions", "launchoptions"]))
        .map(str::to_string))
}

/// Set launch options for `appid`, creating the path to it as needed.
pub fn set_app_launch_options(appid: i64, options: &str) -> Result<()> {
    let path = get_user_data_dir()?.join("config/localconfig.vdf");
    let mut config = parse_vdf(&read_text(&path)?);

    fn ensure<'a>(parent: &'a mut VdfObject, key: &str) -> &'a mut VdfObject {
        let existing = parent.iter().position(|(k, v)| k.eq_ignore_ascii_case(key) && matches!(v, VdfValue::Obj(_)));
        let index = match existing {
            Some(i) => i,
            None => {
                parent.insert(key.to_string(), VdfValue::Obj(VdfObject::new()));
                parent.get_index_of(key).expect("just inserted")
            }
        };
        parent[index].as_obj_mut().expect("checked object")
    }

    let mut node = &mut config;
    for key in ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", &appid.to_string()] {
        node = ensure(node, key);
    }
    node.insert("LaunchOptions".to_string(), VdfValue::Str(options.to_string()));

    fs::write(&path, serialize_vdf(&config, 0)).map_err(|e| crate::util::io_error(&e, "open", &path))?;
    Ok(())
}

fn playtime_of(appid: i64, app: &VdfObject) -> Playtime {
    Playtime {
        appid,
        playtime: app.str_of(&["Playtime", "playtime"]).map_or(Some(0), parse_int).unwrap_or(0),
        last_played: app.str_of(&["LastPlayed", "lastplayed"]).map_or(Some(0), parse_int).unwrap_or(0),
    }
}

/// Playtime for one app; None when it has neither playtime nor a last-played time.
pub fn get_playtime(appid: i64) -> Result<Option<Playtime>> {
    let config = get_local_config()?;
    Ok(apps_section(&config)
        .and_then(|apps| apps.get(&appid.to_string()))
        .and_then(VdfValue::as_obj)
        .map(|app| playtime_of(appid, app))
        .filter(|p| p.playtime != 0 || p.last_played != 0))
}

/// Playtime for every app in localconfig.vdf that has any, installed or not.
pub fn get_all_playtimes() -> Result<Vec<Playtime>> {
    let config = get_local_config()?;
    let Some(apps) = apps_section(&config) else { return Ok(Vec::new()) };
    Ok(js_entries(apps)
        .into_iter()
        .filter_map(|(key, value)| {
            let app = value.as_obj()?;
            let appid = parse_int(key)?;
            let p = playtime_of(appid, app);
            (p.playtime > 0 || p.last_played > 0).then_some(p)
        })
        .collect())
}
