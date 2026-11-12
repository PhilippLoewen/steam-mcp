//! games: list, get, search, compare, random pick, or find unplayed games.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde_json::{Map, Value, json};

use super::js::encode_uri_component;
use crate::steam::api::HTTP;
use crate::steam::compat::{get_compat_data_size, get_game_proton_version};
use crate::steam::manifests::{AppManifest, read_all_manifests};
use crate::steam::userdata::{get_all_playtimes, get_app_launch_options, get_playtime};
use crate::steam::workshop::read_workshop_manifest;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{format_bytes, format_playtime, format_timestamp, js_num, locale_cmp, num, to_fixed};

pub fn tool() -> Tool {
    Tool {
        name: "games",
        description: "List, get, search, compare, random pick, or find unplayed games",
        params: vec![
            Param::new("action", ParamType::Enum(&["list", "get", "search", "compare", "random", "unplayed"]))
                .required(),
            Param::new("appid", ParamType::Number),
            Param::new("appid2", ParamType::Number),
            Param::new("query", ParamType::String),
            Param::new("search", ParamType::String),
            Param::new("filter", ParamType::String),
            Param::new("include_store", ParamType::Boolean),
            Param::new("sort_by", ParamType::Enum(&["name", "size", "last_played", "playtime"])),
            Param::new("sort_order", ParamType::Enum(&["asc", "desc"])),
            Param::new("limit", ParamType::Number),
            Param::new("offset", ParamType::Number),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[],
        destructive: false,
        open_world: true,
    }
}

async fn handle(args: Args) -> ToolResult {
    match args.str("action").as_deref() {
        Some("list") => ToolResult::or_error(list(&args), "Error listing games"),
        Some("get") => ToolResult::or_error(get(&args).await, "Error getting game details"),
        Some("search") => ToolResult::or_error(search(&args).await, "Error searching games"),
        Some("compare") => ToolResult::or_error(compare(&args), "Error comparing games"),
        Some("random") => ToolResult::or_error(random(&args), "Error picking random game"),
        _ => ToolResult::or_error(unplayed(&args), "Error listing pile of shame"),
    }
}

/// A numeric argument that is present and truthy (non-zero), as `if (!params.x)` checks it.
fn truthy_num(args: &Args, name: &str) -> Option<f64> {
    args.f64(name).filter(|n| *n != 0.0 && !n.is_nan())
}

fn find_manifest(manifests: &[AppManifest], appid: f64) -> Option<&AppManifest> {
    manifests.iter().find(|m| m.appid as f64 == appid)
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `a ?? b` on a looked-up property: missing or null falls back.
fn nullish_or(v: Option<&Value>, fallback: Value) -> Value {
    match v {
        None | Some(Value::Null) => fallback,
        Some(v) => v.clone(),
    }
}

/// JS `Array.prototype.slice(start, end)` index resolution.
fn slice_index(i: f64, len: usize) -> usize {
    let i = i.trunc();
    let len_f = len as f64;
    if i < 0.0 { (len_f + i).max(0.0) as usize } else { i.min(len_f) as usize }
}

fn insert_playtime(out: &mut Map<String, Value>, appid: i64) {
    if let Ok(Some(p)) = get_playtime(appid) {
        out.insert("playtime".into(), format_playtime(p.playtime as f64).into());
        out.insert("playtimeMinutes".into(), p.playtime.into());
    }
}

fn is_tool_entry(name: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| name.contains(p))
}

// ---------------------------------------------------------------------------
// Search scoring
// ---------------------------------------------------------------------------

/// `ROMAN_MAP[w]` on a plain JS object literal.
enum RomanLookup {
    Digit(&'static str),
    /// `Object.prototype.constructor`, inherited by the object literal.
    Constructor,
    Missing,
}

fn roman(word: &str) -> RomanLookup {
    match word {
        "ii" => RomanLookup::Digit("2"),
        "iii" => RomanLookup::Digit("3"),
        "iv" => RomanLookup::Digit("4"),
        "v" => RomanLookup::Digit("5"),
        "vi" => RomanLookup::Digit("6"),
        "vii" => RomanLookup::Digit("7"),
        "viii" => RomanLookup::Digit("8"),
        "ix" => RomanLookup::Digit("9"),
        "x" => RomanLookup::Digit("10"),
        "constructor" => RomanLookup::Constructor,
        _ => RomanLookup::Missing,
    }
}

const OBJECT_CONSTRUCTOR_SOURCE: &str = "function Object() { [native code] }";

fn first_char(w: &str) -> &str {
    w.char_indices().nth(1).map_or(w, |(i, _)| &w[..i])
}

fn score_search(query: &str, name: &str) -> i64 {
    let name_lower = name.to_lowercase();
    let query_lower = query.to_lowercase();

    if name_lower == query_lower {
        return 100;
    }
    if name_lower.contains(&query_lower) {
        return 80;
    }

    let query_tokens: Vec<&str> = query_lower.split_whitespace().collect();
    let name_words: Vec<&str> = name_lower
        .split(|c: char| c.is_whitespace() || matches!(c, '-' | ':' | '_'))
        .filter(|w| !w.is_empty())
        .collect();

    let word_start_match = query_tokens.iter().all(|qt| name_words.iter().any(|nw| nw.starts_with(qt)));
    if word_start_match && !query_tokens.is_empty() {
        return 60;
    }

    let initials: String = name_words.iter().map(|w| first_char(w)).collect();
    let initials_normalized: String = name_words
        .iter()
        .map(|w| match roman(w) {
            RomanLookup::Digit(d) => first_char(d),
            RomanLookup::Constructor => "",
            RomanLookup::Missing => first_char(w),
        })
        .collect();
    let name_abbrev: String = name_words
        .iter()
        .map(|w| match roman(w) {
            RomanLookup::Digit(d) => d,
            RomanLookup::Constructor => OBJECT_CONSTRUCTOR_SOURCE,
            RomanLookup::Missing => first_char(w),
        })
        .collect();

    if name_abbrev == query_lower || initials == query_lower || initials_normalized == query_lower {
        return 40;
    }
    if name_abbrev.starts_with(&query_lower) {
        return 30;
    }
    0
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

fn list(args: &Args) -> Result<ToolResult> {
    let mut games = read_all_manifests()?;

    if let Some(search) = args.str("search").filter(|s| !s.is_empty()) {
        let term = search.to_lowercase();
        games.retain(|g| g.name.to_lowercase().contains(&term));
    }

    let playtimes: HashMap<i64, i64> = get_all_playtimes()?.into_iter().map(|p| (p.appid, p.playtime)).collect();
    let playtime = |appid: i64| playtimes.get(&appid).copied().unwrap_or(0);

    let sort_by = args.str("sort_by").unwrap_or_else(|| "name".into());
    let desc = args.str("sort_order").as_deref() == Some("desc");
    games.sort_by(|a, b| {
        let ord = match sort_by.as_str() {
            "size" => a.size_on_disk.cmp(&b.size_on_disk),
            "last_played" => a.last_played.cmp(&b.last_played),
            "playtime" => playtime(a.appid).cmp(&playtime(b.appid)),
            _ => locale_cmp(&a.name, &b.name),
        };
        if desc { ord.reverse() } else { ord }
    });

    let total = games.len();
    let limit = args.f64("limit").unwrap_or(50.0);
    let offset = args.f64("offset").unwrap_or(0.0);
    let start = slice_index(offset, total);
    let end = slice_index(offset + limit, total);

    let results: Vec<Value> = games
        .get(start..end.max(start))
        .unwrap_or_default()
        .iter()
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "sizeOnDisk": format_bytes(g.size_on_disk as f64),
                "lastPlayed": format_timestamp(g.last_played),
                "libraryPath": g.library_path.display().to_string(),
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "total": total,
        "offset": num(offset),
        "limit": num(limit),
        "returned": results.len(),
        "games": results,
    })))
}

async fn get(args: &Args) -> Result<ToolResult> {
    let Some(appid_arg) = truthy_num(args, "appid") else {
        return Ok(ToolResult::error("appid is required for get action"));
    };

    let manifests = read_all_manifests()?;
    let Some(manifest) = find_manifest(&manifests, appid_arg) else {
        return Ok(ToolResult::error(format!(
            "Game with appid {} not found among installed games.",
            js_num(appid_arg)
        )));
    };
    let appid = manifest.appid;
    let library_path = manifest.library_path.display().to_string();

    let mut game = Map::new();
    game.insert("appid".into(), appid.into());
    game.insert("name".into(), manifest.name.clone().into());
    game.insert("installDir".into(), manifest.installdir.clone().into());
    game.insert("sizeOnDisk".into(), format_bytes(manifest.size_on_disk as f64).into());
    game.insert("sizeOnDiskBytes".into(), manifest.size_on_disk.into());
    game.insert("buildId".into(), manifest.buildid.clone().into());
    game.insert("lastUpdated".into(), format_timestamp(manifest.last_updated).into());
    game.insert("lastPlayed".into(), format_timestamp(manifest.last_played).into());
    game.insert("stateFlags".into(), manifest.state_flags.into());
    game.insert("libraryPath".into(), library_path.clone().into());

    if let Ok(Some(opts)) = get_app_launch_options(appid)
        && !opts.is_empty()
    {
        game.insert("launchOptions".into(), opts.into());
    }

    insert_playtime(&mut game, appid);

    if let Ok(Some(version)) = get_game_proton_version(appid) {
        let prefix_size = get_compat_data_size(&manifest.library_path, appid);
        game.insert(
            "proton".into(),
            json!({
                "version": version,
                "prefixPath": format!("{library_path}/steamapps/compatdata/{appid}"),
                "prefixSize": format_bytes(prefix_size as f64),
            }),
        );
    }

    let workshop_path = format!("{library_path}/steamapps/workshop/appworkshop_{appid}.acf");
    if let Ok(workshop) = read_workshop_manifest(Path::new(&workshop_path))
        && !workshop.items.is_empty()
    {
        let items: Vec<Value> = workshop
            .items
            .iter()
            .take(10)
            .map(|item| {
                json!({
                    "publishedFileId": item.published_file_id,
                    "size": format_bytes(item.size as f64),
                    "timeUpdated": format_timestamp(item.time_updated),
                })
            })
            .collect();
        game.insert(
            "workshop".into(),
            json!({
                "itemCount": workshop.items.len(),
                "totalSize": format_bytes(workshop.size_on_disk as f64),
                "items": items,
            }),
        );
    }

    if args.bool("include_store") == Some(true) {
        let url = format!("https://store.steampowered.com/api/appdetails?appids={appid}");
        if let Some(data) = fetch_json(&url).await
            && let Some(entry) = data.get(appid.to_string())
            && entry.get("success").is_some_and(is_truthy)
            && let Some(d) = entry.get("data").filter(|d| is_truthy(d))
        {
            insert_store_details(&mut game, d);
        }
    }

    Ok(ToolResult::json(&Value::Object(game)))
}

/// GET `url` and parse the body as JSON when the response is ok; None otherwise.
async fn fetch_json(url: &str) -> Option<Value> {
    let response = HTTP.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Copy the store fields into `game` in order. Stops at the point where the
/// TS code would throw (and its catch would keep the fields set so far).
fn insert_store_details(game: &mut Map<String, Value>, d: &Value) -> Option<()> {
    game.insert("short_description".into(), nullish_or(d.get("short_description"), Value::Null));

    let genres = match d.get("genres") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(genres)) => genres
            .iter()
            .map(|g| match g {
                Value::Null => None,
                _ => Some(g.get("description").cloned().unwrap_or(Value::Null)),
            })
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };
    game.insert("genres".into(), genres.into());

    game.insert("developers".into(), nullish_or(d.get("developers"), json!([])));
    game.insert("publishers".into(), nullish_or(d.get("publishers"), json!([])));

    let metacritic = match d.get("metacritic").filter(|m| is_truthy(m)) {
        Some(m) => Value::Object(pick(m, &["score", "url"])),
        None => Value::Null,
    };
    game.insert("metacritic".into(), metacritic);
    game.insert("platforms".into(), nullish_or(d.get("platforms"), Value::Null));

    let price = match d.get("price_overview").filter(|p| is_truthy(p)) {
        Some(p) => Value::Object(pick(p, &["final_formatted", "discount_percent", "currency"])),
        None if d.get("is_free").is_some_and(is_truthy) => "Free".into(),
        None => "N/A".into(),
    };
    game.insert("price".into(), price);
    game.insert("release_date".into(), nullish_or(d.get("release_date"), Value::Null));
    game.insert("pc_requirements".into(), nullish_or(d.get("pc_requirements"), Value::Null));
    Some(())
}

/// `{ a: v.a, b: v.b }` with undefined properties left out, as JSON.stringify does.
fn pick(v: &Value, keys: &[&str]) -> Map<String, Value> {
    keys.iter().filter_map(|k| v.get(*k).map(|x| (k.to_string(), x.clone()))).collect()
}

/// A store search hit's price, as the TS formats `item.price?.final`.
fn store_price(item: &Value) -> String {
    let final_price = match item.get("price").and_then(|p| p.get("final")) {
        None | Some(Value::Null) => return "N/A".into(),
        Some(f) => f,
    };
    if final_price.as_f64() == Some(0.0) && final_price.is_number() {
        return "Free".into();
    }
    let n = match final_price {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => f64::NAN,
    };
    format!("${}", to_fixed(n / 100.0, 2))
}

async fn store_search(query: &str, installed: &HashSet<i64>) -> Vec<Value> {
    let url =
        format!("https://store.steampowered.com/api/storesearch/?term={}&l=english&cc=US", encode_uri_component(query));
    let Some(data) = fetch_json(&url).await else { return Vec::new() };
    let Some(Value::Array(items)) = data.get("items") else { return Vec::new() };
    if items.iter().any(Value::is_null) {
        return Vec::new();
    }
    items
        .iter()
        .filter(|item| {
            let id = item.get("id").and_then(Value::as_f64);
            !id.is_some_and(|id| id.fract() == 0.0 && installed.contains(&(id as i64)))
        })
        .map(|item| {
            let mut o = Map::new();
            if let Some(v) = item.get("id") {
                o.insert("appid".into(), v.clone());
            }
            if let Some(v) = item.get("name") {
                o.insert("name".into(), v.clone());
            }
            if let Some(v) = item.get("type") {
                o.insert("type".into(), v.clone());
            }
            o.insert("price".into(), store_price(item).into());
            o.insert("installed".into(), false.into());
            Value::Object(o)
        })
        .collect()
}

async fn search(args: &Args) -> Result<ToolResult> {
    let Some(query) = args.str("query").filter(|q| !q.is_empty()) else {
        return Ok(ToolResult::error("query is required for search action"));
    };

    let manifests = read_all_manifests()?;
    let installed: HashSet<i64> = manifests.iter().map(|m| m.appid).collect();

    let mut results: Vec<(&AppManifest, i64)> =
        manifests.iter().map(|m| (m, score_search(&query, &m.name))).filter(|(_, s)| *s > 0).collect();
    results.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| locale_cmp(&a.name, &b.name)));

    let top: Vec<Value> = results
        .iter()
        .take(20)
        .map(|(m, score)| json!({ "appid": m.appid, "name": m.name, "score": score, "installed": true }))
        .collect();

    let include_store = args.bool("include_store") == Some(true);
    let store_results = if include_store { store_search(&query, &installed).await } else { Vec::new() };

    let mut output = Map::new();
    output.insert("query".into(), query.into());
    output.insert("resultCount".into(), top.len().into());
    output.insert("totalMatches".into(), results.len().into());
    output.insert("results".into(), top.into());
    if include_store {
        let count = store_results.len();
        output.insert("storeResults".into(), store_results.into());
        output.insert("storeResultCount".into(), count.into());
    }
    Ok(ToolResult::json(&Value::Object(output)))
}

fn compare(args: &Args) -> Result<ToolResult> {
    let (Some(appid1), Some(appid2)) = (truthy_num(args, "appid"), truthy_num(args, "appid2")) else {
        return Ok(ToolResult::error("appid and appid2 are required for compare action"));
    };

    let manifests = read_all_manifests()?;
    let (Some(m1), Some(m2)) = (find_manifest(&manifests, appid1), find_manifest(&manifests, appid2)) else {
        let mut missing = Vec::new();
        if find_manifest(&manifests, appid1).is_none() {
            missing.push(js_num(appid1));
        }
        if find_manifest(&manifests, appid2).is_none() {
            missing.push(js_num(appid2));
        }
        return Ok(ToolResult::error(format!("Game(s) not found among installed games: {}", missing.join(", "))));
    };

    let build = |m: &AppManifest| {
        let mut info = Map::new();
        info.insert("appid".into(), m.appid.into());
        info.insert("name".into(), m.name.clone().into());
        info.insert("sizeOnDisk".into(), format_bytes(m.size_on_disk as f64).into());
        info.insert("sizeOnDiskBytes".into(), m.size_on_disk.into());
        info.insert("lastPlayed".into(), format_timestamp(m.last_played).into());
        info.insert("libraryPath".into(), m.library_path.display().to_string().into());
        insert_playtime(&mut info, m.appid);
        let proton = match get_game_proton_version(m.appid) {
            Ok(Some(v)) => v,
            Ok(None) => "Native".into(),
            Err(_) => "Unknown".into(),
        };
        info.insert("protonVersion".into(), proton.into());
        Value::Object(info)
    };

    Ok(ToolResult::json(&json!({ "game1": build(m1), "game2": build(m2) })))
}

fn random(args: &Args) -> Result<ToolResult> {
    let mut games = read_all_manifests()?;
    games.retain(|g| !is_tool_entry(&g.name, &["Steam Linux Runtime", "Proton", "Steamworks Common"]));

    if let Some(filter) = args.str("filter").filter(|f| !f.is_empty()) {
        let term = filter.to_lowercase();
        games.retain(|g| g.name.to_lowercase().contains(&term));
    }

    if games.is_empty() {
        return Ok(ToolResult::text("No games found matching the criteria."));
    }

    let picked = &games[rand::random_range(0..games.len())];
    let mut result = Map::new();
    result.insert("appid".into(), picked.appid.into());
    result.insert("name".into(), picked.name.clone().into());
    result.insert("sizeOnDisk".into(), format_bytes(picked.size_on_disk as f64).into());
    result.insert("lastPlayed".into(), format_timestamp(picked.last_played).into());
    insert_playtime(&mut result, picked.appid);

    Ok(ToolResult::json(&json!({ "totalCandidates": games.len(), "picked": result })))
}

fn unplayed(args: &Args) -> Result<ToolResult> {
    let mut never_played: Vec<AppManifest> = read_all_manifests()?
        .into_iter()
        .filter(|g| {
            g.last_played == 0
                && !is_tool_entry(&g.name, &["Steam Linux Runtime", "Proton", "Steamworks Common", "DFHack"])
        })
        .collect();

    let by_size = args.str("sort_by").is_none_or(|s| s == "size");
    let desc = args.str("sort_order").is_none_or(|s| s == "desc");
    never_played.sort_by(|a, b| {
        let ord = if by_size { a.size_on_disk.cmp(&b.size_on_disk) } else { locale_cmp(&a.name, &b.name) };
        if desc { ord.reverse() } else { ord }
    });

    let total_size: i64 = never_played.iter().map(|g| g.size_on_disk).sum();
    let games: Vec<Value> = never_played
        .iter()
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "sizeOnDisk": format_bytes(g.size_on_disk as f64),
                "sizeOnDiskBytes": g.size_on_disk,
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "count": never_played.len(),
        "totalSizeWasted": format_bytes(total_size as f64),
        "totalSizeWastedBytes": total_size,
        "games": games,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_like_the_ts_search() {
        assert_eq!(score_search("crusader kings iii", "Crusader Kings III"), 100);
        assert_eq!(score_search("kings", "Crusader Kings III"), 80);
        assert_eq!(score_search("cru ki", "Crusader Kings III"), 60);
        assert_eq!(score_search("ck3", "Crusader Kings III"), 40);
        assert_eq!(score_search("cki", "Crusader Kings III"), 40);
        assert_eq!(score_search("ck", "Crusader Kings III: Royal"), 30);
        assert_eq!(score_search("zz", "Crusader Kings III"), 0);
        assert_eq!(score_search("fun", "Constructor Plus"), 30);
    }

    #[test]
    fn slices_like_js() {
        assert_eq!(slice_index(-2.0, 5), 3);
        assert_eq!(slice_index(1.7, 5), 1);
        assert_eq!(slice_index(9.0, 5), 5);
        assert_eq!(encode_uri_component("a b&c'(é)"), "a%20b%26c'(%C3%A9)");
    }
}
