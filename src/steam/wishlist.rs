//! A user's wishlist, with store details and prices for its games.
//!
//! The list comes from IWishlistService/GetWishlist, which needs no key but
//! returns only appids (the store's old wishlistdata endpoint is gone).
//! Names, reviews and release dates come from IStoreBrowseService/GetItems,
//! prices from the store's appdetails in the caller's currency, as
//! `deals check_sale` gets them.

use std::collections::HashMap;

use anyhow::Result;
use futures::StreamExt;
use serde_json::{Value, json};

use super::api::{public_api_request, store_api_request};
use crate::util::format_timestamp;

/// Appids per GetItems or appdetails request.
const BATCH: usize = 50;
/// Store requests in flight at once.
const CONCURRENCY: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct WishlistItem {
    pub appid: i64,
    /// 1 is the top of the user's list; 0 means unranked.
    pub priority: i64,
    pub date_added: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreDetails {
    pub name: Option<String>,
    pub is_free: bool,
    pub review_desc: Option<String>,
    pub release_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorePrice {
    pub discount_percent: i64,
    /// Formatted in the store's currency, like "9,69€".
    pub final_formatted: String,
    pub initial_formatted: String,
}

/// The wishlist of `steamid`; empty when it's private or empty.
pub async fn fetch_wishlist(steamid: &str) -> Result<Vec<WishlistItem>> {
    let data = public_api_request("IWishlistService", "GetWishlist", "v1", &[("steamid", steamid.to_string())]).await?;
    Ok(parse_wishlist(&data))
}

fn parse_wishlist(data: &Value) -> Vec<WishlistItem> {
    let Some(items) = data["response"]["items"].as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            Some(WishlistItem {
                appid: item["appid"].as_i64()?,
                priority: item["priority"].as_i64().unwrap_or(0),
                date_added: item["date_added"].as_i64().unwrap_or(0),
            })
        })
        .collect()
}

/// Run `fetch` over `appids` in batches, a few at a time, and merge what
/// comes back. A failed batch is left out rather than failing the whole call.
async fn batched<T, F, Fut>(appids: &[i64], fetch: F) -> HashMap<i64, T>
where
    F: Fn(Vec<i64>) -> Fut,
    Fut: Future<Output = Result<HashMap<i64, T>>>,
{
    let requests: Vec<Fut> = appids.chunks(BATCH).map(|chunk| fetch(chunk.to_vec())).collect();
    let results: Vec<Result<HashMap<i64, T>>> =
        futures::stream::iter(requests).buffer_unordered(CONCURRENCY).collect().await;
    results.into_iter().flatten().flatten().collect()
}

/// Names, free flag, review summary and release date for `appids`.
pub async fn fetch_store_details(appids: &[i64]) -> HashMap<i64, StoreDetails> {
    batched(appids, |chunk| async move {
        let ids: Vec<Value> = chunk.iter().map(|id| json!({ "appid": id })).collect();
        let request = json!({
            "ids": ids,
            // GetItems needs a country; names and reviews don't depend on it, prices come from appdetails.
            "context": { "language": "english", "country_code": "US" },
            "data_request": { "include_basic_info": true, "include_release": true, "include_reviews": true },
        });
        let data =
            public_api_request("IStoreBrowseService", "GetItems", "v1", &[("input_json", request.to_string())]).await?;
        Ok(parse_store_items(&data))
    })
    .await
}

fn parse_store_items(data: &Value) -> HashMap<i64, StoreDetails> {
    let Some(items) = data["response"]["store_items"].as_array() else { return HashMap::new() };
    items
        .iter()
        .filter(|item| item["success"].as_i64() == Some(1))
        .filter_map(|item| {
            let details = StoreDetails {
                name: item["name"].as_str().map(str::to_string),
                is_free: item["is_free"].as_bool().unwrap_or(false),
                review_desc: item["reviews"]["summary_filtered"]["review_score_label"].as_str().map(str::to_string),
                release_date: item["release"]["steam_release_date"].as_i64().map(format_timestamp),
            };
            Some((item["appid"].as_i64()?, details))
        })
        .collect()
}

/// Current prices for `appids`; games without a price (free, unreleased,
/// delisted) are missing.
pub async fn fetch_prices(appids: &[i64]) -> HashMap<i64, StorePrice> {
    batched(appids, |chunk| async move {
        let ids = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let data =
            store_api_request("/api/appdetails", &[("appids", ids), ("filters", "price_overview".into())]).await?;
        Ok(parse_prices(&data))
    })
    .await
}

fn parse_prices(data: &Value) -> HashMap<i64, StorePrice> {
    let Some(apps) = data.as_object() else { return HashMap::new() };
    apps.iter()
        .filter_map(|(appid, app)| {
            let price = &app["data"]["price_overview"];
            Some((
                appid.parse().ok()?,
                StorePrice {
                    discount_percent: price["discount_percent"].as_i64().unwrap_or(0),
                    final_formatted: price["final_formatted"].as_str()?.to_string(),
                    initial_formatted: price["initial_formatted"].as_str().unwrap_or_default().to_string(),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_wishlist() {
        let data = json!({ "response": { "items": [
            { "appid": 50650, "priority": 29, "date_added": 1433965886 },
            { "appid": 620, "priority": 0, "date_added": 1 },
            { "priority": 3 },
        ] } });
        assert_eq!(
            parse_wishlist(&data),
            [
                WishlistItem { appid: 50650, priority: 29, date_added: 1433965886 },
                WishlistItem { appid: 620, priority: 0, date_added: 1 },
            ]
        );
        // Private or empty.
        assert!(parse_wishlist(&json!({ "response": {} })).is_empty());
    }

    #[test]
    fn parses_store_items() {
        let data = json!({ "response": { "store_items": [
            { "appid": 620, "success": 1, "name": "Portal 2",
              "reviews": { "summary_filtered": { "review_score_label": "Overwhelmingly Positive" } },
              "release": { "steam_release_date": 1303186800 } },
            { "appid": 570, "success": 1, "name": "Dota 2", "is_free": true },
            { "appid": 99999999, "success": 2 },
        ] } });
        let items = parse_store_items(&data);
        assert_eq!(items.len(), 2);
        assert_eq!(items[&620].name.as_deref(), Some("Portal 2"));
        assert_eq!(items[&620].review_desc.as_deref(), Some("Overwhelmingly Positive"));
        assert_eq!(items[&620].release_date, Some(format_timestamp(1303186800)));
        assert!(items[&570].is_free && !items[&620].is_free);
    }

    #[test]
    fn parses_prices() {
        let data = json!({
            "620": { "success": true, "data": { "price_overview": {
                "currency": "EUR", "initial": 969, "final": 484, "discount_percent": 50,
                "initial_formatted": "9,69€", "final_formatted": "4,84€" } } },
            "570": { "success": true, "data": [] },
            "99999999": { "success": false },
        });
        let prices = parse_prices(&data);
        assert_eq!(prices.len(), 1);
        assert_eq!(
            prices[&620],
            StorePrice { discount_percent: 50, final_formatted: "4,84€".into(), initial_formatted: "9,69€".into() }
        );
    }
}
