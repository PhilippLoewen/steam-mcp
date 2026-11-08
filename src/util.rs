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

/// JS `Number.prototype.toFixed`: like `format!("{:.N}")`, except that exact
/// ties round away from zero (JS) instead of to even (Rust).
pub fn to_fixed(n: f64, digits: usize) -> String {
    if !n.is_finite() {
        return js_num(n);
    }
    let exact = format!("{n:.prec$}", prec = digits + 60);
    let (head, tail) = exact.split_at(exact.find('.').map_or(exact.len(), |i| i + 1 + digits));
    let is_tie = tail.starts_with('5') && tail[1..].bytes().all(|b| b == b'0');
    if !is_tie {
        return format!("{n:.digits$}");
    }
    let mut out: Vec<u8> = head.trim_end_matches('.').bytes().collect();
    let mut i = out.len();
    loop {
        if i == 0 || out[i - 1] == b'-' {
            out.insert(i, b'1');
            break;
        }
        i -= 1;
        match out[i] {
            b'.' => continue,
            b'9' => out[i] = b'0',
            d => {
                out[i] = d + 1;
                break;
            }
        }
    }
    String::from_utf8(out).expect("ASCII digits")
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
        assert_eq!(to_fixed(12.25, 1), "12.3");
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(-0.25, 1), "-0.3");
        assert_eq!(to_fixed(9.95, 1), "9.9");
        assert_eq!(to_fixed(99.5, 0), "100");
        assert_eq!(to_fixed(-9.5, 0), "-10");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(f64::INFINITY, 2), "Infinity");
        assert_eq!(format_playtime(f64::NAN), "NaN hours");
        assert_eq!(format_bytes(f64::NAN), "NaN B");
    }
}
