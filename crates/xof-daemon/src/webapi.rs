//! X-Plane 12 Web API client (design §9): resolve dataref ids over REST, then
//! subscribe to position updates over the websocket. Reconnects forever;
//! connection state doubles as "is X-Plane running".

use std::collections::HashMap;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use xof_proto::AircraftState;

const DATAREFS: [&str; 5] = [
    "sim/flightmodel/position/latitude",
    "sim/flightmodel/position/longitude",
    "sim/flightmodel/position/groundspeed",
    "sim/flightmodel/position/hpath",
    "sim/flightmodel/position/y_agl",
];

pub struct WebApi {
    pub aircraft: watch::Receiver<Option<AircraftState>>,
    pub connected: watch::Receiver<bool>,
}

pub fn spawn(port: u16) -> WebApi {
    let (atx, arx) = watch::channel(None);
    let (ctx, crx) = watch::channel(false);
    tokio::spawn(async move {
        loop {
            match session(port, &atx, &ctx).await {
                Ok(()) => tracing::info!("X-Plane Web API session ended"),
                Err(e) => tracing::debug!("X-Plane Web API: {e:#}"),
            }
            let _ = ctx.send(false);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    WebApi { aircraft: arx, connected: crx }
}

async fn resolve_ids(http: &reqwest::Client, port: u16) -> anyhow::Result<HashMap<u64, usize>> {
    let mut ids = HashMap::new();
    for (i, name) in DATAREFS.iter().enumerate() {
        let url = format!("http://127.0.0.1:{port}/api/v2/datarefs");
        let v: Value = http.get(&url).query(&[("filter[name]", *name)]).send().await?.error_for_status()?.json().await?;
        let id = v["data"]
            .as_array()
            .and_then(|a| a.iter().find(|d| d["name"] == *name))
            .and_then(|d| d["id"].as_u64())
            .ok_or_else(|| anyhow::anyhow!("dataref {name} not found"))?;
        ids.insert(id, i);
    }
    Ok(ids)
}

async fn session(port: u16, atx: &watch::Sender<Option<AircraftState>>, ctx: &watch::Sender<bool>) -> anyhow::Result<()> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
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
    let mut values = [f64::NAN; 5];
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
        if values[0].is_finite() && values[1].is_finite() {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
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
