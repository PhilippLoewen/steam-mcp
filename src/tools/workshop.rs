//! workshop: installed Workshop items, per-game stats, and Workshop search.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

use super::js::js_str;
use crate::steam::api::steam_api_request;
use crate::steam::manifests::read_all_manifests;
use crate::steam::paths::get_library_folders;
use crate::steam::workshop::{read_all_workshop_data, read_workshop_manifest};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{format_bytes, format_timestamp, js_num, num};

pub fn tool() -> Tool {
    Tool {
        name: "workshop",
        description: "List workshop items, stats, or search",
        params: vec![
            Param::new("action", ParamType::Enum(&["list", "stats", "search"])).required(),
            Param::new("appid", ParamType::Number),
            Param::new("query", ParamType::String),
            Param::new("count", ParamType::Number),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[],
        destructive: false,
        open_world: true,
    }
}

async fn handle(args: Args) -> ToolResult {
    let action = args.str("action").unwrap_or_default();
    let appid = args.f64("appid");
    let result = match action.as_str() {
        "list" => match appid {
            None => return ToolResult::error("appid is required for action \"list\""),
            Some(appid) => list(appid),
        },
        "stats" => stats(),
        _ => {
            let Some(appid) = appid else {
                return ToolResult::error("appid is required for action \"search\"");
            };
            let query = args.str("query").unwrap_or_default();
            if query.is_empty() {
                return ToolResult::error("query is required for action \"search\"");
            }
            search(appid, &query, args.f64("count").unwrap_or(10.0)).await
        }
    };
    ToolResult::or_error(result, &format!("Error in workshop ({action})"))
}

fn list(appid: f64) -> Result<ToolResult> {
    let workshop = get_library_folders()?.into_iter().find_map(|folder| {
        let path = folder.join(format!("steamapps/workshop/appworkshop_{}.acf", js_num(appid)));
        read_workshop_manifest(&path).ok()
    });
    let Some(workshop) = workshop.filter(|w| !w.items.is_empty()) else {
        return Ok(ToolResult::text(format!("No Workshop items found for appid {}.", js_num(appid))));
    };

    let items: Vec<Value> = workshop
        .items
        .iter()
        .map(|item| {
            json!({
                "publishedFileId": item.published_file_id,
                "size": format_bytes(item.size as f64),
                "lastUpdated": format_timestamp(item.time_updated),
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "totalItems": items.len(),
        "totalSize": format_bytes(workshop.size_on_disk as f64),
        "items": items,
    })))
}

fn stats() -> Result<ToolResult> {
    let all = read_all_workshop_data()?;
    let names: HashMap<i64, String> = read_all_manifests()?.into_iter().map(|m| (m.appid, m.name)).collect();

    let mut total_items = 0;
    let mut total_size = 0i64;
    let mut per_game: Vec<(i64, Value)> = Vec::new();
    for (appid, ws) in &all {
        total_items += ws.items.len();
        total_size += ws.size_on_disk;
        let name = names.get(appid).cloned().unwrap_or_else(|| format!("Unknown ({appid})"));
        per_game.push((
            ws.size_on_disk,
            json!({
                "appid": appid,
                "name": name,
                "itemCount": ws.items.len(),
                "totalSize": format_bytes(ws.size_on_disk as f64),
            }),
        ));
    }
    per_game.sort_by_key(|g| std::cmp::Reverse(g.0));

    Ok(ToolResult::json(&json!({
        "totalItems": total_items,
        "totalSize": format_bytes(total_size as f64),
        "gameCount": per_game.len(),
        "perGameBreakdown": per_game.into_iter().map(|(_, g)| g).collect::<Vec<_>>(),
    })))
}

async fn search(appid: f64, query: &str, count: f64) -> Result<ToolResult> {
    let num_results = count.clamp(1.0, 100.0);
    let data = steam_api_request(
        "IPublishedFileService",
        "QueryFiles",
        "v1",
        &[
            ("query_type", "3".into()),
            ("page", "1".into()),
            ("numperpage", js_num(num_results)),
            ("appid", js_num(appid)),
            ("search_text", query.to_string()),
            ("return_short_description", "1".into()),
            ("return_metadata", "1".into()),
        ],
    )
    .await?;

    let response = match data.get("response") {
        None => return Err(anyhow!("Cannot read properties of undefined (reading 'publishedfiledetails')")),
        Some(Value::Null) => return Err(anyhow!("Cannot read properties of null (reading 'publishedfiledetails')")),
        Some(r) => r,
    };
    let items = response.get("publishedfiledetails").and_then(Value::as_array).cloned().unwrap_or_default();

    let results: Vec<Value> = items.iter().map(search_result).collect();

    let mut output = Map::new();
    output.insert("appid".into(), num(appid));
    output.insert("query".into(), query.into());
    put(&mut output, "total_results", response.get("total"));
    output.insert("showing".into(), results.len().into());
    output.insert("items".into(), results.into());
    Ok(ToolResult::json(&Value::Object(output)))
}

fn search_result(item: &Value) -> Value {
    let description = [item.get("short_description"), item.get("file_description")]
        .into_iter()
        .flatten()
        .find(|v| truthy(v))
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    let description = match description {
        Value::String(s) => Value::String(truncate_utf16(&s, 200)),
        other => other,
    };
    let tags: Vec<Value> = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().map(|t| t.get("tag").cloned().unwrap_or(Value::Null)).collect())
        .unwrap_or_default();
    let preview = item.get("preview_url").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null);

    let mut out = Map::new();
    put(&mut out, "id", item.get("publishedfileid"));
    put(&mut out, "title", item.get("title"));
    out.insert("description".into(), description);
    put(&mut out, "subscriptions", item.get("subscriptions"));
    put(&mut out, "favorites", item.get("favorited"));
    put(&mut out, "views", item.get("views"));
    out.insert("size".into(), format_bytes_js(item.get("file_size")).into());
    out.insert("tags".into(), tags.into());
    out.insert("preview_url".into(), preview);
    out.insert(
        "workshop_url".into(),
        format!("https://steamcommunity.com/sharedfiles/filedetails/?id={}", js_str(item.get("publishedfileid")))
            .into(),
    );
    Value::Object(out)
}

/// `s.length > max ? s.slice(0, max) + '...' : s`, counting UTF-16 units like JS.
fn truncate_utf16(s: &str, max: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() > max { String::from_utf16_lossy(&units[..max]) + "..." } else { s.to_string() }
}

/// Insert `value` unless it is undefined, as JSON.stringify skips undefined.
fn put(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        map.insert(key.into(), v.clone());
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JS `Number(s)` for a string.
fn js_number(s: &str) -> f64 {
    let t = s.trim();
    match t {
        "" => 0.0,
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if t.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) => t.parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// The TS formatBytes applied to an untyped API value (Steam sends file_size as a string).
fn format_bytes_js(v: Option<&Value>) -> String {
    match v {
        Some(Value::Number(n)) => format_bytes(n.as_f64().unwrap_or(0.0)),
        Some(Value::String(s)) => {
            let n = js_number(s);
            if n < 0.0 {
                format_bytes(0.0)
            } else if n >= 1024.0 {
                format_bytes(n)
            } else {
                format!("{s} B")
            }
        }
        other => format!("{} B", js_str(other)),
    }
}
