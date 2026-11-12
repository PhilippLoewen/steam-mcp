//! Locating the Steam install, the active user and the library folders.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};

use crate::util::{home_dir, read_text};
use crate::vdf::{VdfExt, VdfObject, VdfValue, js_entries, parse_int, parse_vdf};

/// SteamID64 of account id 0; SteamID32 = SteamID64 - this.
const STEAMID64_BASE: i64 = 76_561_197_960_265_728;

#[derive(Debug, Clone)]
pub struct UserConfig {
    pub steam_id64: String,
    pub steam_id32: i64,
    pub account_name: String,
    pub persona_name: String,
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// The Steam installation directory.
///
/// Linux: ~/.local/share/Steam, $XDG_DATA_HOME/Steam, ~/.steam/steam.
/// macOS: ~/Library/Application Support/Steam.
/// Windows: the registry's InstallPath, then the usual Program Files locations.
pub fn get_steam_dir() -> Result<PathBuf> {
    let home = home_dir();
    let mut candidates: Vec<PathBuf> = Vec::new();

    if cfg!(target_os = "linux") {
        candidates.push(home.join(".local/share/Steam"));
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            candidates.push(PathBuf::from(xdg).join("Steam"));
        }
        candidates.push(home.join(".steam/steam"));
    } else if cfg!(target_os = "macos") {
        candidates.push(home.join("Library/Application Support/Steam"));
    } else if cfg!(windows) {
        if let Some(path) = windows_registry_install_path() {
            candidates.push(PathBuf::from(path));
        }
        candidates.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
        candidates.push(PathBuf::from(r"C:\Program Files\Steam"));
        candidates.push(home.join("Steam"));
    }

    if let Some(found) = candidates.iter().find(|c| c.exists()) {
        return Ok(found.clone());
    }
    let checked: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    bail!("Steam installation not found. Platform: {}. Checked: {}", platform(), checked.join(", "))
}

fn windows_registry_install_path() -> Option<String> {
    let output = Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\WOW6432Node\Valve\Steam", "/v", "InstallPath"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let re = regex::Regex::new(r"InstallPath\s+REG_SZ\s+(.+)").ok()?;
    re.captures(&text).map(|c| c[1].trim().to_string())
}

/// Pick the active account from loginusers.vdf's "users" section: the one
/// marked MostRecent, else the newest Timestamp (newer clients dropped
/// MostRecent), else the only entry.
pub fn select_user_entry(users: &VdfObject) -> Option<(&str, &VdfObject)> {
    let entries: Vec<(&str, &VdfObject)> =
        users.iter().filter_map(|(k, v)| v.as_obj().map(|o| (k.as_str(), o))).collect();

    if let Some(found) = entries.iter().find(|(_, u)| {
        u.get("MostRecent").and_then(VdfValue::as_str) == Some("1")
            || u.get("mostrecent").and_then(VdfValue::as_str) == Some("1")
    }) {
        return Some(*found);
    }

    let mut newest = None;
    let mut newest_ts = i64::MIN;
    for entry in &entries {
        if let Some(ts) = entry.1.str_of(&["Timestamp", "timestamp"]).and_then(parse_int)
            && ts > newest_ts
        {
            newest = Some(*entry);
            newest_ts = ts;
        }
    }
    if newest.is_some() {
        return newest;
    }

    if entries.len() == 1 { Some(entries[0]) } else { None }
}

/// The account picked by [`select_user_entry`] from config/loginusers.vdf.
pub fn get_user_config() -> Result<UserConfig> {
    let path = get_steam_dir()?.join("config/loginusers.vdf");
    let parsed = parse_vdf(&read_text(&path)?);
    let users = parsed
        .get("users")
        .and_then(VdfValue::as_obj)
        .ok_or_else(|| anyhow!("No \"users\" section found in loginusers.vdf"))?;
    let (id64, user) = select_user_entry(users)
        .ok_or_else(|| anyhow!("No user with MostRecent=1 or a valid Timestamp found in loginusers.vdf"))?;
    let id64_num: i64 = id64.parse().map_err(|_| anyhow!("Cannot convert {id64} to a BigInt"))?;

    Ok(UserConfig {
        steam_id64: id64.to_string(),
        steam_id32: id64_num - STEAMID64_BASE,
        account_name: user.str_or_empty(&["AccountName", "accountname"]),
        persona_name: user.str_or_empty(&["PersonaName", "personaname"]),
    })
}

/// userdata/{steamId32} of the active account.
pub fn get_user_data_dir() -> Result<PathBuf> {
    let steam = get_steam_dir()?;
    let user = get_user_config()?;
    Ok(steam.join("userdata").join(user.steam_id32.to_string()))
}

/// The libraryfolders section of config/libraryfolders.vdf.
pub fn read_library_folders_vdf() -> Result<VdfObject> {
    let path = get_steam_dir()?.join("config/libraryfolders.vdf");
    let mut parsed = parse_vdf(&read_text(&path)?);
    let section = parsed.shift_remove("libraryfolders").or_else(|| parsed.shift_remove("LibraryFolders"));
    match section {
        Some(VdfValue::Obj(o)) => Ok(o),
        _ => bail!("No \"libraryfolders\" section found in libraryfolders.vdf"),
    }
}

/// Every library folder path listed in config/libraryfolders.vdf.
pub fn get_library_folders() -> Result<Vec<PathBuf>> {
    let folders = read_library_folders_vdf()?;
    Ok(js_entries(&folders)
        .into_iter()
        .filter(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|(_, v)| match v {
            // Older format: index -> path.
            VdfValue::Str(p) => Some(PathBuf::from(p)),
            VdfValue::Obj(o) => o.get("path").and_then(VdfValue::as_str).map(PathBuf::from),
        })
        .collect())
}

/// Open a steam:// URL with the platform's handler.
pub fn open_steam_url(url: &str) -> Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/c", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("steam");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    // Reap the child in the background so it doesn't linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Whether the Steam client is running, as far as the process list tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteamState {
    Running,
    NotRunning,
    /// The process list couldn't be read.
    Unknown,
}

/// Look for the Steam client process: `steam` on Linux, `steam_osx` on
/// macOS, `steam.exe` on Windows.
pub fn steam_state() -> SteamState {
    if cfg!(windows) {
        match Command::new("tasklist").args(["/FI", "IMAGENAME eq steam.exe", "/NH"]).stderr(Stdio::null()).output() {
            Ok(o) if o.status.success() => {
                if String::from_utf8_lossy(&o.stdout).to_lowercase().contains("steam.exe") {
                    SteamState::Running
                } else {
                    SteamState::NotRunning
                }
            }
            _ => SteamState::Unknown,
        }
    } else {
        let status = Command::new("pgrep")
            .args(["-xi", "steam(_osx)?"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        pgrep_state(status.map(|s| s.code()))
    }
}

/// pgrep exits 0 on a match, 1 on none, higher on errors; a spawn failure or
/// a kill by signal (no code) can't tell either.
fn pgrep_state(code: std::io::Result<Option<i32>>) -> SteamState {
    match code {
        Ok(Some(0)) => SteamState::Running,
        Ok(Some(1)) => SteamState::NotRunning,
        _ => SteamState::Unknown,
    }
}

/// Whether Steam may be running: for guards that refuse to edit files Steam
/// would overwrite, where not knowing has to count as running.
pub fn steam_may_be_running() -> bool {
    steam_state() != SteamState::NotRunning
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users(text: &str) -> VdfObject {
        match parse_vdf(text).shift_remove("users") {
            Some(VdfValue::Obj(o)) => o,
            _ => panic!("no users"),
        }
    }

    #[test]
    fn pgrep_failures_are_unknown() {
        assert_eq!(pgrep_state(Ok(Some(0))), SteamState::Running);
        assert_eq!(pgrep_state(Ok(Some(1))), SteamState::NotRunning);
        assert_eq!(pgrep_state(Ok(Some(2))), SteamState::Unknown);
        assert_eq!(pgrep_state(Ok(None)), SteamState::Unknown);
        assert_eq!(pgrep_state(Err(std::io::Error::other("no pgrep"))), SteamState::Unknown);
    }

    #[test]
    fn prefers_most_recent() {
        let u =
            users(r#""users" { "1" { "MostRecent" "0" "Timestamp" "9" } "2" { "MostRecent" "1" "Timestamp" "1" } }"#);
        assert_eq!(select_user_entry(&u).unwrap().0, "2");
    }

    #[test]
    fn accepts_lowercase_most_recent() {
        let u = users(r#""users" { "1" { "mostrecent" "1" } "2" { "Timestamp" "5" } }"#);
        assert_eq!(select_user_entry(&u).unwrap().0, "1");
    }

    #[test]
    fn falls_back_to_newest_timestamp() {
        let u = users(r#""users" { "1" { "Timestamp" "100" } "2" { "timestamp" "200" } "3" { "Timestamp" "150" } }"#);
        assert_eq!(select_user_entry(&u).unwrap().0, "2");
    }

    #[test]
    fn falls_back_to_the_only_entry() {
        let u = users(r#""users" { "1" { "AccountName" "a" } }"#);
        assert_eq!(select_user_entry(&u).unwrap().0, "1");
    }

    #[test]
    fn gives_up_on_several_entries_without_hints() {
        let u = users(r#""users" { "1" { "AccountName" "a" } "2" { "AccountName" "b" } }"#);
        assert!(select_user_entry(&u).is_none());
    }

    #[test]
    fn ignores_string_entries() {
        let u = users(r#""users" { "junk" "x" "1" { "Timestamp" "3" } }"#);
        assert_eq!(select_user_entry(&u).unwrap().0, "1");
    }
}
