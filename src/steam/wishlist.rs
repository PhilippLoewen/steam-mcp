//! Fetching a user's wishlist from the store's wishlistdata endpoint.

use serde_json::{Map, Value};

use super::api::HTTP;
use crate::vdf::is_array_index;

/// One page of wishlistdata, or None when it's empty, private or failed.
/// Network and JSON errors fail the whole fetch, like the rejected promise did.
async fn fetch_page(steamid: &str, page: u32) -> anyhow::Result<Option<Map<String, Value>>> {
    let url = format!("https://store.steampowered.com/wishlist/profiles/{steamid}/wishlistdata/?p={page}");
    let resp = HTTP.get(url).send().await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let data: Value = resp.json().await?;
    let Some(obj) = data.as_object() else { return Ok(Some(Map::new())) };
    // { success: 2 } means private or past the last page.
    if obj.get("success").and_then(Value::as_i64) == Some(2) || obj.is_empty() {
        return Ok(None);
    }
    Ok(Some(obj.clone()))
}

/// Pages 0-2 (about 300 items) fetched in parallel and merged; each item gets
/// its `appid` from its key. Items come in JS object order (appids ascending).
pub async fn fetch_all_wishlist_pages(steamid: &str) -> anyhow::Result<Vec<Map<String, Value>>> {
    let pages = futures::future::try_join_all((0..3).map(|p| fetch_page(steamid, p))).await?;
    let mut merged: Map<String, Value> = Map::new();
    for page in pages.into_iter().flatten() {
        for (k, v) in page {
            merged.insert(k, v);
        }
    }
    let mut entries: Vec<(String, Value)> = merged.into_iter().collect();
    entries.sort_by_key(|(k, _)| if is_array_index(k) { (0, k.parse::<u64>().unwrap_or(0)) } else { (1, 0) });
    Ok(entries
        .into_iter()
        .map(|(k, v)| {
            let mut item = v.as_object().cloned().unwrap_or_default();
            let appid = k.parse::<f64>().map(crate::util::num).unwrap_or(Value::Null);
            item.insert("appid".into(), appid);
            item
        })
        .collect())
}
