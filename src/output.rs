//! The output folder: the one place exports and backups are written.
//!
//! `STEAM_MCP_OUTPUT_DIR` sets it; otherwise it is `steam-mcp` in the user's
//! Documents folder. Tool arguments only name new files and folders inside it,
//! and nothing already there is ever replaced. The checks guard against what
//! the model asks for; they don't try to win races against other local
//! programs, which run as the same user anyway.

use std::env;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::util::{home_dir, io_error};

pub const OUTPUT_DIR_ENV: &str = "STEAM_MCP_OUTPUT_DIR";

/// The output folder, created if needed, with symlinks resolved.
pub fn output_dir() -> Result<PathBuf> {
    let dir = match env::var_os(OUTPUT_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        Some(v) => bail!("{OUTPUT_DIR_ENV} must be an absolute path, got {:?}", v),
        None => documents_dir().join("steam-mcp"),
    };
    fs::create_dir_all(&dir).map_err(|e| io_error(&e, "mkdir", &dir))?;
    fs::canonicalize(&dir).map_err(|e| io_error(&e, "realpath", &dir))
}

/// The user's Documents folder: XDG_DOCUMENTS_DIR from user-dirs.dirs on
/// Linux and the BSDs, `~/Documents` elsewhere and as the fallback.
fn documents_dir() -> PathBuf {
    let home = home_dir();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let config = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        if let Some(dir) =
            fs::read_to_string(config.join("user-dirs.dirs")).ok().and_then(|t| parse_documents_dir(&t, &home))
        {
            return dir;
        }
    }
    home.join("Documents")
}

/// The XDG_DOCUMENTS_DIR value of a user-dirs.dirs file. As xdg-user-dirs
/// writes it, the value is a shell double-quoted string, either absolute or
/// starting with `$HOME`; backslash escapes are decoded, nothing is expanded.
#[cfg_attr(any(not(unix), target_os = "macos"), allow(dead_code))]
fn parse_documents_dir(text: &str, home: &Path) -> Option<PathBuf> {
    let value = text.lines().find_map(|l| l.trim().strip_prefix("XDG_DOCUMENTS_DIR="))?;
    let quoted = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    let (home_prefix, rest) = match quoted.strip_prefix("$HOME") {
        Some(rest) => (true, rest),
        None => (false, quoted),
    };
    let mut decoded = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next @ ('"' | '\\' | '$' | '`')) => decoded.push(next),
                Some(next) => {
                    decoded.push('\\');
                    decoded.push(next);
                }
                None => decoded.push('\\'),
            },
            // An unescaped `$` would be a shell expansion we don't perform.
            '$' => return None,
            c => decoded.push(c),
        }
    }
    let value = if home_prefix { format!("$HOME{decoded}") } else { decoded };
    match value.strip_prefix("$HOME") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => Some(home.join(rest.trim_start_matches('/'))),
        Some(_) => None,
        None => value.starts_with('/').then(|| PathBuf::from(value)),
    }
}

/// Why `part`, one `/`-separated piece of a name, can't be a file or folder
/// name, or None when it can. The rules are Windows' (the strictest), applied
/// everywhere so a name means the same file on every platform: no drive,
/// stream (`a:b`) or separator characters, no trailing dot or space, no
/// device names like `CON` or `nul.json`.
fn unusable_part(part: &str) -> Option<&'static str> {
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
    if part.is_empty() || part == "." || part == ".." {
        return Some("must be a plain relative name, without empty parts, '.' or '..'");
    }
    if part.starts_with('.') {
        return Some("names a hidden file or folder");
    }
    if part.chars().any(|c| c.is_control() || "<>:\"|?*\\".contains(c)) {
        return Some("contains a character file names can't have (< > : \" | ? * \\ or a control character)");
    }
    if part.ends_with(['.', ' ']) {
        return Some("ends in a dot or space");
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end().to_ascii_uppercase();
    let numbered =
        |prefix: &str| stem.strip_prefix(prefix).is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit());
    if RESERVED.contains(&stem.as_str()) || numbered("COM") || numbered("LPT") {
        return Some("is a reserved device name");
    }
    None
}

/// `name` as a path inside `dir`: `/`-separated plain names, none of them
/// hidden or unusable as a file name (see `unusable_part`).
pub fn resolve_name(dir: &Path, name: &str) -> Result<PathBuf> {
    let mut path = dir.to_path_buf();
    for part in name.split('/') {
        if let Some(why) = unusable_part(part) {
            bail!("{name:?} {why}. Give a file or folder name inside the output folder {}.", dir.display());
        }
        path.push(part);
    }
    Ok(path)
}

/// Create the folders between `dir` and `path`, refusing to pass through a
/// symlink or anything else that isn't a plain folder.
fn create_parents(dir: &Path, path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else { return Ok(()) };
    let Ok(below) = parent.strip_prefix(dir) else { bail!("{} is outside {}", path.display(), dir.display()) };
    let mut current = dir.to_path_buf();
    for part in below.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => bail!("{} exists and is not a folder", current.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|e| io_error(&e, "mkdir", &current))?
            }
            Err(e) => return Err(io_error(&e, "lstat", &current)),
        }
    }
    Ok(())
}

fn already_exists(path: &Path) -> anyhow::Error {
    anyhow::anyhow!("{} already exists; nothing was overwritten. Choose another name.", path.display())
}

/// `name` resolved inside `dir`, refused when something already has that name.
/// For an early check only: `create_file` is what guarantees it.
pub fn check_new_name(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = resolve_name(dir, name)?;
    let mut current = dir.to_path_buf();
    for part in name.split('/') {
        current.push(part);
        let Ok(meta) = fs::symlink_metadata(&current) else { break };
        if current == path {
            return Err(already_exists(&path));
        }
        if !meta.is_dir() {
            bail!("{} exists and is not a folder", current.display());
        }
    }
    Ok(path)
}

/// Create the new file `name` in `dir`; fails if anything is already there.
pub fn create_file(dir: &Path, name: &str) -> Result<(PathBuf, File)> {
    let path = resolve_name(dir, name)?;
    create_parents(dir, &path)?;
    match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(file) => Ok((path, file)),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(already_exists(&path)),
        Err(e) => Err(io_error(&e, "open", &path)),
    }
}

/// Create a new, empty folder named `base` in `dir`, or `base_2`, `base_3`, …
/// when that is taken. Never reuses an existing folder.
pub fn create_unique_folder(dir: &Path, base: &str) -> Result<PathBuf> {
    resolve_name(dir, base)?;
    for n in 1..=100 {
        let name = if n == 1 { base.to_string() } else { format!("{base}_{n}") };
        let path = resolve_name(dir, &name)?;
        create_parents(dir, &path)?;
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error(&e, "mkdir", &path)),
        }
    }
    bail!("{} and its next 99 numbered names are all taken; try again in a second", dir.join(base).display())
}

/// Local time for file names: sortable and free of characters Windows rejects.
pub fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_documents_dir_from_user_dirs() {
        let home = Path::new("/home/u");
        let text = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOCUMENTS_DIR=\"$HOME/Dokumente\"\n";
        assert_eq!(parse_documents_dir(text, home), Some(PathBuf::from("/home/u/Dokumente")));
        assert_eq!(parse_documents_dir("XDG_DOCUMENTS_DIR=\"/data/docs\"", home), Some(PathBuf::from("/data/docs")));
        assert_eq!(parse_documents_dir("XDG_DOCUMENTS_DIR=\"$HOME\"", home), Some(PathBuf::from("/home/u")));
        assert_eq!(parse_documents_dir("XDG_DOCUMENTS_DIR=\"docs\"", home), None);
        assert_eq!(parse_documents_dir("XDG_DOCUMENTS_DIR=\"$HOMEdocs\"", home), None);
        assert_eq!(
            parse_documents_dir(r#"XDG_DOCUMENTS_DIR="$HOME/Doc\$s \"x\"""#, home),
            Some(PathBuf::from("/home/u/Doc$s \"x\""))
        );
        assert_eq!(parse_documents_dir("XDG_DOCUMENTS_DIR=\"$HOME/$OTHER\"", home), None);
        assert_eq!(parse_documents_dir("", home), None);
    }

    #[test]
    fn only_plain_relative_names_resolve() {
        let dir = Path::new("/out");
        assert_eq!(resolve_name(dir, "lib.json").unwrap(), PathBuf::from("/out/lib.json"));
        assert_eq!(resolve_name(dir, "2026/lib.json").unwrap(), PathBuf::from("/out/2026/lib.json"));
        assert_eq!(resolve_name(dir, "COMX.json").unwrap(), PathBuf::from("/out/COMX.json"));
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "./x",
            "a/./b.json",
            "a//b",
            "a/",
            ".bashrc",
            "a/.ssh/x",
            "a\0b",
            "a.json:stream",
            "C:x",
            "C:\\x",
            "x\\y",
            "\\\\server\\share\\x",
            "a\nb",
            "con",
            "nul.json",
            "Com1.csv",
            "lpt9",
            "CONOUT$",
            "a.",
            "a ",
        ] {
            assert!(resolve_name(dir, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn never_replaces_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_file(dir, "a/b.json").unwrap();
        assert!(create_file(dir, "a/b.json").unwrap_err().to_string().contains("already exists"));
        assert!(check_new_name(dir, "a/b.json").is_err());
        assert!(check_new_name(dir, "a/c.json").is_ok());
        assert!(check_new_name(dir, "a/b.json/x").unwrap_err().to_string().contains("not a folder"));
        assert!(create_file(dir, "a/b.json/x").unwrap_err().to_string().contains("not a folder"));

        assert_eq!(create_unique_folder(dir, "saves").unwrap(), dir.join("saves"));
        assert_eq!(create_unique_folder(dir, "saves").unwrap(), dir.join("saves_2"));
        assert!(create_file(dir, "saves").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn refuses_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::os::unix::fs::symlink(outside.path(), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("target"), dir.join("file.json")).unwrap();
        assert!(create_file(dir, "link/x.json").is_err());
        fs::write(outside.path().join("there.json"), "").unwrap();
        assert!(check_new_name(dir, "link/there.json").unwrap_err().to_string().contains("not a folder"));
        assert!(create_unique_folder(dir, "link/x").is_err());
        assert!(create_file(dir, "file.json").is_err());
        assert!(!outside.path().join("target").exists());
        assert!(!outside.path().join("x.json").exists());
    }
}
