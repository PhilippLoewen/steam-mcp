//! player: profile, level, bans, friends, achievements, owned and recent games.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use futures::future::try_join_all;
use serde_json::{Map, Value, json};

use crate::steam::api::{has_api_key, persona_state, steam_api_request};
use crate::steam::paths::{get_user_config, get_user_data_dir};
use crate::steam::userdata::get_local_config;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{
    format_playtime, format_timestamp, js_num, js_sort, locale_cmp, now_millis, num, read_dir_names, read_text,
    to_fixed,
};
use crate::vdf::{VdfExt, VdfObject, VdfValue, js_entries, parse_vdf};

const ACTIONS: &[&str] =
    &["summary", "level", "bans", "friends", "achievements", "global_achievements", "schema", "owned", "recent"];

pub fn tool() -> Tool {
    Tool {
        name: "player",
        description: "Profile, level, bans, friends, achievements, owned/recent games",
        params: vec![
            Param::new("action", ParamType::Enum(ACTIONS)).required(),
            Param::new("steamid", ParamType::String),
            Param::new("appid", ParamType::Number),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[],
        destructive: false,
        open_world: true,
    }
}

async fn handle(args: Args) -> ToolResult {
    let steamid = args.str("steamid");
    let appid = args.f64("appid");
    match args.str("action").as_deref() {
        Some("summary") => ToolResult::or_error(summary(steamid).await, "Error fetching player summary"),
        Some("level") => ToolResult::or_error(level(steamid).await, "Error fetching player level"),
        Some("bans") => ToolResult::or_error(bans(steamid).await, "Error fetching player bans"),
        Some("friends") => ToolResult::or_error(friends(steamid).await, "Error fetching friend list"),
        Some("achievements") => achievements(steamid, appid).await,
        Some("global_achievements") => global_achievements(appid).await,
        Some("schema") => schema(appid).await,
        Some("owned") => ToolResult::or_error(owned(steamid).await, "Error fetching owned games"),
        _ => ToolResult::or_error(recent(steamid).await, "Error fetching recently played games"),
    }
}

// ---------------------------------------------------------------------------
// JS value semantics over untyped API responses
// ---------------------------------------------------------------------------

/// `obj?.key`, with null treated as absent (for `??` chains).
fn get<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    v.and_then(|v| v.get(key)).filter(|v| !v.is_null())
}

/// Insert `value` unless it is undefined, as JSON.stringify skips undefined.
fn put(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        map.insert(key.into(), v.clone());
    }
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

/// A value as a JS template string renders it.
fn js_str(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => js_num(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => {
            a.iter().map(|e| if e.is_null() { String::new() } else { js_str(Some(e)) }).collect::<Vec<_>>().join(",")
        }
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JS `Number(v)`; undefined, objects and garbage become NaN.
fn js_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::String(s)) => {
            let t = s.trim();
            match t {
                "" => 0.0,
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ if t.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) => t.parse().unwrap_or(f64::NAN),
                _ => f64::NAN,
            }
        }
        _ => f64::NAN,
    }
}

/// `v === n` for a number literal.
fn is_number(v: Option<&Value>, n: f64) -> bool {
    v.and_then(Value::as_f64) == Some(n)
}

fn array(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

/// formatTimestamp on an API value; NaN reaches `toISOString`, which throws.
fn timestamp(v: Option<&Value>) -> Result<String> {
    let n = js_number(v);
    if n.is_nan() {
        return Err(anyhow!("Invalid time value"));
    }
    Ok(format_timestamp(n as i64))
}

/// formatPlaytime on an API value.
fn playtime(v: Option<&Value>) -> String {
    if v.is_some_and(Value::is_null) {
        return "null minutes".into();
    }
    let n = js_number(v);
    if n.is_nan() { "NaN hours".into() } else { format_playtime(n) }
}

/// `PERSONA_STATES[state] ?? \`Unknown (${state})\``.
fn online_status(state: Option<&Value>) -> String {
    state
        .and_then(Value::as_f64)
        .filter(|f| f.fract() == 0.0)
        .and_then(|f| persona_state(f as i64))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Unknown ({})", js_str(state)))
}

/// The TypeError Node throws for `x.key` when `x` is undefined or null.
fn read_error(v: Option<&Value>, key: &str) -> anyhow::Error {
    let what = if v.is_some() { "null" } else { "undefined" };
    anyhow!("Cannot read properties of {what} (reading '{key}')")
}

/// `data.response`, failing like Node when the next property read hits undefined.
fn response<'a>(data: &'a Value, next: &str) -> Result<&'a Value> {
    match data.get("response") {
        Some(r) if !r.is_null() => Ok(r),
        other => Err(read_error(other, next)),
    }
}

fn default_steamid(steamid: Option<String>) -> Result<String> {
    match steamid {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Ok(get_user_config()?.steam_id64),
    }
}

fn steamid_or_user(steamid: Option<String>) -> Result<String> {
    match steamid {
        Some(s) => Ok(s),
        None => Ok(get_user_config()?.steam_id64),
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

async fn summary(steamid: Option<String>) -> Result<ToolResult> {
    let steamid = default_steamid(steamid)?;
    let data = steam_api_request("ISteamUser", "GetPlayerSummaries", "v2", &[("steamids", steamid.clone())]).await?;

    let players = array(get(data.get("response"), "players"));
    let Some(player) = players.first() else {
        return Ok(ToolResult::error(format!("No player found for Steam ID {steamid}")));
    };
    let field = |k: &str| player.get(k);

    let created = field("timecreated");
    let account_created = if truthy(created) { timestamp(created)? } else { "Unknown".into() };
    let account_age = if truthy(created) {
        let now = now_millis() as f64 / 1000.0;
        let days = ((now - js_number(created)) / 86400.0).floor();
        format!("{} years ({} days)", to_fixed(days / 365.25, 1), js_num(days))
    } else {
        "Unknown".into()
    };
    let last_logoff = if truthy(field("lastlogoff")) { timestamp(field("lastlogoff"))? } else { "Unknown".into() };

    let mut out = Map::new();
    put(&mut out, "steamid", field("steamid"));
    put(&mut out, "personaName", field("personaname"));
    put(&mut out, "profileUrl", field("profileurl"));
    put(&mut out, "avatar", field("avatar"));
    put(&mut out, "avatarFull", field("avatarfull"));
    out.insert("onlineStatus".into(), online_status(field("personastate")).into());
    out.insert("lastLogoff".into(), last_logoff.into());
    out.insert("accountCreated".into(), account_created.into());
    out.insert("accountAge".into(), account_age.into());
    if truthy(field("gameextrainfo")) {
        put(&mut out, "currentlyPlaying", field("gameextrainfo"));
        out.insert("currentGameId".into(), get(Some(player), "gameid").cloned().unwrap_or(Value::Null));
    }
    if truthy(field("loccountrycode")) {
        put(&mut out, "country", field("loccountrycode"));
    }
    Ok(ToolResult::json(&Value::Object(out)))
}

async fn level(steamid: Option<String>) -> Result<ToolResult> {
    let steamid = default_steamid(steamid)?;
    let params = [("steamid", steamid.clone())];
    let (level_data, badges_data) = futures::try_join!(
        steam_api_request("IPlayerService", "GetSteamLevel", "v1", &params),
        steam_api_request("IPlayerService", "GetBadges", "v1", &params),
    )?;

    let level = get(level_data.get("response"), "player_level").cloned().unwrap_or(json!(0));
    let resp = badges_data.get("response");
    let badges = array(get(resp, "badges"));
    let stat = |k: &str| get(resp, k).map_or(0.0, |v| js_number(Some(v)));
    let player_xp = stat("player_xp");
    let xp_current = stat("player_xp_needed_current_level");
    let xp_next = stat("player_xp_needed_next_level");

    let total_badge_xp: f64 =
        badges.iter().map(|b| if truthy(b.get("xp")) { js_number(b.get("xp")) } else { 0.0 }).sum();
    let xp_to_next = xp_next - player_xp;
    let progress = if xp_next > xp_current {
        format!("{}%", to_fixed((player_xp - xp_current) / (xp_next - xp_current) * 100.0, 1))
    } else {
        "100%".into()
    };

    Ok(ToolResult::json(&json!({
        "steamid": steamid,
        "level": level,
        "currentXp": num(player_xp),
        "xpNeededForCurrentLevel": num(xp_current),
        "xpNeededForNextLevel": num(xp_next),
        "xpToNextLevel": num(if xp_to_next > 0.0 { xp_to_next } else { 0.0 }),
        "xpProgress": progress,
        "badgeCount": badges.len(),
        "totalBadgeXp": num(total_badge_xp),
    })))
}

async fn bans(steamid: Option<String>) -> Result<ToolResult> {
    let steamid = default_steamid(steamid)?;
    let data = steam_api_request("ISteamUser", "GetPlayerBans", "v1", &[("steamids", steamid.clone())]).await?;

    let players = array(data.get("players"));
    let Some(ban) = players.first() else {
        return Ok(ToolResult::error(format!("No ban data found for Steam ID {steamid}")));
    };
    let field = |k: &str| ban.get(k);
    let banned =
        truthy(field("VACBanned")) || truthy(field("CommunityBanned")) || js_number(field("NumberOfGameBans")) > 0.0;

    let mut out = Map::new();
    put(&mut out, "steamid", field("SteamId"));
    put(&mut out, "communityBanned", field("CommunityBanned"));
    put(&mut out, "vacBanned", field("VACBanned"));
    put(&mut out, "numberOfVacBans", field("NumberOfVACBans"));
    put(&mut out, "daysSinceLastBan", field("DaysSinceLastBan"));
    put(&mut out, "numberOfGameBans", field("NumberOfGameBans"));
    put(&mut out, "economyBan", field("EconomyBan"));
    out.insert(
        "summary".into(),
        if banned { "This account has bans on record." } else { "This account is in good standing." }.into(),
    );
    Ok(ToolResult::json(&Value::Object(out)))
}

/// GetPlayerSummaries in batches of 100, keyed by steamid.
async fn batch_fetch_summaries(steamids: &[String]) -> Result<HashMap<String, Value>> {
    let params: Vec<[(&str, String); 1]> = steamids.chunks(100).map(|batch| [("steamids", batch.join(","))]).collect();
    let requests = params.iter().map(|p| steam_api_request("ISteamUser", "GetPlayerSummaries", "v2", p));
    let results = try_join_all(requests).await?;
    let mut map = HashMap::new();
    for result in &results {
        for player in array(get(result.get("response"), "players")) {
            if let Some(id) = player.get("steamid").and_then(Value::as_str) {
                map.insert(id.to_string(), player);
            }
        }
    }
    Ok(map)
}

/// Playing > online > offline > no summary.
fn sort_priority(summary: Option<&Value>) -> u8 {
    let Some(s) = summary else { return 3 };
    if truthy(s.get("gameextrainfo")) {
        0
    } else if js_number(s.get("personastate")) >= 1.0 {
        1
    } else {
        2
    }
}

async fn friends(steamid: Option<String>) -> Result<ToolResult> {
    if has_api_key() {
        return friends_api(steamid).await;
    }

    let config = get_local_config()?;
    let Some(root) = config.obj_of(&["UserLocalConfigStore", "userlocalconfigstore"]) else {
        return Ok(ToolResult::error("Could not find UserLocalConfigStore in localconfig.vdf"));
    };
    let Some(friends) = root.obj_of(&["friends", "Friends"]) else {
        return Ok(ToolResult::text(
            "No friends section found in localconfig.vdf. Friends data may not be cached locally.",
        ));
    };

    let list: Vec<Value> = js_entries(friends)
        .into_iter()
        .filter(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|(key, value)| {
            let friend = value.as_obj()?;
            let name = friend.str_of(&["name", "Name"]).unwrap_or("");
            let name = if name.is_empty() { format!("Unknown ({key})") } else { name.to_string() };
            Some(json!({ "steamId": key, "name": name }))
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "source": "local",
        "count": list.len(),
        "note": "This is locally cached data only — online/offline status is not available without a Steam API key. Set the STEAM_API_KEY environment variable for live status.",
        "friends": list,
    })))
}

async fn friends_api(steamid: Option<String>) -> Result<ToolResult> {
    let steamid = default_steamid(steamid)?;
    let data = steam_api_request(
        "ISteamUser",
        "GetFriendList",
        "v1",
        &[("steamid", steamid.clone()), ("relationship", "friend".into())],
    )
    .await?;

    let friends = array(get(data.get("friendslist"), "friends"));
    if friends.is_empty() {
        return Ok(ToolResult::text(format!("No friends found for Steam ID {steamid} (the profile may be private).")));
    }

    let ids: Vec<String> = friends.iter().map(|f| js_str(f.get("steamid"))).collect();
    let summaries = batch_fetch_summaries(&ids).await?;

    let mut enriched = Vec::with_capacity(friends.len());
    for f in &friends {
        let id = f.get("steamid");
        let summary = id.and_then(Value::as_str).and_then(|id| summaries.get(id));
        let name = get(summary, "personaname").cloned().unwrap_or_else(|| format!("Unknown ({})", js_str(id)).into());
        let status = match summary {
            Some(s) => online_status(s.get("personastate")),
            None => "Unknown".into(),
        };

        let mut entry = Map::new();
        put(&mut entry, "steamid", id);
        entry.insert("name".into(), name.clone());
        entry.insert("onlineStatus".into(), status.into());
        entry.insert("friendSince".into(), timestamp(f.get("friend_since"))?.into());
        if truthy(summary.and_then(|s| s.get("gameextrainfo"))) {
            put(&mut entry, "currentlyPlaying", summary.and_then(|s| s.get("gameextrainfo")));
            entry.insert("currentGameId".into(), get(summary, "gameid").cloned().unwrap_or(Value::Null));
        }
        if truthy(summary.and_then(|s| s.get("profileurl"))) {
            put(&mut entry, "profileUrl", summary.and_then(|s| s.get("profileurl")));
        }
        enriched.push((sort_priority(summary), js_str(Some(&name)).to_lowercase(), entry));
    }

    enriched.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| locale_cmp(&a.1, &b.1)));

    let count = |pred: fn(u8) -> bool| enriched.iter().filter(|e| pred(e.0)).count();
    let playing = count(|p| p == 0);
    let online = count(|p| p == 1);
    let offline = count(|p| p >= 2);
    let entries: Vec<Value> = enriched.into_iter().map(|(_, _, e)| Value::Object(e)).collect();

    Ok(ToolResult::json(&json!({
        "source": "api",
        "steamid": steamid,
        "totalFriends": friends.len(),
        "currentlyPlaying": playing,
        "online": online,
        "offline": offline,
        "friends": entries,
    })))
}

async fn achievements(steamid: Option<String>, appid: Option<f64>) -> ToolResult {
    let Some(appid) = appid.filter(|a| *a != 0.0) else {
        return ToolResult::error("appid is required for the achievements action");
    };

    if has_api_key()
        && let Ok(result) = achievements_api(steamid, appid).await
    {
        return result;
    }
    ToolResult::or_error(achievements_local(appid), "Error fetching player achievements")
}

async fn achievements_api(steamid: Option<String>, appid: f64) -> Result<ToolResult> {
    let steamid = steamid_or_user(steamid)?;
    let data = steam_api_request(
        "ISteamUserStats",
        "GetPlayerAchievements",
        "v1",
        &[("steamid", steamid), ("appid", js_num(appid))],
    )
    .await?;

    let stats = data.get("playerstats").filter(|v| !v.is_null()).ok_or_else(|| anyhow!("no playerstats"))?;
    let game_name = stats.get("gameName");
    if !truthy(stats.get("success")) || !truthy(stats.get("achievements")) {
        let name = if truthy(game_name) { js_str(game_name) } else { js_num(appid) };
        return Ok(ToolResult::text(format!("Game \"{name}\" has no achievements or the API returned success=false.")));
    }

    let all = stats.get("achievements").and_then(Value::as_array).ok_or_else(|| anyhow!("not an array"))?;
    let total = all.len();
    let mut unlocked: Vec<&Value> = all.iter().filter(|a| is_number(a.get("achieved"), 1.0)).collect();
    let mut locked: Vec<&Value> = all.iter().filter(|a| is_number(a.get("achieved"), 0.0)).collect();
    let pct = if total > 0 { to_fixed(unlocked.len() as f64 / total as f64 * 100.0, 1) } else { "0.0".into() };

    let display = |a: &Value| get(Some(a), "name").or_else(|| a.get("apiname")).cloned();
    js_sort(&mut unlocked, |a, b| js_number(b.get("unlocktime")) - js_number(a.get("unlocktime")));
    locked.sort_by(|a, b| locale_cmp(&js_str(display(a).as_ref()), &js_str(display(b).as_ref())));

    let mut unlocked_list = Vec::with_capacity(unlocked.len());
    for a in &unlocked {
        let mut e = Map::new();
        put(&mut e, "name", display(a).as_ref());
        put(&mut e, "apiname", a.get("apiname"));
        e.insert("unlockedAt".into(), timestamp(a.get("unlocktime"))?.into());
        unlocked_list.push(Value::Object(e));
    }
    let locked_list: Vec<Value> = locked
        .iter()
        .map(|a| {
            let mut e = Map::new();
            put(&mut e, "name", display(a).as_ref());
            put(&mut e, "apiname", a.get("apiname"));
            e.insert("description".into(), get(Some(a), "description").cloned().unwrap_or(Value::Null));
            Value::Object(e)
        })
        .collect();

    let mut out = Map::new();
    out.insert("source".into(), "api".into());
    put(&mut out, "gameName", game_name);
    put(&mut out, "steamID", stats.get("steamID"));
    out.insert("completion".into(), format!("{}/{total} ({pct}%)", unlocked.len()).into());
    out.insert("totalAchievements".into(), total.into());
    out.insert("unlockedCount".into(), unlocked.len().into());
    out.insert("lockedCount".into(), locked.len().into());
    out.insert("unlocked".into(), unlocked_list.into());
    out.insert("locked".into(), locked_list.into());
    Ok(ToolResult::json(&Value::Object(out)))
}

/// A parsed VDF object as JSON.stringify writes it (integer-like keys first).
fn vdf_json(obj: &VdfObject) -> Value {
    Value::Object(
        js_entries(obj)
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    VdfValue::Str(s) => Value::String(s.clone()),
                    VdfValue::Obj(o) => vdf_json(o),
                };
                (k.to_string(), v)
            })
            .collect(),
    )
}

/// `fs.readdirSync`, which returns names sorted.
fn readdir_sorted(dir: &std::path::Path) -> Vec<String> {
    let mut names = read_dir_names(dir).unwrap_or_default();
    names.sort();
    names
}

fn achievements_local(appid: f64) -> Result<ToolResult> {
    let app_dir = get_user_data_dir()?.join(js_num(appid));
    if !app_dir.exists() {
        return Ok(ToolResult::text(format!(
            "No userdata directory found for appid {}. The game may not have been played or may not store local data.",
            js_num(appid)
        )));
    }

    let stats_dir = app_dir.join("stats");
    let stats_files = if stats_dir.exists() { readdir_sorted(&stats_dir) } else { Vec::new() };

    let remote_cache_path = app_dir.join("remotecache.vdf");
    let remote_cache = if remote_cache_path.exists() {
        read_text(&remote_cache_path).ok().map(|content| vdf_json(&parse_vdf(&content)))
    } else {
        None
    };

    let mut out = Map::new();
    out.insert("source".into(), "local".into());
    out.insert("appid".into(), num(appid));
    out.insert("userdataPath".into(), app_dir.display().to_string().into());
    out.insert("filesInAppDir".into(), json!(readdir_sorted(&app_dir)));
    if stats_files.is_empty() {
        out.insert("statsDir".into(), Value::Null);
        out.insert("statsNote".into(), "No stats directory found for this game.".into());
    } else {
        out.insert("statsDir".into(), stats_dir.display().to_string().into());
        out.insert("statsFiles".into(), json!(stats_files));
    }
    match remote_cache {
        Some(data) => {
            out.insert("remotecache".into(), data);
        }
        None => {
            out.insert("remotecache".into(), Value::Null);
            out.insert("remotecacheNote".into(), "No remotecache.vdf found for this game.".into());
        }
    }
    Ok(ToolResult::json(&Value::Object(out)))
}

async fn global_achievements(appid: Option<f64>) -> ToolResult {
    let Some(appid) = appid.filter(|a| *a != 0.0) else {
        return ToolResult::error("appid is required for the global_achievements action");
    };
    ToolResult::or_error(global_achievements_run(appid).await, "Error fetching global achievement stats")
}

async fn global_achievements_run(appid: f64) -> Result<ToolResult> {
    let id = js_num(appid);
    let global_params = [("gameid", id.clone())];
    let schema_params = [("appid", id.clone())];
    let (global, schema) = futures::try_join!(
        steam_api_request("ISteamUserStats", "GetGlobalAchievementPercentagesForApp", "v2", &global_params),
        steam_api_request("ISteamUserStats", "GetSchemaForGame", "v2", &schema_params),
    )?;

    let percentages = array(get(get(Some(&global), "achievementpercentages"), "achievements"));
    let game = get(Some(&schema), "game");
    let schema_achievements = array(get(get(game, "availableGameStats"), "achievements"));
    let game_name = get(game, "gameName").cloned().unwrap_or_else(|| format!("App {id}").into());

    let mut by_name: HashMap<String, &Value> = HashMap::new();
    for sa in &schema_achievements {
        by_name.insert(js_str(sa.get("name")), sa);
    }

    let mut merged: Vec<(f64, Value)> = percentages
        .iter()
        .map(|entry| {
            let name = entry.get("name");
            let schema = name.and_then(Value::as_str).and_then(|n| by_name.get(n)).copied();
            let percent = (js_number(entry.get("percent")) * 100.0 + 0.5).floor() / 100.0;
            let mut e = Map::new();
            put(&mut e, "name", name);
            put(&mut e, "displayName", get(schema, "displayName").or(name));
            e.insert("percent".into(), num(percent));
            e.insert("description".into(), get(schema, "description").cloned().unwrap_or(Value::Null));
            (percent, Value::Object(e))
        })
        .collect();
    js_sort(&mut merged, |a, b| b.0 - a.0);

    Ok(ToolResult::json(&json!({
        "gameName": game_name,
        "appid": num(appid),
        "totalAchievements": merged.len(),
        "achievements": merged.into_iter().map(|(_, e)| e).collect::<Vec<_>>(),
    })))
}

async fn schema(appid: Option<f64>) -> ToolResult {
    let Some(appid) = appid.filter(|a| *a != 0.0) else {
        return ToolResult::error("appid is required for the schema action");
    };
    ToolResult::or_error(schema_run(appid).await, "Error fetching game schema")
}

async fn schema_run(appid: f64) -> Result<ToolResult> {
    let data = steam_api_request("ISteamUserStats", "GetSchemaForGame", "v2", &[("appid", js_num(appid))]).await?;

    let game = get(Some(&data), "game");
    let game_name = get(game, "gameName").cloned().unwrap_or_else(|| format!("App {}", js_num(appid)).into());
    let stats = get(game, "availableGameStats");

    let achievements: Vec<Value> = array(get(stats, "achievements"))
        .iter()
        .map(|a| {
            let mut e = Map::new();
            put(&mut e, "name", a.get("name"));
            put(&mut e, "displayName", a.get("displayName"));
            let description = a.get("description").filter(|d| truthy(Some(d))).cloned().unwrap_or(Value::Null);
            e.insert("description".into(), description);
            e.insert("hidden".into(), is_number(a.get("hidden"), 1.0).into());
            Value::Object(e)
        })
        .collect();
    let stat_list: Vec<Value> = array(get(stats, "stats"))
        .iter()
        .map(|s| {
            let mut e = Map::new();
            put(&mut e, "name", s.get("name"));
            put(&mut e, "displayName", s.get("displayName"));
            Value::Object(e)
        })
        .collect();

    Ok(ToolResult::json(&json!({
        "gameName": game_name,
        "appid": num(appid),
        "achievementCount": achievements.len(),
        "statCount": stat_list.len(),
        "achievements": achievements,
        "stats": stat_list,
    })))
}

async fn owned(steamid: Option<String>) -> Result<ToolResult> {
    let steamid = steamid_or_user(steamid)?;
    let data = steam_api_request(
        "IPlayerService",
        "GetOwnedGames",
        "v1",
        &[("steamid", steamid), ("include_appinfo", "1".into()), ("include_played_free_games", "1".into())],
    )
    .await?;

    let resp = response(&data, "games")?;
    let mut games = array(get(Some(resp), "games"));
    js_sort(&mut games, |a, b| js_number(b.get("playtime_forever")) - js_number(a.get("playtime_forever")));

    let list: Vec<Value> = games
        .iter()
        .map(|g| {
            let mut e = Map::new();
            put(&mut e, "appid", g.get("appid"));
            put(&mut e, "name", g.get("name"));
            e.insert("playtime".into(), playtime(g.get("playtime_forever")).into());
            let two_weeks = g.get("playtime_2weeks");
            if truthy(two_weeks) && js_number(two_weeks) > 0.0 {
                e.insert("playtime_2weeks".into(), playtime(two_weeks).into());
            }
            e.insert(
                "icon_url".into(),
                format!(
                    "https://media.steampowered.com/steamcommunity/public/images/apps/{}/{}.jpg",
                    js_str(g.get("appid")),
                    js_str(g.get("img_icon_url"))
                )
                .into(),
            );
            Value::Object(e)
        })
        .collect();

    let mut out = Map::new();
    put(&mut out, "game_count", resp.get("game_count"));
    out.insert("showing".into(), list.len().into());
    out.insert("games".into(), list.into());
    Ok(ToolResult::json(&Value::Object(out)))
}

async fn recent(steamid: Option<String>) -> Result<ToolResult> {
    let steamid = steamid_or_user(steamid)?;
    let data = steam_api_request(
        "IPlayerService",
        "GetRecentlyPlayedGames",
        "v1",
        &[("steamid", steamid), ("count", "10".into())],
    )
    .await?;

    let resp = response(&data, "total_count")?;
    let games: Vec<Value> = array(get(Some(resp), "games"))
        .iter()
        .map(|g| {
            let mut e = Map::new();
            put(&mut e, "appid", g.get("appid"));
            put(&mut e, "name", g.get("name"));
            e.insert("playtime_2weeks".into(), playtime(g.get("playtime_2weeks")).into());
            e.insert("playtime_forever".into(), playtime(g.get("playtime_forever")).into());
            Value::Object(e)
        })
        .collect();

    let mut out = Map::new();
    put(&mut out, "total_count", resp.get("total_count"));
    out.insert("games".into(), games.into());
    Ok(ToolResult::json(&Value::Object(out)))
}
