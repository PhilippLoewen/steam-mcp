//! library: library folders, aggregate stats, and library categories.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Map, Value, json};

use crate::steam::api::{has_api_key, steam_api_request, store_api_request};
use crate::steam::categories::{
    Category, CategoryEdit, CategoryError, KEY_PREFIX, add_apps, create_category, delete_category, edit_categories,
    find_category, parse_categories, random_collection_id, read_categories, remove_apps, rename_category,
    steam_running_error,
};
use crate::steam::library::get_libraries;
use crate::steam::manifests::read_all_manifests;
use crate::steam::millennium::{
    BridgeOptions, BridgeOutcome, BridgeRequest, bridge_category, run_bridge_edit, verify_bridge_edit,
};
use crate::steam::paths::{get_user_config, is_steam_running};
use crate::steam::userdata::get_all_playtimes;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, Write, boxed};
use crate::util::{format_bytes, format_playtime, format_timestamp, locale_cmp, now_seconds};

const ACTIONS: &[&str] = &[
    "list",
    "stats",
    "categories",
    "category",
    "category_add",
    "category_remove",
    "category_create",
    "category_rename",
    "category_delete",
];

pub fn tool() -> Tool {
    Tool {
        name: "library",
        description: "List library folders, get aggregate stats, or read and edit library categories (Steam collections: Favorites, Hidden, tag/custom shelves). Edits need Steam closed, or the steam-mcp-bridge Millennium plugin while it runs.",
        params: vec![
            Param::new("action", ParamType::Enum(ACTIONS)).default("list"),
            Param::new("name", ParamType::String)
                .describe("Category name or key (for \"category\" and \"category_*\")"),
            Param::new("appids", ParamType::ArrayOfIntegers)
                .describe("Appids to add or remove (for \"category_add\"/\"category_remove\")"),
            Param::new("newName", ParamType::String).describe("New name (for \"category_rename\")"),
            Param::new("includeDeleted", ParamType::Boolean)
                .describe("Include deleted categories (for \"categories\")"),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[
            Write::Action("category_add"),
            Write::Action("category_remove"),
            Write::Action("category_create"),
            Write::Action("category_rename"),
            Write::Action("category_delete"),
        ],
        destructive: true,
        open_world: true,
    }
}

/// Everything a category edit touches outside its own logic, so tests can stand in for Steam.
pub trait CategoryEnv {
    fn steam_running(&self) -> bool;
    async fn bridge(&self, request: &BridgeRequest) -> Result<BridgeOutcome>;
    fn edit_file(&self, edit: &(dyn Fn(&str) -> Result<CategoryEdit> + Sync)) -> Result<CategoryEdit>;
    fn read_categories(&self) -> Result<Option<Vec<Category>>>;
    async fn app_names(&self, appids: &[i64]) -> HashMap<i64, String>;
}

/// The real Steam install, Millennium socket and web APIs.
pub struct LiveEnv;

impl CategoryEnv for LiveEnv {
    fn steam_running(&self) -> bool {
        is_steam_running()
    }

    async fn bridge(&self, request: &BridgeRequest) -> Result<BridgeOutcome> {
        run_bridge_edit(request, &BridgeOptions::default()).await
    }

    fn edit_file(&self, edit: &(dyn Fn(&str) -> Result<CategoryEdit> + Sync)) -> Result<CategoryEdit> {
        edit_categories(edit)
    }

    fn read_categories(&self) -> Result<Option<Vec<Category>>> {
        read_categories(false)
    }

    async fn app_names(&self, appids: &[i64]) -> HashMap<i64, String> {
        resolve_app_names(appids).await
    }
}

/// Best-effort appid -> name: local manifests, then owned games (API key),
/// then the public store API. Unresolvable appids are left out.
async fn resolve_app_names(appids: &[i64]) -> HashMap<i64, String> {
    let mut names = HashMap::new();
    if appids.is_empty() {
        return names;
    }
    if let Ok(manifests) = read_all_manifests() {
        for m in manifests {
            names.insert(m.appid, m.name);
        }
    }

    if appids.iter().any(|id| !names.contains_key(id))
        && has_api_key()
        && let Ok(user) = get_user_config()
    {
        let params = [
            ("steamid", user.steam_id64),
            ("include_appinfo", "1".to_string()),
            ("include_played_free_games", "1".to_string()),
        ];
        if let Ok(owned) = steam_api_request("IPlayerService", "GetOwnedGames", "v1", &params).await
            && let Some(games) = owned["response"]["games"].as_array()
        {
            for g in games {
                if let (Some(id), Some(name)) = (g["appid"].as_i64(), g["name"].as_str()) {
                    names.entry(id).or_insert_with(|| name.to_string());
                }
            }
        }
    }

    let missing: Vec<i64> = appids.iter().copied().filter(|id| !names.contains_key(id)).collect();
    let lookups = missing.into_iter().map(|appid| async move {
        let params = [("appids", appid.to_string()), ("filters", "basic".to_string())];
        let data = store_api_request("/api/appdetails", &params).await.ok()?;
        let entry = &data[appid.to_string()];
        if entry["success"].as_bool() != Some(true) {
            return None;
        }
        entry["data"]["name"].as_str().filter(|n| !n.is_empty()).map(|n| (appid, n.to_string()))
    });
    names.extend(futures::future::join_all(lookups).await.into_iter().flatten());
    names
}

async fn describe_category(env: &impl CategoryEnv, category: &Category) -> Value {
    let names = env.app_names(&category.appids).await;
    let mut out = Map::new();
    out.insert("name".into(), category.name.clone().into());
    out.insert("key".into(), category.key.clone().into());
    out.insert("kind".into(), category.kind.as_str().into());
    out.insert("version".into(), category.version.clone().into());
    out.insert("timestamp".into(), category.timestamp.clone().unwrap_or(Value::Null));
    out.insert(
        "appids".into(),
        category.appids.iter().map(|id| json!({ "appid": id, "name": names.get(id) })).collect(),
    );
    if let Some(spec) = &category.filter_spec {
        out.insert("filterSpec".into(), spec.clone());
    }
    Value::Object(out)
}

/// Result field names for the appids a membership edit changed and skipped.
pub struct MembershipFields {
    pub affected: &'static str,
    pub skipped: &'static str,
}

/// Report a bridge edit the plugin never acknowledged by what the categories
/// file shows. Still an error when the file doesn't show it done: Steam may
/// not have saved it yet, or the edit never ran.
async fn read_back_bridge_edit(env: &impl CategoryEnv, request: &BridgeRequest, timeout: &str) -> ToolResult {
    let categories = env.read_categories().ok().flatten().unwrap_or_default();
    let (applied, category) = verify_bridge_edit(request, &categories);
    if !applied {
        return ToolResult::error(format!(
            "{timeout} The categories file doesn't show it yet; check again before retrying."
        ));
    }
    let mut out = Map::new();
    out.insert("changed".into(), Value::Null);
    out.insert("via".into(), "millennium".into());
    out.insert("verifiedByReadback".into(), true.into());
    out.insert("note".into(), format!("{timeout} The categories file shows it applied.").into());
    if let Some(c) = category {
        out.insert("category".into(), describe_category(env, c).await);
    }
    ToolResult::json(&Value::Object(out))
}

fn insert_membership(
    out: &mut Map<String, Value>,
    fields: Option<&MembershipFields>,
    affected: Option<&[i64]>,
    skipped: Option<&[i64]>,
) {
    if let Some(f) = fields {
        out.insert(f.affected.into(), json!(affected.unwrap_or_default()));
        out.insert(f.skipped.into(), json!(skipped.unwrap_or_default()));
    }
}

/// Run a category edit and report the edited category (or just its key once
/// deleted). While Steam runs, `request` goes through the Millennium bridge;
/// otherwise `file_edit` rewrites the categories file.
pub async fn run_category_edit(
    env: &impl CategoryEnv,
    action: &str,
    request: &BridgeRequest,
    file_edit: &(dyn Fn(&str) -> Result<CategoryEdit> + Sync),
    fields: Option<&MembershipFields>,
) -> ToolResult {
    let result: Result<ToolResult> = async {
        if env.steam_running() {
            let bridged = match env.bridge(request).await {
                Ok(b) => b,
                Err(e) => match e.downcast_ref::<CategoryError>() {
                    Some(ce) if ce.bridge_timeout => return Ok(read_back_bridge_edit(env, request, &ce.message).await),
                    _ => return Err(e),
                },
            };
            let bridged = match bridged {
                BridgeOutcome::Unavailable(why) => return Err(steam_running_error(Some(&why)).into()),
                BridgeOutcome::Done(r) => r,
            };
            let mut out = Map::new();
            out.insert("changed".into(), bridged.changed.into());
            out.insert("via".into(), "millennium".into());
            insert_membership(&mut out, fields, bridged.affected.as_deref(), bridged.skipped.as_deref());
            match &bridged.collection {
                Some(c) => {
                    out.insert("category".into(), describe_category(env, &bridge_category(c)).await);
                }
                None => {
                    out.insert("key".into(), format!("{KEY_PREFIX}{}", bridged.id).into());
                }
            }
            return Ok(ToolResult::json(&Value::Object(out)));
        }

        let result = env.edit_file(file_edit)?;
        let category = parse_categories(&result.raw, false).into_iter().find(|c| c.key == result.key);
        let mut out = Map::new();
        out.insert("changed".into(), result.changed.into());
        insert_membership(&mut out, fields, result.affected.as_deref(), result.skipped.as_deref());
        match category {
            Some(c) => {
                out.insert("category".into(), describe_category(env, &c).await);
            }
            None => {
                out.insert("key".into(), result.key.clone().into());
            }
        }
        Ok(ToolResult::json(&Value::Object(out)))
    }
    .await;

    result.unwrap_or_else(|e| match e.downcast_ref::<CategoryError>() {
        Some(ce) => ToolResult::error(ce.message.clone()),
        None => ToolResult::error(format!("Error in {action}: {e}")),
    })
}

fn required_param(names: &str, action: &str) -> ToolResult {
    ToolResult::error(format!("Action \"{action}\" requires {names}"))
}

async fn handle(args: Args) -> ToolResult {
    let action = args.str("action").unwrap_or_else(|| "list".into());
    let env = LiveEnv;
    let name = args.str("name").filter(|n| !n.is_empty());
    let appids = args.ints("appids").filter(|a| !a.is_empty());

    match action.as_str() {
        "category_add" | "category_remove" => {
            let (Some(name), Some(appids)) = (name, appids) else {
                return required_param("\"name\" and \"appids\"", &action);
            };
            let add = action == "category_add";
            let request = if add {
                BridgeRequest::Add { name: name.clone(), appids: appids.clone() }
            } else {
                BridgeRequest::Remove { name: name.clone(), appids: appids.clone() }
            };
            let fields = if add {
                MembershipFields { affected: "added", skipped: "alreadyPresent" }
            } else {
                MembershipFields { affected: "removed", skipped: "notPresent" }
            };
            let edit = |raw: &str| {
                if add {
                    add_apps(raw, &name, &appids, now_seconds())
                } else {
                    remove_apps(raw, &name, &appids, now_seconds())
                }
            };
            run_category_edit(&env, &action, &request, &edit, Some(&fields)).await
        }
        "category_create" => {
            let Some(name) = name else { return required_param("\"name\"", &action) };
            let request = BridgeRequest::Create { name: name.clone() };
            let edit = |raw: &str| create_category(raw, &name, now_seconds(), &random_collection_id());
            run_category_edit(&env, &action, &request, &edit, None).await
        }
        "category_rename" => {
            let (Some(name), Some(new_name)) = (name, args.str("newName").filter(|n| !n.is_empty())) else {
                return required_param("\"name\" and \"newName\"", &action);
            };
            let request = BridgeRequest::Rename { name: name.clone(), new_name: new_name.clone() };
            let edit = |raw: &str| rename_category(raw, &name, &new_name, now_seconds());
            run_category_edit(&env, &action, &request, &edit, None).await
        }
        "category_delete" => {
            let Some(name) = name else { return required_param("\"name\"", &action) };
            let request = BridgeRequest::Delete { name: name.clone() };
            let edit = |raw: &str| delete_category(raw, &name, now_seconds());
            run_category_edit(&env, &action, &request, &edit, None).await
        }
        "categories" => ToolResult::or_error(list_categories(&args), "Error listing categories"),
        "category" => {
            let Some(name) = name else {
                return ToolResult::error("The \"name\" parameter is required for action \"category\"");
            };
            let include_deleted = args.bool("includeDeleted").unwrap_or(false);
            let categories = match read_categories(include_deleted) {
                Ok(c) => c.unwrap_or_default(),
                Err(e) => return ToolResult::error(format!("Error reading category: {e}")),
            };
            match find_category(&categories, &name) {
                Some(c) => ToolResult::json(&describe_category(&env, c).await),
                None => {
                    let names: Vec<&str> = categories.iter().map(|c| c.name.as_str()).collect();
                    let available = if names.is_empty() { "(none)".to_string() } else { names.join(", ") };
                    ToolResult::error(format!("Category \"{name}\" not found. Available: {available}"))
                }
            }
        }
        "stats" => ToolResult::or_error(stats(), "Error getting library stats"),
        _ => ToolResult::or_error(list(), "Error listing libraries"),
    }
}

fn list_categories(args: &Args) -> Result<ToolResult> {
    let include_deleted = args.bool("includeDeleted").unwrap_or(false);
    let categories = read_categories(include_deleted)?.unwrap_or_default();
    if categories.is_empty() {
        return Ok(ToolResult::json(&json!({
            "collections": [],
            "note": "No cloud-storage-namespace-1.json found or it contains no categories",
        })));
    }
    let collections: Vec<Value> = categories
        .iter()
        .map(|c| {
            let mut o = Map::new();
            o.insert("name".into(), c.name.clone().into());
            o.insert("key".into(), c.key.clone().into());
            o.insert("kind".into(), c.kind.as_str().into());
            o.insert("added".into(), c.added.into());
            o.insert("removed".into(), c.removed.into());
            o.insert("hasFilter".into(), c.has_filter.into());
            if include_deleted {
                o.insert("isDeleted".into(), c.is_deleted.into());
            }
            Value::Object(o)
        })
        .collect();
    Ok(ToolResult::json(&json!({ "collections": collections })))
}

fn list() -> Result<ToolResult> {
    let results: Vec<Value> = get_libraries()?
        .iter()
        .map(|lib| {
            json!({
                "path": lib.path,
                "label": lib.label,
                "totalSize": format_bytes(lib.total_size as f64),
                "freeSpace": format_bytes(lib.free_space as f64),
                "gameCount": lib.appids.len(),
                "mounted": lib.mounted,
            })
        })
        .collect();
    Ok(ToolResult::json(&Value::Array(results)))
}

fn stats() -> Result<ToolResult> {
    let libraries = get_libraries()?;
    let manifests = read_all_manifests()?;
    let total_size: i64 = manifests.iter().map(|m| m.size_on_disk).sum();

    let games_per_library: Vec<Value> = libraries
        .iter()
        .map(|lib| {
            let size: i64 = manifests.iter().filter(|m| m.library_path == lib.path).map(|m| m.size_on_disk).sum();
            json!({
                "path": lib.path,
                "label": lib.label,
                "gameCount": lib.appids.len(),
                "totalSize": format_bytes(size as f64),
            })
        })
        .collect();

    let mut by_size: Vec<_> = manifests.iter().collect();
    by_size.sort_by_key(|m| std::cmp::Reverse(m.size_on_disk));
    let top_by_size: Vec<Value> = by_size
        .iter()
        .take(10)
        .map(|m| json!({ "appid": m.appid, "name": m.name, "size": format_bytes(m.size_on_disk as f64) }))
        .collect();

    let mut by_played: Vec<_> = manifests.iter().filter(|m| m.last_played > 0).collect();
    by_played.sort_by_key(|m| std::cmp::Reverse(m.last_played));
    let top_recent: Vec<Value> = by_played
        .iter()
        .take(10)
        .map(|m| json!({ "appid": m.appid, "name": m.name, "lastPlayed": format_timestamp(m.last_played) }))
        .collect();

    let mut shame: Vec<_> = manifests.iter().filter(|m| m.last_played == 0).collect();
    shame.sort_by(|a, b| locale_cmp(&a.name, &b.name));
    let shame: Vec<Value> = shame
        .iter()
        .map(|m| json!({ "appid": m.appid, "name": m.name, "size": format_bytes(m.size_on_disk as f64) }))
        .collect();

    let playtimes: HashMap<i64, i64> = get_all_playtimes()?.into_iter().map(|p| (p.appid, p.playtime)).collect();
    let total_minutes: i64 = manifests.iter().map(|m| playtimes.get(&m.appid).copied().unwrap_or(0)).sum();

    Ok(ToolResult::json(&json!({
        "totalGames": manifests.len(),
        "totalSize": format_bytes(total_size as f64),
        "totalPlaytime": format_playtime(total_minutes as f64),
        "gamesPerLibrary": games_per_library,
        "top10BySize": top_by_size,
        "top10RecentlyPlayed": top_recent,
        "pileOfShame": { "count": shame.len(), "games": shame },
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::steam::millennium::{BridgeCollection, BridgeResult};

    /// A stand-in Steam: fixed running state, scripted bridge and file results, no names.
    struct FakeEnv {
        running: bool,
        bridge: Mutex<Option<Result<BridgeOutcome>>>,
        file: Mutex<Option<CategoryEdit>>,
        categories: Vec<Category>,
        bridge_calls: Mutex<usize>,
        file_calls: Mutex<usize>,
    }

    impl FakeEnv {
        fn new(running: bool) -> Self {
            Self {
                running,
                bridge: Mutex::new(None),
                file: Mutex::new(None),
                categories: Vec::new(),
                bridge_calls: Mutex::new(0),
                file_calls: Mutex::new(0),
            }
        }
    }

    impl CategoryEnv for FakeEnv {
        fn steam_running(&self) -> bool {
            self.running
        }

        async fn bridge(&self, _request: &BridgeRequest) -> Result<BridgeOutcome> {
            *self.bridge_calls.lock().unwrap() += 1;
            self.bridge.lock().unwrap().take().expect("unexpected bridge call")
        }

        fn edit_file(&self, _edit: &(dyn Fn(&str) -> Result<CategoryEdit> + Sync)) -> Result<CategoryEdit> {
            *self.file_calls.lock().unwrap() += 1;
            Ok(self.file.lock().unwrap().take().expect("unexpected file edit"))
        }

        fn read_categories(&self) -> Result<Option<Vec<Category>>> {
            Ok(Some(self.categories.clone()))
        }

        async fn app_names(&self, _appids: &[i64]) -> HashMap<i64, String> {
            HashMap::new()
        }
    }

    fn create() -> BridgeRequest {
        BridgeRequest::Create { name: "RPG".into() }
    }

    fn file_edit(raw: &str) -> Result<CategoryEdit> {
        Ok(CategoryEdit {
            raw: raw.into(),
            key: "user-collections.uc-abc".into(),
            changed: true,
            affected: None,
            skipped: None,
        })
    }

    fn done(result: BridgeResult) -> Mutex<Option<Result<BridgeOutcome>>> {
        Mutex::new(Some(Ok(BridgeOutcome::Done(result))))
    }

    fn parsed(result: &ToolResult) -> Value {
        assert!(!result.is_error, "{}", result.text);
        serde_json::from_str(&result.text).unwrap()
    }

    #[tokio::test]
    async fn edits_through_the_bridge_while_steam_runs() {
        let env = FakeEnv {
            bridge: done(BridgeResult {
                changed: true,
                id: "uc-abc".into(),
                collection: Some(BridgeCollection {
                    id: "uc-abc".into(),
                    name: "RPG".into(),
                    dynamic: false,
                    appids: vec![],
                }),
                affected: None,
                skipped: None,
            }),
            ..FakeEnv::new(true)
        };
        let out = parsed(&run_category_edit(&env, "category_create", &create(), &file_edit, None).await);
        assert_eq!(*env.file_calls.lock().unwrap(), 0);
        assert_eq!(out["changed"], true);
        assert_eq!(out["via"], "millennium");
        assert_eq!(out["category"]["key"], "user-collections.uc-abc");
        assert_eq!(out["category"]["kind"], "explicit");
    }

    #[tokio::test]
    async fn refuses_with_the_bridge_status_without_a_usable_bridge() {
        let env = FakeEnv {
            bridge: Mutex::new(Some(Ok(BridgeOutcome::Unavailable(
                "steam-mcp-bridge is installed but disabled".into(),
            )))),
            ..FakeEnv::new(true)
        };
        let result = run_category_edit(&env, "category_create", &create(), &file_edit, None).await;
        assert!(result.is_error);
        assert!(result.text.contains("Steam is currently running"));
        assert!(result.text.contains("Millennium bridge: steam-mcp-bridge is installed but disabled."));
        assert_eq!(*env.file_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn edits_the_file_while_steam_is_closed() {
        let env = FakeEnv {
            file: Mutex::new(Some(CategoryEdit {
                raw: "[]".into(),
                key: "user-collections.uc-abc".into(),
                changed: true,
                affected: None,
                skipped: None,
            })),
            ..FakeEnv::new(false)
        };
        let out = parsed(&run_category_edit(&env, "category_create", &create(), &file_edit, None).await);
        assert_eq!(*env.bridge_calls.lock().unwrap(), 0);
        assert_eq!(out, json!({ "changed": true, "key": "user-collections.uc-abc" }));
    }

    #[tokio::test]
    async fn names_the_membership_fields_on_both_routes() {
        let fields = MembershipFields { affected: "added", skipped: "alreadyPresent" };
        let env = FakeEnv {
            bridge: done(BridgeResult {
                changed: true,
                id: "favorite".into(),
                collection: None,
                affected: Some(vec![440]),
                skipped: Some(vec![730]),
            }),
            ..FakeEnv::new(true)
        };
        let out = parsed(&run_category_edit(&env, "category_add", &create(), &file_edit, Some(&fields)).await);
        assert_eq!((&out["added"], &out["alreadyPresent"]), (&json!([440]), &json!([730])));

        let env = FakeEnv {
            file: Mutex::new(Some(CategoryEdit {
                raw: "[]".into(),
                key: "user-collections.favorite".into(),
                changed: false,
                affected: Some(vec![]),
                skipped: Some(vec![440]),
            })),
            ..FakeEnv::new(false)
        };
        let out = parsed(&run_category_edit(&env, "category_add", &create(), &file_edit, Some(&fields)).await);
        assert_eq!((&out["added"], &out["alreadyPresent"]), (&json!([]), &json!([440])));
    }

    fn rpg(appids: &[i64]) -> Category {
        crate::steam::millennium::bridge_category(&BridgeCollection {
            id: "uc-abc".into(),
            name: "RPG".into(),
            dynamic: false,
            appids: appids.to_vec(),
        })
    }

    fn timed_out(categories: Vec<Category>) -> FakeEnv {
        FakeEnv {
            bridge: Mutex::new(Some(Err(CategoryError::timeout("The plugin did not answer within 15s.").into()))),
            categories,
            ..FakeEnv::new(true)
        }
    }

    #[tokio::test]
    async fn timeout_reports_applied_when_the_file_shows_it() {
        let add = BridgeRequest::Add { name: "RPG".into(), appids: vec![440] };
        let env = timed_out(vec![rpg(&[10, 440])]);
        let out = parsed(&run_category_edit(&env, "category_add", &add, &file_edit, None).await);
        assert_eq!(out["via"], "millennium");
        assert_eq!(out["verifiedByReadback"], true);
        assert_eq!(out["category"]["key"], "user-collections.uc-abc");
        assert_eq!(out["category"]["appids"][1]["appid"], 440);
    }

    #[tokio::test]
    async fn timeout_stays_an_error_when_the_file_does_not_show_it() {
        let add = BridgeRequest::Add { name: "RPG".into(), appids: vec![440] };
        let env = timed_out(vec![rpg(&[10])]);
        let result = run_category_edit(&env, "category_add", &add, &file_edit, None).await;
        assert!(result.is_error);
        assert!(result.text.contains("did not answer") && result.text.contains("doesn't show it yet"));
    }
}
