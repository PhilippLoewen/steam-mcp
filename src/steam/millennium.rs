//! Live category and launch options edits through the steam-mcp-bridge
//! Millennium plugin (millennium-plugin/ in this repo), so Steam doesn't have
//! to be closed.
//!
//! Millennium serves a local socket (MEP) whose plugin.config.set fires the
//! plugin's Lua config.on_change. A request goes to config key request:<id>;
//! the plugin runs it inside the Steam client and answers in response:<id>,
//! which is polled here.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::categories::{Category, CategoryError, KEY_PREFIX, category_kind, find_category};
use crate::util::js_num;

pub const BRIDGE_PLUGIN: &str = "steam-mcp-bridge";

/// Largest MEP frame accepted. Bridge answers are a few kilobytes; the cap
/// only keeps a misbehaving peer from making us buffer without bound.
const MAX_FRAME: usize = 4 * 1024 * 1024;

pub fn mep_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("MILLENNIUM_MEP_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        let temp = std::env::var_os("TEMP").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows\Temp"));
        return temp.join("millennium-mep.sock");
    }
    PathBuf::from("/tmp/millennium-mep.sock")
}

/// MEP frame: 4-byte little-endian length, then a msgpack payload.
pub fn encode_frame(message: &Value) -> Vec<u8> {
    let payload = rmp_serde::to_vec(message).expect("JSON values always encode");
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Splits a byte stream into decoded MEP frames.
#[derive(Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while self.buffer.len() >= 4 {
            let length = u32::from_le_bytes(self.buffer[..4].try_into().expect("4 bytes")) as usize;
            if length == 0 || length > MAX_FRAME {
                bail!("Invalid MEP frame length {length}");
            }
            if self.buffer.len() < 4 + length {
                break;
            }
            messages.push(rmp_serde::from_slice(&self.buffer[4..4 + length])?);
            self.buffer.drain(..4 + length);
        }
        Ok(messages)
    }
}

#[cfg(unix)]
type Stream = tokio::net::UnixStream;

/// A connection to Millennium's MEP socket. Calls are sequential; frames that
/// don't answer the current call (events, answers past their timeout) are dropped.
#[cfg(unix)]
pub struct MepClient {
    stream: Stream,
    reader: FrameReader,
    queued: std::collections::VecDeque<Value>,
    next_id: u64,
}

#[cfg(unix)]
impl MepClient {
    /// Connect, refusing a socket another user owns or serves: in a shared
    /// /tmp anyone could bind the default path before Millennium does.
    pub async fn connect(socket_path: &Path, timeout: Duration) -> Result<Self> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        // SAFETY: geteuid has no preconditions and can't fail.
        let me = unsafe { libc::geteuid() };
        let meta = std::fs::symlink_metadata(socket_path)?;
        if !meta.file_type().is_socket() {
            bail!("{} is not a socket (symlinks aren't followed)", socket_path.display());
        }
        if meta.uid() != me {
            bail!("{} belongs to another user (uid {}), so it isn't trusted", socket_path.display(), meta.uid());
        }
        let stream = tokio::time::timeout(timeout, Stream::connect(socket_path))
            .await
            .map_err(|_| anyhow!("Timed out connecting to {}", socket_path.display()))??;
        let peer = stream.peer_cred()?.uid();
        if peer != me {
            bail!("{} is served by another user (uid {peer}), so it isn't trusted", socket_path.display());
        }
        Ok(Self { stream, reader: FrameReader::default(), queued: Default::default(), next_id: 0 })
    }

    async fn next_message(&mut self) -> Result<Value> {
        use tokio::io::AsyncReadExt;
        loop {
            if let Some(m) = self.queued.pop_front() {
                return Ok(m);
            }
            // On the heap: in the future's state it would cost every caller 64 KiB of stack.
            let mut buf = vec![0u8; 64 * 1024];
            let n = self.stream.read(&mut buf).await?;
            if n == 0 {
                bail!("Millennium closed the connection");
            }
            self.queued.extend(self.reader.push(&buf[..n])?);
        }
    }

    pub async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        use tokio::io::AsyncWriteExt;
        self.next_id += 1;
        let id = self.next_id.to_string();
        let frame = encode_frame(&json!({ "id": id, "method": method, "params": params }));
        let exchange = async {
            self.stream.write_all(&frame).await?;
            loop {
                let message = self.next_message().await?;
                if message.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                    continue;
                }
                return match message.get("error") {
                    Some(Value::String(e)) if !e.is_empty() => Err(anyhow!("{e}")),
                    _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
            }
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| anyhow!("Millennium did not answer {method} within {}s", js_num(timeout.as_secs_f64())))?
    }

    pub async fn close(mut self) {
        use tokio::io::AsyncWriteExt;
        let _ = self.stream.shutdown().await;
    }
}

/// A category edit for the bridge plugin.
#[derive(Debug, Clone)]
pub enum BridgeRequest {
    Add { name: String, appids: Vec<i64> },
    Remove { name: String, appids: Vec<i64> },
    Create { name: String },
    Delete { name: String },
    Rename { name: String, new_name: String },
}

impl BridgeRequest {
    pub fn to_json(&self) -> Value {
        match self {
            BridgeRequest::Add { name, appids } => json!({ "action": "add", "name": name, "appids": appids }),
            BridgeRequest::Remove { name, appids } => json!({ "action": "remove", "name": name, "appids": appids }),
            BridgeRequest::Create { name } => json!({ "action": "create", "name": name }),
            BridgeRequest::Delete { name } => json!({ "action": "delete", "name": name }),
            BridgeRequest::Rename { name, new_name } => {
                json!({ "action": "rename", "name": name, "newName": new_name })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BridgeCollection {
    /// Collection id as Steam's collectionStore knows it, e.g. "favorite" or "uc-AbC123".
    pub id: String,
    pub name: String,
    pub dynamic: bool,
    pub appids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BridgeResult {
    pub changed: bool,
    pub id: String,
    /// Absent once deleted.
    pub collection: Option<BridgeCollection>,
    pub affected: Option<Vec<i64>>,
    pub skipped: Option<Vec<i64>>,
}

fn int_list(v: Option<&Value>) -> Option<Vec<i64>> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect())
}

impl BridgeResult {
    pub fn from_json(v: &Value) -> Self {
        let collection = v.get("collection").and_then(Value::as_object).map(|c| BridgeCollection {
            id: c.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            name: c.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            dynamic: c.get("dynamic").and_then(Value::as_bool).unwrap_or(false),
            appids: int_list(c.get("appids")).unwrap_or_default(),
        });
        Self {
            changed: v.get("changed").and_then(Value::as_bool).unwrap_or(false),
            id: v.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
            collection,
            affected: int_list(v.get("affected")),
            skipped: int_list(v.get("skipped")),
        }
    }
}

/// Whether `categories` (read from the file) show `request` as done, for when
/// the plugin didn't answer. Steam saves edits made in the client to that file.
pub fn verify_bridge_edit<'a>(request: &BridgeRequest, categories: &'a [Category]) -> (bool, Option<&'a Category>) {
    match request {
        BridgeRequest::Add { name, appids } | BridgeRequest::Remove { name, appids } => {
            let wanted = matches!(request, BridgeRequest::Add { .. });
            let category = find_category(categories, name);
            let applied = category.is_some_and(|c| appids.iter().all(|id| c.appids.contains(id) == wanted));
            (applied, category)
        }
        BridgeRequest::Create { name } => {
            let category = find_category(categories, name);
            (category.is_some(), category)
        }
        BridgeRequest::Rename { new_name, .. } => {
            let category = find_category(categories, new_name);
            (category.is_some(), category)
        }
        BridgeRequest::Delete { name } => (find_category(categories, name).is_none(), None),
    }
}

pub struct BridgeOptions {
    pub socket_path: PathBuf,
    pub timeout: Duration,
    pub poll: Duration,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self { socket_path: mep_socket_path(), timeout: Duration::from_secs(15), poll: Duration::from_millis(100) }
    }
}

/// What came of a bridge request that didn't fail.
#[derive(Debug, Clone, PartialEq)]
pub enum BridgeOutcome<T = BridgeResult> {
    Done(T),
    /// Why the bridge couldn't be used; nothing was changed in Steam.
    Unavailable(String),
}

/// A failure after the request reached the bridge, worded for the user.
#[derive(Debug, Clone)]
pub struct BridgeError {
    pub message: String,
    /// The plugin got the request but never answered (it may still have applied it).
    pub timeout: bool,
    /// Whether the request ran is unknown: it timed out or the connection dropped.
    pub outcome_unknown: bool,
}

impl BridgeError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), timeout: false, outcome_unknown: false }
    }

    fn timeout(message: impl Into<String>) -> Self {
        Self { message: message.into(), timeout: true, outcome_unknown: true }
    }

    /// Whether `e` is a bridge failure after which the request may or may not have run.
    pub fn outcome_unknown(e: &anyhow::Error) -> bool {
        e.downcast_ref::<BridgeError>().is_some_and(|be| be.outcome_unknown)
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BridgeError {}

/// Run a category edit through the bridge plugin. Returns Unavailable when the
/// bridge can't be reached (no socket, plugin missing or stopped) so the caller
/// can fall back. Once the request is sent, every failure is a CategoryError.
pub async fn run_bridge_edit(request: &BridgeRequest, options: &BridgeOptions) -> Result<BridgeOutcome> {
    match run_bridge(&request.to_json(), options).await {
        Ok(BridgeOutcome::Done(result)) => Ok(BridgeOutcome::Done(BridgeResult::from_json(&result))),
        Ok(BridgeOutcome::Unavailable(why)) => Ok(BridgeOutcome::Unavailable(why)),
        Err(e) => Err(match e.downcast::<BridgeError>() {
            Ok(be) if be.timeout => CategoryError::timeout(be.message).into(),
            Ok(be) => CategoryError::new(be.message).into(),
            Err(e) => e,
        }),
    }
}

/// A launch options read or write for the bridge plugin.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchOptionsRequest {
    Get {
        appid: i64,
    },
    /// With `expected`, the plugin refuses when Steam's current launch options differ from it.
    Set {
        appid: i64,
        options: String,
        expected: Option<String>,
    },
}

impl LaunchOptionsRequest {
    pub fn to_json(&self) -> Value {
        match self {
            LaunchOptionsRequest::Get { appid } => json!({ "action": "get_launch_options", "appid": appid }),
            LaunchOptionsRequest::Set { appid, options, expected } => {
                let mut request = json!({ "action": "set_launch_options", "appid": appid, "options": options });
                if let Some(expected) = expected {
                    request["expected"] = expected.clone().into();
                }
                request
            }
        }
    }
}

/// A game's launch options as the running Steam client holds them.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeLaunchOptions {
    pub launch_options: String,
    /// For a write: whether Steam's launch options changed.
    pub changed: bool,
    /// For a write the plugin never confirmed: a read afterwards showed it applied.
    pub verified_by_readback: bool,
}

/// How long the read after an unconfirmed write may take.
const READBACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Read or set a game's launch options inside the running Steam client.
/// Unavailable as for [`run_bridge_edit`], and also when the installed plugin
/// predates launch options (it refuses them without changing anything); errors
/// are BridgeErrors. A write the plugin doesn't confirm in time is read back.
pub async fn bridge_launch_options(
    request: &LaunchOptionsRequest,
    options: &BridgeOptions,
) -> Result<BridgeOutcome<BridgeLaunchOptions>> {
    let err = match exchange_launch_options(request, options).await {
        Err(e) if e.downcast_ref::<BridgeError>().is_some_and(|be| be.timeout) => e,
        other => return other,
    };
    let LaunchOptionsRequest::Set { appid, options: wanted, expected } = request else {
        return Err(BridgeError::timeout(format!(
            "The {BRIDGE_PLUGIN} Millennium plugin did not answer within {}s.",
            js_num(options.timeout.as_secs_f64())
        ))
        .into());
    };
    // Reads don't wait for queued writes in the plugin, so a write it already
    // took can still land after a read-back that doesn't show it yet.
    let readback = BridgeOptions { socket_path: options.socket_path.clone(), timeout: READBACK_TIMEOUT, ..*options };
    match exchange_launch_options(&LaunchOptionsRequest::Get { appid: *appid }, &readback).await {
        Ok(BridgeOutcome::Done(now)) if now.launch_options == *wanted => {
            // Without `expected`, Steam may have held these options already; assume the write did it.
            let changed = expected.as_ref().is_none_or(|previous| previous != wanted);
            Ok(BridgeOutcome::Done(BridgeLaunchOptions { changed, verified_by_readback: true, ..now }))
        }
        Ok(BridgeOutcome::Done(_)) => Err(BridgeError::timeout(format!(
            "{err} Steam doesn't show the new launch options yet; check them before retrying."
        ))
        .into()),
        _ => Err(err),
    }
}

/// [`run_bridge`] for a request a plugin built before `feature` was bridged
/// refuses, without changing anything: such a plugin counts as unavailable.
async fn run_bridge_feature(request: &Value, options: &BridgeOptions, feature: &str) -> Result<BridgeOutcome<Value>> {
    match run_bridge(request, options).await {
        Err(e) if e.downcast_ref::<BridgeError>().is_some_and(|be| be.message.starts_with("Unknown action")) => {
            Ok(BridgeOutcome::Unavailable(format!(
                "the installed {BRIDGE_PLUGIN} is too old to handle {feature}; rebuild it \
                 (bun run build in millennium-plugin/) and restart Steam"
            )))
        }
        other => other,
    }
}

async fn exchange_launch_options(
    request: &LaunchOptionsRequest,
    options: &BridgeOptions,
) -> Result<BridgeOutcome<BridgeLaunchOptions>> {
    let result = match run_bridge_feature(&request.to_json(), options, "launch options").await? {
        BridgeOutcome::Done(result) => result,
        BridgeOutcome::Unavailable(why) => return Ok(BridgeOutcome::Unavailable(why)),
    };
    let Some(launch_options) = result.get("launchOptions").and_then(Value::as_str) else {
        return Err(BridgeError::new(format!("{BRIDGE_PLUGIN} sent no launch options in its answer")).into());
    };
    Ok(BridgeOutcome::Done(BridgeLaunchOptions {
        launch_options: launch_options.to_string(),
        changed: result.get("changed").and_then(Value::as_bool).unwrap_or(false),
        verified_by_readback: false,
    }))
}

/// A non-Steam shortcut edit for the bridge plugin. Fields are as stored in
/// shortcuts.vdf (exe and start folder quoted, as Steam writes them).
#[derive(Debug, Clone, PartialEq)]
pub enum ShortcutRequest {
    Add { name: String, exe: String, start_dir: String, launch_options: String },
    Remove { appid: i64 },
}

impl ShortcutRequest {
    pub fn to_json(&self) -> Value {
        match self {
            ShortcutRequest::Add { name, exe, start_dir, launch_options } => json!({
                "action": "add_shortcut",
                "name": name,
                "exe": exe,
                "startDir": start_dir,
                "launchOptions": launch_options,
            }),
            ShortcutRequest::Remove { appid } => json!({ "action": "remove_shortcut", "appid": appid }),
        }
    }
}

/// A shortcut as the running Steam client holds it: the one added, or the one removed.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeShortcut {
    /// Steam picks it when adding.
    pub appid: i64,
    pub name: String,
    pub exe: String,
    pub start_dir: String,
    pub launch_options: String,
}

/// Add or remove a non-Steam shortcut inside the running Steam client.
/// Unavailable as for [`bridge_launch_options`]; errors are BridgeErrors.
pub async fn bridge_shortcut(
    request: &ShortcutRequest,
    options: &BridgeOptions,
) -> Result<BridgeOutcome<BridgeShortcut>> {
    let result = match run_bridge_feature(&request.to_json(), options, "shortcuts").await? {
        BridgeOutcome::Done(result) => result,
        BridgeOutcome::Unavailable(why) => return Ok(BridgeOutcome::Unavailable(why)),
    };
    let text = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let Some(appid) = result.get("appid").and_then(Value::as_i64) else {
        return Err(BridgeError::new(format!("{BRIDGE_PLUGIN} sent no shortcut appid in its answer")).into());
    };
    Ok(BridgeOutcome::Done(BridgeShortcut {
        appid,
        name: text("name"),
        exe: text("exe"),
        start_dir: text("startDir"),
        launch_options: text("launchOptions"),
    }))
}

/// Send one JSON request to the bridge plugin and return its result. Returns
/// Unavailable when the bridge can't be reached (no socket, plugin missing or
/// stopped). Once the request is sent, every failure is a BridgeError.
#[cfg(unix)]
async fn run_bridge(request: &Value, options: &BridgeOptions) -> Result<BridgeOutcome<Value>> {
    let socket_path = &options.socket_path;
    if !socket_path.exists() {
        return Ok(BridgeOutcome::Unavailable(format!(
            "Millennium is not running (no socket at {})",
            socket_path.display()
        )));
    }
    let mut client = match MepClient::connect(socket_path, Duration::from_secs(2)).await {
        Ok(c) => c,
        Err(e) => return Ok(BridgeOutcome::Unavailable(format!("can't connect to Millennium: {e}"))),
    };
    let outcome = bridge_exchange(&mut client, request, options).await;
    client.close().await;
    outcome
}

#[cfg(unix)]
async fn bridge_exchange(
    client: &mut MepClient,
    request: &Value,
    options: &BridgeOptions,
) -> Result<BridgeOutcome<Value>> {
    const CALL_TIMEOUT: Duration = Duration::from_secs(5);
    let status = match client.call("plugin.status", json!({ "name": BRIDGE_PLUGIN }), CALL_TIMEOUT).await {
        Ok(s) => s,
        Err(e) => return Ok(BridgeOutcome::Unavailable(format!("{BRIDGE_PLUGIN} is not installed ({e})"))),
    };
    if status.get("running") != Some(&Value::Bool(true)) {
        let state = if status.get("enabled").is_some_and(truthy) { "not running" } else { "disabled" };
        return Ok(BridgeOutcome::Unavailable(format!("{BRIDGE_PLUGIN} is installed but {state}")));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let request_key = format!("request:{id}");
    let response_key = format!("response:{id}");

    let exchange: Result<Option<Map<String, Value>>> = async {
        let set = json!({ "name": BRIDGE_PLUGIN, "key": request_key, "value": request.to_string() });
        client.call("plugin.config.set", set, CALL_TIMEOUT).await?;
        // Every call and pause is cut to the time left, so the wait ends at the deadline.
        let deadline = tokio::time::Instant::now() + options.timeout;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let params = json!({ "name": BRIDGE_PLUGIN, "key": response_key });
            let got = match client.call("plugin.config.get", params, CALL_TIMEOUT.min(left)).await {
                Ok(got) => got,
                Err(_) if tokio::time::Instant::now() >= deadline => return Ok(None),
                Err(e) => return Err(e),
            };
            match got.get("value") {
                Some(v) if truthy(v) => return Ok(Some(v.as_object().cloned().unwrap_or_default())),
                _ => {
                    tokio::time::sleep(
                        options.poll.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                    )
                    .await
                }
            }
        }
    }
    .await;

    let response = exchange.map_err(|e| BridgeError {
        message: format!("Lost the connection to Millennium ({e}); the edit may or may not have been applied."),
        timeout: false,
        outcome_unknown: true,
    })?;

    let Some(response) = response else {
        // Withdraw the request and any late answer, but don't wait long for a slow Millennium.
        let withdraw = async {
            for key in [&request_key, &response_key] {
                let params = json!({ "name": BRIDGE_PLUGIN, "key": key });
                let _ = client.call("plugin.config.delete", params, CALL_TIMEOUT).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), withdraw).await;
        return Err(BridgeError::timeout(format!(
            "The {BRIDGE_PLUGIN} Millennium plugin did not answer within {}s; the edit may or may not have been applied.",
            js_num(options.timeout.as_secs_f64())
        ))
        .into());
    };
    let _ =
        client.call("plugin.config.delete", json!({ "name": BRIDGE_PLUGIN, "key": response_key }), CALL_TIMEOUT).await;

    if response.get("ok") != Some(&Value::Bool(true)) {
        let error = match response.get("error") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "undefined".to_string(),
        };
        return Err(BridgeError::new(error).into());
    }
    Ok(BridgeOutcome::Done(response.get("result").cloned().unwrap_or(Value::Null)))
}

/// Unix sockets aren't wired up on this platform, so the bridge is never available.
#[cfg(not(unix))]
async fn run_bridge(_request: &Value, options: &BridgeOptions) -> Result<BridgeOutcome<Value>> {
    if !options.socket_path.exists() {
        return Ok(BridgeOutcome::Unavailable(format!(
            "Millennium is not running (no socket at {})",
            options.socket_path.display()
        )));
    }
    Ok(BridgeOutcome::Unavailable("can't connect to Millennium: not supported on this platform".to_string()))
}

/// JS truthiness for JSON values.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A bridge collection shaped like a parsed file entry, for shared reporting.
pub fn bridge_category(collection: &BridgeCollection) -> Category {
    Category {
        name: collection.name.clone(),
        key: format!("{KEY_PREFIX}{}", collection.id),
        kind: category_kind(&collection.id, collection.dynamic),
        version: None,
        timestamp: None,
        appids: collection.appids.clone(),
        added: collection.appids.len(),
        removed: 0,
        has_filter: collection.dynamic,
        filter_spec: None,
        is_deleted: false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::*;

    #[test]
    fn frames_round_trip_across_chunks() {
        let a = encode_frame(&json!({ "id": "1", "method": "plugin.list", "params": {} }));
        let b = encode_frame(&json!({ "id": "2", "result": [1, 2, 3], "error": null }));
        let bytes = [a.clone(), b].concat();
        let mut reader = FrameReader::default();
        assert!(reader.push(&bytes[..3]).unwrap().is_empty());
        assert_eq!(
            reader.push(&bytes[3..a.len() + 5]).unwrap(),
            [json!({ "id": "1", "method": "plugin.list", "params": {} })]
        );
        assert_eq!(
            reader.push(&bytes[a.len() + 5..]).unwrap(),
            [json!({ "id": "2", "result": [1, 2, 3], "error": null })]
        );
    }

    #[test]
    fn frame_has_little_endian_length_prefix() {
        let frame = encode_frame(&json!("x"));
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize, frame.len() - 4);
    }

    type Plugin = Arc<dyn Fn(Value) -> Option<Value> + Send + Sync>;

    #[derive(Clone, Default)]
    struct Fake {
        /// None: plugin.status fails as "not found". Some((running, enabled)).
        status: Option<(bool, bool)>,
        plugin: Option<Plugin>,
        config: Arc<Mutex<HashMap<String, Value>>>,
        calls: Arc<Mutex<Vec<String>>>,
        /// How long plugin.config.get takes to answer.
        get_delay: Duration,
    }

    fn installed() -> Fake {
        Fake { status: Some((true, true)), ..Default::default() }
    }

    /// Minimal Millennium MEP server: answers plugin.status and plugin.config.*,
    /// and plays the bridge plugin by answering request:<id> keys via `plugin`.
    fn serve(socket: &Path, fake: Fake) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let fake = fake.clone();
                tokio::spawn(async move {
                    let mut reader = FrameReader::default();
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        for message in reader.push(&buf[..n]).unwrap() {
                            let id = message["id"].clone();
                            let method = message["method"].as_str().unwrap().to_string();
                            let params = &message["params"];
                            fake.calls.lock().unwrap().push(method.clone());
                            let key = params["key"].as_str().unwrap_or_default().to_string();
                            let (result, error) = if params["name"] != BRIDGE_PLUGIN {
                                (Value::Null, json!(format!("plugin not found: {}", params["name"])))
                            } else {
                                match method.as_str() {
                                    "plugin.status" => match fake.status {
                                        None => (Value::Null, json!(format!("plugin not found: {BRIDGE_PLUGIN}"))),
                                        Some((running, enabled)) => (
                                            json!({ "name": BRIDGE_PLUGIN, "running": running, "enabled": enabled }),
                                            Value::Null,
                                        ),
                                    },
                                    "plugin.config.set" => {
                                        let mut config = fake.config.lock().unwrap();
                                        config.insert(key.clone(), params["value"].clone());
                                        if let (Some(rid), Some(plugin)) = (key.strip_prefix("request:"), &fake.plugin)
                                        {
                                            config.remove(&key);
                                            let request =
                                                serde_json::from_str(params["value"].as_str().unwrap()).unwrap();
                                            if let Some(answer) = plugin(request) {
                                                config.insert(format!("response:{rid}"), answer);
                                            }
                                        }
                                        (json!({ "success": true }), Value::Null)
                                    }
                                    "plugin.config.get" => {
                                        tokio::time::sleep(fake.get_delay).await;
                                        let value =
                                            fake.config.lock().unwrap().get(&key).cloned().unwrap_or(Value::Null);
                                        (json!({ "value": value }), Value::Null)
                                    }
                                    "plugin.config.delete" => {
                                        fake.config.lock().unwrap().remove(&key);
                                        (json!({ "success": true }), Value::Null)
                                    }
                                    other => (Value::Null, json!(format!("unknown method {other}"))),
                                }
                            };
                            let frame = encode_frame(&json!({ "id": id, "result": result, "error": error }));
                            if stream.write_all(&frame).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        })
    }

    fn options(dir: &tempfile::TempDir) -> BridgeOptions {
        BridgeOptions {
            socket_path: dir.path().join("mep.sock"),
            timeout: Duration::from_secs(15),
            poll: Duration::from_millis(5),
        }
    }

    fn create() -> BridgeRequest {
        BridgeRequest::Create { name: "X".into() }
    }

    fn unavailable(outcome: BridgeOutcome) -> String {
        match outcome {
            BridgeOutcome::Unavailable(why) => why,
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let why = unavailable(run_bridge_edit(&create(), &options(&dir)).await.unwrap());
        assert!(why.contains("Millennium is not running"));
    }

    #[tokio::test]
    async fn missing_disabled_or_stopped_plugin_is_unavailable() {
        for (status, expected) in [
            (None, "not installed"),
            (Some((false, false)), "installed but disabled"),
            (Some((false, true)), "installed but not running"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let opts = options(&dir);
            let server = serve(&opts.socket_path, Fake { status, ..Default::default() });
            let why = unavailable(run_bridge_edit(&create(), &opts).await.unwrap());
            assert!(why.contains(expected), "{why}");
            server.abort();
        }
    }

    #[tokio::test]
    async fn sends_request_and_returns_result() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let fake = Fake {
            plugin: Some(Arc::new(move |req| {
                seen_in.lock().unwrap().push(req);
                Some(json!({ "ok": true, "result": {
                    "changed": true, "id": "uc-abc",
                    "collection": { "id": "uc-abc", "name": "RPG", "dynamic": false, "appids": [10, 440] },
                    "affected": [440], "skipped": []
                }}))
            })),
            ..installed()
        };
        let config = fake.config.clone();
        let _server = serve(&opts.socket_path, fake);

        let request = BridgeRequest::Add { name: "RPG".into(), appids: vec![440] };
        let outcome = run_bridge_edit(&request, &opts).await.unwrap();

        assert_eq!(*seen.lock().unwrap(), [json!({ "action": "add", "name": "RPG", "appids": [440] })]);
        assert_eq!(
            outcome,
            BridgeOutcome::Done(BridgeResult {
                changed: true,
                id: "uc-abc".into(),
                collection: Some(BridgeCollection {
                    id: "uc-abc".into(),
                    name: "RPG".into(),
                    dynamic: false,
                    appids: vec![10, 440]
                }),
                affected: Some(vec![440]),
                skipped: Some(vec![]),
            })
        );
        assert!(config.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn plugin_error_is_a_category_error() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let fake = Fake {
            plugin: Some(Arc::new(|_| {
                Some(json!({ "ok": false, "error": "Category \"Nope\" not found. Available: RPG" }))
            })),
            ..installed()
        };
        let _server = serve(&opts.socket_path, fake);
        let err = run_bridge_edit(&BridgeRequest::Delete { name: "Nope".into() }, &opts).await.unwrap_err();
        let ce = err.downcast_ref::<CategoryError>().expect("CategoryError");
        assert!(ce.message.contains("not found") && !ce.bridge_timeout);
    }

    #[tokio::test]
    async fn a_slow_millennium_doesnt_stretch_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let opts = BridgeOptions { timeout: Duration::from_millis(200), ..options(&dir) };
        // Each answer takes longer than the whole timeout.
        let fake = Fake { get_delay: Duration::from_secs(3), ..installed() };
        let _server = serve(&opts.socket_path, fake);
        let started = std::time::Instant::now();
        let err = run_bridge_edit(&create(), &opts).await.unwrap_err();
        assert!(err.downcast_ref::<CategoryError>().is_some_and(|e| e.bridge_timeout), "{err}");
        // The deadline plus the clean-up calls, not the 3 s answer.
        assert!(started.elapsed() < Duration::from_millis(2500), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn timeout_withdraws_request_and_response() {
        let dir = tempfile::tempdir().unwrap();
        let opts = BridgeOptions { timeout: Duration::from_millis(50), ..options(&dir) };
        let fake = installed();
        let (config, calls) = (fake.config.clone(), fake.calls.clone());
        let _server = serve(&opts.socket_path, fake);
        let err = run_bridge_edit(&create(), &opts).await.unwrap_err();
        assert!(err.downcast_ref::<CategoryError>().is_some_and(|e| e.bridge_timeout));
        assert!(config.lock().unwrap().is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(calls[calls.len() - 2..], ["plugin.config.delete", "plugin.config.delete"]);
    }

    #[tokio::test]
    async fn refuses_a_path_that_is_not_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mep.sock");
        std::fs::write(&file, "").unwrap();
        let err = MepClient::connect(&file, Duration::from_secs(2)).await.err().unwrap();
        assert!(err.to_string().contains("not a socket"), "{err}");
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut reader = FrameReader::default();
        assert!(reader.push(&((MAX_FRAME + 1) as u32).to_le_bytes()).is_err());
    }

    #[tokio::test]
    async fn unanswered_call_fails_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mep.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let _server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink).await;
        });
        let mut client = MepClient::connect(&socket, Duration::from_secs(2)).await.unwrap();
        let err = client.call("plugin.status", json!({ "name": BRIDGE_PLUGIN }), Duration::from_millis(30)).await;
        assert!(err.unwrap_err().to_string().contains("did not answer"));
    }

    #[tokio::test]
    async fn dropped_connection_after_request_is_a_category_error() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let listener = UnixListener::bind(&opts.socket_path).unwrap();
        let _server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = FrameReader::default();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                for message in reader.push(&buf[..n]).unwrap() {
                    if message["method"] == "plugin.status" {
                        let frame =
                            encode_frame(&json!({ "id": message["id"], "result": { "running": true }, "error": null }));
                        stream.write_all(&frame).await.unwrap();
                    } else {
                        return;
                    }
                }
            }
        });
        let err = run_bridge_edit(&create(), &opts).await.unwrap_err();
        let ce = err.downcast_ref::<CategoryError>().expect("CategoryError");
        assert!(ce.message.contains("may or may not have been applied"));
    }

    #[tokio::test]
    async fn launch_options_requests_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let fake = Fake {
            plugin: Some(Arc::new(move |req: Value| {
                seen_in.lock().unwrap().push(req.clone());
                let changed = req["action"] == "set_launch_options";
                Some(json!({ "ok": true, "result": { "appid": 10, "launchOptions": "-novid", "changed": changed } }))
            })),
            ..installed()
        };
        let _server = serve(&opts.socket_path, fake);

        let answer = |changed| {
            BridgeOutcome::Done(BridgeLaunchOptions {
                launch_options: "-novid".into(),
                changed,
                verified_by_readback: false,
            })
        };
        let get = bridge_launch_options(&LaunchOptionsRequest::Get { appid: 10 }, &opts).await.unwrap();
        assert_eq!(get, answer(false));
        let set = LaunchOptionsRequest::Set { appid: 10, options: "-novid".into(), expected: Some(String::new()) };
        let set = bridge_launch_options(&set, &opts).await.unwrap();
        assert_eq!(set, answer(true));
        let unchecked = LaunchOptionsRequest::Set { appid: 10, options: "-x".into(), expected: None };
        bridge_launch_options(&unchecked, &opts).await.unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            [
                json!({ "action": "get_launch_options", "appid": 10 }),
                json!({ "action": "set_launch_options", "appid": 10, "options": "-novid", "expected": "" }),
                json!({ "action": "set_launch_options", "appid": 10, "options": "-x" }),
            ]
        );
    }

    #[tokio::test]
    async fn an_old_plugin_counts_as_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let fake = Fake {
            plugin: Some(Arc::new(|_| Some(json!({ "ok": false, "error": "Unknown action \"get_launch_options\"" })))),
            ..installed()
        };
        let _server = serve(&opts.socket_path, fake);
        for request in [
            LaunchOptionsRequest::Get { appid: 10 },
            LaunchOptionsRequest::Set { appid: 10, options: "-x".into(), expected: None },
        ] {
            let why = unavailable_launch(bridge_launch_options(&request, &opts).await.unwrap());
            assert!(why.contains("too old") && why.contains("rebuild"), "{why}");
        }
    }

    fn unavailable_launch(outcome: BridgeOutcome<BridgeLaunchOptions>) -> String {
        match outcome {
            BridgeOutcome::Unavailable(why) => why,
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_read_timeout_is_not_called_an_edit() {
        let dir = tempfile::tempdir().unwrap();
        let opts = BridgeOptions { timeout: Duration::from_millis(50), ..options(&dir) };
        let _server = serve(&opts.socket_path, installed());
        let err = bridge_launch_options(&LaunchOptionsRequest::Get { appid: 10 }, &opts).await.unwrap_err();
        assert!(err.downcast_ref::<BridgeError>().is_some_and(|e| e.timeout), "{err}");
        assert!(!err.to_string().contains("edit"), "{err}");
    }

    /// A plugin that applies writes but never answers them, and answers reads.
    fn silent_writer(steam_holds: &'static str) -> Fake {
        Fake {
            plugin: Some(Arc::new(move |req: Value| {
                (req["action"] == "get_launch_options").then(
                    || json!({ "ok": true, "result": { "appid": 10, "launchOptions": steam_holds, "changed": false } }),
                )
            })),
            ..installed()
        }
    }

    #[tokio::test]
    async fn an_unanswered_write_is_read_back() {
        let set = LaunchOptionsRequest::Set { appid: 10, options: "-novid".into(), expected: None };

        let dir = tempfile::tempdir().unwrap();
        let opts = BridgeOptions { timeout: Duration::from_millis(50), ..options(&dir) };
        let _server = serve(&opts.socket_path, silent_writer("-novid"));
        let outcome = bridge_launch_options(&set, &opts).await.unwrap();
        assert_eq!(
            outcome,
            BridgeOutcome::Done(BridgeLaunchOptions {
                launch_options: "-novid".into(),
                changed: true,
                verified_by_readback: true
            })
        );

        let dir = tempfile::tempdir().unwrap();
        let opts = BridgeOptions { timeout: Duration::from_millis(50), ..options(&dir) };
        let _server = serve(&opts.socket_path, silent_writer("-old"));
        let err = bridge_launch_options(&set, &opts).await.unwrap_err();
        assert!(err.downcast_ref::<BridgeError>().is_some_and(|e| e.timeout), "{err}");
        assert!(err.to_string().contains("doesn't show the new launch options yet"), "{err}");
    }

    #[tokio::test]
    async fn shortcut_requests_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in = seen.clone();
        let fake = Fake {
            plugin: Some(Arc::new(move |req: Value| {
                seen_in.lock().unwrap().push(req);
                Some(json!({ "ok": true, "result": {
                    "appid": 3000000002u64, "name": "Game", "exe": "\"/g/run\"", "startDir": "\"/g\"", "launchOptions": ""
                }}))
            })),
            ..installed()
        };
        let _server = serve(&opts.socket_path, fake);

        let add = ShortcutRequest::Add {
            name: "Game".into(),
            exe: "\"/g/run\"".into(),
            start_dir: "\"/g\"".into(),
            launch_options: String::new(),
        };
        let added = bridge_shortcut(&add, &opts).await.unwrap();
        assert_eq!(
            added,
            BridgeOutcome::Done(BridgeShortcut {
                appid: 3000000002,
                name: "Game".into(),
                exe: "\"/g/run\"".into(),
                start_dir: "\"/g\"".into(),
                launch_options: String::new(),
            })
        );
        bridge_shortcut(&ShortcutRequest::Remove { appid: 3000000002 }, &opts).await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                json!({ "action": "add_shortcut", "name": "Game", "exe": "\"/g/run\"", "startDir": "\"/g\"", "launchOptions": "" }),
                json!({ "action": "remove_shortcut", "appid": 3000000002u64 }),
            ]
        );
    }

    #[tokio::test]
    async fn an_old_plugin_cannot_edit_shortcuts() {
        let dir = tempfile::tempdir().unwrap();
        let opts = options(&dir);
        let fake = Fake {
            plugin: Some(Arc::new(|_| Some(json!({ "ok": false, "error": "Unknown action \"remove_shortcut\"" })))),
            ..installed()
        };
        let _server = serve(&opts.socket_path, fake);
        match bridge_shortcut(&ShortcutRequest::Remove { appid: 3000000002 }, &opts).await.unwrap() {
            BridgeOutcome::Unavailable(why) => assert!(why.contains("too old to handle shortcuts"), "{why}"),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    fn rpg() -> Category {
        bridge_category(&BridgeCollection {
            id: "uc-abc".into(),
            name: "RPG".into(),
            dynamic: false,
            appids: vec![10, 440],
        })
    }

    #[test]
    fn verify_checks_membership() {
        let cats = [rpg()];
        let add = |name: &str, appids: &[i64]| BridgeRequest::Add { name: name.into(), appids: appids.to_vec() };
        let remove = |appids: &[i64]| BridgeRequest::Remove { name: "RPG".into(), appids: appids.to_vec() };
        assert_eq!(verify_bridge_edit(&add("rpg", &[440]), &cats), (true, Some(&cats[0])));
        assert!(!verify_bridge_edit(&add("RPG", &[440, 730]), &cats).0);
        assert!(verify_bridge_edit(&remove(&[730]), &cats).0);
        assert!(!verify_bridge_edit(&remove(&[440]), &cats).0);
        assert!(!verify_bridge_edit(&add("Nope", &[440]), &cats).0);
    }

    #[test]
    fn verify_checks_existence() {
        let cats = [rpg()];
        assert!(verify_bridge_edit(&BridgeRequest::Create { name: "RPG".into() }, &cats).0);
        assert!(!verify_bridge_edit(&BridgeRequest::Create { name: "New".into() }, &cats).0);
        assert!(verify_bridge_edit(&BridgeRequest::Rename { name: "Old".into(), new_name: "RPG".into() }, &cats).0);
        assert!(!verify_bridge_edit(&BridgeRequest::Delete { name: "RPG".into() }, &cats).0);
        assert!(verify_bridge_edit(&BridgeRequest::Delete { name: "RPG".into() }, &[]).0);
    }

    #[test]
    fn bridge_category_maps_ids_and_kinds() {
        let fav = bridge_category(&BridgeCollection {
            id: "favorite".into(),
            name: "Favorites".into(),
            dynamic: false,
            appids: vec![1, 2],
        });
        assert_eq!(
            (fav.key.as_str(), fav.kind, fav.added, fav.has_filter),
            ("user-collections.favorite", super::super::categories::CategoryKind::Explicit, 2, false)
        );
        let dynamic = BridgeCollection { id: "uc-x".into(), name: "Dyn".into(), dynamic: true, appids: vec![] };
        assert_eq!(bridge_category(&dynamic).kind, super::super::categories::CategoryKind::Filter);
        let tag = BridgeCollection { id: "from-tag-RPG".into(), name: "RPG".into(), dynamic: false, appids: vec![] };
        assert_eq!(bridge_category(&tag).kind, super::super::categories::CategoryKind::Tag);
    }
}
