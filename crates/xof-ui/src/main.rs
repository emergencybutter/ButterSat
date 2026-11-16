//! ButterSat control UI (design §10). A pure control plane: all work happens
//! in `xof-daemon`, which this app starts if it isn't running and talks to
//! over the local IPC socket. Closing the window never unmounts; the On/Off
//! switch ([`xof_proto::power`]) does.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;
use xof_proto::config::{DaemonConfig, Dirs};
use xof_proto::{method, power, Client, Status};

#[derive(Default)]
struct Conn(Mutex<Option<Client>>);

/// This executable's folder, where the daemon binary ships.
fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."))
}

/// The persisted On/Off switch.
fn switched_on() -> bool {
    DaemonConfig::load(&Dirs::standard().config_file).map(|c| c.startup.enabled).unwrap_or(true)
}

const OFF: &str = "ButterSat is off";

/// Connect, starting the daemon if ButterSat is switched on; forwards daemon
/// events to the window as `daemon-event`. Errors with [`OFF`] when switched
/// off and no daemon is running.
async fn ensure(app: &AppHandle, conn: &Conn) -> Result<Client, String> {
    let mut guard = conn.0.lock().await;
    if let Some(c) = guard.as_ref() {
        return Ok(c.clone());
    }
    let client = match Client::connect().await {
        Ok(c) => c,
        Err(_) if !switched_on() => return Err(OFF.into()),
        Err(_) => power::turn_on(&exe_dir()).await.map_err(|e| e.to_string())?,
    };
    let mut rx = client.subscribe().await.map_err(|e| e.to_string())?;
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            let _ = app2.emit("daemon-event", ev);
        }
        // Daemon went away: drop the cached client so the next call reconnects.
        if let Some(conn) = app2.try_state::<Conn>() {
            *conn.0.lock().await = None;
        }
        let _ = app2.emit("daemon-disconnected", ());
    });
    *guard = Some(client.clone());
    Ok(client)
}

/// Generic JSON-RPC passthrough to the daemon.
#[tauri::command]
async fn rpc(app: AppHandle, conn: State<'_, Conn>, method: String, params: Option<Value>) -> Result<Value, String> {
    let client = ensure(&app, &conn).await?;
    match client.call::<_, Value>(&method, params.unwrap_or(Value::Null)).await {
        Ok(v) => Ok(v),
        Err(e) => {
            if e.to_string().contains("closed") {
                *conn.0.lock().await = None;
            }
            Err(e.to_string())
        }
    }
}

/// The On/Off switch state.
#[tauri::command]
fn power_state() -> Value {
    serde_json::json!({ "on": switched_on() })
}

/// Switch on: persist, start the daemon (it mounts and enables our
/// scenery_packs.ini entry).
#[tauri::command]
async fn power_on(app: AppHandle, conn: State<'_, Conn>) -> Result<Value, String> {
    power::turn_on(&exe_dir()).await.map_err(|e| e.to_string())?;
    ensure(&app, &conn).await?;
    Ok(power_state())
}

/// Switch off: unmount, disable our scenery_packs.ini entry, stop the daemon.
/// Refused by the daemon while X-Plane is connected.
#[tauri::command]
async fn power_off(conn: State<'_, Conn>) -> Result<Value, String> {
    power::turn_off().await.map_err(|e| e.to_string())?;
    *conn.0.lock().await = None;
    Ok(power_state())
}

/// Licence/attribution text shown in About.
#[tauri::command]
fn about() -> Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "licence": "MIT",
        "notices": [
            "Mesh catalog licensed under ODbL 1.0 (https://opendatacommons.org/licenses/odbl/1-0/) — © OpenStreetMap contributors",
            "Elevation data: U.S. Geological Survey 3D Elevation Program (3DEP, public domain, 17 U.S.C. § 105) / Copernicus DEM GLO-30",
            "US open imagery: U.S. Geological Survey (USGS) The National Map (3DEP / NAIP / HRO) — public domain (17 U.S.C. § 105)",
            "France open imagery: © IGN France — Géoplateforme BD ORTHO (Licence Ouverte / Open Licence Etalab 2.0)",
            "Switzerland open imagery: © swisstopo — SWISSIMAGE (Open Government Data, free use with source attribution)",
            "Netherlands open imagery: Beeldmateriaal Nederland / PDOK Luchtfoto (CC BY 4.0)",
            "Spain open imagery: © Instituto Geográfico Nacional — PNOA (CC BY 4.0)",
            "Austria open imagery: Datenquelle: basemap.at (CC BY 4.0)",
            "Commercial fallback imagery (opt-in only): Esri World Imagery (© Esri, Maxar, Earthstar Geographics, and the GIS User Community) / Bing Maps Aerial (© Microsoft)",
            "Flight plan route data: SimBrief / Navigraph (https://www.simbrief.com)",
            "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos — https://github.com/winfsp/winfsp",
            "MapLibre GL JS — BSD-3-Clause",
        ]
    })
}

/// Check the update feed (`https://satsrv.butr.cc/updates/latest.json`).
#[tauri::command]
async fn update_check(app: AppHandle) -> Result<Value, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(u) => Ok(serde_json::json!({
            "available": true,
            "current_version": u.current_version,
            "version": u.version,
            "body": u.body,
        })),
        None => Ok(serde_json::json!({
            "available": false,
            "current_version": env!("CARGO_PKG_VERSION"),
        })),
    }
}

/// Download and install a pending update. Refuses while X-Plane is connected so
/// an active flight is never interrupted.
#[tauri::command]
async fn update_install(app: AppHandle, conn: State<'_, Conn>) -> Result<Value, String> {
    if let Ok(client) = ensure(&app, &conn).await {
        if let Ok(st) = client.call::<_, Status>(method::STATUS, Value::Null).await {
            if st.sim_connected {
                return Err("Close X-Plane before installing an update so your flight is not interrupted.".into());
            }
        }
    }
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "installed": false, "message": "Already up to date." }));
    };
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| e.to_string())?;
    if let Ok(client) = ensure(&app, &conn).await {
        if let Ok(st) = client.call::<_, Status>(method::STATUS, Value::Null).await {
            if st.sim_connected {
                return Err("Close X-Plane before installing an update so your flight is not interrupted.".into());
            }
        }
        let _ = client.call::<_, Value>(method::SHUTDOWN, Value::Null).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    update.install(&bytes).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "installed": true, "version": update.version }))
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Conn::default())
        .invoke_handler(tauri::generate_handler![
            rpc,
            power_state,
            power_on,
            power_off,
            about,
            update_check,
            update_install
        ])
        .run(tauri::generate_context!())
        .expect("error while running ButterSat");
}
