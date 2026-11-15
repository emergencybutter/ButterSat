//! X-Plane 12 Web API client (design §9): resolve dataref ids over REST, then
//! subscribe to position updates over the websocket. Reconnects forever;
//! connection state doubles as "is X-Plane running".
//!
//! Loading detection (measured on 12.4.4): datarefs exist from ~5 s into a
//! flight load, but the websocket sends nothing until the sim runs again;
//! then `framerate_period` (smoothed) decays from seconds to normal within
//! ~2 s. It changes every frame, also while paused, so update silence means
//! "not rendering".

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use xof_proto::AircraftState;

const DATAREFS: [&str; 6] = [
    "sim/flightmodel/position/latitude",
    "sim/flightmodel/position/longitude",
    "sim/flightmodel/position/groundspeed",
    "sim/flightmodel/position/hpath",
    "sim/flightmodel/position/y_agl",
    "sim/time/framerate_period",
];

/// No update for this long: X-Plane isn't rendering (loading).
const SILENCE: Duration = Duration::from_millis(1500);
/// Frames slower than this: still loading (or about to be).
const SLOW_FRAME_S: f64 = 0.25;

#[derive(Default)]
struct Frames {
    last_update: Option<Instant>,
    period_s: f64,
}

pub struct WebApi {
    pub aircraft: watch::Receiver<Option<AircraftState>>,
    pub connected: watch::Receiver<bool>,
    frames: Arc<parking_lot::Mutex<Frames>>,
}

impl WebApi {
    /// Is X-Plane loading scenery? True when not connected (no flight yet,
    /// X-Plane not running, or the Web API disabled), when updates stopped,
    /// or when frames are very slow.
    pub fn loading(&self) -> bool {
        if !*self.connected.borrow() {
            return true;
        }
        let f = self.frames.lock();
        match f.last_update {
            None => true,
            Some(t) => t.elapsed() > SILENCE || f.period_s > SLOW_FRAME_S,
        }
    }
}

pub fn spawn(port: u16) -> WebApi {
    let (atx, arx) = watch::channel(None);
    let (ctx, crx) = watch::channel(false);
    let frames = Arc::new(parking_lot::Mutex::new(Frames::default()));
    let fr = frames.clone();
    tokio::spawn(async move {
        loop {
            match session(port, &atx, &ctx, &fr).await {
                Ok(()) => tracing::info!("X-Plane Web API session ended"),
                Err(e) => tracing::debug!("X-Plane Web API: {e:#}"),
            }
            let _ = ctx.send(false);
            *fr.lock() = Frames::default();
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    WebApi { aircraft: arx, connected: crx, frames }
}

async fn resolve_ids(http: &reqwest::Client, port: u16) -> anyhow::Result<HashMap<u64, usize>> {
    let mut ids = HashMap::new();
    for (i, name) in DATAREFS.iter().enumerate() {
        let url = format!("http://127.0.0.1:{port}/api/v2/datarefs");
        let v: Value =
            http.get(&url).query(&[("filter[name]", *name)]).send().await?.error_for_status()?.json().await?;
        let id = v["data"]
            .as_array()
            .and_then(|a| a.iter().find(|d| d["name"] == *name))
            .and_then(|d| d["id"].as_u64())
            .ok_or_else(|| anyhow::anyhow!("dataref {name} not found"))?;
        ids.insert(id, i);
    }
    Ok(ids)
}

async fn session(
    port: u16,
    atx: &watch::Sender<Option<AircraftState>>,
    ctx: &watch::Sender<bool>,
    frames: &parking_lot::Mutex<Frames>,
) -> anyhow::Result<()> {
    // A handful of lookups per session: no pooling, so servers that close
    // connections after each response work too.
    let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).pool_max_idle_per_host(0).build()?;
    let ids = resolve_ids(&http, port).await?;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/v2")).await?;
    let sub = json!({
        "req_id": 1,
        "type": "dataref_subscribe_values",
        "params": { "datarefs": ids.keys().map(|id| json!({ "id": id })).collect::<Vec<_>>() }
    });
    ws.send(Message::Text(sub.to_string().into())).await?;
    let _ = ctx.send(true);
    tracing::info!("connected to X-Plane Web API on port {port}");
    let mut values = [f64::NAN; DATAREFS.len()];
    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let Message::Text(text) = msg else { continue };
        let v: Value = match serde_json::from_str(text.as_str()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"] != "dataref_update_values" {
            continue;
        }
        if let Some(data) = v["data"].as_object() {
            for (k, val) in data {
                if let (Ok(id), Some(x)) = (k.parse::<u64>(), val.as_f64()) {
                    if let Some(&i) = ids.get(&id) {
                        values[i] = x;
                    }
                }
            }
        }
        {
            let mut f = frames.lock();
            f.last_update = Some(Instant::now());
            if values[5].is_finite() {
                f.period_s = values[5];
            }
        }
        if values[0].is_finite() && values[1].is_finite() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let _ = atx.send(Some(AircraftState {
                lat: values[0],
                lon: values[1],
                groundspeed_ms: if values[2].is_finite() { values[2].clamp(0.0, 1000.0) } else { 0.0 },
                track_deg: if values[3].is_finite() { values[3] } else { 0.0 },
                agl_m: if values[4].is_finite() { values[4] } else { 0.0 },
                unix_time: now,
            }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(connected: bool, f: Frames) -> WebApi {
        let (_a, aircraft) = watch::channel(None);
        let (_c, conn) = watch::channel(connected);
        WebApi { aircraft, connected: conn, frames: Arc::new(parking_lot::Mutex::new(f)) }
    }

    #[test]
    fn loading_signal() {
        let now = Some(Instant::now());
        assert!(api(false, Frames { last_update: now, period_s: 0.02 }).loading(), "not connected");
        assert!(api(true, Frames::default()).loading(), "connected, no update yet (flight loading)");
        assert!(!api(true, Frames { last_update: now, period_s: 0.02 }).loading(), "flying");
        assert!(api(true, Frames { last_update: now, period_s: 2.0 }).loading(), "frames still slow after load");
        let stale = Some(Instant::now() - Duration::from_secs(3));
        assert!(api(true, Frames { last_update: stale, period_s: 0.02 }).loading(), "updates stopped");
    }
}
