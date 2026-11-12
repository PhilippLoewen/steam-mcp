//! Formatting, filesystem and JSON helpers shared by the tools.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;

/// "1.23 GB", "456.78 MB", "512 B".
pub fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    // Not f64::max: NaN must stay NaN, as in JS.
    let mut value = if bytes < 0.0 { 0.0 } else { bytes };
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{} B", js_num(value));
    }
    format!("{} {}", to_fixed(value, 2), UNITS[unit])
}

/// "12.3 hours" or "45 minutes".
pub fn format_playtime(minutes: f64) -> String {
    let minutes = if minutes < 0.0 { 0.0 } else { minutes };
    if minutes < 60.0 {
        return format!("{} minutes", js_num(minutes));
    }
    format!("{} hours", to_fixed(minutes / 60.0, 1))
}

/// ISO 8601 with milliseconds, like JS `toISOString()`; "N/A" for zero or negative.
pub fn format_timestamp(unix_seconds: i64) -> String {
    if unix_seconds <= 0 {
        return "N/A".to_string();
    }
    iso_from_millis(unix_seconds.saturating_mul(1000))
}

/// JS `new Date(ms).toISOString()`.
pub fn iso_from_millis(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "Invalid Date".to_string())
}

/// Current time as Unix seconds.
pub fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Current time as Unix milliseconds.
pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A number as JS prints it in a template string: integral values without ".0".
pub fn js_num(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// A number as JSON.stringify writes it: integral values as integers, NaN/Infinity as null.
pub fn num(n: f64) -> Value {
    if !n.is_finite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        Value::from(n as i64)
    } else {
        Value::from(n)
    }
}

/// `n` with `digits` decimals; exact ties round to even. NaN and infinities
/// print as JS does ("NaN", "Infinity").
pub fn to_fixed(n: f64, digits: usize) -> String {
    if n.is_finite() { format!("{n:.digits$}") } else { js_num(n) }
}

/// `Array.prototype.sort` with a JS comparator: stable, `b` goes first only
/// when `cmp(a, b) > 0`, so NaN counts as equal. Unlike `sort_by`, an
/// inconsistent comparator can't panic.
pub fn js_sort<T>(v: &mut Vec<T>, cmp: impl Fn(&T, &T) -> f64 + Copy) {
    if v.len() < 2 {
        return;
    }
    let mut right = v.split_off(v.len() / 2);
    js_sort(v, cmp);
    js_sort(&mut right, cmp);
    let mut left = std::mem::take(v).into_iter().peekable();
    let mut right = right.into_iter().peekable();
    while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
        let next = if cmp(a, b) > 0.0 { right.next() } else { left.next() };
        v.extend(next);
    }
    v.extend(left);
    v.extend(right);
}

/// `String.prototype.localeCompare`: ICU collation with default options, as Node uses.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    use icu_collator::options::CollatorOptions;
    use icu_collator::{Collator, CollatorBorrowed};
    static COLLATOR: std::sync::LazyLock<CollatorBorrowed<'static>> = std::sync::LazyLock::new(|| {
        Collator::try_new(Default::default(), CollatorOptions::default()).expect("compiled collation data")
    });
    COLLATOR.compare(a, b)
}

/// Recursive size of regular files under `dir`; symlinks and unreadable entries are skipped.
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let mut size = 0;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            size += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else if ft.is_dir() {
            size += dir_size(&entry.path());
        }
    }
    size
}

/// Recursive count of regular files under `dir`.
pub fn count_files(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    let mut count = 0;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() {
            count += 1;
        } else if ft.is_dir() {
            count += count_files(&entry.path());
        }
    }
    count
}

/// Describe an I/O error the way Node does ("ENOENT: no such file or directory, open '<path>'").
pub fn io_error(err: &std::io::Error, syscall: &str, path: &Path) -> anyhow::Error {
    use std::io::ErrorKind::*;
    let code = match err.kind() {
        NotFound => "ENOENT: no such file or directory".to_string(),
        PermissionDenied => "EACCES: permission denied".to_string(),
        AlreadyExists => "EEXIST: file already exists".to_string(),
        IsADirectory => "EISDIR: illegal operation on a directory".to_string(),
        NotADirectory => "ENOTDIR: not a directory".to_string(),
        _ => err.to_string(),
    };
    anyhow::anyhow!("{code}, {syscall} '{}'", path.display())
}

/// Read a UTF-8 text file, with Node-style error messages.
pub fn read_text(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).map_err(|e| io_error(&e, "open", path))?;
    Ok(String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

/// File names in `dir` sorted bytewise, as Node's readdirSync returns them,
/// or an error when it can't be read.
pub fn read_dir_names(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> =
        fs::read_dir(dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    Ok(names)
}

/// Directory entries sorted by name, as Node's readdirSync returns them.
pub fn sorted_entries(dir: &Path) -> std::io::Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

/// Free bytes on the filesystem holding `path` (`bfree * bsize`, like Node's statfs).
#[cfg(unix)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bfree as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

/// Held for every read-modify-write of a Steam or MangoHud file. Tool calls
/// run concurrently, so without it parallel edits would overwrite each other.
pub static FILE_EDIT_LOCK: Mutex<()> = Mutex::new(());

pub fn file_edit_lock() -> std::sync::MutexGuard<'static, ()> {
    FILE_EDIT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Replace `file` with `content` without ever leaving it half-written: write a
/// sibling temp file, flush it to disk and rename it over `file`. An existing
/// file's permissions carry over. When `file` is a symlink, its target is
/// replaced and the link stays.
pub fn write_atomic(file: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let file = match fs::symlink_metadata(file) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(file)?,
        _ => file.to_path_buf(),
    };
    let permissions = match fs::metadata(&file) {
        Ok(meta) => Some(meta.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let _ = fs::remove_file(&tmp);
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Replacing a file: nothing but the owner may read the content before
        // its permissions are copied over. A new file gets the usual 0666 & !umask.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, if permissions.is_some() { 0o600 } else { 0o666 });
        let mut out = options.open(&tmp)?;
        if let Some(permissions) = permissions {
            out.set_permissions(permissions)?;
        }
        out.write_all(content)?;
        out.sync_all()?;
        fs::rename(&tmp, &file)?;
        sync_parent(&file);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Flush `path`'s directory entry to disk, so a rename survives a power loss.
/// Best effort: not every platform or filesystem supports it.
fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(dir) = fs::File::open(if dir.as_os_str().is_empty() { Path::new(".") } else { dir })
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Backups of one file that `backup_file` keeps; older ones are deleted.
const BACKUPS_KEPT: usize = 5;

/// The timestamp in backup names, in UTC so names sort in creation order
/// across DST changes: `20261112T153000.123456789Z`.
const BACKUP_STAMP: &str = "%Y%m%dT%H%M%S%.9fZ";

/// Whether `stamp` is exactly what BACKUP_STAMP produces.
fn is_backup_stamp(stamp: &str) -> bool {
    stamp.len() == 26
        && stamp.char_indices().all(|(i, c)| match i {
            8 => c == 'T',
            15 => c == '.',
            25 => c == 'Z',
            _ => c.is_ascii_digit(),
        })
}

/// Copy `file` to `<file>.<UTC timestamp>.bak` before an edit, keeping the
/// newest few, so a run of edits can't overwrite the only copy of the
/// original. Other `.bak` files are never touched. Does nothing when `file`
/// doesn't exist.
pub fn backup_file(file: &Path) -> std::io::Result<()> {
    if !file.exists() {
        return Ok(());
    }
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else { return Ok(()) };
    let name = name.to_string_lossy();
    let mut time = chrono::Utc::now();
    let backup = loop {
        let candidate = dir.join(format!("{name}.{}.bak", time.format(BACKUP_STAMP)));
        if !candidate.exists() {
            break candidate;
        }
        time += chrono::Duration::nanoseconds(1);
    };
    fs::copy(file, &backup)?;
    // Best effort: Windows can't flush a file opened read-only.
    if let Ok(copy) = fs::File::open(&backup) {
        let _ = copy.sync_all();
    }
    sync_parent(&backup);

    // Keep the new backup whatever its name says (the clock may have gone back),
    // plus the newest others.
    let prefix = format!("{name}.");
    let new_name = backup.file_name().map(|n| n.to_string_lossy().into_owned());
    let mut others: Vec<String> = read_dir_names(dir)?
        .into_iter()
        .filter(|n| Some(n) != new_name.as_ref())
        .filter(|n| n.strip_prefix(&prefix).and_then(|rest| rest.strip_suffix(".bak")).is_some_and(is_backup_stamp))
        .collect();
    others.sort();
    for old in &others[..others.len().saturating_sub(BACKUPS_KEPT - 1)] {
        // Pruning only tidies up; the new backup exists, so the edit may go ahead.
        let _ = fs::remove_file(dir.join(old));
    }
    Ok(())
}

/// The user's home directory.
pub fn home_dir() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default()
}

/// Parse results cached per file and invalidated when its mtime changes.
pub struct FileCache<T> {
    entries: Mutex<HashMap<PathBuf, (SystemTime, T)>>,
}

impl<T: Clone> FileCache<T> {
    pub fn new() -> Self {
        Self { entries: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, path: &Path, parse: impl FnOnce(&str) -> T) -> std::io::Result<T> {
        let mtime = fs::metadata(path)?.modified()?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, data)) = entries.get(path)
            && *cached == mtime
        {
            return Ok(data.clone());
        }
        let content = fs::read_to_string(path)?;
        let data = parse(&content);
        entries.insert(path.to_path_buf(), (mtime, data.clone()));
        Ok(data)
    }
}

impl<T: Clone> Default for FileCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_the_js_helpers() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1024.0), "1.00 KB");
        assert_eq!(format_bytes(1_610_612_736.0), "1.50 GB");
        assert_eq!(format_playtime(45.0), "45 minutes");
        assert_eq!(format_playtime(90.0), "1.5 hours");
        assert_eq!(format_timestamp(0), "N/A");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14T22:13:20.000Z");
        assert_eq!(num(3.0), Value::from(3));
        assert_eq!(num(f64::NAN), Value::Null);
        assert_eq!(locale_cmp("ŌKAMI HD", "Portal"), Ordering::Less);
        assert_eq!(locale_cmp("Éclair", "Echo"), Ordering::Greater);
        assert_eq!(locale_cmp("Elan", "Élan"), Ordering::Less);
        assert_eq!(locale_cmp("apple", "Apple"), Ordering::Less);
        assert_eq!(locale_cmp("Zelda", "apple"), Ordering::Greater);
        assert_eq!(locale_cmp("Æon", "Age"), Ordering::Less);
        assert_eq!(to_fixed(12.25, 1), "12.2");
        assert_eq!(to_fixed(0.125, 2), "0.12");
        assert_eq!(to_fixed(-0.25, 1), "-0.2");
        assert_eq!(to_fixed(9.95, 1), "9.9");
        assert_eq!(to_fixed(99.5, 0), "100");
        assert_eq!(to_fixed(-9.5, 0), "-10");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(f64::INFINITY, 2), "Infinity");
        assert_eq!(format_playtime(f64::NAN), "NaN hours");
        assert_eq!(format_bytes(f64::NAN), "NaN B");
    }

    #[test]
    fn write_atomic_replaces_content_and_keeps_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        fs::write(&file, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            write_atomic(&file, b"new").unwrap();
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        write_atomic(&file, b"newer").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "newer");
        assert_eq!(read_dir_names(dir.path()).unwrap(), ["f.txt"]);
    }

    #[test]
    fn backup_file_keeps_the_newest_few() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.vdf");
        backup_file(&file).unwrap();
        assert!(read_dir_names(dir.path()).unwrap().is_empty());

        let foreign = ["f.vdf.bak", "f.vdf.2020-before-migration.bak", "f.vdf.20200101T000000.000000000Z.bak.old"];
        for name in foreign {
            fs::write(dir.path().join(name), "made elsewhere").unwrap();
        }
        // An old generated backup, so pruning has to rank it by name, not by when it was made.
        fs::write(dir.path().join("f.vdf.20000101T000000.000000000Z.bak"), "ancient").unwrap();
        for i in 0..BACKUPS_KEPT + 2 {
            fs::write(&file, i.to_string()).unwrap();
            backup_file(&file).unwrap();
        }
        let names = read_dir_names(dir.path()).unwrap();
        let backups: Vec<&String> = names
            .iter()
            .filter(|n| n.strip_prefix("f.vdf.").and_then(|r| r.strip_suffix(".bak")).is_some_and(is_backup_stamp))
            .collect();
        assert_eq!(backups.len(), BACKUPS_KEPT);
        assert_eq!(fs::read_to_string(dir.path().join(backups[0])).unwrap(), "2");
        assert_eq!(fs::read_to_string(dir.path().join(backups[4])).unwrap(), (BACKUPS_KEPT + 1).to_string());
        for name in foreign {
            assert!(names.contains(&name.to_string()), "{name}");
        }
    }

    #[test]
    fn backup_file_keeps_the_new_backup_when_the_clock_went_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.vdf");
        for i in 0..BACKUPS_KEPT {
            fs::write(dir.path().join(format!("f.vdf.2999010{i}T000000.000000000Z.bak")), "future").unwrap();
        }
        fs::write(&file, "current").unwrap();
        backup_file(&file).unwrap();
        let names = read_dir_names(dir.path()).unwrap();
        let backups: Vec<&String> = names.iter().filter(|n| n.ends_with(".bak")).collect();
        assert_eq!(backups.len(), BACKUPS_KEPT);
        assert!(backups.iter().any(|n| fs::read_to_string(dir.path().join(n)).unwrap() == "current"));
    }

    #[test]
    #[cfg(unix)]
    fn write_atomic_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.conf");
        let link = dir.path().join("link.conf");
        fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }
}
