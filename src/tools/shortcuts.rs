//! shortcuts: list, add, remove and import non-Steam game shortcuts.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::steam::launch_options::{check_launch_options, check_shortcut_exe, check_start_dir};
use crate::steam::shortcuts::{ShortcutEntry, add_shortcuts, read_shortcuts, remove_shortcut};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, Write, boxed};
use crate::util::{format_timestamp, home_dir, js_num};

pub fn tool() -> Tool {
    Tool {
        name: "shortcuts",
        description: "Non-Steam shortcuts: list, add, remove, import from Lutris",
        params: vec![
            Param::new("action", ParamType::Enum(&["list", "add", "remove", "import_lutris"])).required(),
            Param::new("appid", ParamType::Number),
            Param::new("name", ParamType::String),
            Param::new("exe", ParamType::String).describe("Absolute path to the executable"),
            Param::new("start_dir", ParamType::String)
                .describe("Working directory (defaults to exe's parent directory)"),
            Param::new("launch_options", ParamType::String),
            Param::new("tags", ParamType::ArrayOfStrings),
            Param::new("game_ids", ParamType::ArrayOfNumbers).describe("Lutris game IDs to import; omit for all"),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[Write::Action("add"), Write::Action("remove"), Write::Action("import_lutris")],
        destructive: true,
        open_world: false,
    }
}

async fn handle(args: Args) -> ToolResult {
    match args.str("action").as_deref() {
        Some("list") => ToolResult::or_error(list(), "Error listing shortcuts"),
        Some("add") => ToolResult::or_error(add(&args), "Error adding shortcut"),
        Some("remove") => ToolResult::or_error(remove(&args), "Error removing shortcut"),
        _ => ToolResult::or_error(import_lutris(&args).await, "Error importing Lutris games"),
    }
}

/// djb2 over the UTF-16 code units of exe + name, with the high bit set.
fn generate_shortcut_id(exe: &str, name: &str) -> i64 {
    let hash = exe
        .encode_utf16()
        .chain(name.encode_utf16())
        .fold(5381u32, |h, c| h.wrapping_mul(33).wrapping_add(u32::from(c)));
    i64::from(hash | 0x8000_0000)
}

/// Node's POSIX `path.dirname`.
/// Node's `path.dirname` on Windows, where it understands drives and both separators.
#[cfg(windows)]
fn dirname(p: &str) -> String {
    match std::path::Path::new(p).parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_string_lossy().into_owned(),
        Some(_) => ".".into(),
        None => p.into(),
    }
}

/// Node's POSIX `path.dirname`.
#[cfg(not(windows))]
fn dirname(p: &str) -> String {
    let bytes = p.as_bytes();
    if bytes.is_empty() {
        return ".".into();
    }
    let has_root = bytes[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => if has_root { "/" } else { "." }.into(),
        Some(1) if has_root => "//".into(),
        Some(i) => p[..i].into(),
    }
}

fn shortcut_json(s: &ShortcutEntry) -> Value {
    json!({
        "appid": s.appid,
        "name": s.app_name,
        "exe": s.exe,
        "startDir": s.start_dir,
        "launchOptions": s.launch_options,
        "tags": s.tags,
    })
}

fn list() -> Result<ToolResult> {
    let shortcuts = read_shortcuts()?;
    if shortcuts.is_empty() {
        return Ok(ToolResult::text("No non-Steam game shortcuts found."));
    }
    let results: Vec<Value> = shortcuts
        .iter()
        .map(|s| {
            json!({
                "appid": s.appid,
                "name": s.app_name,
                "exe": s.exe,
                "startDir": s.start_dir,
                "launchOptions": s.launch_options,
                "lastPlayed": format_timestamp(s.last_play_time),
                "tags": s.tags,
            })
        })
        .collect();
    Ok(ToolResult::json(&json!({ "count": results.len(), "shortcuts": results })))
}

fn add(args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    let name = args.str("name").filter(|s| !s.is_empty());
    let exe = args.str("exe").filter(|s| !s.is_empty());
    let (Some(name), Some(exe)) = (name, exe) else {
        return Ok(ToolResult::error("Error: \"name\" and \"exe\" are required for the add action."));
    };
    let launch_options = args.str("launch_options").unwrap_or_default();
    // Save the checked paths, not links that could be repointed later.
    let checked = check_shortcut_exe(&exe).and_then(|exe| {
        let start_dir = check_start_dir(&args.str("start_dir").unwrap_or_else(|| dirname(&exe)))?;
        check_launch_options(&launch_options)?;
        Ok((exe, start_dir))
    });
    let (exe, start_dir) = match checked {
        Ok(checked) => checked,
        Err(why) => {
            return Ok(ToolResult::error(format!(
                "Shortcut NOT added: {why}. For anything else, add the shortcut in Steam yourself."
            )));
        }
    };

    // Quoted, as Steam writes these fields itself: Steam puts them into the
    // command line it may run through a shell, and the checks above leave
    // nothing a shell would interpret inside double quotes.
    let entry = ShortcutEntry {
        appid: generate_shortcut_id(&exe, &name),
        start_dir: format!("\"{start_dir}\""),
        app_name: name,
        exe: format!("\"{exe}\""),
        launch_options,
        last_play_time: 0,
        tags: args.strings("tags").unwrap_or_default(),
    };
    add_shortcuts(std::slice::from_ref(&entry))?;

    Ok(ToolResult::json(&json!({
        "message": "Shortcut added successfully",
        "shortcut": shortcut_json(&entry),
    })))
}

fn remove(args: &Args) -> Result<ToolResult> {
    let _guard = crate::util::file_edit_lock();
    let appid = args.f64("appid");
    let name = args.str("name");
    if appid.is_none() && name.is_none() {
        return Ok(ToolResult::error("Error: Either appid or name must be provided."));
    }

    let removed = remove_shortcut(|s| match appid {
        Some(id) => s.appid as f64 == id,
        None => Some(&s.app_name) == name.as_ref(),
    })?;
    let Some(removed) = removed else {
        let target = match appid {
            Some(id) => format!("appid {}", js_num(id)),
            None => format!("name \"{}\"", name.unwrap_or_default()),
        };
        return Ok(ToolResult::error(format!("Shortcut not found matching {target}.")));
    };

    Ok(ToolResult::json(&json!({
        "message": "Shortcut removed successfully",
        "removed": shortcut_json(&removed),
    })))
}

/// `execSync` of a shell command with a timeout; None on spawn failure, timeout or non-zero exit.
async fn exec(command: &str, timeout: Duration) -> Option<String> {
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = tokio::time::timeout(timeout, child.wait_with_output()).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A value as JS `${value}` prints it.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.as_f64().map(js_num).unwrap_or_else(|| n.to_string()),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Array(_) | Value::Object(_) => v.to_string(),
    }
}

async fn import_lutris(args: &Args) -> Result<ToolResult> {
    let Some(raw_json) = exec("lutris --list-games --json", Duration::from_millis(15_000)).await else {
        return Ok(ToolResult::error(
            "Error: Could not run \"lutris --list-games --json\". Is Lutris installed and in PATH?",
        ));
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw_json) else {
        return Ok(ToolResult::error("Error: Failed to parse Lutris JSON output."));
    };

    let game_ids = args.numbers("game_ids").filter(|ids| !ids.is_empty());
    let Value::Array(mut lutris_games) = parsed else {
        return Err(anyhow!(if game_ids.is_some() {
            "lutrisGames.filter is not a function"
        } else {
            "lutrisGames is not iterable"
        }));
    };
    if let Some(ids) = game_ids {
        lutris_games.retain(|g| g.get("id").and_then(Value::as_f64).is_some_and(|id| ids.contains(&id)));
    }
    if lutris_games.is_empty() {
        return Ok(ToolResult::text("No matching Lutris games found."));
    }

    let _guard = crate::util::file_edit_lock();
    let existing_names: HashSet<String> = read_shortcuts()?.iter().map(|s| s.app_name.to_lowercase()).collect();
    let mut new_shortcuts = Vec::new();
    let mut imported: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for game in &lutris_games {
        let name = game
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Cannot read properties of undefined (reading 'toLowerCase')"))?;
        if existing_names.contains(&name.to_lowercase()) {
            skipped.push(name.to_string());
            continue;
        }
        let id = game.get("id").map_or_else(|| "undefined".to_string(), js_string);
        new_shortcuts.push(ShortcutEntry {
            appid: generate_shortcut_id("lutris", name),
            app_name: name.to_string(),
            exe: "lutris".into(),
            start_dir: home_dir().to_string_lossy().into_owned(),
            launch_options: format!("lutris:rungameid/{id}"),
            last_play_time: 0,
            tags: vec!["Lutris".into()],
        });
        imported.push(name.to_string());
    }

    if !new_shortcuts.is_empty() {
        add_shortcuts(&new_shortcuts)?;
    }

    Ok(ToolResult::json(&json!({
        "message": format!(
            "Imported {} game(s), skipped {} already-existing shortcut(s).",
            imported.len(),
            skipped.len()
        ),
        "imported": imported,
        "skipped": skipped,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_id_matches_js() {
        assert_eq!(generate_shortcut_id("lutris", "Game"), 4185753730);
        assert_eq!(
            generate_shortcut_id("/usr/bin/some very long executable path ünï😀", "Name With Emoji 😀"),
            2427097506
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn dirname_matches_node() {
        for (p, d) in [
            ("/usr/bin/game", "/usr/bin"),
            ("/game", "/"),
            ("game", "."),
            ("/a/b/", "/a"),
            ("//a", "//"),
            ("a/b", "a"),
            ("a//b", "a/"),
            ("///", "/"),
            ("//a//b//", "//a/"),
        ] {
            assert_eq!(dirname(p), d, "{p}");
        }
    }
}
