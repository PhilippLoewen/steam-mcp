//! game_info: reviews, news, media, HowLongToBeat times and PCGamingWiki articles.

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};

use crate::steam::api::HTTP;
use crate::steam::paths::get_user_data_dir;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{iso_from_millis, js_num, num, to_fixed};
use crate::vdf::{is_array_index, parse_int};

const CDN_BASE: &str = "https://cdn.akamai.steamstatic.com/steam/apps";

const GRID_SUFFIXES: &[(&str, &str)] = &[
    ("_hero.jpg", "hero"),
    ("_hero.png", "hero"),
    ("p.jpg", "portrait"),
    ("p.png", "portrait"),
    (".jpg", "grid"),
    (".png", "grid"),
    ("_logo.png", "logo"),
    ("_logo.jpg", "logo"),
    ("_icon.jpg", "icon"),
    ("_icon.png", "icon"),
];

const HLTB_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub fn tool() -> Tool {
    Tool {
        name: "game_info",
        description: "Reviews, news, media, HLTB times, PCGamingWiki fixes",
        params: vec![
            Param::new("action", ParamType::Enum(&["reviews", "news", "media", "hltb", "pcgamingwiki"])).required(),
            Param::new("appid", ParamType::Number),
            Param::new("name", ParamType::String),
            Param::new("count", ParamType::Number),
            Param::new("filter", ParamType::Enum(&["recent", "updated", "all"])),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[],
        destructive: false,
        open_world: true,
    }
}

async fn handle(args: Args) -> ToolResult {
    let action = args.str("action").unwrap_or_default();
    let result = match action.as_str() {
        "reviews" => reviews(&args).await,
        "news" => news(&args).await,
        "media" => Ok(media(&args)),
        "hltb" => hltb(&args).await,
        _ => pcgamingwiki(&args).await,
    };
    ToolResult::or_error(result, &format!("Error in game_info/{action}"))
}

/// The appid when JS would consider it truthy.
fn appid_arg(args: &Args) -> Option<f64> {
    args.f64("appid").filter(|a| *a != 0.0 && !a.is_nan())
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `${v}` in a JS template string.
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

/// `v.key` in JS: throws on undefined and null, undefined for missing keys.
fn read<I>(v: Option<&Value>, key: I) -> Result<Option<&Value>>
where
    I: serde_json::value::Index + std::fmt::Display,
{
    match v {
        None => bail!("Cannot read properties of undefined (reading '{key}')"),
        Some(Value::Null) => bail!("Cannot read properties of null (reading '{key}')"),
        Some(v) => Ok(v.get(key)),
    }
}

/// Copy `src[key]` into `dst[as_key]` unless it is undefined.
fn put(dst: &mut Map<String, Value>, as_key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        dst.insert(as_key.into(), v.clone());
    }
}

fn number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { 0.0 } else { t.parse().unwrap_or(f64::NAN) }
        }
        _ => f64::NAN,
    }
}

/// `formatPlaytime`, keeping JS's NaN behaviour.
fn playtime(minutes: f64) -> String {
    let minutes = if minutes < 0.0 { 0.0 } else { minutes };
    if minutes < 60.0 {
        return format!("{} minutes", js_num(minutes));
    }
    format!("{} hours", fixed(minutes / 60.0, 1))
}

/// `formatTimestamp`, including the RangeError for invalid dates.
fn timestamp(seconds: f64) -> Result<String> {
    if seconds <= 0.0 {
        return Ok("N/A".into());
    }
    let ms = seconds * 1000.0;
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        bail!("Invalid time value");
    }
    Ok(iso_from_millis(ms.trunc() as i64))
}

fn fixed(n: f64, digits: usize) -> String {
    if n.is_infinite() { js_num(n) } else { to_fixed(n, digits) }
}

/// The first `n` UTF-16 code units, like `String.prototype.slice(0, n)`.
fn slice_utf16(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn reviews(args: &Args) -> Result<ToolResult> {
    let Some(appid) = appid_arg(args) else {
        return Ok(ToolResult::error("appid is required for reviews"));
    };
    let filter = args.str("filter").unwrap_or_else(|| "all".into());
    let per_page = args.f64("count").unwrap_or(10.0);

    let url = format!(
        "https://store.steampowered.com/appreviews/{}?json=1&filter={filter}&language=english&num_per_page={}&purchase_type=all",
        js_num(appid),
        js_num(per_page)
    );
    let resp = HTTP.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(ToolResult::error(format!("Steam reviews API returned status {}", resp.status().as_u16())));
    }
    let data: Value = resp.json().await?;
    if !truthy(data.get("success")) {
        return Ok(ToolResult::error(format!("Failed to fetch reviews for appid {}", js_num(appid))));
    }

    let qs = data.get("query_summary");
    let mut summary = Map::new();
    for key in ["review_score_desc", "total_positive", "total_negative", "total_reviews"] {
        put(&mut summary, key, read(qs, key)?);
    }

    let raw = match data.get("reviews") {
        Some(Value::Array(a)) => a.as_slice(),
        _ => &[],
    };
    let mut reviews = Vec::with_capacity(raw.len());
    for r in raw {
        let r = Some(r);
        let mut out = Map::new();
        put(&mut out, "voted_up", read(r, "voted_up")?);
        let review = match read(r, "review")? {
            Some(Value::String(s)) if s.encode_utf16().count() > 500 => {
                Some(Value::String(slice_utf16(s, 500) + "..."))
            }
            v @ (None | Some(Value::Null)) => {
                read(v, "length")?;
                None
            }
            v => v.cloned(),
        };
        put(&mut out, "review", review.as_ref());
        let author = read(r, "author")?;
        out.insert("playtime_at_review".into(), playtime(number(read(author, "playtime_at_review")?)).into());
        out.insert("timestamp".into(), timestamp(number(read(r, "timestamp_created")?))?.into());
        for key in ["votes_up", "votes_funny", "steam_purchase", "written_during_early_access"] {
            put(&mut out, key, read(r, key)?);
        }
        reviews.push(Value::Object(out));
    }

    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "summary": summary,
        "reviews_returned": reviews.len(),
        "reviews": reviews,
    })))
}

async fn news(args: &Args) -> Result<ToolResult> {
    let Some(appid) = appid_arg(args) else {
        return Ok(ToolResult::error("appid is required for news"));
    };
    let count = args.f64("count").unwrap_or(5.0);

    let url = format!(
        "https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid={}&count={}&maxlength=500",
        js_num(appid),
        js_num(count)
    );
    let resp = HTTP.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(ToolResult::error(format!("Steam news API returned status {}", resp.status().as_u16())));
    }
    let data: Value = resp.json().await?;

    let items = match data.get("appnews").and_then(|n| n.get("newsitems")) {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Ok(ToolResult::text(format!("No news found for appid {}", js_num(appid)))),
    };

    let mut news = Vec::with_capacity(items.len());
    for item in items {
        let item = Some(item);
        let author = read(item, "author")?;
        let mut out = Map::new();
        put(&mut out, "title", read(item, "title")?);
        out.insert("date".into(), timestamp(number(read(item, "date")?))?.into());
        out.insert(
            "author".into(),
            if truthy(author) { author.cloned().unwrap_or_default() } else { "Unknown".into() },
        );
        put(&mut out, "url", read(item, "url")?);
        put(&mut out, "feedLabel", read(item, "feedlabel")?);
        put(&mut out, "contents", read(item, "contents")?);
        news.push(Value::Object(out));
    }

    Ok(ToolResult::json(&json!({
        "appid": num(appid),
        "count": news.len(),
        "news": news,
    })))
}

fn media(args: &Args) -> ToolResult {
    let Some(appid) = appid_arg(args) else {
        return ToolResult::error("appid is required for media");
    };
    let id = js_num(appid);

    let mut overrides = Vec::new();
    if let Ok(user_data) = get_user_data_dir() {
        let grid = user_data.join("config").join("grid");
        if grid.exists() {
            for (suffix, label) in GRID_SUFFIXES {
                let file = grid.join(format!("{id}{suffix}"));
                if file.exists() {
                    overrides.push(json!({ "label": label, "path": file.to_string_lossy() }));
                }
            }
        }
    }

    ToolResult::json(&json!({
        "appid": num(appid),
        "cdn": {
            "header": format!("{CDN_BASE}/{id}/header.jpg"),
            "capsule": format!("{CDN_BASE}/{id}/capsule_616x353.jpg"),
            "hero": format!("{CDN_BASE}/{id}/hero_capsule.jpg"),
            "libraryHero": format!("{CDN_BASE}/{id}/library_hero.jpg"),
            "logo": format!("{CDN_BASE}/{id}/logo.png"),
        },
        "localOverrides": if overrides.is_empty() { Value::Null } else { Value::Array(overrides) },
    }))
}

fn seconds_to_hours(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_f64) {
        Some(s) if s > 0.0 => fixed(s / 3600.0, 1).into(),
        _ => Value::Null,
    }
}

async fn hltb(args: &Args) -> Result<ToolResult> {
    let Some(name) = args.str("name").filter(|n| !n.is_empty()) else {
        return Ok(ToolResult::error("name is required for hltb"));
    };

    let search_terms: Vec<&str> = name.split_whitespace().collect();
    let body = json!({
        "searchType": "games",
        "searchTerms": search_terms,
        "searchPage": 1,
        "size": 5,
        "searchOptions": {
            "games": {
                "userId": 0,
                "platform": "",
                "sortCategory": "popular",
                "rangeCategory": "main",
                "rangeTime": { "min": null, "max": null },
                "gameplay": { "perspective": "", "flow": "", "genre": "", "subGenre": "" },
                "rangeYear": { "min": "", "max": "" },
                "modifier": "",
            },
            "users": { "sortCategory": "postcount" },
            "lists": { "sortCategory": "follows" },
            "filter": "",
            "sort": 0,
            "randomizer": 0,
        },
    });

    let resp = HTTP
        .post("https://howlongtobeat.com/api/search")
        .header("Content-Type", "application/json")
        .header("Referer", "https://howlongtobeat.com")
        .header("User-Agent", HLTB_USER_AGENT)
        .body(body.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let error_text = resp.text().await.unwrap_or_default();
        let detail =
            if error_text.is_empty() { String::new() } else { format!("Response: {}", slice_utf16(&error_text, 200)) };
        return Ok(ToolResult::error(format!(
            "HowLongToBeat API returned HTTP {status}. The API may have changed or be temporarily unavailable. \
             You can check manually at https://howlongtobeat.com/?q={}\n{detail}",
            encode_uri_component(&name)
        )));
    }

    let data: Value = resp.json().await?;
    let games = match read(Some(&data), "data")? {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Ok(ToolResult::text(format!("No results found for \"{name}\" on HowLongToBeat."))),
    };

    let mut results = Vec::new();
    for game in games.iter().take(5) {
        let game = Some(game);
        let id = read(game, "game_id")?;
        let image = read(game, "game_image")?;
        let mut out = Map::new();
        put(&mut out, "name", read(game, "game_name")?);
        put(&mut out, "hltb_id", id);
        out.insert("main_story_hours".into(), seconds_to_hours(read(game, "comp_main")?));
        out.insert("main_plus_extras_hours".into(), seconds_to_hours(read(game, "comp_plus")?));
        out.insert("completionist_hours".into(), seconds_to_hours(read(game, "comp_100")?));
        out.insert("all_styles_hours".into(), seconds_to_hours(read(game, "comp_all")?));
        out.insert(
            "image_url".into(),
            if truthy(image) {
                format!("https://howlongtobeat.com/games/{}", js_str(image)).into()
            } else {
                Value::Null
            },
        );
        out.insert("hltb_url".into(), format!("https://howlongtobeat.com/game/{}", js_str(id)).into());
        results.push(Value::Object(out));
    }

    Ok(ToolResult::json(&json!({
        "query": name,
        "result_count": results.len(),
        "results": results,
    })))
}

/// First value of an object in JS `Object.values` order.
fn first_js_value(obj: &Map<String, Value>) -> Option<&Value> {
    obj.iter()
        .filter(|(k, _)| is_array_index(k))
        .min_by_key(|(k, _)| k.parse::<u64>().unwrap_or(0))
        .or_else(|| obj.iter().next())
        .map(|(_, v)| v)
}

async fn fetch_intro(page: &str) -> Result<Option<String>> {
    let url = format!(
        "https://www.pcgamingwiki.com/w/api.php?action=query&titles={}&prop=extracts&exintro=true&explaintext=true&format=json",
        encode_uri_component(page)
    );
    let resp = HTTP.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let data: Value = resp.json().await?;
    let Some(Value::Object(pages)) = data.get("query").and_then(|q| q.get("pages")) else { return Ok(None) };
    Ok(first_js_value(pages)
        .and_then(|p| p.get("extract"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

async fn fetch_sections(page: &str) -> Result<Vec<Value>> {
    let url = format!(
        "https://www.pcgamingwiki.com/w/api.php?action=parse&page={}&prop=sections&format=json",
        encode_uri_component(page)
    );
    let resp = HTTP.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let data: Value = resp.json().await?;
    Ok(match data.get("parse").and_then(|p| p.get("sections")) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    })
}

fn section_level(v: Option<&Value>) -> Value {
    let text = match v {
        Some(Value::String(s)) => s.clone(),
        other => js_str(other),
    };
    parse_int(&text).map_or(Value::Null, Value::from)
}

async fn pcgamingwiki(args: &Args) -> Result<ToolResult> {
    let appid = appid_arg(args);
    let name = args.str("name").filter(|n| !n.is_empty());
    if appid.is_none() && name.is_none() {
        return Ok(ToolResult::error("Please provide either an appid or a game name to search."));
    }

    let mut page_name: Option<String> = None;

    if let Some(appid) = appid {
        let url = format!(
            "https://www.pcgamingwiki.com/w/api.php?action=cargoquery&tables=Infobox_game\
             &fields=Infobox_game._pageName=Page,Infobox_game.Steam_AppID\
             &where=Infobox_game.Steam_AppID={}&format=json",
            js_num(appid)
        );
        let resp = HTTP.get(url).send().await?;
        if !resp.status().is_success() {
            return Ok(ToolResult::error(format!(
                "PCGamingWiki API returned status {} when searching by appid.",
                resp.status().as_u16()
            )));
        }
        let data: Value = resp.json().await?;
        if let Some(Value::Array(rows)) = data.get("cargoquery")
            && let Some(first) = rows.first()
        {
            let page = read(read(Some(first), "title")?, "Page")?;
            page_name = page.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        }
    }

    if page_name.is_none()
        && let Some(name) = &name
    {
        let url = format!(
            "https://www.pcgamingwiki.com/w/api.php?action=opensearch&search={}&limit=5&format=json",
            encode_uri_component(name)
        );
        let resp = HTTP.get(url).send().await?;
        if !resp.status().is_success() {
            return Ok(ToolResult::error(format!(
                "PCGamingWiki API returned status {} when searching by name.",
                resp.status().as_u16()
            )));
        }
        let data: Value = resp.json().await?;
        if let Some(Value::Array(titles)) = read(Some(&data), 1)? {
            page_name = titles.first().and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        }
    }

    let Some(page_name) = page_name else {
        return Ok(ToolResult::text(match appid {
            Some(id) => format!("No PCGamingWiki article found for appid {}.", js_num(id)),
            None => format!("No PCGamingWiki article found for \"{}\".", name.unwrap_or_default()),
        }));
    };

    let intro = fetch_intro(&page_name).await.ok().flatten();
    let raw_sections = fetch_sections(&page_name).await.unwrap_or_default();

    let mut sections = Vec::with_capacity(raw_sections.len());
    for s in &raw_sections {
        let s = Some(s);
        let mut out = Map::new();
        put(&mut out, "index", read(s, "index")?);
        put(&mut out, "title", read(s, "line")?);
        out.insert("level".into(), section_level(read(s, "level")?));
        sections.push(Value::Object(out));
    }

    let wiki_url = format!("https://www.pcgamingwiki.com/wiki/{}", encode_uri_component(&page_name.replace(' ', "_")));

    Ok(ToolResult::json(&json!({
        "gameName": page_name,
        "pcgamingwiki_url": wiki_url,
        "intro": intro.unwrap_or_else(|| "(no intro text available)".into()),
        "sections": sections,
    })))
}
