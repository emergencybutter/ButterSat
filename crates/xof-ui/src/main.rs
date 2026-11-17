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

// ------------------------------------------------------------------ updates
//
// The NSIS installer carries the app *and* its sidecars (xof-daemon, xof-cli),
// so one update replaces all three. Sequence: we stop the daemon (plain
// shutdown, the On/Off switch is untouched), the plugin starts the installer
// with /UPDATE and exits; the installer's PREINSTALL hook stops any daemon
// again, replaces the files and relaunches the app, which starts the new
// daemon if ButterSat is on.

const SIM_BUSY: &str = "Close X-Plane before installing an update so your flight is not interrupted.";

/// The updater; `BUTTERSAT_UPDATE_ENDPOINT` overrides the feed (testing only:
/// a plain-http feed also needs `dangerousInsecureTransportProtocol`).
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut b = app.updater_builder();
    if let Ok(url) = std::env::var("BUTTERSAT_UPDATE_ENDPOINT") {
        let url = url.parse().map_err(|e| format!("BUTTERSAT_UPDATE_ENDPOINT: {e}"))?;
        b = b.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    b.build().map_err(|e| e.to_string())
}

/// Connect only if a daemon already runs (never start one just to ask).
async fn ensure_if_running(app: &AppHandle, conn: &Conn) -> Result<Client, String> {
    if conn.0.lock().await.is_some() || Client::connect().await.is_ok() {
        return ensure(app, conn).await;
    }
    Err("daemon not running".into())
}

fn xplane_process_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq X-Plane.exe", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("X-Plane.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("pgrep")
            .args(["-f", "X-Plane"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// X-Plane is running or connected: never install then.
async fn sim_busy(app: &AppHandle, conn: &Conn) -> bool {
    if xplane_process_running() {
        return true;
    }
    if let Ok(client) = ensure_if_running(app, conn).await {
        if let Ok(st) = client.call::<_, Status>(method::STATUS, Value::Null).await {
            return st.sim_connected;
        }
    }
    false
}

/// Download and install `update` (on Windows the process exits and the
/// installer relaunches the app). Refuses while X-Plane is busy; stops the
/// daemon so its files can be replaced.
/// One install at a time: the background updater and the banner's button
/// used to start two parallel downloads of the same installer.
static INSTALLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

async fn install(app: &AppHandle, conn: &Conn, update: tauri_plugin_updater::Update) -> Result<Value, String> {
    use std::sync::atomic::Ordering;
    if INSTALLING.swap(true, Ordering::SeqCst) {
        // Already downloading: the window follows it via update-progress.
        return Ok(serde_json::json!({ "installed": false, "message": "already installing" }));
    }
    let r = install_once(app, conn, update).await;
    // Reached only on failure (a successful install exits the process).
    INSTALLING.store(false, Ordering::SeqCst);
    r
}

async fn install_once(app: &AppHandle, conn: &Conn, update: tauri_plugin_updater::Update) -> Result<Value, String> {
    if sim_busy(app, conn).await {
        return Err(SIM_BUSY.into());
    }
    let _ = app.emit("update-installing", &update.version);
    // Progress for the banner (a slow download looked like a failure).
    let (app2, version) = (app.clone(), update.version.clone());
    let mut received: u64 = 0;
    let mut last = std::time::Instant::now() - Duration::from_secs(1);
    let bytes = update
        .download(
            move |chunk, total| {
                received += chunk as u64;
                if last.elapsed() >= Duration::from_millis(250) || Some(received) == total {
                    last = std::time::Instant::now();
                    let _ = app2.emit(
                        "update-progress",
                        serde_json::json!({ "version": version, "received": received, "total": total }),
                    );
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if sim_busy(app, conn).await {
        return Err(SIM_BUSY.into());
    }
    if let Ok(client) = ensure_if_running(app, conn).await {
        let _ = client.call::<_, Value>(method::SHUTDOWN, Value::Null).await;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if Client::connect().await.is_err() {
                break;
            }
        }
    }
    update.install(&bytes).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "installed": true, "version": update.version }))
}

fn auto_update_enabled() -> bool {
    DaemonConfig::load(&Dirs::standard().config_file).map(|c| c.startup.auto_update).unwrap_or(true)
}

/// One background round: check, wait for X-Plane to close, install.
async fn auto_update_once(app: &AppHandle) -> Result<(), String> {
    let Some(update) = updater(app)?.check().await.map_err(|e| e.to_string())? else {
        return Ok(());
    };
    let _ = app.emit("update-available", &update.version);
    let conn = app.state::<Conn>();
    while sim_busy(app, &conn).await {
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
    install(app, &conn, update).await.map(|_| ())
}

/// Background updates: shortly after start, then every 6 h.
async fn auto_update(app: AppHandle) {
    let delay = std::env::var("BUTTERSAT_UPDATE_DELAY_S").ok().and_then(|s| s.parse().ok()).unwrap_or(20);
    tokio::time::sleep(Duration::from_secs(delay)).await;
    loop {
        if auto_update_enabled() {
            if let Err(e) = auto_update_once(&app).await {
                let _ = app.emit("update-error", e);
            }
        }
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}

/// Check the update feed (`https://satsrv.butr.cc/updates/latest.json`).
#[tauri::command]
async fn update_check(app: AppHandle) -> Result<Value, String> {
    let updater = updater(&app)?;
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

/// Download and install a pending update now (the banner's button). Refuses
/// while X-Plane runs so an active flight is never interrupted.
#[tauri::command]
async fn update_install(app: AppHandle, conn: State<'_, Conn>) -> Result<Value, String> {
    let Some(update) = updater(&app)?.check().await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "installed": false, "message": "Already up to date." }));
    };
    install(&app, &conn, update).await
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Conn::default())
        .setup(|app| {
            tauri::async_runtime::spawn(auto_update(app.handle().clone()));
            Ok(())
        })
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
