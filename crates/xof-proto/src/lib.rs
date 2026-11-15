//! IPC between the ButterSat daemon and its clients (UI, CLI): newline-
//! delimited JSON-RPC 2.0 over a per-user local socket (design §12), plus the
//! shared configuration and status types.

pub mod config;
pub mod xplane;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use interprocess::local_socket::tokio::{prelude::*, Stream};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, Name};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, oneshot, Mutex};

pub use xof_core::cache::AreaInfo;
pub use xof_core::engine::EngineStatsSnapshot;
pub use xof_mesh::MeshStatus;

pub const JSONRPC: &str = "2.0";

/// Method names.
pub mod method {
    pub const STATUS: &str = "status";
    pub const HEALTH: &str = "health";
    pub const MOUNT: &str = "mount";
    pub const UNMOUNT: &str = "unmount";
    pub const SHUTDOWN: &str = "shutdown";
    pub const GET_CONFIG: &str = "get_config";
    pub const SET_CONFIG: &str = "set_config";
    pub const CACHE_STATS: &str = "cache_stats";
    pub const CACHE_EVICT_AREA: &str = "cache_evict_area";
    pub const CATALOG_STATUS: &str = "catalog_status";
    pub const CATALOG_REFRESH: &str = "catalog_refresh";
    pub const CATALOG_TILES: &str = "catalog_tiles";
    pub const PREPARE_DEPARTURE: &str = "prepare_departure";
    pub const PREFETCH_ROUTE: &str = "prefetch_route";
    pub const PACKS_LIST: &str = "packs_list";
    pub const PACKS_IMPORT: &str = "packs_import";
    pub const SUBSCRIBE: &str = "subscribe";
    /// Server → client notification.
    pub const EVENT: &str = "event";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: Option<u64>, result: Value) -> Self {
        Self { jsonrpc: JSONRPC.into(), id, result: Some(result), error: None }
    }
    pub fn err(id: Option<u64>, code: i64, message: impl Into<String>) -> Self {
        Self { jsonrpc: JSONRPC.into(), id, result: None, error: Some(RpcError { code, message: message.into() }) }
    }
}

/// Server → client notification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Event,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Event {
    Stats(Box<Status>),
    Aircraft(AircraftState),
    Log(LogLine),
    Health(HealthReport),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AircraftState {
    pub lat: f64,
    pub lon: f64,
    pub groundspeed_ms: f64,
    pub track_deg: f64,
    pub agl_m: f64,
    pub unix_time: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogLine {
    pub level: String,
    pub message: String,
    pub unix_time: f64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthCheck {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HealthReport {
    pub checks: Vec<HealthCheck>,
}

impl HealthReport {
    pub fn worst(&self) -> Severity {
        let rank = |s: Severity| match s {
            Severity::Ok => 0,
            Severity::Info => 1,
            Severity::Warn => 2,
            Severity::Error => 3,
        };
        self.checks.iter().map(|c| c.severity).max_by_key(|s| rank(*s)).unwrap_or(Severity::Ok)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct XPlaneInfo {
    pub path: String,
    pub custom_scenery: String,
    pub global_scenery: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PrefetchStatus {
    pub enabled: bool,
    pub queued: usize,
    pub mesh_tiles_done: u64,
    pub imagery_tiles_done: u64,
    pub reduced_zoom: bool,
    /// Prefetch area outline for the UI map: (lat, lon) polygon.
    pub area: Vec<(f64, f64)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Status {
    pub version: String,
    pub mounted: bool,
    pub mount_point: Option<String>,
    pub mount_error: Option<String>,
    pub xplane: Option<XPlaneInfo>,
    pub sim_connected: bool,
    /// X-Plane is loading scenery (or not running): long read budget.
    #[serde(default)]
    pub sim_loading: bool,
    pub aircraft: Option<AircraftState>,
    pub provider: String,
    pub engine: EngineStatsSnapshot,
    pub mesh: Option<MeshStatus>,
    pub prefetch: PrefetchStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub imagery_bytes: u64,
    pub imagery_cap_bytes: u64,
    pub imagery_tiles: u64,
    pub areas: Vec<AreaInfo>,
    pub mesh_bytes: u64,
    pub mesh_cap_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PackInfo {
    pub path: String,
    pub tiles: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PrepareDeparture {
    #[serde(default)]
    pub icao: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PrepareResult {
    pub lat: f64,
    pub lon: f64,
    pub name: Option<String>,
    pub mesh_tiles: usize,
    pub imagery_tiles: usize,
}

/// One catalog tile for the UI map.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CatalogTile {
    pub tile: String,
    pub lat: i32,
    pub lon: i32,
    pub ready: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvictArea {
    pub provider_byte: u8,
    pub area: xof_core::TileCoord,
}

/// The per-user socket name.
pub fn socket_name() -> std::io::Result<Name<'static>> {
    if cfg!(windows) {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        let clean: String = user.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        format!("buttersat-{clean}").to_ns_name::<GenericNamespaced>().map(|n| n.into_owned())
    } else {
        socket_path().to_string_lossy().into_owned().to_fs_name::<GenericFilePath>().map(|n| n.into_owned())
    }
}

/// Socket file path on Unix (unused on Windows).
pub fn socket_path() -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("ButterSat").join("daemon.sock")
    } else {
        dirs::runtime_dir().or_else(dirs::cache_dir).unwrap_or_else(std::env::temp_dir).join("buttersat.sock")
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>;

/// Async JSON-RPC client.
#[derive(Clone)]
pub struct Client {
    writer: Arc<Mutex<tokio::io::WriteHalf<Stream>>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    events: broadcast::Sender<Event>,
}

impl Client {
    pub async fn connect() -> anyhow::Result<Self> {
        let stream = Stream::connect(socket_name()?).await?;
        Ok(Self::from_stream(stream))
    }

    fn from_stream(stream: Stream) -> Self {
        let (r, w) = tokio::io::split(stream);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let p = pending.clone();
        let ev = events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if v.get("method").and_then(Value::as_str) == Some(method::EVENT) {
                    if let Ok(n) = serde_json::from_value::<RpcNotification>(v) {
                        let _ = ev.send(n.params);
                    }
                } else if let Ok(resp) = serde_json::from_value::<RpcResponse>(v) {
                    if let Some(id) = resp.id {
                        if let Some(tx) = p.lock().await.remove(&id) {
                            let _ = tx.send(resp);
                        }
                    }
                }
            }
            // Connection closed: fail all pending calls.
            p.lock().await.clear();
        });
        Self { writer: Arc::new(Mutex::new(w)), pending, next_id: Arc::new(AtomicU64::new(1)), events }
    }

    /// Call a method and decode its result.
    pub async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> anyhow::Result<R> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = RpcRequest {
            jsonrpc: JSONRPC.into(),
            id: Some(id),
            method: method.into(),
            params: serde_json::to_value(params)?,
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        {
            let mut w = self.writer.lock().await;
            w.write_all(&line).await?;
            w.flush().await?;
        }
        let resp = rx.await.map_err(|_| anyhow::anyhow!("daemon closed the connection"))?;
        if let Some(e) = resp.error {
            anyhow::bail!("{} (code {})", e.message, e.code);
        }
        Ok(serde_json::from_value(resp.result.unwrap_or(Value::Null))?)
    }

    /// Subscribe to pushed events.
    pub async fn subscribe(&self) -> anyhow::Result<broadcast::Receiver<Event>> {
        let rx = self.events.subscribe();
        let _: Value = self.call(method::SUBSCRIBE, Value::Null).await?;
        Ok(rx)
    }
}
