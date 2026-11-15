//! The texture engine: turns read requests on synthesized DDS files into
//! imagery fetches, composition and BC1 encoding (design §5).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{BoxFuture, Shared};
use futures::FutureExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::cache::{ByteLru, TileStore};
use crate::dds::{self, Segment};
use crate::names::TextureId;
use crate::provider::{decode_tile, Fetched, Fetcher, Priority, Provider};
use crate::raster::{placeholder_tile, Rgba, Tile};
use crate::tile::{TileCoord, TILE_PX};
use crate::bc1;

/// Engine tuning.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Time budget for a live read before degrading.
    pub deadline_ms: u64,
    /// RAM budget for encoded segments (T2).
    pub segment_ram_mb: usize,
    /// RAM budget for decoded imagery tiles.
    pub decoded_ram_mb: usize,
    /// How many zoom levels up to search for a fallback ancestor.
    pub max_fallback_levels: u8,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { deadline_ms: 1200, segment_ram_mb: 1024, decoded_ram_mb: 256, max_fallback_levels: 8 }
    }
}

/// Who is reading (design §4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReaderClass {
    /// X-Plane: full pipeline.
    XPlane,
    /// Indexers, antivirus, ...: cache or low-res only.
    Other,
}

/// Per-read context.
#[derive(Clone, Copy, Debug)]
pub struct ReadCtx {
    pub deadline: Instant,
    pub class: ReaderClass,
}

impl ReadCtx {
    pub fn xplane(budget: Duration) -> Self {
        Self { deadline: Instant::now() + budget, class: ReaderClass::XPlane }
    }
}

#[derive(Clone)]
struct SegBuf {
    data: Arc<Vec<u8>>,
    degraded: bool,
}

type SegKey = (TextureId, Segment);
type SegFuture = Shared<BoxFuture<'static, Result<SegBuf, String>>>;

/// Counters exposed to the UI.
#[derive(Debug, Default)]
pub struct EngineStats {
    pub reads: AtomicU64,
    pub read_bytes: AtomicU64,
    pub segments_built: AtomicU64,
    pub degraded_built: AtomicU64,
    pub upgrades: AtomicU64,
    pub tiles_decoded: AtomicU64,
    /// Read latency histogram: <10ms, <50, <200, <1000, <3000, >=3000.
    pub latency: [AtomicU64; 6],
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EngineStatsSnapshot {
    pub reads: u64,
    pub read_bytes: u64,
    pub segments_built: u64,
    pub degraded_built: u64,
    pub upgrades: u64,
    pub tiles_decoded: u64,
    pub latency_buckets_ms: Vec<(u64, u64)>,
    pub fetch_requests: u64,
    pub fetch_bytes: u64,
    pub fetch_no_data: u64,
    pub fetch_errors: u64,
    pub segment_ram_bytes: u64,
    pub decoded_ram_bytes: u64,
    pub reduced_zoom: bool,
}

const LATENCY_BOUNDS: [u64; 6] = [10, 50, 200, 1000, 3000, u64::MAX];

enum TileState {
    Data(Arc<Tile>),
    NoData,
    Unavailable,
}

struct Inner {
    cfg: EngineConfig,
    fetcher: Fetcher,
    store: Arc<TileStore>,
    segs: ByteLru<SegKey, SegBuf>,
    decoded: ByteLru<TileCoord, Arc<Tile>>,
    inflight: Mutex<HashMap<SegKey, SegFuture>>,
    upgrading: Mutex<HashSet<SegKey>>,
    reduced_zoom: AtomicBool,
    stats: EngineStats,
    cpu: rayon::ThreadPool,
}

/// Cheap to clone handle.
#[derive(Clone)]
pub struct TextureEngine {
    inner: Arc<Inner>,
}

impl TextureEngine {
    pub fn new(cfg: EngineConfig, provider: Arc<dyn Provider>, store: Arc<TileStore>) -> anyhow::Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(1).max(1);
        let cpu = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("xof-cpu-{i}"))
            .build()?;
        Ok(Self {
            inner: Arc::new(Inner {
                segs: ByteLru::new(cfg.segment_ram_mb << 20),
                decoded: ByteLru::new(cfg.decoded_ram_mb << 20),
                fetcher: Fetcher::new(provider)?,
                store,
                cfg,
                inflight: Mutex::new(HashMap::new()),
                upgrading: Mutex::new(HashSet::new()),
                reduced_zoom: AtomicBool::new(false),
                stats: EngineStats::default(),
                cpu,
            }),
        })
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.cfg
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        self.inner.fetcher.provider()
    }

    pub fn store(&self) -> &Arc<TileStore> {
        &self.inner.store
    }

    /// Default live-read context using the configured deadline.
    pub fn live_ctx(&self) -> ReadCtx {
        ReadCtx::xplane(Duration::from_millis(self.inner.cfg.deadline_ms))
    }

    /// Read `[off, off+len)` of a synthesized texture.
    pub async fn read(&self, tex: &TextureId, off: u64, len: u64, ctx: ReadCtx) -> Result<Vec<u8>, String> {
        let started = Instant::now();
        let pieces = dds::pieces(off, len);
        let mut unique: Vec<Segment> = pieces.iter().map(|p| p.segment).collect();
        unique.dedup();
        let bufs = futures::future::join_all(unique.iter().map(|s| self.segment(tex, *s, ctx))).await;
        let mut map = HashMap::new();
        for (s, b) in unique.iter().zip(bufs) {
            map.insert(*s, b?);
        }
        let total: u64 = pieces.iter().map(|p| p.len).sum();
        let mut out = vec![0u8; total as usize];
        for p in &pieces {
            let src = &map[&p.segment];
            out[p.out_off as usize..(p.out_off + p.len) as usize]
                .copy_from_slice(&src[p.seg_off as usize..(p.seg_off + p.len) as usize]);
        }
        let s = &self.inner.stats;
        s.reads.fetch_add(1, Ordering::Relaxed);
        s.read_bytes.fetch_add(total, Ordering::Relaxed);
        let ms = started.elapsed().as_millis() as u64;
        let b = LATENCY_BOUNDS.iter().position(|&ub| ms < ub).unwrap_or(5);
        s.latency[b].fetch_add(1, Ordering::Relaxed);
        Ok(out)
    }

    /// Called when a texture is opened: build the header + tail eagerly so the
    /// fallback tile (ZL-4) is available as early as possible.
    pub fn on_open(&self, tex: &TextureId, class: ReaderClass) {
        let this = self.clone();
        let tex = tex.clone();
        tokio::spawn(async move {
            let ctx = ReadCtx { deadline: Instant::now() + Duration::from_secs(30), class };
            let _ = this.segment(&tex, Segment::Tail, ctx).await;
        });
    }

    /// Does RAM hold any degraded segment of this texture?
    pub fn is_degraded(&self, tex: &TextureId) -> bool {
        !self.inner.segs.keys_where(|k, v| &k.0 == tex && v.degraded).is_empty()
    }

    /// Dynamic zoom (design §5.5): when set, mip 0 is built from ZL-1.
    pub fn set_reduced_zoom(&self, on: bool) {
        let was = self.inner.reduced_zoom.swap(on, Ordering::Relaxed);
        if was && !on {
            for k in self.inner.segs.keys_where(|_, v| v.degraded) {
                self.schedule_upgrade(k);
            }
        }
    }

    pub fn reduced_zoom(&self) -> bool {
        self.inner.reduced_zoom.load(Ordering::Relaxed)
    }

    /// Make sure a raw imagery tile is in T1 (prefetch). Never decodes.
    pub async fn warm_tile(&self, t: TileCoord) -> bool {
        let provider = self.provider().id().to_string();
        let store = self.inner.store.clone();
        let cached = tokio::task::spawn_blocking(move || store.get(&provider, t).ok().flatten())
            .await
            .ok()
            .flatten();
        if cached.is_some() {
            return true;
        }
        match self.inner.fetcher.fetch(t, Priority::Background).await {
            Ok(f) => {
                self.store_put(t, f).await;
                true
            }
            Err(_) => false,
        }
    }

    pub fn stats(&self) -> EngineStatsSnapshot {
        let s = &self.inner.stats;
        let f = &self.inner.fetcher.stats;
        let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
        EngineStatsSnapshot {
            reads: r(&s.reads),
            read_bytes: r(&s.read_bytes),
            segments_built: r(&s.segments_built),
            degraded_built: r(&s.degraded_built),
            upgrades: r(&s.upgrades),
            tiles_decoded: r(&s.tiles_decoded),
            latency_buckets_ms: LATENCY_BOUNDS.iter().zip(&s.latency).map(|(b, c)| (*b, r(c))).collect(),
            fetch_requests: r(&f.requests),
            fetch_bytes: r(&f.bytes),
            fetch_no_data: r(&f.no_data),
            fetch_errors: r(&f.errors),
            segment_ram_bytes: self.inner.segs.used() as u64,
            decoded_ram_bytes: self.inner.decoded.used() as u64,
            reduced_zoom: self.reduced_zoom(),
        }
    }

    // ---- segments -------------------------------------------------------

    async fn segment(&self, tex: &TextureId, seg: Segment, ctx: ReadCtx) -> Result<Arc<Vec<u8>>, String> {
        if seg == Segment::Header {
            static HEADER: std::sync::OnceLock<Arc<Vec<u8>>> = std::sync::OnceLock::new();
            return Ok(HEADER.get_or_init(|| Arc::new(dds::header().to_vec())).clone());
        }
        let key = (tex.clone(), seg);
        if let Some(b) = self.inner.segs.get(&key) {
            return Ok(b.data);
        }
        let fut = {
            let mut map = self.inner.inflight.lock();
            if let Some(f) = map.get(&key) {
                f.clone()
            } else {
                let this = self.clone();
                let k = key.clone();
                // Spawned so that a caller giving up never cancels the build.
                let handle = tokio::spawn(async move { this.build_segment(k, ctx).await });
                let f = async move { handle.await.map_err(|e| e.to_string())? }.boxed().shared();
                map.insert(key.clone(), f.clone());
                f
            }
        };
        let res = fut.await;
        self.inner.inflight.lock().remove(&key);
        res.map(|b| b.data)
    }

    async fn build_segment(&self, key: SegKey, ctx: ReadCtx) -> Result<SegBuf, String> {
        let (tex, seg) = &key;
        let buf = match *seg {
            Segment::Header => unreachable!(),
            Segment::Band { mip, band } => self.build_band(tex, mip, band, ctx).await?,
            Segment::Tail => self.build_tail(tex, ctx).await?,
        };
        debug_assert_eq!(buf.data.len() as u64, seg.len());
        let s = &self.inner.stats;
        s.segments_built.fetch_add(1, Ordering::Relaxed);
        if buf.degraded {
            s.degraded_built.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.segs.insert(key.clone(), buf.clone(), buf.data.len());
        if buf.degraded && !(matches!(seg, Segment::Band { mip: 0, .. }) && self.reduced_zoom()) {
            self.schedule_upgrade(key);
        }
        Ok(buf)
    }

    async fn build_band(&self, tex: &TextureId, mip: u8, band: u8, ctx: ReadCtx) -> Result<SegBuf, String> {
        let n = 16u32 >> mip;
        let origin = tex.origin_at(mip);
        let y = origin.y + band as u32;
        // Dynamic zoom: build mip 0 from the parent level, upscaled.
        let reduced = mip == 0 && self.reduced_zoom();
        let tiles: Vec<TileCoord> = (0..n).map(|dx| TileCoord::new(origin.z, origin.x + dx, y)).collect();
        let (imgs, degraded) = if reduced {
            let parents: Vec<TileCoord> = tiles.iter().map(|t| t.ancestor(1).unwrap()).collect();
            let mut uniq = parents.clone();
            uniq.dedup();
            let (pimgs, _) = self.gather(tex, &uniq, ctx).await;
            let lookup: HashMap<TileCoord, Arc<Tile>> = uniq.into_iter().zip(pimgs).collect();
            let imgs = tiles
                .iter()
                .zip(&parents)
                .map(|(t, p)| {
                    let sx = ((t.x & 1) * 128) as f32;
                    let sy = ((t.y & 1) * 128) as f32;
                    Arc::new(lookup[p].crop_scale(sx, sy, 128.0, TILE_PX))
                })
                .collect();
            (imgs, true)
        } else {
            self.gather(tex, &tiles, ctx).await
        };
        let data = self
            .cpu(move || {
                let mut strip = Rgba::new(n * TILE_PX, TILE_PX);
                for (i, img) in imgs.iter().enumerate() {
                    strip.blit(img, i as u32 * TILE_PX, 0);
                }
                bc1::encode(&strip)
            })
            .await;
        Ok(SegBuf { data: Arc::new(data), degraded })
    }

    async fn build_tail(&self, tex: &TextureId, ctx: ReadCtx) -> Result<SegBuf, String> {
        let t = tex.origin_at(4);
        let (imgs, degraded) = self.gather(tex, &[t], ctx).await;
        let base = imgs.into_iter().next().unwrap();
        let data = self
            .cpu(move || {
                let mut out = Vec::with_capacity(dds::TAIL_LEN as usize);
                let mut level: Rgba = (*base).clone();
                for mip in dds::BANDED_MIPS..dds::MIP_COUNT {
                    if mip > dds::BANDED_MIPS {
                        level = level.downsample_linear();
                    }
                    out.extend_from_slice(&bc1::encode(&level));
                }
                out
            })
            .await;
        Ok(SegBuf { data: Arc::new(data), degraded })
    }

    /// Get imagery for `tiles`, waiting at most until the deadline; missing
    /// tiles are filled from ancestors. Returns images and whether any was a
    /// temporary (upgradable) fill.
    async fn gather(&self, tex: &TextureId, tiles: &[TileCoord], ctx: ReadCtx) -> (Vec<Arc<Tile>>, bool) {
        let handles: Vec<_> = tiles
            .iter()
            .map(|&t| {
                let this = self.clone();
                let allow_net = ctx.class == ReaderClass::XPlane || t.z + 2 <= tex.zl;
                tokio::spawn(async move { this.tile(t, Priority::Live, allow_net).await })
            })
            .collect();
        let mut out = Vec::with_capacity(tiles.len());
        let mut degraded = false;
        for (h, &t) in handles.into_iter().zip(tiles) {
            let state = match tokio::time::timeout_at(ctx.deadline, h).await {
                Ok(Ok(s)) => s,
                _ => TileState::Unavailable,
            };
            match state {
                TileState::Data(img) => out.push(img),
                TileState::NoData => out.push(self.fallback(t).await.0),
                TileState::Unavailable => {
                    degraded = true;
                    out.push(self.fallback(t).await.0);
                }
            }
        }
        (out, degraded)
    }

    /// Best local (no network) stand-in for tile `t`: an upscaled crop of the
    /// nearest cached ancestor, or a neutral placeholder.
    async fn fallback(&self, t: TileCoord) -> (Arc<Tile>, bool) {
        for k in 1..=self.inner.cfg.max_fallback_levels.min(t.z) {
            let a = t.ancestor(k).unwrap();
            if let TileState::Data(img) = self.tile(a, Priority::Live, false).await {
                let size = TILE_PX as f32 / (1u32 << k) as f32;
                let sx = (t.x - (a.x << k)) as f32 * size;
                let sy = (t.y - (a.y << k)) as f32 * size;
                let scaled = self.cpu(move || img.crop_scale(sx, sy, size, TILE_PX)).await;
                return (Arc::new(scaled), true);
            }
        }
        (Arc::new(placeholder_tile()), false)
    }

    /// Decoded tile from RAM, T1, or (if allowed) the network.
    async fn tile(&self, t: TileCoord, prio: Priority, allow_net: bool) -> TileState {
        if let Some(img) = self.inner.decoded.get(&t) {
            return TileState::Data(img);
        }
        let provider = self.provider().id().to_string();
        let store = self.inner.store.clone();
        let cached = tokio::task::spawn_blocking(move || store.get(&provider, t).ok().flatten())
            .await
            .ok()
            .flatten();
        let fetched = match cached {
            Some(f) => f,
            None if allow_net => match self.inner.fetcher.fetch(t, prio).await {
                Ok(f) => {
                    self.store_put(t, f.clone()).await;
                    f
                }
                Err(e) => {
                    tracing::debug!("fetch {t} failed: {e}");
                    return TileState::Unavailable;
                }
            },
            None => return TileState::Unavailable,
        };
        match fetched {
            Fetched::NoData => TileState::NoData,
            Fetched::Data(bytes) => {
                let decoded = self.cpu(move || decode_tile(&bytes)).await;
                match decoded {
                    Ok(img) => {
                        let img = Arc::new(img);
                        self.inner.stats.tiles_decoded.fetch_add(1, Ordering::Relaxed);
                        self.inner.decoded.insert(t, img.clone(), img.data.len());
                        TileState::Data(img)
                    }
                    Err(e) => {
                        tracing::warn!("decode {t} failed: {e}");
                        TileState::NoData
                    }
                }
            }
        }
    }

    async fn store_put(&self, t: TileCoord, f: Fetched) {
        let provider = self.provider().id().to_string();
        let store = self.inner.store.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.put(&provider, t, &f) {
                tracing::warn!("cache put {t}: {e}");
            }
        })
        .await;
    }

    fn schedule_upgrade(&self, key: SegKey) {
        if !self.inner.upgrading.lock().insert(key.clone()) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let (tex, seg) = &key;
            let tiles: Vec<TileCoord> = match *seg {
                Segment::Band { mip, band } => {
                    let o = tex.origin_at(mip);
                    (0..16u32 >> mip).map(|dx| TileCoord::new(o.z, o.x + dx, o.y + band as u32)).collect()
                }
                Segment::Tail => vec![tex.origin_at(4)],
                Segment::Header => vec![],
            };
            // Wait (no deadline) for the real imagery at background priority.
            let all_ok = futures::future::join_all(tiles.iter().map(|&t| this.tile(t, Priority::Background, true)))
                .await
                .iter()
                .all(|s| !matches!(s, TileState::Unavailable));
            let still_reduced = matches!(seg, Segment::Band { mip: 0, .. }) && this.reduced_zoom();
            if all_ok && !still_reduced {
                let ctx = ReadCtx { deadline: Instant::now() + Duration::from_secs(60), class: ReaderClass::XPlane };
                let rebuilt = match *seg {
                    Segment::Band { mip, band } => this.build_band(tex, mip, band, ctx).await,
                    Segment::Tail => this.build_tail(tex, ctx).await,
                    Segment::Header => unreachable!(),
                };
                if let Ok(buf) = rebuilt {
                    if !buf.degraded {
                        this.inner.stats.upgrades.fetch_add(1, Ordering::Relaxed);
                    }
                    this.inner.segs.insert(key.clone(), buf.clone(), buf.data.len());
                }
            }
            this.inner.upgrading.lock().remove(&key);
        });
    }

    async fn cpu<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner.cpu.spawn(move || {
            let _ = tx.send(f());
        });
        rx.await.expect("cpu task panicked")
    }
}
