//! Mesh streaming (design §6): catalog-driven listing, download + verify +
//! decompress + Laminar raster splice, mesh cache, offline behaviour, deferred
//! catalog updates.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use xof_core::DsfTile;

use crate::catalog::{CatalogClient, Manifest, MaskEntry, Refresh, TileEntry};
use crate::dsf::{self, Rasters};

/// Which tiles to present.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    #[default]
    World,
    /// Inclusive lat/lon boxes `[south, west, north, east]` (degrees).
    Boxes(Vec<[f64; 4]>),
}

impl Coverage {
    pub fn contains(&self, t: DsfTile) -> bool {
        match self {
            Coverage::World => true,
            Coverage::Boxes(b) => b.iter().any(|[s, w, n, e]| {
                let (lat, lon) = (t.lat as f64 + 0.5, t.lon as f64 + 0.5);
                lat >= *s && lat <= *n && lon >= *w && lon <= *e
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MeshConfig {
    /// Built-in / configured origin base URLs used to find the manifest.
    pub bootstrap_mirrors: Vec<String>,
    pub cache_dir: PathBuf,
    /// `<X-Plane>/Global Scenery/X-Plane 12 Global Scenery`, if found.
    pub global_scenery: Option<PathBuf>,
    pub coverage: Coverage,
    pub pinned: HashSet<DsfTile>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RasterIndexEntry {
    gs_len: u64,
    gs_mtime: u64,
    /// Bytes the splice adds; `None` when the GS file has no rasters.
    extra: Option<u64>,
}

#[derive(Debug, Default)]
pub struct MeshStats {
    pub downloads: AtomicU64,
    pub download_bytes: AtomicU64,
    pub splices: AtomicU64,
    pub failures: AtomicU64,
    pub waits_ms: AtomicU64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MeshStatus {
    pub catalog_version: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub attribution: Vec<String>,
    pub online: bool,
    pub pending_update: Option<String>,
    pub tiles_in_catalog: usize,
    pub tiles_listed: usize,
    pub downloads: u64,
    pub download_bytes: u64,
    pub splices: u64,
    pub failures: u64,
    pub in_flight: usize,
    pub cache_bytes: u64,
}

type EnsureFuture = Shared<BoxFuture<'static, Result<PathBuf, String>>>;

pub struct MeshStreamer {
    cfg: MeshConfig,
    client: CatalogClient,
    active: RwLock<Arc<Manifest>>,
    masks_by_name: RwLock<HashMap<String, MaskEntry>>,
    pending: Mutex<Option<Arc<Manifest>>>,
    online: AtomicBool,
    raster_index: Mutex<HashMap<DsfTile, RasterIndexEntry>>,
    rasters_lru: Mutex<lru_lite::Lru<DsfTile, Arc<Option<Rasters>>>>,
    inflight: Mutex<HashMap<DsfTile, EnsureFuture>>,
    pub stats: MeshStats,
    /// Cached result of `listed_tiles` (invalidated on catalog/online/cache changes).
    listed: RwLock<Option<Arc<Vec<DsfTile>>>>,
}

mod lru_lite {
    //! A tiny count-bounded LRU (raster blobs are ~8 MB, keep a handful).
    use std::collections::VecDeque;
    pub struct Lru<K, V> {
        cap: usize,
        items: VecDeque<(K, V)>,
    }
    impl<K: PartialEq, V: Clone> Lru<K, V> {
        pub fn new(cap: usize) -> Self {
            Self { cap, items: VecDeque::new() }
        }
        pub fn get(&mut self, k: &K) -> Option<V> {
            let i = self.items.iter().position(|(kk, _)| kk == k)?;
            let item = self.items.remove(i)?;
            let v = item.1.clone();
            self.items.push_front(item);
            Some(v)
        }
        pub fn put(&mut self, k: K, v: V) {
            if let Some(i) = self.items.iter().position(|(kk, _)| kk == &k) {
                self.items.remove(i);
            }
            self.items.push_front((k, v));
            self.items.truncate(self.cap);
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

impl MeshStreamer {
    pub fn new(cfg: MeshConfig, keys: Vec<ed25519_dalek::VerifyingKey>) -> anyhow::Result<Arc<Self>> {
        for d in ["catalog", "objects", "spliced"] {
            std::fs::create_dir_all(cfg.cache_dir.join(d))?;
        }
        let client = CatalogClient::new(keys, &cfg.cache_dir.join("catalog"))?;
        let index: HashMap<DsfTile, RasterIndexEntry> = std::fs::read(cfg.cache_dir.join("raster-index.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<(DsfTile, RasterIndexEntry)>>(&b).ok())
            .map(|v| v.into_iter().collect())
            .unwrap_or_default();
        let this = Arc::new(Self {
            cfg,
            client,
            active: RwLock::new(Arc::new(Manifest::empty())),
            masks_by_name: RwLock::new(HashMap::new()),
            pending: Mutex::new(None),
            online: AtomicBool::new(false),
            raster_index: Mutex::new(index),
            rasters_lru: Mutex::new(lru_lite::Lru::new(6)),
            inflight: Mutex::new(HashMap::new()),
            stats: MeshStats::default(),
            listed: RwLock::new(None),
        });
        if let Some(m) = this.client.load_cached() {
            this.set_active(Arc::new(m));
        }
        Ok(this)
    }

    fn set_active(&self, m: Arc<Manifest>) {
        let mut masks = HashMap::new();
        for t in m.tiles.values() {
            for mk in &t.masks {
                masks.insert(mk.name.to_ascii_lowercase(), mk.clone());
            }
        }
        *self.masks_by_name.write() = masks;
        *self.active.write() = m;
        self.invalidate_listing();
    }

    pub fn manifest(&self) -> Arc<Manifest> {
        self.active.read().clone()
    }

    fn mirrors(&self) -> Vec<String> {
        let m = self.active.read();
        let mut v = m.mirrors.clone();
        for b in &self.cfg.bootstrap_mirrors {
            if !v.contains(b) {
                v.push(b.clone());
            }
        }
        v
    }

    /// Refresh the catalog. The first successful manifest becomes active
    /// immediately; later changes wait in `pending` until `promote_pending`
    /// (called when X-Plane is not running).
    pub async fn refresh(&self) -> anyhow::Result<bool> {
        let mut mirrors = self.cfg.bootstrap_mirrors.clone();
        for m in &self.active.read().mirrors {
            if !mirrors.contains(m) {
                mirrors.push(m.clone());
            }
        }
        match self.client.refresh(&mirrors).await {
            Ok(Refresh::Updated(m)) => {
                self.set_online(true);
                let m = Arc::new(m);
                let is_first = self.active.read().tiles.is_empty();
                if is_first {
                    self.set_active(m);
                } else if m.catalog_version != self.active.read().catalog_version {
                    *self.pending.lock() = Some(m);
                }
                Ok(true)
            }
            Ok(Refresh::Unchanged) => {
                self.set_online(true);
                Ok(false)
            }
            Err(e) => {
                self.set_online(false);
                Err(e)
            }
        }
    }

    /// Apply a pending catalog update (only when X-Plane isn't running).
    pub fn promote_pending(&self) -> bool {
        if let Some(m) = self.pending.lock().take() {
            self.set_active(m);
            return true;
        }
        false
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Tiles to present in `Earth nav data`. Offline, only tiles we can serve
    /// without the network.
    fn set_online(&self, on: bool) {
        if self.online.swap(on, Ordering::Relaxed) != on {
            self.invalidate_listing();
        }
    }

    fn invalidate_listing(&self) {
        *self.listed.write() = None;
    }

    /// Tiles to present (cached; cheap to call on every lookup).
    pub fn listed_tiles(&self) -> Arc<Vec<DsfTile>> {
        if let Some(l) = self.listed.read().clone() {
            return l;
        }
        let l = Arc::new(self.compute_listed());
        *self.listed.write() = Some(l.clone());
        l
    }

    /// Is `t` currently presented?
    pub fn is_listed(&self, t: DsfTile) -> bool {
        self.listed_tiles().binary_search(&t).is_ok()
    }

    fn compute_listed(&self) -> Vec<DsfTile> {
        let m = self.manifest();
        let online = self.is_online();
        m.tiles
            .iter()
            .filter_map(|(k, e)| DsfTile::parse(k).map(|t| (t, e)))
            .filter(|(t, _)| self.cfg.coverage.contains(*t))
            .filter(|(t, e)| online || self.is_cached(*t, e))
            .map(|(t, _)| t)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn entry(&self, t: DsfTile) -> Option<TileEntry> {
        self.active.read().tile(t).cloned()
    }

    fn object_path(&self, blake3: &str, ext: &str) -> PathBuf {
        self.cfg.cache_dir.join("objects").join(format!("{blake3}.{ext}"))
    }

    fn gs_path(&self, t: DsfTile) -> Option<PathBuf> {
        self.cfg.global_scenery.as_ref().map(|g| g.join(t.dsf_rel_path()))
    }

    fn gs_fingerprint(&self, t: DsfTile) -> Option<(u64, u64)> {
        let md = std::fs::metadata(self.gs_path(t)?).ok()?;
        let mtime = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some((md.len(), mtime))
    }

    fn spliced_path(&self, t: DsfTile, e: &TileEntry) -> PathBuf {
        let fp = match self.gs_fingerprint(t) {
            Some((len, mtime)) => format!("{len:x}-{mtime:x}"),
            None => "nogs".into(),
        };
        self.cfg.cache_dir.join("spliced").join(format!("{}.{}.{fp}.dsf", t.name(), &e.dsf.object.blake3[..16]))
    }

    /// Is the final (spliced) DSF already on disk, so a read needs no network?
    pub fn is_dsf_ready(&self, t: DsfTile) -> bool {
        self.entry(t).is_some_and(|e| self.spliced_path(t, &e).exists())
    }

    fn is_cached(&self, t: DsfTile, e: &TileEntry) -> bool {
        self.spliced_path(t, e).exists() || self.object_path(&e.dsf.object.blake3, "dsf.zst").exists()
    }

    /// Load (and cache) the Laminar rasters for a tile from Global Scenery.
    /// Blocking; local disk only.
    fn rasters(&self, t: DsfTile) -> Arc<Option<Rasters>> {
        if let Some(r) = self.rasters_lru.lock().get(&t) {
            return r;
        }
        let r = self.gs_path(t).filter(|p| p.exists()).and_then(|p| {
            match dsf::read_dsf_file(&p).and_then(|d| dsf::extract_rasters(&d)) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Global Scenery {}: {e}", p.display());
                    None
                }
            }
        });
        let r = Arc::new(r);
        self.rasters_lru.lock().put(t, r.clone());
        r
    }

    /// Bytes the splice adds for tile `t` (0 without Global Scenery). Uses the
    /// persistent index; computes on miss. Blocking, local only.
    pub fn raster_extra(&self, t: DsfTile) -> u64 {
        let Some((gs_len, gs_mtime)) = self.gs_fingerprint(t) else {
            return 0;
        };
        if let Some(e) = self.raster_index.lock().get(&t) {
            if e.gs_len == gs_len && e.gs_mtime == gs_mtime {
                return e.extra.unwrap_or(0);
            }
        }
        let extra = self.rasters(t).as_ref().as_ref().map(|r| r.extra_len());
        let mut idx = self.raster_index.lock();
        idx.insert(t, RasterIndexEntry { gs_len, gs_mtime, extra });
        let v: Vec<_> = idx.iter().map(|(k, v)| (*k, *v)).collect();
        drop(idx);
        if let Ok(json) = serde_json::to_vec(&v) {
            let _ = write_atomic(&self.cfg.cache_dir.join("raster-index.json"), &json);
        }
        extra.unwrap_or(0)
    }

    /// Size X-Plane sees for a tile's DSF. Blocking (may index locally).
    pub fn dsf_size(&self, t: DsfTile) -> Option<u64> {
        let e = self.entry(t)?;
        Some(dsf::spliced_len(e.dsf.stripped_size, self.raster_extra(t)))
    }

    /// Make sure the spliced DSF for `t` is on disk; returns its path.
    /// Concurrent calls share one download/splice.
    pub async fn ensure_dsf(self: &Arc<Self>, t: DsfTile) -> Result<PathBuf, String> {
        let fut = {
            let mut map = self.inflight.lock();
            if let Some(f) = map.get(&t) {
                f.clone()
            } else {
                let this = self.clone();
                let h = tokio::spawn(async move { this.build_dsf(t).await.map_err(|e| format!("{t}: {e:#}")) });
                let f = async move { h.await.map_err(|e| e.to_string())? }.boxed().shared();
                map.insert(t, f.clone());
                f
            }
        };
        let r = fut.await;
        self.inflight.lock().remove(&t);
        if r.is_err() {
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    async fn build_dsf(self: &Arc<Self>, t: DsfTile) -> anyhow::Result<PathBuf> {
        let e = self.entry(t).ok_or_else(|| anyhow::anyhow!("not in catalog"))?;
        let out = self.spliced_path(t, &e);
        if out.exists() {
            touch(&out);
            return Ok(out);
        }
        let obj_path = self.object_path(&e.dsf.object.blake3, "dsf.zst");
        let compressed = match tokio::fs::read(&obj_path).await {
            Ok(d) if blake3::hash(&d).to_hex().as_str() == e.dsf.object.blake3 => d,
            _ => {
                let d = self.client.fetch_object(&self.mirrors(), &e.dsf.object).await?;
                self.stats.downloads.fetch_add(1, Ordering::Relaxed);
                self.stats.download_bytes.fetch_add(d.len() as u64, Ordering::Relaxed);
                let p = obj_path.clone();
                let d2 = d.clone();
                tokio::task::spawn_blocking(move || write_atomic(&p, &d2)).await??;
                d
            }
        };
        let this = self.clone();
        let out2 = out.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let stripped = zstd::decode_all(&compressed[..])?;
            anyhow::ensure!(stripped.len() as u64 == e.dsf.stripped_size, "decompressed size mismatch");
            anyhow::ensure!(!dsf::has_raster_data(&stripped)?, "hosted DSF contains raster data");
            let expected = this.dsf_size(t).unwrap_or(stripped.len() as u64);
            let final_bytes = match this.rasters(t).as_ref() {
                Some(r) => {
                    this.stats.splices.fetch_add(1, Ordering::Relaxed);
                    dsf::splice(&stripped, r)?
                }
                None => stripped,
            };
            anyhow::ensure!(final_bytes.len() as u64 == expected, "spliced size differs from advertised size");
            write_atomic(&out2, &final_bytes)?;
            if !this.is_online() {
                this.invalidate_listing();
            }
            Ok(())
        })
        .await??;
        Ok(out)
    }

    /// Read part of a tile's DSF (downloads/splices on first use).
    pub async fn read_dsf(self: &Arc<Self>, t: DsfTile, off: u64, len: u64) -> Result<Vec<u8>, String> {
        let started = std::time::Instant::now();
        let path = self.ensure_dsf(t).await?;
        self.stats.waits_ms.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        read_range(&path, off, len).await.map_err(|e| e.to_string())
    }

    pub fn mask(&self, name: &str) -> Option<MaskEntry> {
        self.masks_by_name.read().get(&name.to_ascii_lowercase()).cloned()
    }

    async fn ensure_mask(&self, m: &MaskEntry) -> anyhow::Result<PathBuf> {
        let p = self.object_path(&m.object.blake3, "png");
        if p.exists() {
            return Ok(p);
        }
        let d = self.client.fetch_object(&self.mirrors(), &m.object).await?;
        let p2 = p.clone();
        tokio::task::spawn_blocking(move || write_atomic(&p2, &d)).await??;
        Ok(p)
    }

    pub async fn read_mask(&self, name: &str, off: u64, len: u64) -> Result<Vec<u8>, String> {
        let m = self.mask(name).ok_or("unknown mask")?;
        let p = self.ensure_mask(&m).await.map_err(|e| e.to_string())?;
        read_range(&p, off, len).await.map_err(|e| e.to_string())
    }

    /// Textures X-Plane will read for this tile (from the DSF's terrain
    /// definitions), downloading the DSF if needed.
    pub async fn textures(self: &Arc<Self>, t: DsfTile) -> Result<Vec<xof_core::TextureId>, String> {
        let path = self.ensure_dsf(t).await?;
        tokio::task::spawn_blocking(move || -> Result<Vec<xof_core::TextureId>, String> {
            use std::io::Read;
            // DEFN comes right after the header: a prefix is normally enough.
            let mut prefix = Vec::new();
            std::fs::File::open(&path)
                .and_then(|f| f.take(8 << 20).read_to_end(&mut prefix))
                .map_err(|e| e.to_string())?;
            let names = match dsf::terrain_names(&prefix) {
                Ok(n) => n,
                Err(_) => {
                    dsf::terrain_names(&std::fs::read(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?
                }
            };
            let mut out: Vec<xof_core::TextureId> = names
                .iter()
                .filter_map(|n| xof_core::names::TerName::parse(n.rsplit('/').next().unwrap_or(n)))
                .map(|t| t.texture)
                .collect();
            out.sort_by_key(|t| (t.row, t.col, t.zl));
            out.dedup_by_key(|t| (t.row, t.col, t.zl));
            Ok(out)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Prefetch a tile: DSF (with splice) and its masks.
    pub async fn prefetch(self: &Arc<Self>, t: DsfTile) -> Result<(), String> {
        self.ensure_dsf(t).await?;
        if let Some(e) = self.entry(t) {
            for m in &e.masks {
                self.ensure_mask(m).await.map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    pub fn status(&self) -> MeshStatus {
        let m = self.manifest();
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MeshStatus {
            catalog_version: m.catalog_version.clone(),
            license: m.license.clone(),
            attribution: m.attribution.clone(),
            online: self.is_online(),
            pending_update: self.pending.lock().as_ref().map(|p| p.catalog_version.clone()),
            tiles_in_catalog: m.tiles.len(),
            tiles_listed: self.listed_tiles().len(),
            downloads: r(&self.stats.downloads),
            download_bytes: r(&self.stats.download_bytes),
            splices: r(&self.stats.splices),
            failures: r(&self.stats.failures),
            in_flight: self.inflight.lock().len(),
            cache_bytes: dir_size(&self.cfg.cache_dir.join("objects")) + dir_size(&self.cfg.cache_dir.join("spliced")),
        }
    }

    /// Evict least-recently-used cached files until below `cap` bytes, never
    /// touching pinned tiles. Spliced files can always be regenerated locally.
    pub fn evict_to(&self, cap: u64) -> u64 {
        let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        for d in ["objects", "spliced"] {
            if let Ok(rd) = std::fs::read_dir(self.cfg.cache_dir.join(d)) {
                for f in rd.flatten() {
                    if let Ok(md) = f.metadata() {
                        files.push((f.path(), md.len(), md.modified().unwrap_or(UNIX_EPOCH)));
                    }
                }
            }
        }
        let pinned_hashes: HashSet<String> = {
            let m = self.manifest();
            self.cfg.pinned.iter().filter_map(|t| m.tile(*t)).map(|e| e.dsf.object.blake3[..16].to_string()).collect()
        };
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.2);
        let mut freed = 0;
        for (p, len, _) in files {
            if total <= cap {
                break;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if pinned_hashes.iter().any(|h| name.contains(h.as_str())) {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                total -= len;
                freed += len;
            }
        }
        freed
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cfg.cache_dir
    }

    pub fn global_scenery(&self) -> Option<&Path> {
        self.cfg.global_scenery.as_deref()
    }

    /// Mark the time of last use (for LRU).
    pub fn now() -> u64 {
        now_secs()
    }
}

fn touch(p: &Path) {
    if let Ok(f) = std::fs::File::options().append(true).open(p) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p).map(|rd| rd.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0)
}

async fn read_range(path: &Path, off: u64, len: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut f = tokio::fs::File::open(path).await?;
    let size = f.metadata().await?.len();
    if off >= size {
        return Ok(Vec::new());
    }
    let n = len.min(size - off) as usize;
    f.seek(std::io::SeekFrom::Start(off)).await?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).await?;
    Ok(buf)
}
