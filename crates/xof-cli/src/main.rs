//! xof-cli: control the ButterSat daemon, or run a standalone mount for
//! testing (`xof-cli run`).

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use xof_proto::{method, CacheStats, Client, Event, HealthReport, PackInfo, PrepareResult, Severity, Status};

#[derive(Parser)]
#[command(name = "xof-cli", version, about = "Control the ButterSat daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon status.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Health checks.
    Health,
    /// Mount the scenery.
    Mount,
    /// Unmount the scenery.
    Unmount,
    /// Switch ButterSat on: start the daemon, mount, and add or re-enable our
    /// scenery_packs.ini entry. Stays on across restarts.
    On,
    /// Switch ButterSat off: unmount, disable our scenery_packs.ini entry, stop
    /// the daemon, and keep it from starting at login. Refused while X-Plane
    /// is running.
    Off,
    /// Stop the daemon without switching ButterSat off (it starts again with
    /// the app or at login).
    Shutdown,
    /// Clean up ButterSat before uninstalling: stop the daemon, unmount, remove
    /// our `scenery_packs.ini` entry and login registration, and optionally
    /// delete cached imagery and meshes.
    Uninstall {
        /// Also delete user config, logs, and cached imagery/meshes.
        #[arg(long)]
        remove_cache: bool,
    },
    /// Show the configuration (TOML).
    Config,
    /// Set a configuration value, e.g. `imagery.provider bing`.
    Set { key: String, value: String },
    /// Cache usage and location.
    Cache,
    /// Move the caches to another folder or drive (X-Plane must be closed).
    CacheMove {
        path: String,
        /// Start empty there instead of moving the current cache (the old
        /// folder is left in place).
        #[arg(long)]
        fresh: bool,
    },
    /// Refresh the mesh catalog.
    CatalogRefresh,
    /// Prefetch meshes and imagery around an airport (ICAO) before flying.
    Prepare { icao: String },
    /// Prefetch along your latest SimBrief route.
    Route {
        #[arg(long)]
        simbrief_user: Option<String>,
    },
    /// List imported packs.
    Packs,
    /// Import an Ortho4XP/AutoOrtho pack folder.
    Import { path: PathBuf },
    /// Follow daemon logs and stats.
    Watch,
    /// Standalone mount without the daemon (testing / M1): imported packs +
    /// on-demand textures, no catalog.
    Run {
        /// Ortho4XP-style pack folder(s).
        #[arg(long, required = true)]
        pack: Vec<PathBuf>,
        /// Mount point (Windows: must not exist).
        #[arg(long)]
        mount: PathBuf,
        #[arg(long, default_value = "esri")]
        provider: String,
        #[arg(long, default_value_t = 1200)]
        deadline_ms: u64,
        /// Imagery cache file (default: a temp dir).
        #[arg(long)]
        cache: Option<PathBuf>,
    },
}

fn set_path(v: &mut Value, key: &str, val: &str) -> anyhow::Result<()> {
    let mut cur = v;
    let parts: Vec<&str> = key.split('.').collect();
    for p in &parts[..parts.len() - 1] {
        cur = cur.get_mut(*p).ok_or_else(|| anyhow::anyhow!("unknown section '{p}'"))?;
    }
    let last = parts[parts.len() - 1];
    let slot = cur.get_mut(last).ok_or_else(|| anyhow::anyhow!("unknown key '{key}'"))?;
    *slot = match slot {
        Value::Bool(_) => Value::Bool(val.parse()?),
        Value::Number(_) => serde_json::from_str(val)?,
        Value::Array(_) => {
            Value::Array(val.split(',').filter(|s| !s.is_empty()).map(|s| Value::String(s.trim().into())).collect())
        }
        _ if val.is_empty() || val == "none" => Value::Null,
        _ => Value::String(val.into()),
    };
    Ok(())
}

fn print_status(s: &Status) {
    println!("ButterSat {}", s.version);
    println!("  mounted:     {} {}", s.mounted, s.mount_point.as_deref().unwrap_or(""));
    if let Some(e) = &s.mount_error {
        println!("  mount error: {e}");
    }
    println!("  X-Plane:     {}", s.xplane.as_ref().map(|x| x.path.as_str()).unwrap_or("not found"));
    println!("  sim:         {}", if s.sim_connected { "connected" } else { "not running" });
    if let Some(a) = &s.aircraft {
        println!(
            "  aircraft:    {:.4} {:.4}  {:.0} kt  {:.0} ft AGL",
            a.lat,
            a.lon,
            a.groundspeed_ms * 1.943_84,
            a.agl_m * 3.280_84
        );
    }
    println!(
        "  imagery:     {}  reads {}  tiles fetched {}  degraded {}  upgrades {}",
        s.provider, s.engine.reads, s.engine.fetch_requests, s.engine.degraded_built, s.engine.upgrades
    );
    if !s.provider_attribution.is_empty() {
        println!("  attribution: {}", s.provider_attribution);
    }
    if let Some(m) = &s.mesh {
        println!(
            "  mesh:        catalog {} ({} tiles, {}, {}){}  downloads {}",
            m.catalog_version,
            m.tiles_in_catalog,
            if m.online { "online" } else { "offline" },
            if m.license.is_empty() { "ODbL-1.0" } else { &m.license },
            m.pending_update.as_deref().map(|v| format!(", update {v} pending")).unwrap_or_default(),
            m.downloads
        );
        if !m.attribution.is_empty() {
            println!("  mesh attr:   {}", m.attribution.join(" · "));
        }
    }
    println!(
        "  prefetch:    queued {}  mesh {}  imagery {}{}",
        s.prefetch.queued,
        s.prefetch.mesh_tiles_done,
        s.prefetch.imagery_tiles_done,
        if s.prefetch.reduced_zoom { "  (dynamic zoom: reduced)" } else { "" }
    );
}

async fn run_standalone(
    pack: Vec<PathBuf>,
    mount: PathBuf,
    provider: String,
    deadline_ms: u64,
    cache: Option<PathBuf>,
) -> anyhow::Result<()> {
    use xof_core::cache::TileStore;
    use xof_core::engine::{EngineConfig, TextureEngine};
    let p = xof_core::provider::builtin(&provider).ok_or_else(|| anyhow::anyhow!("unknown provider {provider}"))?;
    let tmp;
    let cache = match cache {
        Some(c) => c,
        None => {
            tmp = std::env::temp_dir().join("xof-cli-cache");
            tmp.join("imagery.redb")
        }
    };
    let store = TileStore::open(&cache)?;
    let engine = TextureEngine::new(EngineConfig { deadline_ms, ..Default::default() }, p, store)?;
    let mut names: Vec<String> = xof_vfs::process::DEFAULT_XPLANE_PROCESSES.iter().map(|s| s.to_string()).collect();
    names.push("xof-cli".into());
    let scenery = xof_vfs::Scenery::new(
        xof_vfs::SceneryConfig { packs: pack, ter: Default::default(), xplane_processes: names },
        None,
        engine.clone(),
        tokio::runtime::Handle::current(),
    );
    let mp = mount.clone();
    let mounted = tokio::task::spawn_blocking(move || xof_vfs::mount(scenery, &mp)).await??;
    println!("mounted at {} — press Ctrl+C to unmount", mount.display());
    let e = Arc::new(engine);
    let stats = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tick.tick().await;
            let s = e.stats();
            eprintln!(
                "reads {}  fetched {} ({} MB)  degraded {}  upgrades {}",
                s.reads,
                s.fetch_requests,
                s.fetch_bytes >> 20,
                s.degraded_built,
                s.upgrades
            );
        }
    });
    tokio::signal::ctrl_c().await?;
    stats.abort();
    tokio::task::spawn_blocking(move || drop(mounted)).await?;
    Ok(())
}

async fn stop_daemon_and_wait() -> anyhow::Result<bool> {
    let Ok(c) = Client::connect().await else {
        return Ok(false);
    };
    let _ = c.call::<_, Value>(method::SHUTDOWN, Value::Null).await;
    let lock_path = xof_proto::config::Dirs::standard().data.join("daemon.lock");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let connected = Client::connect().await.is_ok();
        let locked = std::fs::OpenOptions::new().write(true).open(&lock_path).is_ok_and(|f| f.try_lock().is_err());
        if !connected && !locked {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(true)
}

fn disable_autostart() {
    #[cfg(windows)]
    {
        if std::env::var_os("XOF_HOME").is_none() {
            let _ = std::process::Command::new("reg")
                .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "ButterSat", "/f"])
                .output();
        }
    }
}

async fn run_uninstall(remove_cache: bool) -> anyhow::Result<()> {
    let _ = stop_daemon_and_wait().await;
    let dirs = xof_proto::config::Dirs::standard();
    let cfg = xof_proto::config::DaemonConfig::load(&dirs.config_file).unwrap_or_default();

    disable_autostart();

    if let Some(mp) = &cfg.mount.path {
        let _ = std::fs::remove_dir(mp);
    }
    let mut xp_paths = Vec::new();
    if let Some(p) = &cfg.xplane.path {
        xp_paths.push(xof_proto::xplane::native_path(p));
    }
    if std::env::var_os("XOF_HOME").is_none() {
        for p in xof_proto::xplane::detect_installs() {
            if !xp_paths.contains(&p) {
                xp_paths.push(p);
            }
        }
    }
    for xp in &xp_paths {
        if let Ok(true) = xof_proto::xplane::remove_scenery_pack(xp) {
            println!("removed {} from {}/Custom Scenery/scenery_packs.ini", xof_proto::xplane::PACK_NAME, xp.display());
        }
    }

    if remove_cache {
        if let Some(relocated) = &cfg.storage.cache_path {
            let _ = std::fs::remove_file(relocated.join("imagery.redb"));
            let _ = std::fs::remove_dir_all(relocated.join("mesh"));
            let _ = std::fs::remove_dir(relocated);
        }
        let _ = std::fs::remove_dir_all(&dirs.cache);
        let _ = std::fs::remove_dir_all(&dirs.data);
        let _ = std::fs::remove_file(&dirs.config_file);
        if let Some(cfg_dir) = dirs.config_file.parent() {
            let _ = std::fs::remove_dir(cfg_dir);
        }
        println!("removed cached data and configuration");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { pack, mount, provider, deadline_ms, cache } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
                )
                .init();
            return run_standalone(pack, mount, provider, deadline_ms, cache).await;
        }
        Cmd::On => {
            let exe = std::env::current_exe()?;
            let c = xof_proto::power::turn_on(exe.parent().unwrap_or(std::path::Path::new("."))).await?;
            let s: Status = c.call(method::STATUS, Value::Null).await?;
            println!("ButterSat is on{}", if s.mounted { "" } else { " (mounting…)" });
            return Ok(());
        }
        Cmd::Off => {
            xof_proto::power::turn_off().await?;
            println!("ButterSat is off: scenery unmounted and disabled in scenery_packs.ini");
            return Ok(());
        }
        Cmd::Shutdown => {
            if stop_daemon_and_wait().await? {
                println!("daemon stopped");
            } else {
                println!("daemon not running");
            }
            return Ok(());
        }
        Cmd::Uninstall { remove_cache } => {
            return run_uninstall(remove_cache).await;
        }
        _ => {}
    }
    let c = Client::connect().await.map_err(|e| anyhow::anyhow!("cannot reach xof-daemon ({e}); is it running?"))?;
    match cli.cmd {
        Cmd::Status { json } => {
            let s: Status = c.call(method::STATUS, Value::Null).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_status(&s);
            }
        }
        Cmd::Health => {
            let h: HealthReport = c.call(method::HEALTH, Value::Null).await?;
            for ch in &h.checks {
                let mark = match ch.severity {
                    Severity::Ok => "ok  ",
                    Severity::Info => "info",
                    Severity::Warn => "WARN",
                    Severity::Error => "ERR ",
                };
                println!("[{mark}] {}: {}", ch.title, ch.message);
                if let Some(f) = &ch.fix {
                    if ch.severity != Severity::Ok {
                        println!("       → {f}");
                    }
                }
            }
        }
        Cmd::Mount => println!("mounted at {}", c.call::<_, String>(method::MOUNT, Value::Null).await?),
        Cmd::Unmount => {
            c.call::<_, Value>(method::UNMOUNT, Value::Null).await?;
            println!("unmounted");
        }
        Cmd::On | Cmd::Off | Cmd::Shutdown | Cmd::Uninstall { .. } => unreachable!(),
        Cmd::Config => {
            let v: xof_proto::config::DaemonConfig = c.call(method::GET_CONFIG, Value::Null).await?;
            print!("{}", toml::to_string_pretty(&v)?);
        }
        Cmd::Set { key, value } => {
            let mut v: Value = c.call(method::GET_CONFIG, Value::Null).await?;
            set_path(&mut v, &key, &value)?;
            let r: Value = c.call(method::SET_CONFIG, v).await?;
            println!("{}", r.get("note").and_then(Value::as_str).unwrap_or("applied"));
        }
        Cmd::Cache => {
            let s: CacheStats = c.call(method::CACHE_STATS, Value::Null).await?;
            println!(
                "imagery: {} MB / {} GB ({} tiles, {} areas)",
                s.imagery_bytes >> 20,
                s.imagery_cap_bytes >> 30,
                s.imagery_tiles,
                s.areas.len()
            );
            println!("mesh:    {} MB / {} GB", s.mesh_bytes >> 20, s.mesh_cap_bytes >> 30);
            let st: Status = c.call(method::STATUS, Value::Null).await?;
            println!("folder:  {} ({:.1} GB free)", st.storage.cache_path, st.storage.available_bytes as f64 / 1e9);
        }
        Cmd::CacheMove { path, fresh } => {
            let info: xof_proto::StorageInfo =
                c.call(method::STORAGE_INFO, serde_json::json!({ "path": path })).await?;
            anyhow::ensure!(info.writable, "can't write to {}", info.path);
            println!(
                "{} {} -> {} ({:.1} GB; {:.1} GB free there)",
                if fresh { "starting fresh:" } else { "moving" },
                info.current_path,
                info.path,
                info.current_cache_bytes as f64 / 1e9,
                info.available_bytes as f64 / 1e9
            );
            let mode = if fresh { "fresh" } else { "move" };
            let _: Value = c.call(method::STORAGE_RELOCATE, serde_json::json!({ "path": path, "mode": mode })).await?;
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let st: Status = c.call(method::STATUS, Value::Null).await?;
                let Some(j) = st.storage.job else { continue };
                let pct = if j.total_bytes > 0 { j.done_bytes * 100 / j.total_bytes } else { 100 };
                eprint!("\r{:>3}% {}          ", pct, j.message);
                match j.state.as_str() {
                    "done" => {
                        eprintln!();
                        println!("{}", j.message);
                        break;
                    }
                    "failed" => {
                        eprintln!();
                        anyhow::bail!("{}", j.message);
                    }
                    _ => {}
                }
            }
        }
        Cmd::CatalogRefresh => {
            let v: Value = c.call(method::CATALOG_REFRESH, Value::Null).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Prepare { icao } => {
            let r: PrepareResult = c.call(method::PREPARE_DEPARTURE, json!({ "icao": icao })).await?;
            println!(
                "{} ({:.4}, {:.4}): queued {} mesh tiles, {} imagery tiles",
                r.name.unwrap_or_default(),
                r.lat,
                r.lon,
                r.mesh_tiles,
                r.imagery_tiles
            );
        }
        Cmd::Route { simbrief_user } => {
            let r: Value = c.call(method::PREFETCH_ROUTE, json!({ "simbrief_user": simbrief_user })).await?;
            println!("{r}");
        }
        Cmd::Packs => {
            let packs: Vec<PackInfo> = c.call(method::PACKS_LIST, Value::Null).await?;
            for p in packs {
                println!("{}  ({} tiles)", p.path, p.tiles.len());
            }
        }
        Cmd::Import { path } => {
            let dest: String = c.call(method::PACKS_IMPORT, json!({ "path": path })).await?;
            println!("imported to {dest}");
        }
        Cmd::Watch => {
            let mut rx = c.subscribe().await?;
            while let Ok(ev) = rx.recv().await {
                match ev {
                    Event::Log(l) => println!("[{}] {}", l.level, l.message),
                    Event::Stats(s) => eprintln!(
                        "reads {}  fetched {}  degraded {}  queued {}",
                        s.engine.reads, s.engine.fetch_requests, s.engine.degraded_built, s.prefetch.queued
                    ),
                    Event::Aircraft(_) | Event::Health(_) => {}
                }
            }
        }
        Cmd::Run { .. } => unreachable!(),
    }
    Ok(())
}
