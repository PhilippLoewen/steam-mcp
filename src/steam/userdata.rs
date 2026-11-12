//! userdata/{id}/config/localconfig.vdf: launch options and playtime.

use std::fs;
use std::path::Path;

use anyhow::{Result, anyhow, bail};

use crate::util::{backup_file, io_error, read_text, write_atomic};
use crate::vdf::{VdfExt, VdfObject, VdfValue, js_entries, parse_int, parse_vdf, parse_vdf_strict, serialize_vdf};

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
    set_launch_options_in(&get_user_data_dir()?.join("config/localconfig.vdf"), appid, options)
}

/// Rewrite the localconfig.vdf at `path`. The file is Steam's whole per-user
/// configuration, so it is only written back when it parsed completely, and a
/// timestamped backup is taken first.
fn set_launch_options_in(path: &Path, appid: i64, options: &str) -> Result<()> {
    let unsafe_to_edit =
        |why: String| anyhow!("{} can't be edited safely ({why}); nothing was written", path.display());
    let bytes = fs::read(path).map_err(|e| io_error(&e, "open", path))?;
    let text = String::from_utf8(bytes).map_err(|_| unsafe_to_edit("it is not valid UTF-8".into()))?;
    let mut config = parse_vdf_strict(&text).map_err(|e| unsafe_to_edit(e.to_string()))?;
    if !config.keys().any(|k| k.eq_ignore_ascii_case("UserLocalConfigStore")) {
        bail!("{} has no UserLocalConfigStore section; nothing was written", path.display());
    }

    /// The object under `key` (any case), created when missing. A value of
    /// another kind under that name is refused, never replaced.
    fn ensure<'a>(parent: &'a mut VdfObject, key: &str) -> Result<&'a mut VdfObject> {
        let matching = |k: &String| k.eq_ignore_ascii_case(key);
        let index = match parent.iter().position(|(k, v)| matching(k) && matches!(v, VdfValue::Obj(_))) {
            Some(i) => i,
            None if parent.keys().any(matching) => bail!("\"{key}\" is not a section"),
            None => parent.insert_full(key.to_string(), VdfValue::Obj(VdfObject::new())).0,
        };
        Ok(parent[index].as_obj_mut().expect("checked object"))
    }

    let mut node = &mut config;
    for key in ["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", &appid.to_string()] {
        node = ensure(node, key).map_err(|e| unsafe_to_edit(e.to_string()))?;
    }
    // Steam matches keys case-insensitively, so replace an existing value in
    // whatever case it has rather than add a second key next to it.
    let value = VdfValue::Str(options.to_string());
    match node.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("LaunchOptions")) {
        Some((_, VdfValue::Obj(_))) => return Err(unsafe_to_edit("\"LaunchOptions\" is a section".into())),
        Some((_, existing)) => *existing = value,
        None => {
            node.insert("LaunchOptions".to_string(), value);
        }
    }

    backup_file(path).map_err(|e| io_error(&e, "backup", path))?;
    write_atomic(path, serialize_vdf(&config, 0).as_bytes()).map_err(|e| io_error(&e, "open", path))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"friends\"\n\t{\n\t\t\"x\"\t\t\"1\"\n\t}\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"10\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"Playtime\"\t\t\"5\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n}\n";

    #[test]
    fn sets_launch_options_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localconfig.vdf");
        fs::write(&path, CONFIG).unwrap();

        set_launch_options_in(&path, 10, "-novid").unwrap();

        let config = parse_vdf_strict(&fs::read_to_string(&path).unwrap()).unwrap();
        let app = apps_section(&config).unwrap()["10"].as_obj().unwrap();
        assert_eq!(app.str_of(&["LaunchOptions"]), Some("-novid"));
        assert_eq!(app.str_of(&["Playtime"]), Some("5"));
        assert!(config.navigate(&["UserLocalConfigStore", "friends"]).is_some());
        let backups: Vec<String> =
            crate::util::read_dir_names(dir.path()).unwrap().into_iter().filter(|n| n.ends_with(".bak")).collect();
        assert_eq!(fs::read_to_string(dir.path().join(&backups[0])).unwrap(), CONFIG);
    }

    #[test]
    fn replaces_launch_options_in_any_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localconfig.vdf");
        fs::write(&path, CONFIG.replace("\"Playtime\"\t\t\"5\"", "\"launchoptions\"\t\t\"-old\"")).unwrap();

        set_launch_options_in(&path, 10, "-new").unwrap();

        let config = parse_vdf_strict(&fs::read_to_string(&path).unwrap()).unwrap();
        let app = apps_section(&config).unwrap()["10"].as_obj().unwrap();
        assert_eq!(app.len(), 1);
        assert_eq!(app.str_of(&["launchoptions"]), Some("-new"));
    }

    #[test]
    fn refuses_to_rewrite_a_truncated_or_foreign_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localconfig.vdf");
        let duplicate = CONFIG.replace("\"friends\"", "\"Software\" \"x\"\n\t\"friends\"");
        let scalar_apps = CONFIG.replace("\"apps\"\n\t\t\t\t{", "\"apps\" \"x\"\n\t\t\t\t\"other\"\n\t\t\t\t{");
        for content in [
            &CONFIG[..CONFIG.len() / 2],
            "",
            "\"Other\"\n{\n}\n",
            "\"UserLocalConfigStore\" \"unexpected\"",
            &duplicate,
            &scalar_apps,
        ] {
            fs::write(&path, content).unwrap();
            let err = set_launch_options_in(&path, 10, "-novid").unwrap_err();
            assert!(err.to_string().contains("nothing was written"), "{content}: {err}");
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }

        let mut latin1 = CONFIG.as_bytes().to_vec();
        latin1.splice(latin1.len() - 3..latin1.len() - 3, *b"\"n\" \"Caf\xe9\"\n");
        fs::write(&path, &latin1).unwrap();
        assert!(set_launch_options_in(&path, 10, "-novid").is_err());
        assert_eq!(fs::read(&path).unwrap(), latin1);
    }
}
