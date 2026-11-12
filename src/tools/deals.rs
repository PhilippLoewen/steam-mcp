//! deals: sale checks, wishlist, wishlist deals and library value estimates.

use std::cmp::Ordering;

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Map, Value, json};

use crate::steam::api::{HTTP, steam_api_request};
use crate::steam::paths::get_user_config;
use crate::steam::wishlist::fetch_all_wishlist_pages;
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{js_num, js_sort, locale_cmp, to_fixed};

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub fn tool() -> Tool {
    Tool {
        name: "deals",
        description: "Sale checks, wishlist, deals, library value",
        params: vec![
            Param::new("action", ParamType::Enum(&["check_sale", "wishlist_deals", "wishlist", "library_value"]))
                .required(),
            Param::new("appid", ParamType::Number),
            Param::new("name", ParamType::String),
            Param::new("steamid", ParamType::SteamId),
        ],
        handler: |args| boxed(handle(args)),
        writes: &[],
        destructive: false,
        open_world: true,
    }
}

async fn handle(args: Args) -> ToolResult {
    match args.str("action").as_deref() {
        Some("check_sale") => ToolResult::or_error(check_sale(&args).await, "Error checking sale"),
        Some("wishlist_deals") => ToolResult::or_error(wishlist_deals(&args).await, "Error fetching wishlist deals"),
        Some("wishlist") => ToolResult::or_error(wishlist(&args).await, "Error fetching wishlist"),
        _ => ToolResult::or_error(library_value(&args).await, "Error estimating library value"),
    }
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

/// JS `a || b`.
fn js_or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    if truthy(a) { a } else { b }
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

/// JS `Number(v)` for JSON values.
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

/// `v.key` in JS: throws on undefined and null, undefined for missing keys.
fn read<'a>(v: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>> {
    match v {
        None => bail!("Cannot read properties of undefined (reading '{key}')"),
        Some(Value::Null) => bail!("Cannot read properties of null (reading '{key}')"),
        Some(v) => Ok(v.get(key)),
    }
}

/// Insert unless undefined, as JSON.stringify drops undefined values.
fn put(dst: &mut Map<String, Value>, key: &str, v: Option<&Value>) {
    if let Some(v) = v {
        dst.insert(key.into(), v.clone());
    }
}

fn fixed(n: f64, digits: usize) -> String {
    if n.is_infinite() { js_num(n) } else { to_fixed(n, digits) }
}

/// `formatPlaytime`, keeping JS's NaN behaviour.
fn playtime(minutes: f64) -> String {
    let minutes = if minutes < 0.0 { 0.0 } else { minutes };
    if minutes < 60.0 {
        return format!("{} minutes", js_num(minutes));
    }
    format!("{} hours", fixed(minutes / 60.0, 1))
}

/// JS `parseFloat` for the "12.34 USD" strings built here.
fn parse_float(s: &str) -> f64 {
    s.split(' ').next().and_then(|n| n.parse().ok()).unwrap_or(f64::NAN)
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

async fn search_store_by_name(name: &str) -> Result<Option<Value>> {
    let url =
        format!("https://store.steampowered.com/api/storesearch/?term={}&l=english&cc=US", encode_uri_component(name));
    let resp = HTTP.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let data: Value = resp.json().await?;
    Ok(match read(Some(&data), "items")? {
        Some(Value::Array(items)) => items.first().filter(|i| truthy(Some(i))).cloned(),
        _ => None,
    })
}

async fn check_sale(args: &Args) -> Result<ToolResult> {
    let (appid, mut game_name) = match args.value("appid") {
        Some(id) => (Some(id.clone()), None),
        None => {
            let Some(name) = args.str("name").filter(|n| !n.is_empty()) else {
                return Ok(ToolResult::error("Please provide either an appid or a game name to search for."));
            };
            let Some(item) = search_store_by_name(&name).await? else {
                return Ok(ToolResult::error(format!("No games found matching \"{name}\" on the Steam store.")));
            };
            (item.get("id").cloned(), item.get("name").cloned())
        }
    };
    let id = js_str(appid.as_ref());

    let (price_resp, basic_resp) = futures::try_join!(
        HTTP.get(format!("https://store.steampowered.com/api/appdetails?appids={id}&filters=price_overview")).send(),
        HTTP.get(format!("https://store.steampowered.com/api/appdetails?appids={id}&filters=basic")).send(),
    )?;
    if !price_resp.status().is_success() || !basic_resp.status().is_success() {
        return Ok(ToolResult::error(format!(
            "Steam store API returned an error (price: {}, basic: {})",
            price_resp.status().as_u16(),
            basic_resp.status().as_u16()
        )));
    }
    let price_data: Value = price_resp.json().await?;
    let basic_data: Value = basic_resp.json().await?;

    let price_entry = read(Some(&price_data), &id)?;
    let basic_entry = read(Some(&basic_data), &id)?;
    let basic_success = basic_entry.and_then(|e| e.get("success"));

    if !truthy(game_name.as_ref()) && truthy(basic_success) {
        game_name = read(read(basic_entry, "data")?, "name")?.cloned();
    }

    if !truthy(price_entry) || !truthy(price_entry.and_then(|e| e.get("success"))) {
        return Ok(ToolResult::error(format!(
            "Could not fetch price data for appid {id}. The app may not exist or the store page may be restricted."
        )));
    }

    let price_overview = read(price_entry.and_then(|e| e.get("data")), "price_overview")?;
    let is_free = truthy(basic_success) && truthy(read(read(basic_entry, "data")?, "is_free")?);

    let name = match game_name {
        Some(n) if !n.is_null() => n,
        _ => format!("App {id}").into(),
    };
    let mut output = Map::new();
    output.insert("name".into(), name);
    put(&mut output, "appid", appid.as_ref());

    if !truthy(price_overview) {
        output.insert("on_sale".into(), false.into());
        output.insert(
            "note".into(),
            if is_free {
                "This game is free to play."
            } else {
                "No pricing information available. The game may be free, delisted, or not sold individually."
            }
            .into(),
        );
        return Ok(ToolResult::json(&Value::Object(output)));
    }

    let field = |key: &str| price_overview.and_then(|p| p.get(key));
    let initial = number(field("initial"));
    let fin = number(field("final"));
    let discount = field("discount_percent");
    let on_sale = number(discount) > 0.0;
    let currency = js_str(field("currency"));

    output.insert("on_sale".into(), on_sale.into());
    put(&mut output, "discount_percent", discount);
    output.insert("original_price".into(), format!("{} {currency}", fixed(initial / 100.0, 2)).into());
    output.insert("current_price".into(), format!("{} {currency}", fixed(fin / 100.0, 2)).into());
    output.insert(
        "savings".into(),
        if on_sale { format!("{} {currency}", fixed((initial - fin) / 100.0, 2)) } else { "0.00".into() }.into(),
    );
    Ok(ToolResult::json(&Value::Object(output)))
}

fn steamid_arg(args: &Args) -> Result<String> {
    match args.str("steamid") {
        Some(id) => Ok(id),
        None => Ok(get_user_config()?.steam_id64),
    }
}

fn empty_wishlist(steamid: &str) -> ToolResult {
    ToolResult::text(format!("No wishlist data found for Steam ID {steamid}. The wishlist may be private or empty."))
}

async fn wishlist_deals(args: &Args) -> Result<ToolResult> {
    let steamid = steamid_arg(args)?;
    let all_items = fetch_all_wishlist_pages(&steamid).await?;
    if all_items.is_empty() {
        return Ok(empty_wishlist(&steamid));
    }

    let mut deals = Vec::new();
    for item in &all_items {
        if truthy(item.get("free")) || truthy(item.get("is_free_game")) {
            continue;
        }
        let Some(Value::Array(subs)) = item.get("subs") else { continue };
        let mut discounted = Vec::new();
        for sub in subs {
            if number(read(Some(sub), "discount_pct")?) > 0.0 {
                discounted.push(sub);
            }
        }
        js_sort(&mut discounted, |a, b| number(b.get("discount_pct")) - number(a.get("discount_pct")));
        let Some(best) = discounted.first() else { continue };

        let mut deal = Map::new();
        put(&mut deal, "appid", item.get("appid"));
        put(&mut deal, "name", item.get("name"));
        put(&mut deal, "discount_percent", best.get("discount_pct"));
        put(&mut deal, "price", best.get("price"));
        put(&mut deal, "review_desc", item.get("review_desc"));
        deals.push(deal);
    }
    js_sort(&mut deals, |a, b| number(b.get("discount_percent")) - number(a.get("discount_percent")));

    Ok(ToolResult::json(&json!({
        "steamid": steamid,
        "total_wishlist_items": all_items.len(),
        "deals_found": deals.len(),
        "deals": deals,
    })))
}

async fn wishlist(args: &Args) -> Result<ToolResult> {
    let steamid = steamid_arg(args)?;
    let all_items = fetch_all_wishlist_pages(&steamid).await?;
    if all_items.is_empty() {
        return Ok(empty_wishlist(&steamid));
    }

    let mut items = Vec::with_capacity(all_items.len());
    for item in &all_items {
        let free = js_or(item.get("free"), item.get("is_free_game"));
        let price = if truthy(free) {
            "Free".into()
        } else if let Some(Value::Array(subs)) = item.get("subs")
            && let Some(sub) = subs.first()
        {
            let price = read(Some(sub), "price")?;
            let pct = read(Some(sub), "discount_pct")?;
            json!({
                "price": js_or(price, None).cloned().unwrap_or_else(|| "N/A".into()),
                "discount_pct": js_or(pct, None).cloned().unwrap_or_else(|| 0.into()),
            })
        } else {
            "N/A".into()
        };

        let mut out = Map::new();
        put(&mut out, "appid", item.get("appid"));
        put(&mut out, "name", item.get("name"));
        put(&mut out, "priority", item.get("priority"));
        put(&mut out, "review_desc", item.get("review_desc"));
        put(&mut out, "release_date", item.get("release_string"));
        put(&mut out, "free", free);
        out.insert("price".into(), price);
        put(&mut out, "type", item.get("type"));
        items.push(out);
    }

    let priority = |m: &Map<String, Value>| match m.get("priority") {
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => MAX_SAFE_INTEGER,
        p => number(p),
    };
    js_sort(&mut items, |a, b| {
        let (pa, pb) = (priority(a), priority(b));
        if pa != pb {
            return pa - pb;
        }
        let name = |m: &Map<String, Value>| m.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
        match locale_cmp(&name(a), &name(b)) {
            Ordering::Less => -1.0,
            Ordering::Equal => 0.0,
            Ordering::Greater => 1.0,
        }
    });

    Ok(ToolResult::json(&json!({
        "steamid": steamid,
        "total_count": items.len(),
        "items": items,
    })))
}

struct Price {
    initial: f64,
    fin: f64,
    currency: String,
}

async fn fetch_app_price(appid: Option<&Value>) -> Option<Price> {
    let id = js_str(appid);
    let url = format!("https://store.steampowered.com/api/appdetails?appids={id}&filters=price_overview");
    let resp = HTTP.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    let entry = data.get(&id).filter(|e| truthy(Some(e)))?;
    if !truthy(entry.get("success")) {
        return None;
    }
    let overview = entry.get("data")?.get("price_overview").filter(|p| truthy(Some(p)))?;
    Some(Price {
        initial: number(overview.get("initial")),
        fin: number(overview.get("final")),
        currency: js_str(overview.get("currency")),
    })
}

async fn library_value(args: &Args) -> Result<ToolResult> {
    const SAMPLE_SIZE: usize = 100;
    const BATCH_SIZE: usize = 20;

    let steamid = steamid_arg(args)?;
    let owned = steam_api_request(
        "IPlayerService",
        "GetOwnedGames",
        "v1",
        &[("steamid", steamid.clone()), ("include_appinfo", "1".into()), ("include_played_free_games", "1".into())],
    )
    .await?;

    let response = read(Some(&owned), "response")?;
    let all_games: Vec<Value> = match read(response, "games")? {
        Some(Value::Array(games)) => games.clone(),
        _ => Vec::new(),
    };
    let total_owned = read(response, "game_count")?.cloned();

    if all_games.is_empty() {
        return Ok(ToolResult::text(format!("No games found for Steam ID {steamid}. The profile may be private.")));
    }

    let mut sample = all_games.clone();
    js_sort(&mut sample, |a, b| number(b.get("playtime_forever")) - number(a.get("playtime_forever")));
    sample.truncate(SAMPLE_SIZE);

    let mut prices = Vec::with_capacity(sample.len());
    for batch in sample.chunks(BATCH_SIZE) {
        prices.extend(join_all(batch.iter().map(|g| fetch_app_price(g.get("appid")))).await);
    }

    let mut total_initial = 0.0;
    let mut total_final = 0.0;
    let mut games_with_price = 0;
    let mut currency = "USD".to_string();
    let mut priced_games = Vec::new();

    for (game, price) in sample.iter().zip(&prices) {
        let Some(price) = price else { continue };
        games_with_price += 1;
        total_initial += price.initial;
        total_final += price.fin;
        currency.clone_from(&price.currency);

        let mut entry = Map::new();
        put(&mut entry, "appid", game.get("appid"));
        put(&mut entry, "name", game.get("name"));
        entry.insert("playtime".into(), playtime(number(game.get("playtime_forever"))).into());
        entry.insert("original_price".into(), format!("{} {}", fixed(price.initial / 100.0, 2), price.currency).into());
        entry.insert("current_price".into(), format!("{} {}", fixed(price.fin / 100.0, 2), price.currency).into());
        priced_games.push(entry);
    }

    let all_playtime: f64 = all_games.iter().map(|g| number(g.get("playtime_forever"))).sum();
    let cost_per_hour = if all_playtime > 0.0 {
        format!("{} {currency}/hr", fixed((total_initial / 100.0) / (all_playtime / 60.0), 2))
    } else {
        "N/A".into()
    };

    let note = if (sample.len() as f64) < number(total_owned.as_ref()) {
        format!(
            "Only checked top {} games by playtime out of {} total. Actual library value is likely higher.",
            sample.len(),
            js_str(total_owned.as_ref())
        )
    } else {
        "Checked all owned games.".into()
    };

    let original =
        |m: &Map<String, Value>| parse_float(m.get("original_price").and_then(Value::as_str).unwrap_or_default());
    js_sort(&mut priced_games, |a, b| original(b) - original(a));
    priced_games.truncate(20);

    let mut output = Map::new();
    output.insert("steamid".into(), steamid.into());
    put(&mut output, "total_games_owned", total_owned.as_ref());
    output.insert("games_checked".into(), sample.len().into());
    output.insert("games_with_price".into(), games_with_price.into());
    output.insert("games_free_or_delisted".into(), (sample.len() - games_with_price).into());
    output.insert("estimated_value".into(), format!("{} {currency}", fixed(total_initial / 100.0, 2)).into());
    output.insert("current_store_value".into(), format!("{} {currency}", fixed(total_final / 100.0, 2)).into());
    output.insert("total_playtime".into(), playtime(all_playtime).into());
    output.insert("cost_per_hour_estimate".into(), cost_per_hour.into());
    output.insert("note".into(), note.into());
    output.insert("top_valued_games".into(), priced_games.into_iter().map(Value::Object).collect::<Vec<_>>().into());
    Ok(ToolResult::json(&Value::Object(output)))
}
