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
use xof_proto::{
    method, CacheStats, Event, EvictArea, HealthCheck, HealthReport, HistorySample, PackInfo, PrepareDeparture,
    PrepareResult, Severity, Status, StorageJob, StorageRelocate, StorageStatus, XPlaneInfo,
};
use xof_vfs::{Scenery, SceneryConfig};

use crate::prefetch::Prefetcher;
use crate::storage;
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
    /// Cache folder in use (configured, or the default as a fallback).
    cache_root: Mutex<PathBuf>,
    storage_error: Mutex<Option<String>>,
    storage_job: Mutex<Option<StorageJob>>,
    history: Mutex<std::collections::VecDeque<HistorySample>>,
    /// Download bytes from engines/streamers replaced by a rebuild, so the
    /// cumulative counter never goes backwards.
    download_base: Mutex<(u64, u64)>,
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

fn build_stack(
    dirs: &Dirs,
    cache_root: &Path,
    cfg: &DaemonConfig,
    store: Arc<TileStore>,
    xp: Option<&Path>,
    rt: &tokio::runtime::Handle,
) -> anyhow::Result<Stack> {
    let provider =
        provider::configured(&cfg.imagery.provider, cfg.imagery.prefer_open, cfg.imagery.provider_notice_accepted)
            .ok_or_else(|| anyhow::anyhow!("unknown imagery provider '{}'", cfg.imagery.provider))?;
    let engine = {
        let _g = rt.enter();
        TextureEngine::new(
            EngineConfig {
                deadline_ms: cfg.imagery.deadline_ms,
                segment_ram_mb: cfg.imagery.segment_ram_mb,
                decoded_ram_mb: cfg.imagery.decoded_ram_mb,
                read_ahead: cfg.imagery.read_ahead,
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
                cache_dir: cache_root.join("mesh"),
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
        let mut storage_error = None;
        let configured = cfg.storage.cache_path.as_deref().map(xplane::native_path);
        let (cache_root, store) = match configured {
            Some(root) => match TileStore::open(&root.join("imagery.redb")) {
                Ok(s) => (root, s),
                Err(e) => {
                    let msg =
                        format!("cache folder {} unusable ({e:#}); using {}", root.display(), dirs.cache.display());
                    tracing::warn!("{msg}");
                    storage_error = Some(msg);
                    (dirs.cache.clone(), TileStore::open(&dirs.cache.join("imagery.redb"))?)
                }
            },
            None => (dirs.cache.clone(), TileStore::open(&dirs.cache.join("imagery.redb"))?),
        };
        tracing::info!("cache folder: {}", cache_root.display());
        let stack = build_stack(&dirs, &cache_root, &cfg, store.clone(), xp.as_deref(), &rt)?;
        let port = if cfg.xplane.web_api_port == 0 { 8086 } else { cfg.xplane.web_api_port };
        let web = crate::webapi::spawn(port);
        let prefetch =
            Prefetcher::spawn(cfg.prefetch.clone(), stack.engine.clone(), stack.mesh.clone(), web.aircraft.clone());
        let (events, _) = broadcast::channel(512);
        let (shutdown, _) = watch::channel(false);
        Ok(Arc::new(Self {
            dirs,
            config_path,
            cfg: Mutex::new(cfg),
            store,
            cache_root: Mutex::new(cache_root),
            storage_error: Mutex::new(storage_error),
            storage_job: Mutex::new(None),
            history: Mutex::new(std::collections::VecDeque::new()),
            download_base: Mutex::new((0, 0)),
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
            let mp = self
                .mount_point()
                .ok_or_else(|| anyhow::anyhow!("X-Plane 12 not found and no mount path configured"))?;
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
            sim_loading: self.web.loading(),
            aircraft: self.web.aircraft.borrow().clone(),
            provider: stack.engine.provider().name().to_string(),
            provider_attribution: stack.engine.provider().attribution().to_string(),
            engine: stack.engine.stats(),
            mesh: stack.mesh.as_ref().map(|m| m.status()),
            prefetch: self.prefetch.status(),
            storage: self.storage_status(),
        }
    }

    pub fn health(&self) -> HealthReport {
        let mut checks = Vec::new();
        let mut add = |id: &str, title: &str, severity: Severity, message: String, fix: Option<&str>| {
            checks.push(HealthCheck {
                id: id.into(),
                title: title.into(),
                severity,
                message,
                fix: fix.map(str::to_string),
            });
        };
        {
            let st = self.storage_status();
            let free_gb = st.available_bytes as f64 / 1e9;
            if let Some(e) = st.error {
                add(
                    "storage",
                    "Cache folder",
                    Severity::Warn,
                    e,
                    Some("Reconnect the drive, or choose another folder in Settings → Storage."),
                );
            } else if st.available_bytes < (5u64 << 30) {
                add(
                    "storage",
                    "Cache folder",
                    Severity::Warn,
                    format!("Only {free_gb:.1} GB free on the cache drive ({})", st.cache_path),
                    Some("Move the cache to a larger drive in Settings → Storage, or lower the cache sizes."),
                );
            } else {
                add("storage", "Cache folder", Severity::Ok, format!("{} ({free_gb:.0} GB free)", st.cache_path), None);
            }
        }
        let driver = xof_vfs::driver_name();
        if xof_vfs::driver_available() {
            add("driver", "Filesystem driver", Severity::Ok, format!("{driver} is installed"), None);
        } else {
            let fix = if cfg!(windows) {
                "Reinstall ButterSat (the installer includes WinFsp)."
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
                    Some(_) => add(
                        "global_scenery",
                        "Global Scenery",
                        Severity::Ok,
                        "Installed (used for water and season data)".into(),
                        None,
                    ),
                    None => add(
                        "global_scenery",
                        "Global Scenery",
                        Severity::Warn,
                        "Not found: meshes will lack X-Plane's water depth and season layers".into(),
                        Some("Install X-Plane 12's global scenery for your region."),
                    ),
                }
                let ini = p.join("Custom Scenery/scenery_packs.ini");
                let ok =
                    std::fs::read_to_string(&ini).map(|s| xplane::insert_pack_entry(&s).is_none()).unwrap_or(false);
                if ok {
                    add("scenery_packs", "scenery_packs.ini", Severity::Ok, "ButterSat entry present".into(), None);
                } else {
                    add(
                        "scenery_packs",
                        "scenery_packs.ini",
                        Severity::Info,
                        "Entry will be added when the scenery is mounted (or by X-Plane on next start)".into(),
                        None,
                    );
                }
            }
            None => add(
                "xplane",
                "X-Plane 12",
                Severity::Error,
                "Not found".into(),
                Some("Start X-Plane 12 once so it records its location, or set the path in Settings."),
            ),
        }
        if self.is_mounted() {
            add(
                "mount",
                "Scenery mount",
                Severity::Ok,
                format!("Mounted at {}", self.mount_point().map(|p| p.display().to_string()).unwrap_or_default()),
                None,
            );
        } else {
            let msg = self.mount_error.lock().clone().unwrap_or_else(|| "Not mounted".into());
            add(
                "mount",
                "Scenery mount",
                Severity::Warn,
                msg,
                Some("Use Mount in the app; X-Plane must be restarted to see newly mounted scenery."),
            );
        }
        if *self.web.connected.borrow() {
            add("web_api", "X-Plane Web API", Severity::Ok, "Connected (prefetch active)".into(), None);
        } else {
            add(
                "web_api",
                "X-Plane Web API",
                Severity::Info,
                "X-Plane is not running or its Web API is disabled; prefetch is idle".into(),
                Some("Start X-Plane 12 (Web API is on by default, port 8086)."),
            );
        }
        let cfg = self.cfg.lock().clone();
        if cfg.mesh.enabled {
            if trusted_keys(&cfg).is_empty() {
                add(
                    "catalog",
                    "Mesh catalog",
                    Severity::Warn,
                    "No trusted catalog signing key configured: streamed meshes are disabled".into(),
                    Some("Add the catalog's public key under [mesh].public_keys in config.toml."),
                );
            } else if let Some(m) = {
                // Release the stack lock before doing work (no guard in the
                // if-let scrutinee).
                let mesh = self.stack.lock().mesh.clone();
                mesh
            } {
                let s = m.status();
                if s.online {
                    add(
                        "catalog",
                        "Mesh catalog",
                        Severity::Ok,
                        format!("Catalog {} ({} tiles)", s.catalog_version, s.tiles_in_catalog),
                        None,
                    );
                } else if s.tiles_in_catalog > 0 {
                    add(
                        "catalog",
                        "Mesh catalog",
                        Severity::Warn,
                        format!(
                            "Offline: using cached catalog {} ({} cached tiles listed)",
                            s.catalog_version, s.tiles_listed
                        ),
                        None,
                    );
                } else {
                    add(
                        "catalog",
                        "Mesh catalog",
                        Severity::Warn,
                        "Catalog unreachable and nothing cached".into(),
                        Some("Check your internet connection."),
                    );
                }
            }
        }
        let selected_non_open = provider::builtin(&cfg.imagery.provider).is_some_and(|p| !p.is_open());
        if selected_non_open && !cfg.imagery.provider_notice_accepted {
            if cfg.imagery.prefer_open {
                add(
                    "provider",
                    "Imagery provider",
                    Severity::Warn,
                    format!(
                        "Open regional imagery (US, FR, CH, NL, ES, AT) is active; {} fallback is disabled outside those regions until you accept its terms notice",
                        cfg.imagery.provider
                    ),
                    Some("Review and accept the provider terms notice in Settings → Imagery to enable global fallback imagery."),
                );
            } else {
                add(
                    "provider",
                    "Imagery provider",
                    Severity::Error,
                    format!(
                        "{} is disabled until you accept its terms notice — no imagery will be fetched",
                        cfg.imagery.provider
                    ),
                    Some("Accept the provider terms notice in Settings → Imagery, or enable open regional imagery."),
                );
            }
        } else {
            let prov_name = self.stack.lock().engine.provider().name().to_string();
            add("provider", "Imagery provider", Severity::Ok, prov_name, None);
        }
        if cfg!(windows) {
            add("defender", "Windows Defender", Severity::Info, "Excluding the mount and cache folders from real-time scanning avoids needless downloads".into(), Some("The installer offers this; or add the folders under Windows Security → Virus & threat protection → Exclusions."));
        }
        HealthReport { checks }
    }

    /// Total bytes downloaded (imagery + meshes), monotonic across rebuilds.
    fn downloaded(&self) -> u64 {
        let (img, mesh) = {
            let st = self.stack.lock();
            (st.engine.stats().fetch_bytes, st.mesh.as_ref().map(|m| m.status().download_bytes).unwrap_or(0))
        };
        let mut base = self.download_base.lock();
        // A rebuilt engine/streamer starts from zero: keep what was counted.
        if img + mesh < base.1 {
            base.0 += base.1;
        }
        base.1 = img + mesh;
        base.0 + base.1
    }

    /// Record one activity sample (every 5 s; 2 h kept).
    pub fn sample_history(&self) {
        let c = self.cache_stats_light();
        let s = HistorySample {
            t: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            download_bytes: self.downloaded(),
            cache_bytes: c.0,
            cache_cap_bytes: c.1,
        };
        let mut h = self.history.lock();
        h.push_back(s);
        while h.len() > 1440 {
            h.pop_front();
        }
    }

    /// Cache bytes and cap without listing areas.
    fn cache_stats_light(&self) -> (u64, u64) {
        let cfg = self.cfg.lock().clone();
        let mesh = self.stack.lock().mesh.clone();
        let bytes = self.store.total_bytes().unwrap_or(0) + mesh.as_ref().map(|m| m.status().cache_bytes).unwrap_or(0);
        (bytes, (cfg.imagery.cache_size_gb + cfg.mesh.cache_size_gb) << 30)
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

    /// Sim phase -> live-read budget (design §5.3): long while X-Plane loads,
    /// short in flight. Re-applied every tick, so a rebuilt engine picks it up.
    pub async fn sim_phase(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut was: Option<bool> = None;
        loop {
            tick.tick().await;
            let loading = self.web.loading();
            let (d_fly, d_load, r_fly, r_load) = {
                let c = self.config();
                let i = &c.imagery;
                (i.deadline_ms, i.loading_deadline_ms.max(i.deadline_ms), i.max_requests, i.loading_max_requests)
            };
            {
                let stack = self.stack.lock();
                stack.engine.set_live_deadline_ms(if loading { d_load } else { d_fly });
                stack.engine.set_fetch_limit(if loading { r_load } else { r_fly });
            }
            if was != Some(loading) {
                tracing::info!(
                    "sim {}: read budget {} ms",
                    if loading { "loading" } else { "running" },
                    if loading { d_load } else { d_fly }
                );
                was = Some(loading);
            }
        }
    }

    /// Pre-warm around X-Plane's last position when it changes (X-Plane writes
    /// `last.pos` on exit) and once at start, unless a flight is running.
    pub async fn last_position(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        let mut seen: Option<std::time::SystemTime> = None;
        loop {
            tick.tick().await;
            if !self.config().prefetch.prewarm_last_position || !self.web.loading() {
                continue;
            }
            let Some(xp) = self.xplane.lock().clone() else { continue };
            let p = xp.join("Output").join("preferences").join("last.pos");
            let Ok(mtime) = std::fs::metadata(&p).and_then(|m| m.modified()) else { continue };
            if seen == Some(mtime) {
                continue;
            }
            seen = Some(mtime);
            match std::fs::read_to_string(&p).ok().as_deref().and_then(parse_last_pos) {
                Some((lat, lon)) => {
                    tracing::info!("last X-Plane position {lat:.4},{lon:.4}: pre-warming");
                    self.prefetch.prewarm(lat, lon);
                }
                None => tracing::debug!("unreadable {}", p.display()),
            }
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
                    let mesh = this.stack.lock().mesh.clone();
                    if let Some(m) = mesh {
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
                let mesh = self.stack.lock().mesh.clone();
                if let Some(m) = mesh {
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

    fn storage_status(&self) -> StorageStatus {
        let root = self.cache_root.lock().clone();
        let (available_bytes, total_bytes) = storage::space(&root);
        StorageStatus {
            cache_path: root.display().to_string(),
            available_bytes,
            total_bytes,
            job: self.storage_job.lock().clone(),
            error: self.storage_error.lock().clone(),
        }
    }

    /// Change the cache folder (design §11): `move` brings the current cache
    /// along, `fresh` starts empty there and leaves the old folder in place.
    /// Not while X-Plane is running: the mount goes away during the switch.
    fn start_relocation(self: &Arc<Self>, req: StorageRelocate) -> anyhow::Result<()> {
        anyhow::ensure!(req.mode == "move" || req.mode == "fresh", "mode must be 'move' or 'fresh'");
        anyhow::ensure!(
            !*self.web.connected.borrow(),
            "close X-Plane first: the scenery is unmounted while the cache moves"
        );
        let from = self.cache_root.lock().clone();
        let to = xplane::native_path(Path::new(req.path.trim()));
        storage::validate(&from, &to)?;
        anyhow::ensure!(storage::writable(&to), "can't write to {}", to.display());
        {
            let mut job = self.storage_job.lock();
            anyhow::ensure!(!job.as_ref().is_some_and(|j| j.state == "running"), "a cache move is already running");
            *job = Some(StorageJob {
                to: to.display().to_string(),
                mode: req.mode.clone(),
                state: "running".into(),
                total_bytes: if req.mode == "move" { storage::cache_size(&from) } else { 0 },
                message: "preparing".into(),
                ..Default::default()
            });
        }
        let this = self.clone();
        std::thread::Builder::new().name("cache-move".into()).spawn(move || {
            let res = this.relocate(&from, &to, &req.mode);
            let mut job = this.storage_job.lock();
            if let Some(j) = job.as_mut() {
                match res {
                    Ok(note) => {
                        j.state = "done".into();
                        j.done_bytes = j.total_bytes;
                        j.message = note;
                    }
                    Err(e) => {
                        tracing::error!("cache move failed: {e:#}");
                        j.state = "failed".into();
                        j.message = format!("{e:#}");
                    }
                }
            }
        })?;
        Ok(())
    }

    fn relocate(self: &Arc<Self>, from: &Path, to: &Path, mode: &str) -> anyhow::Result<String> {
        tracing::info!("cache folder: {mode} {} -> {}", from.display(), to.display());
        let was_mounted = self.is_mounted();
        self.unmount();
        self.store.close()?;
        let moved = if mode == "move" {
            let job = |done: u64, total: u64| {
                if let Some(j) = self.storage_job.lock().as_mut() {
                    j.done_bytes = done;
                    j.total_bytes = total;
                    j.message = "moving".into();
                }
            };
            storage::move_cache(from, to, job)
        } else {
            Ok(())
        };
        let result = moved.and_then(|()| {
            self.store.reopen(&to.join("imagery.redb"))?;
            let mut cfg = self.cfg.lock().clone();
            cfg.storage.cache_path = Some(to.to_path_buf());
            cfg.save(&self.config_path)?;
            *self.cfg.lock() = cfg;
            *self.cache_root.lock() = to.to_path_buf();
            *self.storage_error.lock() = None;
            self.rebuild()
        });
        if let Err(e) = result {
            // Back to the old folder, as it was.
            let _ = self.store.reopen(&from.join("imagery.redb"));
            *self.cache_root.lock() = from.to_path_buf();
            let _ = self.rebuild();
            if was_mounted {
                let _ = self.mount();
            }
            return Err(e);
        }
        if was_mounted {
            self.mount()?;
        }
        tracing::info!("cache folder now {}", to.display());
        Ok(if mode == "fresh" {
            format!("Using {}. The old cache is still in {} — delete it to free space.", to.display(), from.display())
        } else {
            format!("Moved to {}.", to.display())
        })
    }

    /// Switch off: persist it, unmount, disable our scenery_packs.ini entry,
    /// then exit.
    fn turn_off(&self) -> anyhow::Result<()> {
        let mut cfg = self.cfg.lock().clone();
        cfg.startup.enabled = false;
        cfg.save(&self.config_path)?;
        let manage = cfg.mount.manage_scenery_packs;
        *self.cfg.lock() = cfg;
        self.unmount();
        if manage {
            if let Some(xp) = self.xplane.lock().clone() {
                match xplane::set_scenery_pack_enabled(&xp, false) {
                    Ok(true) => tracing::info!("disabled {} in scenery_packs.ini (backup written)", xplane::PACK_NAME),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("scenery_packs.ini: {e:#}"),
                }
            }
        }
        tracing::info!("ButterSat switched off");
        let _ = self.shutdown.send(true);
        Ok(())
    }

    /// Rebuild engine/mesh/scenery after a config change; remounts if needed.
    fn rebuild(&self) -> anyhow::Result<()> {
        let cfg = self.cfg.lock().clone();
        let xp = xplane::resolve(cfg.xplane.path.as_deref());
        *self.xplane.lock() = xp.clone();
        let root = self.cache_root.lock().clone();
        let stack = build_stack(&self.dirs, &root, &cfg, self.store.clone(), xp.as_deref(), &self.rt)?;
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
            tokio::task::spawn_blocking(f)
                .await
                .map_err(|e| (-32000, e.to_string()))?
                .map_err(|e| (-32000, format!("{e:#}")))
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
            method::TURN_OFF => {
                if *self.web.connected.borrow() {
                    return Err((-32010, "Close X-Plane first: turning ButterSat off removes its scenery.".into()));
                }
                let this = self.clone();
                blocking(Box::new(move || {
                    this.turn_off()?;
                    Ok(Value::Null)
                }))
                .await
            }
            method::GET_CONFIG => Ok(json!(self.config())),
            method::SET_CONFIG => {
                let mut new: DaemonConfig = serde_json::from_value(params).map_err(bad)?;
                // The cache folder changes only through storage_relocate.
                new.storage = self.cfg.lock().storage.clone();
                new.save(&self.config_path).map_err(internal)?;
                if new.startup.start_at_login != self.cfg.lock().startup.start_at_login {
                    if let Err(e) = crate::autostart::apply(new.startup.start_at_login) {
                        tracing::warn!("start at login: {e:#}");
                    }
                }
                let sim_running = *self.web.connected.borrow();
                *self.cfg.lock() = new;
                if sim_running && self.is_mounted() {
                    // Don't pull the scenery from under a running simulator.
                    // (One lock: guards in a single expression live until its
                    // end, and parking_lot mutexes aren't re-entrant.)
                    let (engine, mesh) = {
                        let st = self.stack.lock();
                        (st.engine.clone(), st.mesh.clone())
                    };
                    self.prefetch.reconfigure(self.config().prefetch, engine, mesh);
                    return Ok(
                        json!({ "applied": false, "note": "Saved. Imagery/mesh changes apply after X-Plane exits." }),
                    );
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
            method::CATALOG_TILES => {
                let mesh = self.stack.lock().mesh.clone();
                let this_tiles = tokio::task::spawn_blocking(move || {
                    mesh.map(|m| {
                        m.listed_tiles()
                            .iter()
                            .map(|&t| xof_proto::CatalogTile {
                                tile: t.name(),
                                lat: t.lat,
                                lon: t.lon,
                                ready: m.is_dsf_ready(t),
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                })
                .await
                .map_err(|e| (-32000, e.to_string()))?;
                Ok(json!(this_tiles))
            }
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
            method::HISTORY => {
                let since = params.get("since").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let h: Vec<HistorySample> = self.history.lock().iter().filter(|s| s.t > since).copied().collect();
                Ok(json!(h))
            }
            method::STORAGE_INFO => {
                #[derive(serde::Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = serde_json::from_value(params).map_err(bad)?;
                let current = self.cache_root.lock().clone();
                blocking(Box::new(move || {
                    let cand = xplane::native_path(Path::new(&p.path));
                    let mut i = storage::info(&cand, &current);
                    i.writable = storage::writable(&cand);
                    let (a, t) = storage::space(&cand);
                    (i.available_bytes, i.total_bytes) = (a, t);
                    Ok(json!(i))
                }))
                .await
            }
            method::STORAGE_RELOCATE => {
                let p: StorageRelocate = serde_json::from_value(params).map_err(bad)?;
                self.start_relocation(p).map_err(internal)?;
                Ok(json!(self.storage_status()))
            }
            method::PACKS_IMPORT => {
                let src = params.get("path").and_then(Value::as_str).ok_or((-32602, "missing path".to_string()))?;
                let src = PathBuf::from(src);
                let dest_root = self.dirs.packs();
                let this = self.clone();
                blocking(Box::new(move || {
                    anyhow::ensure!(
                        src.join("Earth nav data").is_dir(),
                        "not an X-Plane scenery pack (no 'Earth nav data')"
                    );
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

/// `Output/preferences/last.pos`: `lat/lon` as text (X-Plane 12).
pub fn parse_last_pos(s: &str) -> Option<(f64, f64)> {
    let (a, b) = s.trim().split_once('/')?;
    let (lat, lon): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_pos() {
        let (lat, lon) = parse_last_pos(
            "20.89341352900162008/-156.43653712671488165
",
        )
        .unwrap();
        assert!((lat - 20.893413529).abs() < 1e-6 && (lon + 156.436537127).abs() < 1e-6);
        assert_eq!(parse_last_pos("40.6/-73.7"), Some((40.6, -73.7)));
        assert_eq!(parse_last_pos("nonsense"), None);
        assert_eq!(parse_last_pos("95/10"), None);
    }
}
