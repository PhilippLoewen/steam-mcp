//! shortcuts: list, add, remove and import non-Steam game shortcuts.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::steam::launch_options::{check_launch_options, check_shortcut_exe, check_start_dir};
use crate::steam::millennium::{
    BridgeError, BridgeOptions, BridgeOutcome, BridgeShortcut, ShortcutRequest, bridge_list_shortcuts, bridge_shortcut,
};
use crate::steam::paths::steam_may_be_running;
use crate::steam::shortcuts::{ShortcutEntry, add_shortcuts, read_shortcuts, remove_shortcut};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, Write, boxed};
use crate::util::{format_timestamp, home_dir, js_num};

/// The refusal for shortcut edits while Steam runs and the Millennium bridge
/// can't take them; `bridge_status` says why it can't.
fn steam_running_warning(bridge_status: &str) -> String {
    format!(
        "WARNING: Steam is currently running (or that couldn't be checked)!\n\
         Steam keeps shortcuts in memory and rewrites shortcuts.vdf, so an edit made now would be lost.\n\
         Please close Steam first, then try again, or install the steam-mcp-bridge Millennium plugin\n\
         (millennium-plugin/ in the steam-mcp repo) to edit shortcuts while Steam runs.\n\
         Millennium bridge: {bridge_status}.\n\n\
         Shortcuts were NOT modified."
    )
}

/// What a shortcut added through the bridge lacks.
const TAGS_NOT_SET: &str = "Tags aren't set while Steam runs; add the shortcut to a category with \
                            library category_add instead.";

/// Everything a shortcut edit touches outside its own logic, so tests can stand in for Steam.
trait ShortcutEnv {
    fn steam_running(&self) -> bool;
    async fn bridge(&self, request: &ShortcutRequest) -> Result<BridgeOutcome<BridgeShortcut>>;
    /// The shortcuts the running Steam holds, through the bridge.
    async fn list_live(&self) -> Result<BridgeOutcome<Vec<BridgeShortcut>>>;
    /// From shortcuts.vdf.
    fn read(&self) -> Result<Vec<ShortcutEntry>>;
    /// To shortcuts.vdf.
    fn add(&self, entries: &[ShortcutEntry]) -> Result<()>;
    /// From shortcuts.vdf: the first shortcut `matches` accepts.
    fn remove(&self, matches: &dyn Fn(&ShortcutEntry) -> bool) -> Result<Option<ShortcutEntry>>;
}

/// The real shortcuts.vdf and Millennium socket.
struct LiveEnv;

impl ShortcutEnv for LiveEnv {
    fn steam_running(&self) -> bool {
        steam_may_be_running()
    }

    async fn bridge(&self, request: &ShortcutRequest) -> Result<BridgeOutcome<BridgeShortcut>> {
        bridge_shortcut(request, &BridgeOptions::default()).await
    }

    async fn list_live(&self) -> Result<BridgeOutcome<Vec<BridgeShortcut>>> {
        bridge_list_shortcuts(&BridgeOptions::default()).await
    }

    fn read(&self) -> Result<Vec<ShortcutEntry>> {
        read_shortcuts()
    }

    fn add(&self, entries: &[ShortcutEntry]) -> Result<()> {
        add_shortcuts(entries)
    }

    fn remove(&self, matches: &dyn Fn(&ShortcutEntry) -> bool) -> Result<Option<ShortcutEntry>> {
        remove_shortcut(matches)
    }
}

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
    let env = LiveEnv;
    match args.str("action").as_deref() {
        Some("list") => ToolResult::or_error(list(&env).await, "Error listing shortcuts"),
        Some("add") => ToolResult::or_error(add(&env, &args).await, "Error adding shortcut"),
        Some("remove") => ToolResult::or_error(remove(&env, &args).await, "Error removing shortcut"),
        _ => ToolResult::or_error(import_lutris(&env, &args).await, "Error importing Lutris games"),
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

const NO_SHORTCUTS: &str = "No non-Steam game shortcuts found.";

/// While Steam runs, the shortcuts it holds (shortcuts.vdf trails them by a
/// few seconds after a change); otherwise, or without the bridge, the file.
async fn list(env: &impl ShortcutEnv) -> Result<ToolResult> {
    let mut stale = None;
    if env.steam_running() {
        match env.list_live().await? {
            BridgeOutcome::Done(live) => {
                let file = env.read().unwrap_or_default();
                if live.is_empty() {
                    return Ok(ToolResult::text(if file.is_empty() {
                        NO_SHORTCUTS.to_string()
                    } else {
                        format!(
                            "{NO_SHORTCUTS}\nSteam holds none, but shortcuts.vdf lists {}; Steam may still be \
                             loading its library, so try again shortly.",
                            file.len()
                        )
                    }));
                }
                // Steam's list has no tags; take them from the file where it has the shortcut.
                let tags: HashMap<i64, Vec<String>> = file.into_iter().map(|s| (s.appid, s.tags)).collect();
                let results: Vec<Value> = live
                    .iter()
                    .map(|s| {
                        json!({
                            "appid": s.appid,
                            "name": s.name,
                            "exe": s.exe,
                            "startDir": s.start_dir,
                            "launchOptions": s.launch_options,
                            "lastPlayed": format_timestamp(s.last_played),
                            "tags": tags.get(&s.appid).cloned().unwrap_or_default(),
                        })
                    })
                    .collect();
                return Ok(ToolResult::json(
                    &json!({ "count": results.len(), "shortcuts": results, "via": "millennium" }),
                ));
            }
            BridgeOutcome::Unavailable(why) => stale = Some(why),
        }
    }
    let stale_note = stale.map(|why| {
        format!(
            "Steam is running, but the Millennium bridge can't be used ({why}); these are from shortcuts.vdf, \
             which may lag behind Steam."
        )
    });

    let shortcuts = env.read()?;
    if shortcuts.is_empty() {
        return Ok(ToolResult::text(match stale_note {
            Some(note) => format!("{NO_SHORTCUTS}\n{note}"),
            None => NO_SHORTCUTS.to_string(),
        }));
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
    let mut out = json!({ "count": results.len(), "shortcuts": results });
    if let Some(note) = stale_note {
        out["note"] = note.into();
    }
    Ok(ToolResult::json(&out))
}

fn bridged_json(s: &BridgeShortcut) -> Value {
    json!({
        "appid": s.appid,
        "name": s.name,
        "exe": s.exe,
        "startDir": s.start_dir,
        "launchOptions": s.launch_options,
        "tags": [],
    })
}

async fn add(env: &impl ShortcutEnv, args: &Args) -> Result<ToolResult> {
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

    let file_appid = generate_shortcut_id(&exe, &name);
    // Quoted, as Steam writes these fields itself: Steam puts them into the
    // command line it may run through a shell, and the checks above leave
    // nothing a shell would interpret inside double quotes.
    let (exe, start_dir) = (format!("\"{exe}\""), format!("\"{start_dir}\""));
    let tags = args.strings("tags").unwrap_or_default();

    if !env.steam_running() {
        let _guard = crate::util::file_edit_lock();
        // Checked again under the lock: Steam may have started while another edit held it.
        if !env.steam_running() {
            let entry = ShortcutEntry {
                appid: file_appid,
                start_dir,
                app_name: name,
                exe,
                launch_options,
                last_play_time: 0,
                tags,
            };
            env.add(std::slice::from_ref(&entry))?;
            return Ok(ToolResult::json(&json!({
                "message": "Shortcut added successfully",
                "shortcut": shortcut_json(&entry),
            })));
        }
    }

    let request = ShortcutRequest::Add { name, exe, start_dir, launch_options };
    let added = env.bridge(&request).await.map_err(|e| {
        // Unlike launch options, a new shortcut can't be read back without its appid.
        if BridgeError::outcome_unknown(&e) {
            anyhow!("{e} Check the library before adding it again, or it may be added twice.")
        } else {
            e
        }
    })?;
    Ok(match added {
        BridgeOutcome::Unavailable(why) => ToolResult::error(steam_running_warning(&why)),
        BridgeOutcome::Done(added) => {
            let mut out = json!({
                "message": "Shortcut added successfully",
                "shortcut": bridged_json(&added),
                "via": "millennium",
            });
            if !tags.is_empty() {
                out["note"] = TAGS_NOT_SET.into();
            }
            ToolResult::json(&out)
        }
    })
}

async fn remove(env: &impl ShortcutEnv, args: &Args) -> Result<ToolResult> {
    let appid = args.f64("appid");
    let name = args.str("name");
    if appid.is_none() && name.is_none() {
        return Ok(ToolResult::error("Error: Either appid or name must be provided."));
    }
    let not_found = || {
        let target = match appid {
            Some(id) => format!("appid {}", js_num(id)),
            None => format!("name \"{}\"", name.clone().unwrap_or_default()),
        };
        Ok(ToolResult::error(format!("Shortcut not found matching {target}.")))
    };

    if !env.steam_running() {
        let _guard = crate::util::file_edit_lock();
        // Checked again under the lock: Steam may have started while another edit held it.
        if !env.steam_running() {
            let removed = env.remove(&|s| match appid {
                Some(id) => s.appid as f64 == id,
                None => Some(&s.app_name) == name.as_ref(),
            })?;
            let Some(removed) = removed else {
                return not_found();
            };
            return Ok(ToolResult::json(&json!({
                "message": "Shortcut removed successfully",
                "removed": shortcut_json(&removed),
            })));
        }
    }

    // By name, the appid comes from the shortcuts Steam holds, not the file that trails them.
    let target = match appid {
        Some(id) if id.fract() == 0.0 && id > 0.0 => id as i64,
        Some(_) => return not_found(),
        None => {
            let known = known_shortcuts(env, false).await?;
            match known.shortcuts.into_iter().find(|(n, _)| Some(n) == name.as_ref()) {
                Some((_, appid)) if appid > 0 => appid,
                Some((name, _)) => {
                    let source = if known.from_file { " in shortcuts.vdf" } else { "" };
                    return Ok(ToolResult::error(format!(
                        "Shortcut \"{name}\" has no appid{source}, so it can only be removed while Steam is closed."
                    )));
                }
                None => return not_found(),
            }
        }
    };
    Ok(match env.bridge(&ShortcutRequest::Remove { appid: target }).await? {
        BridgeOutcome::Unavailable(why) => ToolResult::error(steam_running_warning(&why)),
        BridgeOutcome::Done(removed) => ToolResult::json(&json!({
            "message": "Shortcut removed successfully",
            "removed": bridged_json(&removed),
            "via": "millennium",
        })),
    })
}

/// The shortcuts' names and appids, for lookups while Steam runs.
struct KnownShortcuts {
    shortcuts: Vec<(String, i64)>,
    /// From shortcuts.vdf, because Steam's own list couldn't be had.
    from_file: bool,
}

/// Steam's own list, or shortcuts.vdf when Steam's can't be had: a plugin too
/// old to list, or a Steam internal the list needs that is missing, shouldn't
/// stop the remove that follows. A list whose outcome is unknown (it timed
/// out) is an error. `strict` is for deciding what to add: then only a bridge
/// that can't list at all falls back to the file, and any failed list is an
/// error, since the file may miss shortcuts added seconds ago and duplicates follow.
async fn known_shortcuts(env: &impl ShortcutEnv, strict: bool) -> Result<KnownShortcuts> {
    let live = match env.list_live().await {
        Ok(BridgeOutcome::Done(live)) => Some(live),
        Ok(BridgeOutcome::Unavailable(_)) => None,
        Err(e) if strict || BridgeError::outcome_unknown(&e) => {
            return Err(anyhow!(
                "{e} Couldn't tell which shortcuts Steam holds, so nothing was changed; try again, or close \
                 Steam to work from shortcuts.vdf."
            ));
        }
        Err(_) => None,
    };
    Ok(match live {
        Some(live) => {
            KnownShortcuts { shortcuts: live.into_iter().map(|s| (s.name, s.appid)).collect(), from_file: false }
        }
        None => KnownShortcuts {
            shortcuts: env.read()?.into_iter().map(|s| (s.app_name, s.appid)).collect(),
            from_file: true,
        },
    })
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

async fn import_lutris(env: &impl ShortcutEnv, args: &Args) -> Result<ToolResult> {
    let Some(raw_json) = exec("lutris --list-games --json", Duration::from_millis(15_000)).await else {
        return Ok(ToolResult::error(
            "Error: Could not run \"lutris --list-games --json\". Is Lutris installed and in PATH?",
        ));
    };
    import_lutris_games(env, args, &raw_json).await
}

/// The Lutris games to add as shortcuts, and the ones left out.
struct LutrisPlan {
    new_shortcuts: Vec<ShortcutEntry>,
    /// A shortcut of that name (any case) exists.
    skipped: Vec<String>,
    /// No numeric Lutris id.
    invalid: Vec<String>,
}

fn plan_lutris_import(lutris_games: &[Value], existing_names: &[String]) -> Result<LutrisPlan> {
    let existing: HashSet<String> = existing_names.iter().map(|n| n.to_lowercase()).collect();
    let mut plan = LutrisPlan { new_shortcuts: Vec::new(), skipped: Vec::new(), invalid: Vec::new() };
    for game in lutris_games {
        let name = game
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Cannot read properties of undefined (reading 'toLowerCase')"))?;
        // The id goes into launch options, so only a plain number is used.
        let Some(id) = game.get("id").and_then(Value::as_u64) else {
            plan.invalid.push(name.to_string());
            continue;
        };
        if existing.contains(&name.to_lowercase()) {
            plan.skipped.push(name.to_string());
            continue;
        }
        plan.new_shortcuts.push(ShortcutEntry {
            appid: generate_shortcut_id("lutris", name),
            app_name: name.to_string(),
            exe: "lutris".into(),
            start_dir: home_dir().to_string_lossy().into_owned(),
            launch_options: format!("lutris:rungameid/{id}"),
            last_play_time: 0,
            tags: vec!["Lutris".into()],
        });
    }
    Ok(plan)
}

async fn import_lutris_games(env: &impl ShortcutEnv, args: &Args, raw_json: &str) -> Result<ToolResult> {
    let Ok(parsed) = serde_json::from_str::<Value>(raw_json) else {
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

    let mut bridged = env.steam_running();
    let mut imported: Vec<String> = Vec::new();
    let mut failed = Vec::new();
    let mut planned = None;

    // The file lock spans reading the existing names and writing, and ends
    // before the bridge is awaited (a held std guard would make this future !Send).
    if !bridged {
        let _guard = crate::util::file_edit_lock();
        // Checked again under the lock: Steam may have started while another edit held it.
        if env.steam_running() {
            bridged = true;
        } else {
            let existing = env.read()?.into_iter().map(|s| s.app_name).collect::<Vec<_>>();
            let plan = plan_lutris_import(&lutris_games, &existing)?;
            if !plan.new_shortcuts.is_empty() {
                env.add(&plan.new_shortcuts)?;
                imported = plan.new_shortcuts.iter().map(|s| s.app_name.clone()).collect();
            }
            planned = Some(plan);
        }
    }

    let plan = match planned {
        Some(plan) => plan,
        None => {
            // Steam's own list: shortcuts.vdf may not show ones just added yet.
            // If the bridge is gone altogether, the first add below is refused.
            let existing: Vec<String> =
                known_shortcuts(env, true).await?.shortcuts.into_iter().map(|(name, _)| name).collect();
            plan_lutris_import(&lutris_games, &existing)?
        }
    };
    let LutrisPlan { new_shortcuts, skipped, invalid } = plan;

    if bridged {
        // One at a time: a game that fails doesn't stop the rest.
        for entry in &new_shortcuts {
            let request = ShortcutRequest::Add {
                name: entry.app_name.clone(),
                exe: entry.exe.clone(),
                start_dir: entry.start_dir.clone(),
                launch_options: entry.launch_options.clone(),
            };
            match env.bridge(&request).await {
                Ok(BridgeOutcome::Done(_)) => imported.push(entry.app_name.clone()),
                // Before anything was added, nothing changed: refuse like the other edits.
                Ok(BridgeOutcome::Unavailable(why)) if imported.is_empty() && failed.is_empty() => {
                    return Ok(ToolResult::error(steam_running_warning(&why)));
                }
                Ok(BridgeOutcome::Unavailable(why)) => {
                    failed.push(json!({ "name": entry.app_name, "error": format!("Millennium bridge: {why}") }));
                }
                Err(e) => failed.push(json!({ "name": entry.app_name, "error": e.to_string() })),
            }
        }
    }

    let mut message =
        format!("Imported {} game(s), skipped {} already-existing shortcut(s).", imported.len(), skipped.len());
    if !invalid.is_empty() {
        message.push_str(&format!(" {} game(s) had no numeric Lutris id and were left out.", invalid.len()));
    }
    if !failed.is_empty() {
        message.push_str(&format!(
            " {} game(s) failed; some may still have been added (see each error), so check the library before retrying.",
            failed.len()
        ));
    }
    let mut out = json!({ "message": message, "imported": imported, "skipped": skipped });
    if !invalid.is_empty() {
        out["invalidId"] = json!(invalid);
    }
    if bridged {
        out["via"] = "millennium".into();
        if !failed.is_empty() {
            out["failed"] = json!(failed);
        }
        if !imported.is_empty() {
            out["note"] = "The Lutris tag isn't set while Steam runs; add the shortcuts to a category with \
                           library category_add instead."
                .into();
        }
    }
    Ok(ToolResult::json(&out))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    type LiveList = Result<BridgeOutcome<Vec<BridgeShortcut>>>;

    /// Stands in for Steam: whether it runs, the bridge's answers in order,
    /// its live shortcut lists in order, and shortcuts.vdf.
    #[derive(Default)]
    struct FakeEnv {
        running: bool,
        answers: RefCell<VecDeque<Result<BridgeOutcome<BridgeShortcut>>>>,
        requests: RefCell<Vec<ShortcutRequest>>,
        lists: RefCell<VecDeque<LiveList>>,
        file: RefCell<Vec<ShortcutEntry>>,
    }

    impl FakeEnv {
        fn running(answers: Vec<Result<BridgeOutcome<BridgeShortcut>>>) -> Self {
            Self { running: true, answers: RefCell::new(answers.into()), ..Default::default() }
        }

        /// Steam's live shortcut list, for the next list_live call.
        fn holding(self, names: &[(i64, &str)]) -> Self {
            let live = names.iter().map(|(appid, name)| live_shortcut(*appid, name)).collect();
            self.lists.borrow_mut().push_back(Ok(BridgeOutcome::Done(live)));
            self
        }

        fn list_unavailable(self) -> Self {
            self.lists.borrow_mut().push_back(Ok(BridgeOutcome::Unavailable("Millennium is not running".into())));
            self
        }
    }

    fn live_shortcut(appid: i64, name: &str) -> BridgeShortcut {
        BridgeShortcut {
            appid,
            name: name.into(),
            exe: "\"/opt/x\"".into(),
            start_dir: "\"/opt\"".into(),
            launch_options: String::new(),
            last_played: 0,
        }
    }

    impl ShortcutEnv for FakeEnv {
        fn steam_running(&self) -> bool {
            self.running
        }

        async fn bridge(&self, request: &ShortcutRequest) -> Result<BridgeOutcome<BridgeShortcut>> {
            self.requests.borrow_mut().push(request.clone());
            self.answers.borrow_mut().pop_front().expect("an answer for every bridge request")
        }

        async fn list_live(&self) -> LiveList {
            self.lists.borrow_mut().pop_front().expect("a live list for every list_live call")
        }

        fn read(&self) -> Result<Vec<ShortcutEntry>> {
            Ok(self.file.borrow().clone())
        }

        fn add(&self, entries: &[ShortcutEntry]) -> Result<()> {
            self.file.borrow_mut().extend_from_slice(entries);
            Ok(())
        }

        fn remove(&self, matches: &dyn Fn(&ShortcutEntry) -> bool) -> Result<Option<ShortcutEntry>> {
            let mut file = self.file.borrow_mut();
            Ok(file.iter().position(matches).map(|i| file.remove(i)))
        }
    }

    fn args(v: Value) -> Args {
        Args(v.as_object().unwrap().clone())
    }

    fn json_of(result: &ToolResult) -> Value {
        serde_json::from_str(&result.text).unwrap_or_else(|_| panic!("not JSON: {}", result.text))
    }

    fn steam_shortcut(appid: i64, name: &str) -> Result<BridgeOutcome<BridgeShortcut>> {
        Ok(BridgeOutcome::Done(BridgeShortcut {
            appid,
            name: name.into(),
            exe: "lutris".into(),
            start_dir: String::new(),
            launch_options: String::new(),
            last_played: 0,
        }))
    }

    fn unavailable() -> Result<BridgeOutcome<BridgeShortcut>> {
        Ok(BridgeOutcome::Unavailable("Millennium is not running (no socket at /tmp/x.sock)".into()))
    }

    fn entry(appid: i64, name: &str) -> ShortcutEntry {
        ShortcutEntry {
            appid,
            app_name: name.into(),
            exe: "\"/opt/x\"".into(),
            start_dir: "\"/opt\"".into(),
            launch_options: String::new(),
            last_play_time: 0,
            tags: vec![],
        }
    }

    /// An executable the shortcut checks accept, under target/ (they look at every parent folder).
    #[cfg(unix)]
    fn game_exe() -> Option<(tempfile::TempDir, String)> {
        use std::os::unix::fs::PermissionsExt;
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&base).unwrap();
        let dir = tempfile::tempdir_in(&base).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let game = dir.path().join("game");
        std::fs::write(&game, "").unwrap();
        std::fs::set_permissions(&game, std::fs::Permissions::from_mode(0o755)).unwrap();
        let checked = check_shortcut_exe(game.to_str().unwrap()).ok()?;
        Some((dir, checked))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn add_while_steam_runs_goes_through_the_bridge() {
        // A checkout in a folder anyone can write to can't hold an acceptable program.
        let Some((_dir, exe)) = game_exe() else { return };
        let start_dir = dirname(&exe);
        let env = FakeEnv::running(vec![steam_shortcut(3000000002, "Game")]);
        let result = add(&env, &args(json!({ "name": "Game", "exe": exe, "tags": ["RPG"] }))).await.unwrap();
        let out = json_of(&result);
        assert_eq!((out["via"].as_str(), out["shortcut"]["appid"].as_i64()), (Some("millennium"), Some(3000000002)));
        assert!(out["note"].as_str().unwrap().contains("category_add"));
        assert_eq!(
            *env.requests.borrow(),
            [ShortcutRequest::Add {
                name: "Game".into(),
                exe: format!("\"{exe}\""),
                start_dir: format!("\"{start_dir}\""),
                launch_options: String::new(),
            }]
        );
        assert!(env.file.borrow().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn add_while_steam_runs_without_the_bridge_is_refused() {
        let Some((_dir, exe)) = game_exe() else { return };
        let env = FakeEnv::running(vec![unavailable()]);
        let result = add(&env, &args(json!({ "name": "Game", "exe": exe }))).await.unwrap();
        assert!(result.is_error);
        assert!(result.text.contains("Millennium bridge: Millennium is not running"), "{}", result.text);
        assert!(result.text.ends_with("Shortcuts were NOT modified."));
        assert!(env.file.borrow().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn add_while_steam_is_closed_writes_the_file() {
        let Some((_dir, exe)) = game_exe() else { return };
        let env = FakeEnv::default();
        let out = json_of(&add(&env, &args(json!({ "name": "Game", "exe": exe }))).await.unwrap());
        assert!(out.get("via").is_none());
        assert_eq!(env.file.borrow()[0].exe, format!("\"{exe}\""));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_file_route_keeps_the_appid_of_the_unquoted_path() {
        let Some((_dir, exe)) = game_exe() else { return };
        let env = FakeEnv::default();
        add(&env, &args(json!({ "name": "Game", "exe": exe }))).await.unwrap();
        assert_eq!(env.file.borrow()[0].appid, generate_shortcut_id(&exe, "Game"));
    }

    /// Steam closed at the first check, running by the second, under the lock.
    struct StartsDuringTheWait {
        inner: FakeEnv,
        checks: std::cell::Cell<u32>,
    }

    impl ShortcutEnv for StartsDuringTheWait {
        fn steam_running(&self) -> bool {
            self.checks.set(self.checks.get() + 1);
            self.checks.get() > 1
        }
        async fn bridge(&self, request: &ShortcutRequest) -> Result<BridgeOutcome<BridgeShortcut>> {
            self.inner.bridge(request).await
        }
        async fn list_live(&self) -> LiveList {
            self.inner.list_live().await
        }
        fn read(&self) -> Result<Vec<ShortcutEntry>> {
            self.inner.read()
        }
        fn add(&self, entries: &[ShortcutEntry]) -> Result<()> {
            self.inner.add(entries)
        }
        fn remove(&self, matches: &dyn Fn(&ShortcutEntry) -> bool) -> Result<Option<ShortcutEntry>> {
            self.inner.remove(matches)
        }
    }

    #[tokio::test]
    async fn a_steam_that_starts_while_waiting_for_the_lock_gets_the_bridge() {
        let env = StartsDuringTheWait {
            inner: FakeEnv::running(vec![steam_shortcut(3000000001, "Old")]).holding(&[(3000000001, "Old")]),
            checks: Default::default(),
        };
        env.inner.file.borrow_mut().push(entry(3000000001, "Old"));
        let out = json_of(&remove(&env, &args(json!({ "name": "Old" }))).await.unwrap());
        assert_eq!(out["via"], "millennium");
        assert_eq!(env.inner.file.borrow().len(), 1, "the file was left alone");

        let env = StartsDuringTheWait {
            inner: FakeEnv::running(vec![unavailable()]).list_unavailable(),
            checks: Default::default(),
        };
        let result = import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);
        assert!(env.inner.file.borrow().is_empty());
    }

    #[tokio::test]
    async fn add_checks_the_program_before_asking_steam() {
        let env = FakeEnv::running(vec![]);
        let result = add(&env, &args(json!({ "name": "Shell", "exe": "/bin/sh" }))).await.unwrap();
        assert!(result.is_error && result.text.contains("NOT added"), "{}", result.text);
        assert!(env.requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn remove_while_steam_runs_finds_the_appid_by_name_in_steams_list() {
        // Just added: Steam holds it, shortcuts.vdf doesn't show it yet.
        let env = FakeEnv::running(vec![steam_shortcut(3000000001, "New")]).holding(&[(3000000001, "New")]);
        let out = json_of(&remove(&env, &args(json!({ "name": "New" }))).await.unwrap());
        assert_eq!((out["via"].as_str(), out["removed"]["name"].as_str()), (Some("millennium"), Some("New")));
        assert_eq!(*env.requests.borrow(), [ShortcutRequest::Remove { appid: 3000000001 }]);
    }

    #[tokio::test]
    async fn remove_while_steam_runs_ignores_names_only_the_file_still_has() {
        let env = FakeEnv::running(vec![]).holding(&[]);
        env.file.borrow_mut().push(entry(3000000001, "Gone"));
        let result = remove(&env, &args(json!({ "name": "Gone" }))).await.unwrap();
        assert!(result.is_error && result.text.contains("not found"), "{}", result.text);
        assert!(env.requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn remove_by_name_without_steams_list_falls_back_to_the_file() {
        // A plugin that can remove but not list (built before listing).
        let env = FakeEnv::running(vec![steam_shortcut(3000000001, "Old")]).list_unavailable();
        env.file.borrow_mut().push(entry(3000000001, "Old"));
        let out = json_of(&remove(&env, &args(json!({ "name": "Old" }))).await.unwrap());
        assert_eq!(out["via"], "millennium");
        assert_eq!(*env.requests.borrow(), [ShortcutRequest::Remove { appid: 3000000001 }]);

        // No bridge at all: the removal itself is refused.
        let env = FakeEnv::running(vec![unavailable()]).list_unavailable();
        env.file.borrow_mut().push(entry(3000000001, "Old"));
        let result = remove(&env, &args(json!({ "name": "Old" }))).await.unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);

        // A file entry without an appid can't be removed through the bridge.
        let env = FakeEnv::running(vec![]).list_unavailable();
        env.file.borrow_mut().push(entry(0, "Ancient"));
        let result = remove(&env, &args(json!({ "name": "Ancient" }))).await.unwrap();
        assert!(result.is_error && result.text.contains("no appid"), "{}", result.text);
        assert!(env.requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn list_while_steam_runs_shows_steams_shortcuts_with_the_files_tags() {
        let mut played = live_shortcut(3000000001, "Old");
        played.last_played = 1_700_000_000;
        let env = FakeEnv::running(vec![]);
        env.lists.borrow_mut().push_back(Ok(BridgeOutcome::Done(vec![played, live_shortcut(3000000002, "New")])));
        let mut tagged = entry(3000000001, "Old");
        tagged.tags = vec!["RPG".into()];
        env.file.borrow_mut().push(tagged);
        let out = json_of(&list(&env).await.unwrap());
        assert_eq!((out["count"].as_u64(), out["via"].as_str()), (Some(2), Some("millennium")));
        assert_eq!(out["shortcuts"][0]["tags"], json!(["RPG"]));
        assert_eq!(out["shortcuts"][0]["lastPlayed"], format_timestamp(1_700_000_000));
        assert_eq!((out["shortcuts"][1]["name"].as_str(), &out["shortcuts"][1]["tags"]), (Some("New"), &json!([])));
    }

    #[tokio::test]
    async fn a_failed_list_falls_back_to_the_file_but_a_timed_out_one_does_not() {
        // A Steam internal the list needs is missing: remove still works by the file's appid.
        let env = FakeEnv::running(vec![steam_shortcut(3000000001, "Old")]);
        env.lists.borrow_mut().push_back(Err(anyhow!("Steam's appStore has no app list to read shortcuts from")));
        env.file.borrow_mut().push(entry(3000000001, "Old"));
        let out = json_of(&remove(&env, &args(json!({ "name": "Old" }))).await.unwrap());
        assert_eq!(out["via"], "millennium");

        // A list that timed out says nothing about what Steam holds.
        let env = FakeEnv::running(vec![]);
        let timed_out =
            BridgeError { message: "did not answer within 15s".into(), timeout: true, outcome_unknown: true };
        env.lists.borrow_mut().push_back(Err(timed_out.into()));
        env.file.borrow_mut().push(entry(3000000001, "Old"));
        assert!(import_lutris_games(&env, &args(json!({})), LUTRIS).await.is_err());
        assert!(env.requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn import_does_not_guess_from_the_file_when_steams_list_fails() {
        // The file may miss a shortcut added seconds ago; importing from it could add it twice.
        let env = FakeEnv::running(vec![]);
        env.lists.borrow_mut().push_back(Err(anyhow!("Steam sent no details for shortcut appid 3000000001")));
        let err = import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap_err();
        assert!(err.to_string().contains("nothing was changed"), "{err}");
        assert!(env.requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn an_empty_live_list_with_shortcuts_in_the_file_says_steam_may_be_loading() {
        let env = FakeEnv::running(vec![]).holding(&[]);
        env.file.borrow_mut().push(entry(5, "Old"));
        let result = list(&env).await.unwrap();
        assert!(result.text.contains("may still be loading"), "{}", result.text);

        let env = FakeEnv::running(vec![]).holding(&[]);
        assert_eq!(list(&env).await.unwrap().text, NO_SHORTCUTS);
    }

    #[tokio::test]
    async fn list_while_steam_runs_without_the_bridge_reads_the_file_with_a_note() {
        let env = FakeEnv::running(vec![]).list_unavailable();
        env.file.borrow_mut().push(entry(5, "Old"));
        let out = json_of(&list(&env).await.unwrap());
        assert_eq!(out["count"], 1);
        assert!(out["note"].as_str().unwrap().contains("may lag"));

        let env = FakeEnv::running(vec![]).list_unavailable();
        let result = list(&env).await.unwrap();
        assert!(result.text.starts_with(NO_SHORTCUTS) && result.text.contains("may lag"), "{}", result.text);

        let env = FakeEnv::default();
        env.file.borrow_mut().push(entry(5, "Old"));
        let out = json_of(&list(&env).await.unwrap());
        assert!(out.get("note").is_none() && out.get("via").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_uncertain_add_warns_about_adding_twice() {
        let Some((_dir, exe)) = game_exe() else { return };
        let uncertain = BridgeError {
            message: "Lost the connection to Millennium (x); the edit may or may not have been applied.".into(),
            timeout: false,
            outcome_unknown: true,
        };
        let env = FakeEnv::running(vec![Err(uncertain.into())]);
        let err = add(&env, &args(json!({ "name": "Game", "exe": exe }))).await.unwrap_err();
        assert!(err.to_string().contains("added twice"), "{err}");

        let env = FakeEnv::running(vec![Err(anyhow!("Steam knows no non-Steam shortcut"))]);
        let err = add(&env, &args(json!({ "name": "Game", "exe": exe }))).await.unwrap_err();
        assert!(!err.to_string().contains("added twice"), "{err}");
    }

    #[tokio::test]
    async fn remove_while_steam_runs_passes_an_appid_through() {
        let env = FakeEnv::running(vec![steam_shortcut(3000000009, "New")]);
        remove(&env, &args(json!({ "appid": 3000000009u64 }))).await.unwrap();
        assert_eq!(*env.requests.borrow(), [ShortcutRequest::Remove { appid: 3000000009 }]);
    }

    #[tokio::test]
    async fn remove_while_steam_runs_of_an_unknown_name_asks_nothing() {
        let env = FakeEnv::running(vec![]).holding(&[(3000000001, "Other")]);
        let result = remove(&env, &args(json!({ "name": "Nope" }))).await.unwrap();
        assert!(result.is_error && result.text.contains("not found"), "{}", result.text);
        assert!(env.requests.borrow().is_empty());
    }

    #[tokio::test]
    async fn remove_while_steam_runs_without_the_bridge_is_refused() {
        let env = FakeEnv::running(vec![unavailable()]);
        let result = remove(&env, &args(json!({ "appid": 3000000009u64 }))).await.unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);
    }

    const LUTRIS: &str = r#"[{"id": 1, "name": "One"}, {"id": 2, "name": "Two"}, {"id": 3, "name": "Kept"}]"#;

    #[tokio::test]
    async fn import_while_steam_runs_adds_each_game_and_reports_failures() {
        // "kept" was just added in Steam: the file doesn't show it, Steam's list does.
        let env = FakeEnv::running(vec![steam_shortcut(3000000001, "One"), Err(anyhow!("Steam refused"))])
            .holding(&[(3000000005, "kept")]);
        let out = json_of(&import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap());
        assert_eq!(out["imported"], json!(["One"]));
        assert_eq!(out["skipped"], json!(["Kept"]));
        assert_eq!(out["failed"], json!([{ "name": "Two", "error": "Steam refused" }]));
        assert_eq!(out["via"], "millennium");
        assert!(out["message"].as_str().unwrap().contains("1 game(s) failed"));
        let requests = env.requests.borrow();
        assert_eq!(requests.len(), 2);
        assert!(matches!(&requests[1], ShortcutRequest::Add { name, exe, launch_options, .. }
            if name == "Two" && exe == "lutris" && launch_options == "lutris:rungameid/2"));
    }

    #[tokio::test]
    async fn import_while_steam_runs_without_the_bridge_is_refused() {
        let env = FakeEnv::running(vec![unavailable()]).list_unavailable();
        let result = import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);
        assert_eq!(env.requests.borrow().len(), 1);

        // The bridge going away after the list, before anything was added, is still a refusal.
        let env = FakeEnv::running(vec![unavailable()]).holding(&[]);
        let result = import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap();
        assert!(result.is_error && result.text.contains("NOT modified"), "{}", result.text);
    }

    #[tokio::test]
    async fn import_without_steams_list_skips_by_the_file() {
        let env = FakeEnv::running(vec![steam_shortcut(3000000001, "One"), steam_shortcut(3000000002, "Two")])
            .list_unavailable();
        env.file.borrow_mut().push(entry(5, "kept"));
        let out = json_of(&import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap());
        assert_eq!((&out["imported"], &out["skipped"]), (&json!(["One", "Two"]), &json!(["Kept"])));
    }

    #[tokio::test]
    async fn import_while_steam_is_closed_writes_the_file() {
        let env = FakeEnv::default();
        let out = json_of(&import_lutris_games(&env, &args(json!({})), LUTRIS).await.unwrap());
        assert_eq!(out["imported"], json!(["One", "Two", "Kept"]));
        assert!(out.get("via").is_none());
        assert_eq!(env.file.borrow().len(), 3);
    }

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
