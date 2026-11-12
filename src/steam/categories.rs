//! Library categories ("user collections") as the Steam client stores them in
//! userdata/<accountid>/config/cloudstorage/cloud-storage-namespace-1.json.
//!
//! That file is a cloud-synced cache owned by the Steam client. Edits are only
//! written while Steam is closed: the changed key is listed in
//! cloud-storage-namespace-1.modified.json so Steam uploads it on its next
//! start. While Steam runs, edits go through the Millennium bridge (millennium.rs).

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rand::Rng;
use serde_json::{Map, Value, json};

use super::paths::{get_steam_dir, get_user_config, steam_may_be_running};
use crate::util::write_atomic;

pub const KEY_PREFIX: &str = "user-collections.";

/// An edit refused for a reason the caller should show as-is.
#[derive(Debug, Clone)]
pub struct CategoryError {
    pub message: String,
    /// The bridge got the request but never answered (it may still have applied it).
    pub bridge_timeout: bool,
}

impl CategoryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), bridge_timeout: false }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self { message: message.into(), bridge_timeout: true }
    }
}

impl fmt::Display for CategoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CategoryError {}

fn refuse<T>(message: impl Into<String>) -> Result<T> {
    Err(CategoryError::new(message).into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryKind {
    Explicit,
    Filter,
    Tag,
}

impl CategoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CategoryKind::Explicit => "explicit",
            CategoryKind::Filter => "filter",
            CategoryKind::Tag => "tag",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Category {
    pub name: String,
    pub key: String,
    pub kind: CategoryKind,
    pub version: Option<String>,
    pub timestamp: Option<Value>,
    /// Explicit membership: added minus removed, deduplicated.
    pub appids: Vec<i64>,
    pub added: usize,
    pub removed: usize,
    pub has_filter: bool,
    pub filter_spec: Option<Value>,
    pub is_deleted: bool,
}

/// `id` is the key without the user-collections. prefix.
pub fn category_kind(id: &str, filter_only: bool) -> CategoryKind {
    if id.starts_with("from-tag-") {
        CategoryKind::Tag
    } else if filter_only {
        CategoryKind::Filter
    } else {
        CategoryKind::Explicit
    }
}

fn as_appid(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
}

fn to_appids(value: Option<&Value>) -> Vec<i64> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(as_appid).collect(),
        _ => Vec::new(),
    }
}

/// A JS-truthy object (arrays count, null doesn't).
fn is_object_like(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_) | Value::Array(_)))
}

/// Parse namespace file text into categories. Tombstones are skipped unless
/// `include_deleted`; malformed entries are ignored and a malformed file yields nothing.
pub fn parse_categories(raw: &str, include_deleted: bool) -> Vec<Category> {
    let Ok(Value::Array(top)) = serde_json::from_str::<Value>(raw) else { return Vec::new() };
    let mut result = Vec::new();
    for pair in &top {
        let Some(pair) = pair.as_array() else { continue };
        let Some(key) = pair.first().and_then(Value::as_str) else { continue };
        let Some(suffix) = key.strip_prefix(KEY_PREFIX) else { continue };
        let Some(entry) = pair.get(1).and_then(Value::as_object) else { continue };

        let value: Map<String, Value> = match entry.get("value") {
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(Value::Object(o)) => o,
                Ok(_) => Map::new(),
                Err(_) => continue,
            },
            _ => Map::new(),
        };

        let is_deleted =
            entry.get("is_deleted") == Some(&Value::Bool(true)) || value.get("is_deleted") == Some(&Value::Bool(true));
        if is_deleted && !include_deleted {
            continue;
        }

        let added = to_appids(value.get("added"));
        let removed: Vec<i64> = to_appids(value.get("removed"));
        let removed_set: HashSet<i64> = removed.iter().copied().collect();
        let mut seen = HashSet::new();
        let appids: Vec<i64> =
            added.iter().copied().filter(|id| seen.insert(*id)).filter(|id| !removed_set.contains(id)).collect();

        let filter_spec = is_object_like(value.get("filterSpec")).then(|| value["filterSpec"].clone());

        result.push(Category {
            name: value.get("name").and_then(Value::as_str).unwrap_or(suffix).to_string(),
            key: key.to_string(),
            kind: category_kind(suffix, filter_spec.is_some() && added.is_empty()),
            version: entry.get("version").and_then(Value::as_str).map(str::to_string),
            timestamp: entry.get("timestamp").filter(|t| t.is_number()).cloned(),
            appids,
            added: added.len(),
            removed: removed.len(),
            has_filter: filter_spec.is_some(),
            filter_spec,
            is_deleted,
        });
    }
    result
}

/// A category by name (case-insensitive), else by exact key (case-insensitive).
pub fn find_category<'a>(categories: &'a [Category], query: &str) -> Option<&'a Category> {
    let q = query.trim().to_lowercase();
    categories
        .iter()
        .find(|c| c.name.to_lowercase() == q)
        .or_else(|| categories.iter().find(|c| c.key.to_lowercase() == q))
}

const NAMESPACE_FILE: &str = "config/cloudstorage/cloud-storage-namespace-1.json";

/// The active account's namespace file, whether it exists or not.
fn active_categories_file() -> Result<PathBuf> {
    Ok(get_steam_dir()?.join("userdata").join(get_user_config()?.steam_id32.to_string()).join(NAMESPACE_FILE))
}

/// The active account's namespace file, else the first account that has one.
/// For reading only: edits go to the active account's file or nowhere.
pub fn find_categories_file() -> Option<PathBuf> {
    let userdata = get_steam_dir().ok()?.join("userdata");
    let rel = Path::new(NAMESPACE_FILE);
    if let Ok(active) = active_categories_file()
        && active.exists()
    {
        return Some(active);
    }
    let accounts = crate::util::read_dir_names(&userdata).ok()?;
    accounts
        .into_iter()
        .filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .map(|d| userdata.join(d).join(rel))
        .find(|c| c.exists())
}

/// The active account's live categories; None when it has no namespace file.
/// For checking what an edit did, which went to that account.
pub fn read_active_categories() -> Result<Option<Vec<Category>>> {
    let file = active_categories_file()?;
    if !file.exists() {
        return Ok(None);
    }
    Ok(Some(parse_categories(&crate::util::read_text(&file)?, false)))
}

/// Categories from disk; None when there is no namespace file.
pub fn read_categories(include_deleted: bool) -> Result<Option<Vec<Category>>> {
    let Some(file) = find_categories_file() else { return Ok(None) };
    Ok(Some(parse_categories(&crate::util::read_text(&file)?, include_deleted)))
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CategoryEdit {
    /// New file text; identical to the input when nothing changed.
    pub raw: String,
    pub key: String,
    pub changed: bool,
    /// Membership edits only: appids actually added or removed.
    pub affected: Option<Vec<i64>>,
    /// Membership edits only: appids already members (add) or not members (remove).
    pub skipped: Option<Vec<i64>>,
}

impl CategoryEdit {
    fn plain(raw: String, key: String, changed: bool) -> Self {
        Self { raw, key, changed, affected: None, skipped: None }
    }
}

/// Favorites and Hidden have fixed ids and can't be renamed or deleted.
fn special(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "favorite" | "favorites" => Some(("favorite", "Favorites")),
        "hidden" => Some(("hidden", "Hidden")),
        _ => None,
    }
}

fn is_special_key(key: &str) -> bool {
    key == format!("{KEY_PREFIX}favorite") || key == format!("{KEY_PREFIX}hidden")
}

const ID_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

pub fn random_collection_id() -> String {
    let mut rng = rand::rng();
    (0..12).map(|_| ID_ALPHABET[rng.random_range(0..ID_ALPHABET.len())] as char).collect()
}

/// Strict counterpart of parse_categories: a file we can't parse, or one with a
/// key listed twice (which entry Steam uses is unclear), must not be rewritten.
fn load_pairs(raw: &str) -> Result<Vec<Value>> {
    let pairs = match serde_json::from_str::<Value>(raw) {
        Err(_) => return refuse("cloud-storage-namespace-1.json is not valid JSON; nothing was changed"),
        Ok(Value::Array(pairs)) => pairs,
        Ok(_) => return refuse("cloud-storage-namespace-1.json is not a JSON array; nothing was changed"),
    };
    let mut seen = HashSet::new();
    for key in pairs.iter().filter_map(|p| p.as_array().and_then(|a| a.first()).and_then(Value::as_str)) {
        if !seen.insert(key) {
            return refuse(format!("cloud-storage-namespace-1.json lists {key} twice; nothing was changed"));
        }
    }
    Ok(pairs)
}

/// The object entry of `key`, which the category resolved from this file had.
fn entry_mut<'a>(pairs: &'a mut [Value], key: &str) -> Result<(usize, &'a mut Map<String, Value>)> {
    let malformed = || refuse(format!("Entry {key} is missing or malformed; nothing was changed"));
    let Some(index) = pair_index(pairs, key) else { return malformed() };
    match pairs[index].get_mut(1).and_then(Value::as_object_mut) {
        Some(meta) => Ok((index, meta)),
        None => malformed(),
    }
}

fn pair_index(pairs: &[Value], key: &str) -> Option<usize> {
    pairs.iter().position(|p| p.as_array().and_then(|a| a.first()).and_then(Value::as_str) == Some(key))
}

fn new_entry(key: &str, value: &Value, now: i64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".into(), key.into());
    m.insert("timestamp".into(), now.into());
    m.insert("value".into(), value.to_string().into());
    m.insert("conflictResolutionMethod".into(), "custom".into());
    m.insert("strMethodId".into(), "union-collections".into());
    m
}

fn stringify(pairs: &[Value]) -> String {
    Value::Array(pairs.to_vec()).to_string()
}

fn names_of(categories: &[Category]) -> String {
    let names: Vec<&str> = categories.iter().map(|c| c.name.as_str()).collect();
    if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
}

fn resolve_category(raw: &str, query: &str) -> Result<Category> {
    let categories = parse_categories(raw, false);
    match find_category(&categories, query) {
        Some(c) => Ok(c.clone()),
        None => refuse(format!("Category \"{query}\" not found. Available: {}", names_of(&categories))),
    }
}

fn assert_name_free(raw: &str, name: &str, except_key: Option<&str>) -> Result<()> {
    let lower = name.to_lowercase();
    if special(&lower).is_some() {
        return refuse(format!("\"{name}\" is reserved for the built-in collection"));
    }
    if let Some(clash) = parse_categories(raw, false)
        .into_iter()
        .find(|c| Some(c.key.as_str()) != except_key && c.name.to_lowercase() == lower)
    {
        return refuse(format!("A category named \"{}\" already exists", clash.name));
    }
    Ok(())
}

fn assert_not_special(category: &Category, verb: &str) -> Result<()> {
    if is_special_key(&category.key) {
        return refuse(format!("\"{}\" is a built-in collection and can't be {verb}", category.name));
    }
    Ok(())
}

fn clean_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return refuse("Category name must not be empty");
    }
    Ok(trimmed.to_string())
}

/// Apply `mutate` to the parsed value of `key`'s entry and bump its
/// timestamp. Every other field and entry is kept as-is.
fn update_value(raw: &str, key: &str, now: i64, mutate: impl FnOnce(&mut Map<String, Value>)) -> Result<CategoryEdit> {
    let mut pairs = load_pairs(raw)?;
    let (_, meta) = entry_mut(&mut pairs, key)?;
    let Some(Value::String(before)) = meta.get("value").cloned() else {
        return refuse(format!("Entry {key} has no value to edit"));
    };
    let mut value: Map<String, Value> = match serde_json::from_str::<Value>(&before)? {
        Value::Object(o) => o,
        _ => Map::new(),
    };
    mutate(&mut value);
    let after = Value::Object(value).to_string();
    if after == before {
        return Ok(CategoryEdit::plain(raw.to_string(), key.to_string(), false));
    }
    meta.insert("value".into(), after.into());
    meta.insert("timestamp".into(), now.into());
    Ok(CategoryEdit::plain(stringify(&pairs), key.to_string(), true))
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Add,
    Remove,
}

fn dedupe(ids: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::new();
    ids.iter().copied().filter(|id| seen.insert(*id)).collect()
}

fn edit_membership(raw: &str, query: &str, appids: &[i64], now: i64, mode: Mode) -> Result<CategoryEdit> {
    let category = resolve_category(raw, query)?;
    if category.has_filter {
        let verb = if mode == Mode::Add { "added" } else { "removed" };
        return refuse(format!(
            "\"{}\" is a dynamic collection; its games come from filters and can't be {verb} by hand",
            category.name
        ));
    }

    let members: HashSet<i64> = category.appids.iter().copied().collect();
    let (mut affected, mut skipped) = (Vec::new(), Vec::new());
    for id in dedupe(appids) {
        if members.contains(&id) == (mode == Mode::Remove) { &mut affected } else { &mut skipped }.push(id);
    }

    let targets: HashSet<i64> = appids.iter().copied().collect();
    let is_target = |v: &Value| as_appid(v).is_some_and(|id| targets.contains(&id));
    let to_add = affected.clone();
    let mut edit = update_value(raw, &category.key, now, |value| {
        let mut added: Vec<Value> = match value.get("added") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        match mode {
            Mode::Add => {
                for id in to_add {
                    if !added.iter().any(|v| as_appid(v) == Some(id) && v.is_number()) {
                        added.push(id.into());
                    }
                }
            }
            Mode::Remove => added.retain(|v| !is_target(v)),
        }
        value.insert("added".into(), Value::Array(added));
        if let Some(Value::Array(removed)) = value.get_mut("removed") {
            removed.retain(|v| !is_target(v));
        }
    })?;
    edit.affected = Some(affected);
    edit.skipped = Some(skipped);
    Ok(edit)
}

/// Add appids to a category. Favorites and Hidden are created when missing,
/// as Steam only stores them once they have members.
pub fn add_apps(raw: &str, query: &str, appids: &[i64], now: i64) -> Result<CategoryEdit> {
    let q = query.trim().to_lowercase();
    let special = special(q.strip_prefix(KEY_PREFIX).unwrap_or(&q));
    if let Some((id, name)) = special
        && find_category(&parse_categories(raw, false), query).is_none()
    {
        let mut pairs = load_pairs(raw)?;
        let key = format!("{KEY_PREFIX}{id}");
        let added = dedupe(appids);
        let mut entry = new_entry(&key, &json!({ "id": id, "name": name, "added": added, "removed": [] }), now);
        match pair_index(&pairs, &key) {
            None => pairs.push(json!([key, entry])),
            Some(index) => {
                // Revive the tombstone, keeping its server version.
                if let Some(version) = pairs[index].get(1).and_then(|m| m.get("version")).cloned() {
                    entry.insert("version".into(), version);
                }
                pairs[index] = json!([key, entry]);
            }
        }
        return Ok(CategoryEdit {
            raw: stringify(&pairs),
            key,
            changed: true,
            affected: Some(added),
            skipped: Some(Vec::new()),
        });
    }
    edit_membership(raw, query, appids, now, Mode::Add)
}

/// Remove appids from a category the way Steam does: drop them from `added`.
pub fn remove_apps(raw: &str, query: &str, appids: &[i64], now: i64) -> Result<CategoryEdit> {
    edit_membership(raw, query, appids, now, Mode::Remove)
}

/// Append a new, empty custom collection with id `uc-<id>`.
pub fn create_category(raw: &str, name: &str, now: i64, id: &str) -> Result<CategoryEdit> {
    let cleaned = clean_name(name)?;
    assert_name_free(raw, &cleaned, None)?;
    let mut pairs = load_pairs(raw)?;
    let collection_id = format!("uc-{id}");
    let key = format!("{KEY_PREFIX}{collection_id}");
    if pair_index(&pairs, &key).is_some() {
        return refuse(format!("Key {key} is already taken"));
    }
    let entry = new_entry(&key, &json!({ "id": collection_id, "name": cleaned, "added": [], "removed": [] }), now);
    pairs.push(json!([key, entry]));
    Ok(CategoryEdit::plain(stringify(&pairs), key, true))
}

pub fn rename_category(raw: &str, query: &str, new_name: &str, now: i64) -> Result<CategoryEdit> {
    let cleaned = clean_name(new_name)?;
    let category = resolve_category(raw, query)?;
    assert_not_special(&category, "renamed")?;
    assert_name_free(raw, &cleaned, Some(&category.key))?;
    update_value(raw, &category.key, now, |value| {
        value.insert("name".into(), cleaned.into());
    })
}

/// Replace the entry with a tombstone, the shape Steam itself writes.
pub fn delete_category(raw: &str, query: &str, now: i64) -> Result<CategoryEdit> {
    let category = resolve_category(raw, query)?;
    assert_not_special(&category, "deleted")?;
    let mut pairs = load_pairs(raw)?;
    let (index, meta) = entry_mut(&mut pairs, &category.key)?;
    let version = meta.get("version").cloned();
    let mut tombstone = Map::new();
    tombstone.insert("key".into(), category.key.clone().into());
    tombstone.insert("timestamp".into(), now.into());
    tombstone.insert("is_deleted".into(), true.into());
    if let Some(v) = version {
        tombstone.insert("version".into(), v);
    }
    pairs[index] = json!([category.key, tombstone]);
    Ok(CategoryEdit::plain(stringify(&pairs), category.key, true))
}

/// Write new namespace text: back the old file up to .bak, replace it
/// atomically and list `key` in the .modified.json sibling so Steam uploads it.
pub fn write_categories_file(file: &Path, raw: &str, key: &str) -> Result<()> {
    let file_str = file.to_string_lossy();
    let modified_path = PathBuf::from(match file_str.strip_suffix(".json") {
        Some(stem) => format!("{stem}.modified.json"),
        None => file_str.into_owned(),
    });
    let mut keys: Vec<Value> = Vec::new();
    if modified_path.exists() {
        match fs::read_to_string(&modified_path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(Value::Array(k)) => keys = k,
            _ => {
                let name = modified_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                return refuse(format!("{name} is not a JSON array; nothing was written"));
            }
        }
    }

    // Mark first: a marked but unchanged key is harmless, an unmarked edit may
    // be replaced by the cloud copy.
    if !keys.iter().any(|k| k.as_str() == Some(key)) {
        keys.push(key.into());
        write_atomic(&modified_path, Value::Array(keys).to_string().as_bytes())?;
    }
    let mut bak = file.as_os_str().to_owned();
    bak.push(".bak");
    fs::copy(file, PathBuf::from(bak))?;
    Ok(write_atomic(file, raw.as_bytes())?)
}

/// The refusal for file edits under a running Steam. `bridge_status` says why
/// the Millennium bridge couldn't take the edit instead, when known.
pub fn steam_running_error(bridge_status: Option<&str>) -> CategoryError {
    let mut lines = vec![
        "WARNING: Steam is currently running (or that couldn't be checked)!".to_string(),
        "Steam keeps categories in memory and overwrites cloud-storage-namespace-1.json when it exits.".to_string(),
        "Please close Steam first, then try again, or install the steam-mcp-bridge Millennium plugin".to_string(),
        "(millennium-plugin/ in the steam-mcp repo) to edit categories while Steam runs.".to_string(),
    ];
    if let Some(status) = bridge_status {
        lines.push(format!("Millennium bridge: {status}."));
    }
    lines.push(String::new());
    lines.push("Categories were NOT modified.".to_string());
    CategoryError::new(lines.join("\n"))
}

/// Run an edit against the active account's namespace file and write the
/// result. Never falls back to another account's file. Refuses while Steam
/// runs (or that can't be told): it would overwrite the file from memory on exit.
pub fn edit_categories(edit: impl FnOnce(&str) -> Result<CategoryEdit>) -> Result<CategoryEdit> {
    let file = active_categories_file()?;
    if !file.exists() {
        return refuse(format!(
            "The active Steam account has no {} yet. Open the Steam library once so Steam creates it.",
            file.display()
        ));
    }
    if steam_may_be_running() {
        return Err(steam_running_error(None).into());
    }
    let _guard = crate::util::file_edit_lock();
    let result = edit(&crate::util::read_text(&file)?)?;
    if result.changed {
        write_categories_file(&file, &result.raw, &result.key)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn entry(key: &str, value: Option<Value>, extra: Value) -> Value {
        let mut m = Map::new();
        m.insert("key".into(), key.into());
        m.insert("timestamp".into(), 1_787_813_434.into());
        if let Some(v) = value {
            m.insert("value".into(), v.to_string().into());
        }
        m.insert("version".into(), "2810".into());
        if let Value::Object(extra) = extra {
            m.extend(extra);
        }
        json!([key, m])
    }

    fn library_fixture() -> String {
        let strategy = [
            255710, 4570, 4580, 8930, 9310, 9350, 9420, 9450, 15620, 20570, 21970, 40100, 56400, 105430, 105450,
            221380, 241930,
        ];
        Value::Array(vec![
            entry("GameReleased", Some(json!({ "apps": [] })), json!({})),
            entry("showcases.2", Some(json!({ "nShowcaseId": 2 })), json!({})),
            entry(
                "user-collections.favorite",
                Some(json!({ "id": "favorite", "name": "Favorites", "added": [440, 230410, 271590, 238370, 553850, 646570, 2379780], "removed": [] })),
                json!({}),
            ),
            entry(
                "user-collections.from-tag-Strategy",
                Some(json!({ "id": "from-tag-Strategy", "name": "Strategy", "added": strategy, "removed": [] })),
                json!({}),
            ),
            entry(
                "user-collections.hidden",
                Some(json!({ "id": "hidden", "name": "Hidden", "added": [1385510, 99830, 654310, 261570, 8980, 383080, 1151640, 1083500], "removed": [] })),
                json!({}),
            ),
            entry(
                "user-collections.from-tag-Nintendo DS",
                Some(json!({ "id": "from-tag-Nintendo DS", "name": "Nintendo DS", "added": [], "removed": [] })),
                json!({}),
            ),
            entry(
                "user-collections.uc-DQIQvQZKuBvY",
                Some(json!({
                    "id": "uc-DQIQvQZKuBvY", "name": "Garfield + Masterhuck", "added": [], "removed": [],
                    "filterSpec": {
                        "nFormatVersion": 2, "strSearchText": "",
                        "filterGroups": [
                            { "rgOptions": [8], "bAcceptUnion": true },
                            { "rgOptions": [33554147, 31376102], "bAcceptUnion": false }
                        ],
                        "setSuggestions": {}
                    }
                })),
                json!({}),
            ),
            entry(
                "user-collections.uc-gn4OC6hGDS0B",
                Some(json!({
                    "id": "uc-gn4OC6hGDS0B", "name": "Gold+", "added": [], "removed": [],
                    "filterSpec": {
                        "nFormatVersion": 2, "strSearchText": "",
                        "filterGroups": [{ "rgOptions": [], "bAcceptUnion": true }, { "rgOptions": [0, 1], "bAcceptUnion": true }]
                    }
                })),
                json!({}),
            ),
            entry("user-collections.from-tag-Humble Bundle 8", None, json!({ "is_deleted": true })),
            entry(
                "user-collections.uc-llVuLcjuUCgW",
                Some(json!({ "id": "uc-llVuLcjuUCgW", "name": "Gone", "added": [1], "removed": [] })),
                json!({ "is_deleted": true }),
            ),
        ])
        .to_string()
    }

    #[test]
    fn parse_returns_only_live_user_collections() {
        let names: Vec<String> = parse_categories(&library_fixture(), false).into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Favorites", "Strategy", "Hidden", "Nintendo DS", "Garfield + Masterhuck", "Gold+"]);
        assert_eq!(parse_categories(&library_fixture(), true).len(), 8);
    }

    #[test]
    fn parse_resolves_explicit_membership() {
        let categories = parse_categories(&library_fixture(), false);
        let favorites = find_category(&categories, "favorites").unwrap();
        assert_eq!(favorites.kind, CategoryKind::Explicit);
        assert_eq!(favorites.appids.len(), 7);
        assert!(favorites.appids.contains(&2379780));
        let strategy = find_category(&categories, "Strategy").unwrap();
        assert_eq!(strategy.kind, CategoryKind::Tag);
        assert_eq!(strategy.appids.len(), 17);
        assert_eq!(find_category(&categories, "Hidden").unwrap().appids.len(), 8);
    }

    #[test]
    fn parse_keeps_filter_spec() {
        let categories = parse_categories(&library_fixture(), false);
        let gold = find_category(&categories, "gold+").unwrap();
        assert_eq!(gold.kind, CategoryKind::Filter);
        assert!(gold.appids.is_empty());
        let groups = gold.filter_spec.as_ref().unwrap()["filterGroups"].as_array().unwrap();
        assert_eq!(groups.last().unwrap(), &json!({ "rgOptions": [0, 1], "bAcceptUnion": true }));
    }

    #[test]
    fn parse_subtracts_removed_and_survives_malformed_input() {
        let raw = Value::Array(vec![entry(
            "user-collections.uc-x",
            Some(json!({ "name": "X", "added": [1, 2, 3], "removed": [2] })),
            json!({}),
        )])
        .to_string();
        assert_eq!(parse_categories(&raw, false)[0].appids, [1, 3]);
        assert!(parse_categories("not json", false).is_empty());
        assert!(parse_categories("{}", false).is_empty());
        assert!(parse_categories(r#"[["user-collections.a",{"value":"{bad"}]]"#, false).is_empty());
    }

    fn fixture() -> String {
        Value::Array(vec![
            entry("showcases.2", Some(json!({ "nShowcaseId": 2 })), json!({})),
            entry(
                "user-collections.favorite",
                Some(json!({ "id": "favorite", "name": "Favorites", "added": [440, 730], "removed": [] })),
                json!({ "conflictResolutionMethod": "custom", "strMethodId": "union-collections" }),
            ),
            entry(
                "user-collections.uc-abc",
                Some(json!({ "id": "uc-abc", "name": "RPG", "added": [10], "removed": [20] })),
                json!({}),
            ),
            entry(
                "user-collections.uc-dyn",
                Some(json!({ "id": "uc-dyn", "name": "Dynamic", "added": [], "removed": [], "filterSpec": { "nFormatVersion": 2, "filterGroups": [] } })),
                json!({}),
            ),
            entry("user-collections.uc-gone", None, json!({ "is_deleted": true })),
        ])
        .to_string()
    }

    fn pairs(raw: &str) -> Vec<Value> {
        serde_json::from_str::<Value>(raw).unwrap().as_array().unwrap().clone()
    }

    fn pair_for(raw: &str, key: &str) -> Value {
        pairs(raw).into_iter().find(|p| p[0] == key).map(|p| p[1].clone()).unwrap()
    }

    fn value_for(raw: &str, key: &str) -> Value {
        serde_json::from_str(pair_for(raw, key)["value"].as_str().unwrap()).unwrap()
    }

    fn err(r: Result<CategoryEdit>) -> String {
        let e = r.unwrap_err();
        assert!(e.downcast_ref::<CategoryError>().is_some(), "not a CategoryError: {e}");
        e.to_string()
    }

    #[test]
    fn add_appends_reports_and_bumps_timestamp() {
        let edit = add_apps(&fixture(), "favorites", &[730, 570, 570], NOW).unwrap();
        assert_eq!(edit.key, "user-collections.favorite");
        assert!(edit.changed);
        assert_eq!(edit.affected.as_deref(), Some(&[570][..]));
        assert_eq!(edit.skipped.as_deref(), Some(&[730][..]));
        let meta = pair_for(&edit.raw, &edit.key);
        assert_eq!(meta["timestamp"], NOW);
        assert_eq!(meta["version"], "2810");
        assert_eq!(meta["strMethodId"], "union-collections");
        assert_eq!(
            value_for(&edit.raw, &edit.key),
            json!({ "id": "favorite", "name": "Favorites", "added": [440, 730, 570], "removed": [] })
        );
    }

    #[test]
    fn add_clears_readded_from_removed() {
        let edit = add_apps(&fixture(), "RPG", &[20], NOW).unwrap();
        let value = value_for(&edit.raw, &edit.key);
        assert_eq!(value["added"], json!([10, 20]));
        assert_eq!(value["removed"], json!([]));
        assert_eq!(find_category(&parse_categories(&edit.raw, false), "RPG").unwrap().appids, [10, 20]);
    }

    #[test]
    fn add_leaves_file_untouched_without_changes() {
        let edit = add_apps(&fixture(), "Favorites", &[440], NOW).unwrap();
        assert!(!edit.changed);
        assert_eq!(edit.raw, fixture());
    }

    #[test]
    fn add_keeps_other_entries_and_order() {
        let edit = add_apps(&fixture(), "RPG", &[1], NOW).unwrap();
        let (before, after) = (pairs(&fixture()), pairs(&edit.raw));
        let keys = |p: &[Value]| p.iter().map(|x| x[0].clone()).collect::<Vec<_>>();
        assert_eq!(keys(&after), keys(&before));
        assert_eq!(after[0], before[0]);
        assert_eq!(after[4], before[4]);
    }

    #[test]
    fn add_creates_hidden_on_demand() {
        let edit = add_apps(&fixture(), "Hidden", &[99], NOW).unwrap();
        assert_eq!(edit.key, "user-collections.hidden");
        let expected_value = json!({ "id": "hidden", "name": "Hidden", "added": [99], "removed": [] }).to_string();
        assert_eq!(
            pair_for(&edit.raw, &edit.key).to_string(),
            json!({
                "key": "user-collections.hidden",
                "timestamp": NOW,
                "value": expected_value,
                "conflictResolutionMethod": "custom",
                "strMethodId": "union-collections",
            })
            .to_string()
        );
    }

    #[test]
    fn add_refuses_dynamic_and_unknown() {
        assert!(err(add_apps(&fixture(), "Dynamic", &[1], NOW)).contains("dynamic"));
        assert!(err(add_apps(&fixture(), "Nope", &[1], NOW)).contains("not found"));
        let mix = Value::Array(vec![entry(
            "user-collections.uc-mix",
            Some(json!({ "name": "Mix", "added": [1], "removed": [], "filterSpec": {} })),
            json!({}),
        )])
        .to_string();
        assert!(err(add_apps(&mix, "Mix", &[2], NOW)).contains("dynamic"));
    }

    #[test]
    fn add_revives_favorites_tombstone_in_place() {
        let raw = Value::Array(vec![
            entry("showcases.2", Some(json!({ "nShowcaseId": 2 })), json!({})),
            entry("user-collections.favorite", None, json!({ "is_deleted": true })),
        ])
        .to_string();
        let edit = add_apps(&raw, "Favorites", &[440], NOW).unwrap();
        let keys: Vec<Value> = pairs(&edit.raw).iter().map(|p| p[0].clone()).collect();
        assert_eq!(keys, [json!("showcases.2"), json!("user-collections.favorite")]);
        let meta = pair_for(&edit.raw, &edit.key);
        assert!(meta.get("is_deleted").is_none());
        assert_eq!(meta["version"], "2810");
        assert_eq!(value_for(&edit.raw, &edit.key)["added"], json!([440]));
    }

    #[test]
    fn remove_drops_and_reports() {
        let edit = remove_apps(&fixture(), "Favorites", &[730, 999], NOW).unwrap();
        assert_eq!(edit.affected.as_deref(), Some(&[730][..]));
        assert_eq!(edit.skipped.as_deref(), Some(&[999][..]));
        assert_eq!(value_for(&edit.raw, &edit.key)["added"], json!([440]));
        assert_eq!(pair_for(&edit.raw, &edit.key)["timestamp"], NOW);

        let edit = remove_apps(&fixture(), "RPG", &[10, 20], NOW).unwrap();
        assert_eq!(edit.affected.as_deref(), Some(&[10][..]));
        let value = value_for(&edit.raw, &edit.key);
        assert_eq!((&value["added"], &value["removed"]), (&json!([]), &json!([])));

        err(remove_apps(&fixture(), "Dynamic", &[1], NOW));
    }

    #[test]
    fn create_appends_uc_collection() {
        let edit = create_category(&fixture(), "  Co-op  ", NOW, "AbCdEfGh1234").unwrap();
        assert_eq!(edit.key, "user-collections.uc-AbCdEfGh1234");
        let value = json!({ "id": "uc-AbCdEfGh1234", "name": "Co-op", "added": [], "removed": [] }).to_string();
        assert_eq!(
            pairs(&edit.raw).last().unwrap().to_string(),
            json!([edit.key, {
                "key": edit.key, "timestamp": NOW, "value": value,
                "conflictResolutionMethod": "custom", "strMethodId": "union-collections",
            }])
            .to_string()
        );
        let random = create_category(&fixture(), "New", NOW, &random_collection_id()).unwrap();
        let re = regex::Regex::new(r"^user-collections\.uc-[A-Za-z0-9]{12}$").unwrap();
        assert!(re.is_match(&random.key));
        assert!(err(create_category(&fixture(), "rpg", NOW, "x")).contains("already exists"));
        assert!(err(create_category(&fixture(), "hidden", NOW, "x")).contains("reserved"));
        err(create_category(&fixture(), "   ", NOW, "x"));
    }

    #[test]
    fn a_key_listed_twice_is_refused_not_a_panic() {
        let value = json!({ "id": "uc-x", "name": "X", "added": [] }).to_string();
        let raw = json!([
            ["user-collections.uc-x", null],
            ["user-collections.uc-x", { "key": "user-collections.uc-x", "value": value }]
        ])
        .to_string();
        for result in [
            rename_category(&raw, "X", "Y", NOW),
            delete_category(&raw, "X", NOW),
            create_category(&raw, "Z", NOW, "abc"),
        ] {
            let err = result.unwrap_err();
            assert!(err.to_string().contains("twice"), "{err}");
        }
    }

    #[test]
    fn rename_changes_only_the_name() {
        let edit = rename_category(&fixture(), "RPG", "Role-playing", NOW).unwrap();
        assert_eq!(
            value_for(&edit.raw, &edit.key),
            json!({ "id": "uc-abc", "name": "Role-playing", "added": [10], "removed": [20] })
        );
        assert!(err(rename_category(&fixture(), "RPG", "dynamic", NOW)).contains("already exists"));
        assert!(err(rename_category(&fixture(), "RPG", "favorites", NOW)).contains("reserved"));
        err(rename_category(&fixture(), "Favorites", "Faves", NOW));
    }

    #[test]
    fn delete_writes_tombstone() {
        let edit = delete_category(&fixture(), "RPG", NOW).unwrap();
        assert_eq!(
            pair_for(&edit.raw, &edit.key).to_string(),
            json!({ "key": "user-collections.uc-abc", "timestamp": NOW, "is_deleted": true, "version": "2810" })
                .to_string()
        );
        assert!(find_category(&parse_categories(&edit.raw, false), "RPG").is_none());
        err(delete_category(&fixture(), "Favorites", NOW));
    }

    struct Dir {
        _tmp: tempfile::TempDir,
        file: PathBuf,
        modified: PathBuf,
    }

    fn dir() -> Dir {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("cloud-storage-namespace-1.json");
        let modified = tmp.path().join("cloud-storage-namespace-1.modified.json");
        fs::write(&file, fixture()).unwrap();
        Dir { _tmp: tmp, file, modified }
    }

    #[test]
    fn write_backs_up_writes_and_marks() {
        let d = dir();
        fs::write(&d.modified, r#"["showcases.2"]"#).unwrap();
        write_categories_file(&d.file, "[]", "user-collections.favorite").unwrap();
        assert_eq!(fs::read_to_string(&d.file).unwrap(), "[]");
        assert_eq!(fs::read_to_string(d.file.with_extension("json.bak")).unwrap(), fixture());
        assert_eq!(fs::read_to_string(&d.modified).unwrap(), r#"["showcases.2","user-collections.favorite"]"#);
        let mut names = crate::util::read_dir_names(d.file.parent().unwrap()).unwrap();
        names.sort();
        assert_eq!(
            names,
            [
                "cloud-storage-namespace-1.json",
                "cloud-storage-namespace-1.json.bak",
                "cloud-storage-namespace-1.modified.json"
            ]
        );
    }

    #[test]
    fn write_creates_modified_without_duplicates() {
        let d = dir();
        write_categories_file(&d.file, "[]", "user-collections.favorite").unwrap();
        write_categories_file(&d.file, "[]", "user-collections.favorite").unwrap();
        assert_eq!(fs::read_to_string(&d.modified).unwrap(), r#"["user-collections.favorite"]"#);
    }

    #[test]
    fn write_refuses_malformed_modified() {
        let d = dir();
        fs::write(&d.modified, "{oops").unwrap();
        let e = write_categories_file(&d.file, "[]", "k").unwrap_err();
        assert!(e.to_string().contains("modified.json"));
        assert_eq!(fs::read_to_string(&d.file).unwrap(), fixture());
        assert!(!d.file.with_extension("json.bak").exists());
    }
}
