//! insights: achievement completion, play timeline, year in review, play history, library export.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;

use anyhow::{Result, anyhow};
use chrono::{Datelike, Local, TimeZone};
use futures::future::join_all;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::output;
use crate::steam::api::{HTTP, steam_api_request};
use crate::steam::manifests::read_all_manifests;
use crate::steam::paths::get_user_config;
use crate::steam::userdata::get_all_playtimes;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{
    format_playtime, format_timestamp, io_error, js_num, js_sort, locale_cmp, now_seconds, num, to_fixed,
};

const ACHIEVEMENT_BATCH_SIZE: usize = 10;
const TWO_WEEKS_SECONDS: i64 = 14 * 24 * 60 * 60;
const STORE_BATCH_SIZE: usize = 50;

pub fn tool() -> Tool {
    Tool {
        name: "insights",
        description: "Completion stats, timeline, year review, play history, export",
        params: vec![
            Param::new("action", ParamType::Enum(&["completion", "timeline", "year_review", "play_history", "export"]))
                .required(),
            Param::new("steamid", ParamType::String),
            Param::new("year", ParamType::Number),
            Param::new("format", ParamType::Enum(&["json", "csv"])),
            Param::new("output", ParamType::String)
                .describe("Export file name inside the output folder; default steam-library-<time>"),
            Param::new("limit", ParamType::Number),
            Param::new("sort_by", ParamType::Enum(&["playtime", "last_played", "name"])),
            Param::new("installed_only", ParamType::Boolean),
            Param::new("resolve_names", ParamType::Boolean),
            Param::new("include_playtime", ParamType::Boolean),
            Param::new("include_achievements", ParamType::Boolean),
        ],
        handler: |args| boxed(handle(args)),
    }
}

async fn handle(args: Args) -> ToolResult {
    match args.str("action").as_deref() {
        Some("completion") => ToolResult::or_error(completion(&args).await, "Error computing completion stats"),
        Some("timeline") => ToolResult::or_error(timeline(&args).await, "Error building gaming timeline"),
        Some("year_review") => ToolResult::or_error(year_review(&args).await, "Error generating year in review"),
        Some("play_history") => ToolResult::or_error(play_history(&args).await, "Error reading play history"),
        _ => ToolResult::or_error(export(&args).await, "Error exporting library"),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct OwnedGame {
    appid: i64,
    name: String,
    playtime_forever: Option<f64>,
    has_community_visible_stats: bool,
}

struct Completion {
    appid: i64,
    name: String,
    unlocked: i64,
    total: i64,
    percentage: f64,
}

/// JS `Math.round`.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// The end index of `arr.slice(0, limit)` for an array of `len` items.
fn slice_end(len: usize, limit: f64) -> usize {
    let n = limit.trunc();
    if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { n.min(len as f64) as usize }
}

/// `steamid` param, or the active account's SteamID64.
fn steamid_of(args: &Args) -> Result<String> {
    match args.str("steamid") {
        Some(s) => Ok(s),
        None => Ok(get_user_config()?.steam_id64),
    }
}

async fn fetch_owned_games(steamid: &str) -> Result<Vec<OwnedGame>> {
    let data = steam_api_request(
        "IPlayerService",
        "GetOwnedGames",
        "v1",
        &[("steamid", steamid.to_string()), ("include_appinfo", "1".into()), ("include_played_free_games", "1".into())],
    )
    .await?;
    let response = data
        .get("response")
        .filter(|r| !r.is_null())
        .ok_or_else(|| anyhow!("Cannot read properties of undefined (reading 'games')"))?;
    let games = response.get("games").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    Ok(games
        .iter()
        .map(|g| OwnedGame {
            appid: g.get("appid").and_then(Value::as_i64).unwrap_or(0),
            name: g.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            playtime_forever: g.get("playtime_forever").and_then(Value::as_f64),
            has_community_visible_stats: truthy(g.get("has_community_visible_stats")),
        })
        .collect())
}

async fn fetch_achievement_list(appid: i64, steamid: &str) -> Option<Value> {
    steam_api_request(
        "ISteamUserStats",
        "GetPlayerAchievements",
        "v1",
        &[("steamid", steamid.to_string()), ("appid", appid.to_string())],
    )
    .await
    .ok()
}

fn count_unlocked(achievements: &[Value]) -> i64 {
    achievements.iter().filter(|a| a.get("achieved").and_then(Value::as_f64) == Some(1.0)).count() as i64
}

async fn fetch_game_completion(appid: i64, name: String, steamid: &str) -> Option<Completion> {
    let data = fetch_achievement_list(appid, steamid).await?;
    let stats = data.get("playerstats")?;
    if !truthy(stats.get("success")) || !truthy(stats.get("achievements")) {
        return None;
    }
    let achievements = stats.get("achievements")?.as_array()?;
    let total = achievements.len() as i64;
    if total == 0 {
        return None;
    }
    let unlocked = count_unlocked(achievements);
    let percentage = js_round(unlocked as f64 / total as f64 * 10000.0) / 100.0;
    Some(Completion { appid, name, unlocked, total, percentage })
}

async fn fetch_completion_batched(games: Vec<(i64, String)>, steamid: &str) -> Vec<Completion> {
    let mut results = Vec::new();
    for batch in games.chunks(ACHIEVEMENT_BATCH_SIZE) {
        let settled =
            join_all(batch.iter().map(|(appid, name)| fetch_game_completion(*appid, name.clone(), steamid))).await;
        results.extend(settled.into_iter().flatten());
    }
    results
}

async fn fetch_store_name(appid: i64) -> Option<String> {
    let url = format!("https://store.steampowered.com/api/appdetails?appids={appid}&filters=basic");
    let resp = HTTP.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    let entry = data.get(appid.to_string())?;
    if !truthy(entry.get("success")) {
        return None;
    }
    entry.get("data")?.get("name")?.as_str().filter(|n| !n.is_empty()).map(str::to_string)
}

/// Names for appids missing from the local manifests, from the store API.
async fn resolve_names(appids: &[i64]) -> HashMap<i64, String> {
    let mut names = HashMap::new();
    for batch in appids.chunks(STORE_BATCH_SIZE) {
        let results = join_all(batch.iter().map(|&appid| async move { (appid, fetch_store_name(appid).await) })).await;
        for (appid, name) in results {
            if let Some(name) = name {
                names.insert(appid, name);
            }
        }
    }
    names
}

/// Unix seconds of local midnight on January 1st, like `new Date(year, 0, 1).getTime() / 1000`.
fn year_start(year: f64) -> f64 {
    let mut y = year.trunc();
    if (0.0..=99.0).contains(&y) {
        y += 1900.0;
    }
    if y.abs() > 275_760.0 {
        return f64::NAN;
    }
    Local.with_ymd_and_hms(y as i32, 1, 1, 0, 0, 0).earliest().map_or(f64::NAN, |d| d.timestamp() as f64)
}

/// "YYYY-MM" of a Unix timestamp in local time.
fn month_key(unix_seconds: i64) -> String {
    match Local.timestamp_opt(unix_seconds, 0).earliest() {
        Some(d) => format!("{}-{:02}", d.year(), d.month()),
        None => "NaN-NaN".into(),
    }
}

fn escape_csv_field(value: &str) -> String {
    // A leading = + - @ (or tab, CR, LF) makes spreadsheets read the field as a formula.
    if value.starts_with(['=', '+', '-', '@', '\t', '\r', '\n']) {
        return escape_csv_field(&format!("'{value}"));
    }
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async fn completion(args: &Args) -> Result<ToolResult> {
    let steamid = steamid_of(args)?;
    let limit = args.f64("limit").unwrap_or(20.0);

    let mut candidates: Vec<OwnedGame> =
        fetch_owned_games(&steamid).await?.into_iter().filter(|g| g.has_community_visible_stats).collect();
    js_sort(&mut candidates, |a, b| b.playtime_forever.unwrap_or(0.0) - a.playtime_forever.unwrap_or(0.0));
    candidates.truncate(slice_end(candidates.len(), limit));

    if candidates.is_empty() {
        return Ok(ToolResult::text("No games with community stats found for this user."));
    }

    let mut completions =
        fetch_completion_batched(candidates.iter().map(|g| (g.appid, g.name.clone())).collect(), &steamid).await;
    js_sort(&mut completions, |a, b| b.percentage - a.percentage);

    let fully_completed = completions.iter().filter(|g| g.percentage == 100.0).count();
    let total_percentage: f64 = completions.iter().map(|g| g.percentage).sum();
    let average_completion = if completions.is_empty() {
        0.0
    } else {
        js_round(total_percentage / completions.len() as f64 * 100.0) / 100.0
    };

    let closest_to_100: Vec<Value> = completions
        .iter()
        .filter(|g| g.percentage > 0.0 && g.percentage < 100.0)
        .take(5)
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "completion": format!("{}/{} ({}%)", g.unlocked, g.total, js_num(g.percentage)),
                "remaining": g.total - g.unlocked,
            })
        })
        .collect();

    let games: Vec<Value> = completions
        .iter()
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "unlocked": g.unlocked,
                "total": g.total,
                "percentage": format!("{}%", js_num(g.percentage)),
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "games_checked": completions.len(),
        "games_with_stats_attempted": candidates.len(),
        "fully_completed": fully_completed,
        "average_completion": format!("{}%", js_num(average_completion)),
        "closest_to_100": closest_to_100,
        "games": games,
    })))
}

struct TimelineEntry {
    appid: i64,
    name: String,
    last_played: i64,
    last_played_formatted: String,
    playtime: String,
    playtime_minutes: i64,
}

async fn timeline(args: &Args) -> Result<ToolResult> {
    let steamid = steamid_of(args)?;
    let limit = args.f64("limit").unwrap_or(30.0);
    let all_playtimes = get_all_playtimes()?;

    let name_map: HashMap<i64, String> = fetch_owned_games(&steamid)
        .await
        .map(|games| games.into_iter().map(|g| (g.appid, g.name)).collect())
        .unwrap_or_default();

    let mut entries: Vec<TimelineEntry> = all_playtimes
        .iter()
        .filter(|p| p.last_played > 0)
        .map(|p| TimelineEntry {
            appid: p.appid,
            name: name_map.get(&p.appid).cloned().unwrap_or_else(|| format!("Unknown ({})", p.appid)),
            last_played: p.last_played,
            last_played_formatted: format_timestamp(p.last_played),
            playtime: format_playtime(p.playtime as f64),
            playtime_minutes: p.playtime,
        })
        .collect();

    let year = args.f64("year").filter(|&y| y != 0.0);
    if let Some(year) = year {
        let start = year_start(year);
        let end = year_start(year + 1.0);
        entries.retain(|e| e.last_played as f64 >= start && (e.last_played as f64) < end);
    }

    entries.sort_by_key(|e| Reverse(e.last_played));

    let mut month_groups: IndexMap<String, (Vec<&TimelineEntry>, i64)> = IndexMap::new();
    for entry in &entries {
        let group = month_groups.entry(month_key(entry.last_played)).or_default();
        group.0.push(entry);
        group.1 += entry.playtime_minutes;
    }

    let mut sorted_months: Vec<(String, (Vec<&TimelineEntry>, i64))> = month_groups.into_iter().collect();
    sorted_months.sort_by(|(a, _), (b, _)| locale_cmp(b, a));
    sorted_months.truncate(slice_end(sorted_months.len(), limit));

    let timeline: Vec<Value> = sorted_months
        .iter()
        .map(|(month, (games, total))| {
            json!({
                "month": month,
                "games_count": games.len(),
                "total_playtime": format_playtime(*total as f64),
                "games": games
                    .iter()
                    .map(|g| json!({
                        "appid": g.appid,
                        "name": g.name,
                        "playtime": g.playtime,
                        "last_played": g.last_played_formatted,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let mut output = Map::new();
    output.insert("total_months".into(), sorted_months.len().into());
    output.insert("total_games".into(), entries.len().into());
    if let Some(year) = year {
        output.insert("filtered_year".into(), num(year));
    }
    output.insert("timeline".into(), timeline.into());
    Ok(ToolResult::json(&Value::Object(output)))
}

struct YearGame {
    appid: i64,
    name: String,
    playtime_minutes: f64,
    last_played: i64,
}

async fn year_review(args: &Args) -> Result<ToolResult> {
    let steamid = steamid_of(args)?;
    let Some(year) = args.f64("year") else {
        return Ok(ToolResult::error("Error: year is required for year_review action."));
    };
    let start = year_start(year);
    let end = year_start(year + 1.0);

    let owned = fetch_owned_games(&steamid).await?;
    let mut name_map: HashMap<i64, &str> = HashMap::new();
    let mut api_playtime_map: HashMap<i64, Option<f64>> = HashMap::new();
    for g in &owned {
        name_map.insert(g.appid, &g.name);
        api_playtime_map.insert(g.appid, g.playtime_forever);
    }

    let all_playtimes = get_all_playtimes()?;
    let mut year_games: Vec<YearGame> = all_playtimes
        .iter()
        .filter(|p| p.last_played as f64 >= start && (p.last_played as f64) < end)
        .map(|p| YearGame {
            appid: p.appid,
            name: name_map.get(&p.appid).map_or_else(|| format!("Unknown ({})", p.appid), |n| n.to_string()),
            playtime_minutes: api_playtime_map.get(&p.appid).copied().flatten().unwrap_or(p.playtime as f64),
            last_played: p.last_played,
        })
        .collect();
    js_sort(&mut year_games, |a, b| b.playtime_minutes - a.playtime_minutes);

    let total_playtime_minutes: f64 = year_games.iter().map(|g| g.playtime_minutes).sum();

    let top5: Vec<Value> = year_games
        .iter()
        .take(5)
        .map(|g| json!({ "appid": g.appid, "name": g.name, "playtime": format_playtime(g.playtime_minutes) }))
        .collect();

    let achievement_results =
        fetch_completion_batched(year_games.iter().take(10).map(|g| (g.appid, g.name.clone())).collect(), &steamid)
            .await;

    let achievement_summary: Vec<Value> = achievement_results
        .iter()
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "completion": format!("{}/{} ({}%)", g.unlocked, g.total, js_num(g.percentage)),
                "unlocked": g.unlocked,
                "total": g.total,
            })
        })
        .collect();

    let total_unlocked: i64 = achievement_results.iter().map(|g| g.unlocked).sum();
    let total_available: i64 = achievement_results.iter().map(|g| g.total).sum();

    let mut month_counts: IndexMap<String, i64> = IndexMap::new();
    for g in &year_games {
        *month_counts.entry(month_key(g.last_played)).or_default() += 1;
    }
    let mut months: Vec<(String, i64)> = month_counts.into_iter().collect();
    months.sort_by(|(a, _), (b, _)| locale_cmp(a, b));
    let monthly_activity: Vec<Value> =
        months.into_iter().map(|(month, count)| json!({ "month": month, "games_active": count })).collect();

    let completion = if total_available > 0 {
        format!("{}%", js_num(js_round(total_unlocked as f64 / total_available as f64 * 10000.0) / 100.0))
    } else {
        "N/A".to_string()
    };

    let all_games: Vec<Value> = year_games
        .iter()
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "playtime": format_playtime(g.playtime_minutes),
                "last_played": format_timestamp(g.last_played),
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "year": num(year),
        "games_played_count": year_games.len(),
        "total_playtime": format_playtime(total_playtime_minutes),
        "total_playtime_minutes": num(total_playtime_minutes),
        "playtime_note": "Playtime is total all-time playtime for these games, not year-specific (Steam API limitation).",
        "top_5_games": top5,
        "monthly_activity": monthly_activity,
        "achievement_stats": {
            "games_checked": achievement_results.len(),
            "total_unlocked": total_unlocked,
            "total_available": total_available,
            "completion": completion,
            "games": achievement_summary,
        },
        "all_games": all_games,
    })))
}

struct HistoryEntry {
    appid: i64,
    name: String,
    playtime_minutes: i64,
    playtime: String,
    last_played_timestamp: i64,
    last_played: String,
    installed: bool,
}

async fn play_history(args: &Args) -> Result<ToolResult> {
    let sort_by = args.str("sort_by").unwrap_or_else(|| "playtime".into());
    let limit = args.f64("limit").unwrap_or(20.0);
    let installed_only = args.bool("installed_only").unwrap_or(false);
    let do_resolve_names = args.bool("resolve_names").unwrap_or(true);

    let manifests = read_all_manifests()?;
    let mut name_map: HashMap<i64, String> = HashMap::new();
    let mut installed_set: HashSet<i64> = HashSet::new();
    for m in manifests {
        installed_set.insert(m.appid);
        name_map.insert(m.appid, m.name);
    }

    let mut playtimes = get_all_playtimes()?;
    if installed_only {
        playtimes.retain(|p| installed_set.contains(&p.appid));
    }

    if do_resolve_names && !installed_only {
        let mut unknown: Vec<_> = playtimes.iter().filter(|p| !name_map.contains_key(&p.appid)).collect();
        unknown.sort_by_key(|p| Reverse(p.playtime));
        let unknown_appids: Vec<i64> = unknown[..slice_end(unknown.len(), limit)].iter().map(|p| p.appid).collect();
        name_map.extend(resolve_names(&unknown_appids).await);
    }

    let mut games: Vec<HistoryEntry> = playtimes
        .iter()
        .map(|p| HistoryEntry {
            appid: p.appid,
            name: name_map.get(&p.appid).cloned().unwrap_or_else(|| format!("Unknown ({})", p.appid)),
            playtime_minutes: p.playtime,
            playtime: format_playtime(p.playtime as f64),
            last_played_timestamp: p.last_played,
            last_played: format_timestamp(p.last_played),
            installed: installed_set.contains(&p.appid),
        })
        .collect();

    match sort_by.as_str() {
        "playtime" => games.sort_by_key(|g| Reverse(g.playtime_minutes)),
        "last_played" => games.sort_by_key(|g| Reverse(g.last_played_timestamp)),
        "name" => games.sort_by(|a, b| locale_cmp(&a.name, &b.name)),
        _ => {}
    }

    let total_playtime_minutes: i64 = games.iter().map(|g| g.playtime_minutes).sum();
    let two_weeks_ago = now_seconds() - TWO_WEEKS_SECONDS;
    let recently_played = games.iter().filter(|g| g.last_played_timestamp > two_weeks_ago).count();

    let most_played = games.iter().reduce(|a, b| if a.playtime_minutes > b.playtime_minutes { a } else { b }).map_or(
        Value::Null,
        |top| json!({ "name": top.name, "playtime": format_playtime(top.playtime_minutes as f64) }),
    );

    let limited: Vec<Value> = games[..slice_end(games.len(), limit)]
        .iter()
        .map(|g| {
            json!({
                "appid": g.appid,
                "name": g.name,
                "playtime": g.playtime,
                "lastPlayed": g.last_played,
                "installed": g.installed,
            })
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "totalPlaytime": format_playtime(total_playtime_minutes as f64),
        "totalPlaytimeMinutes": total_playtime_minutes,
        "totalGames": games.len(),
        "recentlyPlayedCount": recently_played,
        "mostPlayed": most_played,
        "games": limited,
    })))
}

struct ExportRow {
    appid: i64,
    name: String,
    playtime_forever: f64,
    playtime_formatted: String,
    last_played: String,
    achievements_unlocked: Option<i64>,
    achievements_total: Option<i64>,
    completion_pct: Option<String>,
}

impl ExportRow {
    fn to_json(&self) -> Value {
        json!({
            "appid": self.appid,
            "name": self.name,
            "playtime_forever": num(self.playtime_forever),
            "playtime_formatted": self.playtime_formatted,
            "last_played": self.last_played,
            "achievements_unlocked": self.achievements_unlocked,
            "achievements_total": self.achievements_total,
            "completion_pct": self.completion_pct,
        })
    }

    fn to_csv(&self) -> String {
        [
            self.appid.to_string(),
            escape_csv_field(&self.name),
            js_num(self.playtime_forever),
            escape_csv_field(&self.playtime_formatted),
            escape_csv_field(&self.last_played),
            self.achievements_unlocked.map(|n| n.to_string()).unwrap_or_default(),
            self.achievements_total.map(|n| n.to_string()).unwrap_or_default(),
            self.completion_pct.clone().unwrap_or_default(),
        ]
        .join(",")
    }
}

async fn fetch_achievements(appid: i64, steamid: &str) -> Option<(i64, i64)> {
    let data = fetch_achievement_list(appid, steamid).await?;
    let achievements = data
        .get("playerstats")
        .and_then(|s| s.get("achievements"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if achievements.is_empty() {
        return None;
    }
    Some((count_unlocked(achievements), achievements.len() as i64))
}

/// The export's file name: the one given, ending in the format's extension,
/// or a timestamped default.
fn export_name(given: Option<String>, format: &str) -> String {
    match given {
        Some(name) if Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case(format)) => name,
        Some(name) => format!("{name}.{format}"),
        None => format!("steam-library-{}.{format}", output::timestamp()),
    }
}

async fn export(args: &Args) -> Result<ToolResult> {
    let format = args.str("format").unwrap_or_else(|| "json".into());
    let include_playtime = args.bool("include_playtime").unwrap_or(true);
    let include_achievements = args.bool("include_achievements").unwrap_or(false);
    let name = export_name(args.str("output").filter(|s| !s.is_empty()), &format);
    let dir = output::output_dir()?;
    // Fail on a bad or taken name before the slow API calls, not after.
    output::check_new_name(&dir, &name)?;

    let steamid = get_user_config()?.steam_id64;
    let games = fetch_owned_games(&steamid).await?;

    let local_map: HashMap<i64, (i64, i64)> = if include_playtime {
        get_all_playtimes()?.into_iter().map(|p| (p.appid, (p.playtime, p.last_played))).collect()
    } else {
        HashMap::new()
    };

    let mut rows: Vec<ExportRow> = games
        .into_iter()
        .map(|g| {
            let local = local_map.get(&g.appid);
            let playtime = g
                .playtime_forever
                .filter(|&p| p != 0.0)
                .or_else(|| local.map(|l| l.0 as f64).filter(|&p| p != 0.0))
                .unwrap_or(0.0);
            let last_played = local.map_or(0, |l| l.1);
            ExportRow {
                appid: g.appid,
                name: g.name,
                playtime_forever: playtime,
                playtime_formatted: format_playtime(playtime),
                last_played: if last_played > 0 { format_timestamp(last_played) } else { "Never".into() },
                achievements_unlocked: None,
                achievements_total: None,
                completion_pct: None,
            }
        })
        .collect();

    if include_achievements {
        for batch in rows.chunks_mut(10) {
            let results = join_all(batch.iter().map(|row| fetch_achievements(row.appid, &steamid))).await;
            for (row, result) in batch.iter_mut().zip(results) {
                if let Some((unlocked, total)) = result {
                    row.achievements_unlocked = Some(unlocked);
                    row.achievements_total = Some(total);
                    row.completion_pct = Some(if total > 0 {
                        to_fixed(unlocked as f64 / total as f64 * 100.0, 1) + "%"
                    } else {
                        "0.0%".into()
                    });
                }
            }
        }
    }

    js_sort(&mut rows, |a, b| b.playtime_forever - a.playtime_forever);

    let content = if format == "json" {
        serde_json::to_string_pretty(&Value::Array(rows.iter().map(ExportRow::to_json).collect()))?
    } else {
        let header = "appid,name,playtime_forever,playtime_formatted,last_played,achievements_unlocked,achievements_total,completion_pct";
        std::iter::once(header.to_string()).chain(rows.iter().map(ExportRow::to_csv)).collect::<Vec<_>>().join("\n")
    };
    let (path, mut file) = output::create_file(&dir, &name)?;
    if let Err(e) = file.write_all(content.as_bytes()) {
        // Don't leave a partial file holding the name.
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(io_error(&e, "write", &path));
    }

    Ok(ToolResult::json(&json!({
        "path": path.to_string_lossy(),
        "format": format,
        "game_count": rows.len(),
        "include_playtime": include_playtime,
        "include_achievements": include_achievements,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_end_matches_js() {
        assert_eq!(slice_end(5, 20.0), 5);
        assert_eq!(slice_end(5, 2.7), 2);
        assert_eq!(slice_end(5, -1.0), 4);
        assert_eq!(slice_end(5, -10.0), 0);
        assert_eq!(slice_end(5, 0.0), 0);
    }

    #[test]
    fn to_fixed_rounds_ties_to_even() {
        for (x, s) in [
            (12.25, "12.2"),
            (12.5, "12.5"),
            (99.95, "100.0"),
            (9.95, "9.9"),
            (100.0, "100.0"),
            (33.333, "33.3"),
            (0.05, "0.1"),
            (0.25, "0.2"),
        ] {
            assert_eq!(to_fixed(x, 1), s, "{x}");
        }
        assert_eq!(to_fixed(99.5, 0), "100");
    }

    #[test]
    fn export_names_end_in_the_format() {
        assert_eq!(export_name(Some("lib".into()), "json"), "lib.json");
        assert_eq!(export_name(Some("lib.JSON".into()), "json"), "lib.JSON");
        assert_eq!(export_name(Some("lib.csv".into()), "json"), "lib.csv.json");
        let default = export_name(None, "csv");
        assert!(default.starts_with("steam-library-") && default.ends_with(".csv"), "{default}");
    }

    #[test]
    fn csv_escaping() {
        assert_eq!(escape_csv_field("a,b"), "\"a,b\"");
        assert_eq!(escape_csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(escape_csv_field("plain"), "plain");
        assert_eq!(escape_csv_field("=HYPERLINK(\"x\")"), "\"'=HYPERLINK(\"\"x\"\")\"");
        assert_eq!(escape_csv_field("-5"), "'-5");
        assert_eq!(escape_csv_field("Safe\r=1+1"), "\"Safe\r=1+1\"");
        assert_eq!(escape_csv_field("\r=1"), "\"'\r=1\"");
        assert_eq!(escape_csv_field("\n=1"), "\"'\n=1\"");
    }
}
