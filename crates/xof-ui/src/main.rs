//! ButterSat control UI (design §10). A pure control plane: all work happens
//! in `xof-daemon`, which this app starts if it isn't running and talks to
//! over the local IPC socket. Closing the window never unmounts.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Mutex;
use xof_proto::Client;

#[derive(Default)]
struct Conn(Mutex<Option<Client>>);

/// Path of the daemon binary shipped next to this executable.
fn daemon_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(windows) { "xof-daemon.exe" } else { "xof-daemon" };
    let p = dir.join(name);
    p.exists().then_some(p)
}

/// Start the daemon detached so it outlives the UI.
fn spawn_daemon() -> anyhow::Result<()> {
    let path = daemon_path().ok_or_else(|| anyhow::anyhow!("xof-daemon not found next to the app"))?;
    let mut cmd = std::process::Command::new(path);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    Ok(())
}

/// Connect, starting the daemon if needed; forwards daemon events to the
/// window as `daemon-event`.
async fn ensure(app: &AppHandle, conn: &Conn) -> Result<Client, String> {
    let mut guard = conn.0.lock().await;
    if let Some(c) = guard.as_ref() {
        return Ok(c.clone());
    }
    let mut client = Client::connect().await.ok();
    if client.is_none() {
        spawn_daemon().map_err(|e| e.to_string())?;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Ok(c) = Client::connect().await {
                client = Some(c);
                break;
            }
        }
    }
    let client = client.ok_or("could not connect to xof-daemon")?;
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

/// Licence/attribution text shown in About.
#[tauri::command]
fn about() -> Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "licence": "MIT",
        "notices": [
            "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos — https://github.com/winfsp/winfsp",
            "MapLibre GL JS — BSD-3-Clause",
            "Mesh data © OpenStreetMap contributors (ODbL)",
            "Elevation: USGS 3DEP (public domain) / Copernicus DEM",
            "Imagery © the selected provider (Esri World Imagery or Bing Maps)",
        ]
    })
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Conn::default())
        .invoke_handler(tauri::generate_handler![rpc, about])
        .run(tauri::generate_context!())
        .expect("error while running ButterSat");
}
