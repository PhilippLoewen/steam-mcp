//! Steam Web API and Store API clients, plus the shared HTTP client.
//!
//! The Web API key comes from STEAM_API_KEY; the store endpoints need none.

use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

const STEAM_API_BASE: &str = "https://api.steampowered.com";
const STORE_API_BASE: &str = "https://store.steampowered.com";

/// The process-wide HTTP client. Requests give up after 30 seconds, so a
/// stalled server can't leave a tool call hanging.
pub static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("steam-mcp/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("HTTP client")
});

/// Persona states as reported by GetPlayerSummaries.
pub fn persona_state(state: i64) -> Option<&'static str> {
    Some(match state {
        0 => "Offline",
        1 => "Online",
        2 => "Busy",
        3 => "Away",
        4 => "Snooze",
        5 => "Looking to trade",
        6 => "Looking to play",
        _ => return None,
    })
}

/// The Web API key, or an error telling the user where to get one.
pub fn get_api_key() -> Result<String> {
    std::env::var("STEAM_API_KEY").ok().filter(|k| !k.is_empty()).ok_or_else(|| {
        anyhow!("STEAM_API_KEY environment variable is not set. Get your key at https://steamcommunity.com/dev/apikey")
    })
}

pub fn has_api_key() -> bool {
    std::env::var("STEAM_API_KEY").is_ok_and(|k| !k.is_empty())
}

/// The first 200 characters of a body, for error messages.
fn snippet(body: &str) -> String {
    body.chars().take(200).collect()
}

/// GET {STEAM_API_BASE}/{iface}/{method}/{version}/ with the key and `params`.
pub async fn steam_api_request(iface: &str, method: &str, version: &str, params: &[(&str, String)]) -> Result<Value> {
    let key = get_api_key()?;
    let mut url = reqwest::Url::parse(&format!("{STEAM_API_BASE}/{iface}/{method}/{version}/"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("key", &key);
        q.append_pair("format", "json");
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    let resp = HTTP.get(url).send().await.map_err(reqwest::Error::without_url)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Steam API {}: {}", status.as_u16(), snippet(&body));
    }
    Ok(resp.json().await.map_err(reqwest::Error::without_url)?)
}

/// GET a Web API method that needs no key (IWishlistService, IStoreBrowseService).
pub async fn public_api_request(iface: &str, method: &str, version: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut url = reqwest::Url::parse(&format!("{STEAM_API_BASE}/{iface}/{method}/{version}/"))?;
    url.query_pairs_mut().extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
    let resp = HTTP.get(url).send().await.map_err(reqwest::Error::without_url)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Steam API {}: {}", status.as_u16(), snippet(&body));
    }
    Ok(resp.json().await.map_err(reqwest::Error::without_url)?)
}

/// GET {STORE_API_BASE}{path} with `params`; no key needed.
pub async fn store_api_request(path: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut url = reqwest::Url::parse(&format!("{STORE_API_BASE}{path}"))?;
    if !params.is_empty() {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    let resp = HTTP.get(url).send().await.map_err(reqwest::Error::without_url)?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Store API {}: {}", status.as_u16(), snippet(&body));
    }
    Ok(resp.json().await.map_err(reqwest::Error::without_url)?)
}
