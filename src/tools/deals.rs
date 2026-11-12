//! deals: sale checks, wishlist, wishlist deals and library value estimates.

use anyhow::{Result, bail};
use futures::future::join_all;
use serde_json::{Map, Value, json};

use super::js::{encode_uri_component, fixed, js_str, number, truthy};
use crate::steam::api::{HTTP, steam_api_request};
use crate::steam::paths::get_user_config;
use crate::steam::wishlist::{fetch_prices, fetch_store_details, fetch_wishlist};
use crate::tool::{Args, Param, ParamType, Tool, ToolResult, boxed};
use crate::util::{format_timestamp, js_num, js_sort, locale_cmp};

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
    let items = fetch_wishlist(&steamid).await?;
    if items.is_empty() {
        return Ok(empty_wishlist(&steamid));
    }
    let appids: Vec<i64> = items.iter().map(|i| i.appid).collect();
    // One after the other, so no more than CONCURRENCY store requests run at once.
    let details = fetch_store_details(&appids).await;
    let prices = fetch_prices(&appids).await;

    let mut deals: Vec<Map<String, Value>> = items
        .iter()
        .filter_map(|item| {
            let price = prices.get(&item.appid).filter(|p| p.discount_percent > 0)?;
            let info = details.get(&item.appid).cloned().unwrap_or_default();
            let mut deal = Map::new();
            deal.insert("appid".into(), item.appid.into());
            deal.insert("name".into(), info.name.into());
            deal.insert("discount_percent".into(), price.discount_percent.into());
            deal.insert("price".into(), price.final_formatted.clone().into());
            deal.insert("original_price".into(), price.initial_formatted.clone().into());
            deal.insert("review_desc".into(), info.review_desc.into());
            Some(deal)
        })
        .collect();
    js_sort(&mut deals, |a, b| number(b.get("discount_percent")) - number(a.get("discount_percent")));

    Ok(ToolResult::json(&json!({
        "steamid": steamid,
        "total_wishlist_items": items.len(),
        "deals_found": deals.len(),
        "deals": deals,
    })))
}

async fn wishlist(args: &Args) -> Result<ToolResult> {
    let steamid = steamid_arg(args)?;
    let items = fetch_wishlist(&steamid).await?;
    if items.is_empty() {
        return Ok(empty_wishlist(&steamid));
    }
    let appids: Vec<i64> = items.iter().map(|i| i.appid).collect();
    // One after the other, so no more than CONCURRENCY store requests run at once.
    let details = fetch_store_details(&appids).await;
    let prices = fetch_prices(&appids).await;

    let mut out_items: Vec<Map<String, Value>> = items
        .iter()
        .map(|item| {
            let info = details.get(&item.appid).cloned().unwrap_or_default();
            let price = match prices.get(&item.appid) {
                _ if info.is_free => "Free".into(),
                Some(p) => json!({ "price": p.final_formatted, "discount_pct": p.discount_percent }),
                None => "N/A".into(),
            };
            let mut out = Map::new();
            out.insert("appid".into(), item.appid.into());
            out.insert("name".into(), info.name.into());
            out.insert("priority".into(), item.priority.into());
            out.insert("review_desc".into(), info.review_desc.into());
            out.insert("release_date".into(), info.release_date.into());
            out.insert("added".into(), format_timestamp(item.date_added).into());
            out.insert("free".into(), info.is_free.into());
            out.insert("price".into(), price);
            out
        })
        .collect();

    // Ranked items first by rank, unranked (priority 0) last; then by name.
    let rank = |m: &Map<String, Value>| match m.get("priority").and_then(Value::as_i64) {
        Some(0) | None => i64::MAX,
        Some(p) => p,
    };
    let name = |m: &Map<String, Value>| m.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    out_items.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| locale_cmp(&name(a), &name(b))));

    Ok(ToolResult::json(&json!({
        "steamid": steamid,
        "total_count": out_items.len(),
        "items": out_items,
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
