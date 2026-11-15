//! Daemon state: owns the engine, mesh streamer, scenery, mount, prefetcher
//! and handles IPC requests.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};
use xof_core::cache::TileStore;
use xof_core::engine::{EngineConfig, TextureEngine};
use xof_core::names::TerParams;
use xof_core::{provider, DsfTile};
use xof_mesh::{MeshConfig, MeshStreamer};
use xof_proto::config::{DaemonConfig, Dirs, DEFAULT_MESH_KEYS};
use xof_proto::{method, CacheStats, EvictArea, Event, HealthCheck, HealthReport, PackInfo, PrepareDeparture, PrepareResult, Severity, Status, XPlaneInfo};
use xof_vfs::{Scenery, SceneryConfig};

use crate::prefetch::Prefetcher;
use crate::webapi::WebApi;
use crate::{airports, xplane};

/// Components rebuilt when configuration changes.
struct Stack {
    engine: TextureEngine,
    mesh: Option<Arc<MeshStreamer>>,
    scenery: Arc<Scenery>,
}

pub struct App {
    pub dirs: Dirs,
    config_path: PathBuf,
    cfg: Mutex<DaemonConfig>,
    store: Arc<TileStore>,
    stack: Mutex<Stack>,
    mount: Mutex<Option<xof_vfs::Mounted>>,
    mount_error: Mutex<Option<String>>,
    xplane: Mutex<Option<PathBuf>>,
    pub prefetch: Arc<Prefetcher>,
    pub web: WebApi,
    pub events: broadcast::Sender<Event>,
    pub shutdown: watch::Sender<bool>,
    rt: tokio::runtime::Handle,
}

fn trusted_keys(cfg: &DaemonConfig) -> Vec<ed25519_dalek::VerifyingKey> {
    DEFAULT_MESH_KEYS
        .iter()
        .map(|s| s.to_string())
        .chain(cfg.mesh.public_keys.iter().cloned())
        .filter_map(|k| match xof_mesh::catalog::parse_public_key(&k) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("ignoring invalid mesh public key: {e}");
                None
            }
        })
        .collect()
}

/// Imported packs: every folder in the packs dir plus configured extras.
pub fn imported_packs(dirs: &Dirs, cfg: &DaemonConfig) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dirs.packs())
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.join("Earth nav data").is_dir()).collect())
        .unwrap_or_default();
    v.sort();
    v.extend(cfg.packs.extra.iter().cloned());
    v
}

fn build_stack(dirs: &Dirs, cfg: &DaemonConfig, store: Arc<TileStore>, xp: Option<&Path>, rt: &tokio::runtime::Handle) -> anyhow::Result<Stack> {
    let provider = provider::builtin(&cfg.imagery.provider)
        .ok_or_else(|| anyhow::anyhow!("unknown imagery provider '{}'", cfg.imagery.provider))?;
    let engine = {
        let _g = rt.enter();
        TextureEngine::new(
            EngineConfig {
                deadline_ms: cfg.imagery.deadline_ms,
                segment_ram_mb: cfg.imagery.segment_ram_mb,
                decoded_ram_mb: cfg.imagery.decoded_ram_mb,
                ..Default::default()
            },
            provider,
            store,
        )?
    };
    let mesh = if cfg.mesh.enabled {
        let pinned: HashSet<DsfTile> = cfg.mesh.pinned.iter().filter_map(|s| DsfTile::parse(s)).collect();
        Some(MeshStreamer::new(
            MeshConfig {
                bootstrap_mirrors: cfg.mesh.mirrors.clone(),
                cache_dir: dirs.cache.join("mesh"),
                global_scenery: xp.and_then(xplane::global_scenery),
                coverage: cfg.mesh.coverage.clone(),
                pinned,
            },
            trusted_keys(cfg),
        )?)
    } else {
        None
    };
    let mut names = cfg.xplane.process_names.clone();
    names.extend(xof_vfs::process::DEFAULT_XPLANE_PROCESSES.iter().map(|s| s.to_string()));
    let scenery = Scenery::new(
        SceneryConfig { packs: imported_packs(dirs, cfg), ter: TerParams::default(), xplane_processes: names },
        mesh.clone(),
        engine.clone(),
        rt.clone(),
    );
    Ok(Stack { engine, mesh, scenery })
}

impl App {
    pub fn new(dirs: Dirs, config_path: PathBuf, rt: tokio::runtime::Handle) -> anyhow::Result<Arc<Self>> {
        let cfg = DaemonConfig::load(&config_path)?;
        let xp = xplane::resolve(cfg.xplane.path.as_deref());
        match &xp {
            Some(p) => tracing::info!("X-Plane 12 found at {}", p.display()),
            None => tracing::warn!("X-Plane 12 not found"),
        }
        let store = TileStore::open(&dirs.cache.join("imagery.redb"))?;
        let stack = build_stack(&dirs, &cfg, store.clone(), xp.as_deref(), &rt)?;
        let port = if cfg.xplane.web_api_port == 0 { 8086 } else { cfg.xplane.web_api_port };
        let web = crate::webapi::spawn(port);
        let prefetch = Prefetcher::spawn(cfg.prefetch.clone(), stack.engine.clone(), stack.mesh.clone(), web.aircraft.clone());
        let (events, _) = broadcast::channel(512);
        let (shutdown, _) = watch::channel(false);
        Ok(Arc::new(Self {
            dirs,
            config_path,
            cfg: Mutex::new(cfg),
            store,
            stack: Mutex::new(stack),
            mount: Mutex::new(None),
            mount_error: Mutex::new(None),
            xplane: Mutex::new(xp),
            prefetch,
            web,
            events,
            shutdown,
            rt,
        }))
    }

    pub fn config(&self) -> DaemonConfig {
        self.cfg.lock().clone()
    }

    fn mount_point(&self) -> Option<PathBuf> {
        let cfg = self.cfg.lock();
        cfg.mount.path.clone().or_else(|| self.xplane.lock().as_deref().map(xplane::default_mount_point))
    }

    pub fn is_mounted(&self) -> bool {
        self.mount.lock().is_some()
    }

    /// Mount (idempotent). Blocking-safe: call from a blocking context.
    pub fn mount(&self) -> anyhow::Result<PathBuf> {
        if self.is_mounted() {
            return self.mount_point().ok_or_else(|| anyhow::anyhow!("no mount point"));
        }
        let res = (|| {
            let mp = self.mount_point().ok_or_else(|| anyhow::anyhow!("X-Plane 12 not found and no mount path configured"))?;
            prepare_mount_point(&mp)?;
            let scenery = self.stack.lock().scenery.clone();
            let m = xof_vfs::mount(scenery, &mp)?;
            *self.mount.lock() = Some(m);
            if self.cfg.lock().mount.manage_scenery_packs {
                if let Some(xp) = self.xplane.lock().clone() {
                    match xplane::ensure_scenery_pack(&xp) {
                        Ok(true) => tracing::info!("added {} to scenery_packs.ini (backup written)", xplane::PACK_NAME),
                        Ok(false) => {}
                        Err(e) => tracing::warn!("scenery_packs.ini: {e}"),
                    }
                }
            }
            tracing::info!("mounted at {}", mp.display());
            Ok(mp)
        })();
        *self.mount_error.lock() = res.as_ref().err().map(|e: &anyhow::Error| format!("{e:#}"));
        res
    }

    pub fn unmount(&self) {
        if self.mount.lock().take().is_some() {
            tracing::info!("unmounted");
        }
    }

    pub fn status(&self) -> Status {
        let xp = self.xplane.lock().clone();
        let stack = self.stack.lock();
        Status {
            version: env!("CARGO_PKG_VERSION").into(),
            mounted: self.is_mounted(),
            mount_point: self.mount_point().map(|p| p.display().to_string()),
            mount_error: self.mount_error.lock().clone(),
            xplane: xp.as_ref().map(|p| XPlaneInfo {
                path: p.display().to_string(),
                custom_scenery: p.join("Custom Scenery").display().to_string(),
                global_scenery: xplane::global_scenery(p).map(|g| g.display().to_string()),
            }),
            sim_connected: *self.web.connected.borrow(),
            aircraft: self.web.aircraft.borrow().clone(),
            provider: stack.engine.provider().name().to_string(),
            engine: stack.engine.stats(),
            mesh: stack.mesh.as_ref().map(|m| m.status()),
            prefetch: self.prefetch.status(),
        }
    }

    pub fn health(&self) -> HealthReport {
        let mut checks = Vec::new();
        let mut add = |id: &str, title: &str, severity: Severity, message: String, fix: Option<&str>| {
            checks.push(HealthCheck { id: id.into(), title: title.into(), severity, message, fix: fix.map(str::to_string) });
        };
        let driver = xof_vfs::driver_name();
        if xof_vfs::driver_available() {
            add("driver", "Filesystem driver", Severity::Ok, format!("{driver} is installed"), None);
        } else {
            let fix = if cfg!(windows) {
                "Reinstall xporthofs (the installer includes WinFsp)."
            } else if cfg!(target_os = "macos") {
                "Install macFUSE from https://macfuse.github.io (or `brew install --cask macfuse`), then restart."
            } else {
                "Install FUSE 3 (e.g. `sudo apt install fuse3`)."
            };
            add("driver", "Filesystem driver", Severity::Error, format!("{driver} is not available"), Some(fix));
        }
        let xp = self.xplane.lock().clone();
        match &xp {
            Some(p) => {
                add("xplane", "X-Plane 12", Severity::Ok, format!("Found at {}", p.display()), None);
                match xplane::global_scenery(p) {
                    Some(_) => add("global_scenery", "Global Scenery", Severity::Ok, "Installed (used for water and season data)".into(), None),
                    None => add("global_scenery", "Global Scenery", Severity::Warn, "Not found: meshes will lack X-Plane's water depth and season layers".into(), Some("Install X-Plane 12's global scenery for your region.")),
                }
                let ini = p.join("Custom Scenery/scenery_packs.ini");
                let ok = std::fs::read_to_string(&ini).map(|s| xplane::insert_pack_entry(&s).is_none()).unwrap_or(false);
                if ok {
                    add("scenery_packs", "scenery_packs.ini", Severity::Ok, "xporthofs entry present".into(), None);
                } else {
                    add("scenery_packs", "scenery_packs.ini", Severity::Info, "Entry will be added when the scenery is mounted (or by X-Plane on next start)".into(), None);
                }
            }
            None => add("xplane", "X-Plane 12", Severity::Error, "Not found".into(), Some("Start X-Plane 12 once so it records its location, or set the path in Settings.")),
        }
        if self.is_mounted() {
            add("mount", "Scenery mount", Severity::Ok, format!("Mounted at {}", self.mount_point().map(|p| p.display().to_string()).unwrap_or_default()), None);
        } else {
            let msg = self.mount_error.lock().clone().unwrap_or_else(|| "Not mounted".into());
            add("mount", "Scenery mount", Severity::Warn, msg, Some("Use Mount in the app; X-Plane must be restarted to see newly mounted scenery."));
        }
        if *self.web.connected.borrow() {
            add("web_api", "X-Plane Web API", Severity::Ok, "Connected (prefetch active)".into(), None);
        } else {
            add("web_api", "X-Plane Web API", Severity::Info, "X-Plane is not running or its Web API is disabled; prefetch is idle".into(), Some("Start X-Plane 12 (Web API is on by default, port 8086)."));
        }
        let cfg = self.cfg.lock().clone();
        if cfg.mesh.enabled {
            if trusted_keys(&cfg).is_empty() {
                add("catalog", "Mesh catalog", Severity::Warn, "No trusted catalog signing key configured: streamed meshes are disabled".into(), Some("Add the catalog's public key under [mesh].public_keys in config.toml."));
            } else if let Some(m) = &self.stack.lock().mesh {
                let s = m.status();
                if s.online {
                    add("catalog", "Mesh catalog", Severity::Ok, format!("Catalog {} ({} tiles)", s.catalog_version, s.tiles_in_catalog), None);
                } else if s.tiles_in_catalog > 0 {
                    add("catalog", "Mesh catalog", Severity::Warn, format!("Offline: using cached catalog {} ({} cached tiles listed)", s.catalog_version, s.tiles_listed), None);
                } else {
                    add("catalog", "Mesh catalog", Severity::Warn, "Catalog unreachable and nothing cached".into(), Some("Check your internet connection."));
                }
            }
        }
        if !cfg.imagery.provider_notice_accepted {
            add("provider", "Imagery provider", Severity::Info, format!("Using {} — review its terms of use in Settings", cfg.imagery.provider), None);
        }
        if cfg!(windows) {
            add("defender", "Windows Defender", Severity::Info, "Excluding the mount and cache folders from real-time scanning avoids needless downloads".into(), Some("The installer offers this; or add the folders under Windows Security → Virus & threat protection → Exclusions."));
        }
        HealthReport { checks }
    }

    pub fn cache_stats(&self) -> CacheStats {
        let cfg = self.cfg.lock().clone();
        let mesh = self.stack.lock().mesh.clone();
        CacheStats {
            imagery_bytes: self.store.total_bytes().unwrap_or(0),
            imagery_cap_bytes: cfg.imagery.cache_size_gb << 30,
            imagery_tiles: self.store.tile_count().unwrap_or(0),
            areas: self.store.areas().unwrap_or_default(),
            mesh_bytes: mesh.as_ref().map(|m| m.status().cache_bytes).unwrap_or(0),
            mesh_cap_bytes: cfg.mesh.cache_size_gb << 30,
        }
    }

    /// Periodic housekeeping: cache caps, access flush, catalog promotion.
    pub async fn housekeeping(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            let this = self.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let cfg = this.cfg.lock().clone();
                let _ = this.store.flush_access();
                if n % 5 == 1 {
                    if let Ok(freed) = this.store.evict_to(cfg.imagery.cache_size_gb << 30) {
                        if freed > 0 {
                            tracing::info!("imagery cache: evicted {} MB", freed >> 20);
                        }
                    }
                    if let Some(m) = this.stack.lock().mesh.clone() {
                        let freed = m.evict_to(cfg.mesh.cache_size_gb << 30);
                        if freed > 0 {
                            tracing::info!("mesh cache: evicted {} MB", freed >> 20);
                        }
                    }
                }
            })
            .await;
            // Apply catalog updates only while X-Plane isn't running.
            if !*self.web.connected.borrow() {
                if let Some(m) = self.stack.lock().mesh.clone() {
                    if m.promote_pending() {
                        tracing::info!("applied mesh catalog update");
                    }
                }
            }
            if n % (24 * 60) == 0 {
                self.clone().refresh_catalog().await;
            }
        }
    }

    pub async fn refresh_catalog(self: Arc<Self>) {
        let mesh = self.stack.lock().mesh.clone();
        if let Some(m) = mesh {
            match m.refresh().await {
                Ok(true) => tracing::info!("mesh catalog updated ({})", m.manifest().catalog_version),
                Ok(false) => tracing::debug!("mesh catalog unchanged"),
                Err(e) => tracing::warn!("mesh catalog refresh failed: {e:#}"),
            }
        }
    }

    /// Rebuild engine/mesh/scenery after a config change; remounts if needed.
    fn rebuild(&self) -> anyhow::Result<()> {
        let cfg = self.cfg.lock().clone();
        let xp = xplane::resolve(cfg.xplane.path.as_deref());
        *self.xplane.lock() = xp.clone();
        let stack = build_stack(&self.dirs, &cfg, self.store.clone(), xp.as_deref(), &self.rt)?;
        self.prefetch.reconfigure(cfg.prefetch.clone(), stack.engine.clone(), stack.mesh.clone());
        let was_mounted = self.is_mounted();
        self.unmount();
        *self.stack.lock() = stack;
        if was_mounted {
            self.mount()?;
        }
        Ok(())
    }

    /// Handle one JSON-RPC request.
    pub async fn handle(self: &Arc<Self>, method_name: &str, params: Value) -> Result<Value, (i64, String)> {
        let internal = |e: anyhow::Error| (-32000, format!("{e:#}"));
        let bad = |e: serde_json::Error| (-32602, format!("invalid params: {e}"));
        let blocking = |f: Box<dyn FnOnce() -> anyhow::Result<Value> + Send>| async move {
            tokio::task::spawn_blocking(f).await.map_err(|e| (-32000, e.to_string()))?.map_err(|e| (-32000, format!("{e:#}")))
        };
        match method_name {
            method::STATUS => Ok(json!(self.status())),
            method::HEALTH => Ok(json!(self.health())),
            method::MOUNT => {
                let this = self.clone();
                blocking(Box::new(move || Ok(json!(this.mount()?.display().to_string())))).await
            }
            method::UNMOUNT => {
                let this = self.clone();
                blocking(Box::new(move || {
                    this.unmount();
                    Ok(Value::Null)
                }))
                .await
            }
            method::SHUTDOWN => {
                let _ = self.shutdown.send(true);
                Ok(Value::Null)
            }
            method::GET_CONFIG => Ok(json!(self.config())),
            method::SET_CONFIG => {
                let new: DaemonConfig = serde_json::from_value(params).map_err(bad)?;
                new.save(&self.config_path).map_err(internal)?;
                let sim_running = *self.web.connected.borrow();
                *self.cfg.lock() = new;
                if sim_running && self.is_mounted() {
                    // Don't pull the scenery from under a running simulator.
                    self.prefetch.reconfigure(self.config().prefetch, self.stack.lock().engine.clone(), self.stack.lock().mesh.clone());
                    return Ok(json!({ "applied": false, "note": "Saved. Imagery/mesh changes apply after X-Plane exits." }));
                }
                let this = self.clone();
                blocking(Box::new(move || {
                    this.rebuild()?;
                    Ok(json!({ "applied": true }))
                }))
                .await
            }
            method::CACHE_STATS => {
                let this = self.clone();
                blocking(Box::new(move || Ok(json!(this.cache_stats())))).await
            }
            method::CACHE_EVICT_AREA => {
                let a: EvictArea = serde_json::from_value(params).map_err(bad)?;
                let this = self.clone();
                blocking(Box::new(move || Ok(json!(this.store.evict_area(a.provider_byte, a.area)?)))).await
            }
            method::CATALOG_STATUS => Ok(json!(self.stack.lock().mesh.as_ref().map(|m| m.status()))),
            method::CATALOG_REFRESH => {
                self.clone().refresh_catalog().await;
                Ok(json!(self.stack.lock().mesh.as_ref().map(|m| m.status())))
            }
            method::PREPARE_DEPARTURE => {
                let p: PrepareDeparture = serde_json::from_value(params).map_err(bad)?;
                let (lat, lon, name) = match (p.lat, p.lon, p.icao.as_deref()) {
                    (Some(lat), Some(lon), _) => (lat, lon, None),
                    (_, _, Some(icao)) => {
                        let xp = self.xplane.lock().clone().ok_or((-32000, "X-Plane not found".to_string()))?;
                        let icao = icao.to_string();
                        let ap = tokio::task::spawn_blocking(move || airports::find(&xp, &icao))
                            .await
                            .map_err(|e| (-32000, e.to_string()))?
                            .ok_or((-32001, "airport not found".to_string()))?;
                        (ap.lat, ap.lon, Some(format!("{} {}", ap.icao, ap.name)))
                    }
                    _ => return Err((-32602, "give an ICAO code or lat/lon".into())),
                };
                let (mesh_tiles, imagery_tiles) = self.prefetch.prepare(lat, lon);
                Ok(json!(PrepareResult { lat, lon, name, mesh_tiles, imagery_tiles }))
            }
            method::PREFETCH_ROUTE => {
                let user = params
                    .get("simbrief_user")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| self.config().simbrief.user)
                    .ok_or((-32602, "no SimBrief user configured".to_string()))?;
                let pts = airports::simbrief_route(&user).await.map_err(internal)?;
                let (m, i) = self.prefetch.route(&pts);
                Ok(json!({ "waypoints": pts.len(), "mesh_tiles": m, "imagery_tiles": i }))
            }
            method::PACKS_LIST => {
                let cfg = self.config();
                let packs: Vec<PackInfo> = imported_packs(&self.dirs, &cfg)
                    .into_iter()
                    .map(|p| {
                        let mut tiles = Vec::new();
                        if let Ok(rd) = std::fs::read_dir(p.join("Earth nav data")) {
                            for f in rd.flatten() {
                                if let Ok(rd2) = std::fs::read_dir(f.path()) {
                                    for d in rd2.flatten() {
                                        let n = d.file_name().to_string_lossy().into_owned();
                                        if let Some(stem) = n.strip_suffix(".dsf") {
                                            tiles.push(stem.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        PackInfo { path: p.display().to_string(), tiles }
                    })
                    .collect();
                Ok(json!(packs))
            }
            method::PACKS_IMPORT => {
                let src = params.get("path").and_then(Value::as_str).ok_or((-32602, "missing path".to_string()))?;
                let src = PathBuf::from(src);
                let dest_root = self.dirs.packs();
                let this = self.clone();
                blocking(Box::new(move || {
                    anyhow::ensure!(src.join("Earth nav data").is_dir(), "not an X-Plane scenery pack (no 'Earth nav data')");
                    let name = src.file_name().ok_or_else(|| anyhow::anyhow!("bad path"))?;
                    let dest = dest_root.join(name);
                    anyhow::ensure!(!dest.exists(), "a pack with that name is already imported");
                    std::fs::create_dir_all(&dest_root)?;
                    copy_dir(&src, &dest)?;
                    this.rebuild()?;
                    Ok(json!(dest.display().to_string()))
                }))
                .await
            }
            _ => Err((-32601, format!("unknown method '{method_name}'"))),
        }
    }
}

/// WinFsp needs a non-existent directory; FUSE an existing empty one.
fn prepare_mount_point(mp: &Path) -> anyhow::Result<()> {
    let parent = mp.parent().ok_or_else(|| anyhow::anyhow!("bad mount point"))?;
    anyhow::ensure!(parent.is_dir(), "{} does not exist", parent.display());
    if cfg!(windows) {
        if mp.exists() || std::fs::symlink_metadata(mp).is_ok() {
            let empty = std::fs::read_dir(mp).map(|mut r| r.next().is_none()).unwrap_or(true);
            anyhow::ensure!(empty, "{} exists and is not empty", mp.display());
            std::fs::remove_dir(mp)?;
        }
    } else {
        std::fs::create_dir_all(mp)?;
    }
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}
