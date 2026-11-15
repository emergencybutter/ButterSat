//! Imagery providers and the HTTP tile fetcher.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::raster::{Rgba, Tile};
use crate::tile::{TileCoord, TILE_PX};

/// Static description of an imagery source.
pub trait Provider: Send + Sync {
    /// Stable id used in config and cache keys (`esri`, `bing`).
    fn id(&self) -> &str;
    /// Human-readable name.
    fn name(&self) -> &str;
    /// Provider codes found in Ortho4XP file names that map to this provider.
    fn filename_codes(&self) -> &[&str];
    fn max_zoom(&self) -> u8;
    /// URL for a tile.
    fn tile_url(&self, t: TileCoord) -> String;
    /// Maximum concurrent requests.
    fn max_concurrency(&self) -> usize {
        16
    }
    /// Is this response a "no imagery here" placeholder?
    fn is_no_data(&self, status: u16, headers: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        let _ = headers;
        status == 404 || status == 204 || body.is_empty()
    }
}

/// Esri World Imagery (XYZ).
pub struct Esri;

impl Provider for Esri {
    fn id(&self) -> &str {
        "esri"
    }
    fn name(&self) -> &str {
        "Esri World Imagery"
    }
    fn filename_codes(&self) -> &[&str] {
        &["ARC", "Arc", "XO"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{}/{}/{}",
            t.z, t.y, t.x
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        // Esri answers missing tiles with 404 or a tiny "Map data not yet
        // available" image.
        status == 404 || body.len() < 1500
    }
}

/// Bing aerial (quadkey).
pub struct Bing;

impl Provider for Bing {
    fn id(&self) -> &str {
        "bing"
    }
    fn name(&self) -> &str {
        "Bing Maps Aerial"
    }
    fn filename_codes(&self) -> &[&str] {
        &["BI"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        let server = (t.x + t.y) % 4;
        format!("https://ecn.t{server}.tiles.virtualearth.net/tiles/a{}.jpeg?g=136", t.quadkey())
    }
    fn is_no_data(&self, status: u16, h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || body.is_empty() || h.get("X-VE-Tile-Info").is_some_and(|v| v.as_bytes() == b"no-tile")
    }
}

/// Registry of built-in providers.
pub fn builtin(id: &str) -> Option<Arc<dyn Provider>> {
    match id {
        "esri" => Some(Arc::new(Esri)),
        "bing" => Some(Arc::new(Bing)),
        _ => None,
    }
}

pub fn builtin_ids() -> &'static [&'static str] {
    &["esri", "bing"]
}

/// Result of fetching one tile.
#[derive(Clone, Debug)]
pub enum Fetched {
    Data(Bytes),
    NoData,
}

/// Request priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    /// Prefetch / background upgrade.
    Background,
    /// X-Plane is waiting.
    Live,
}

#[derive(Debug, Default)]
pub struct FetchStats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    pub no_data: AtomicU64,
    pub errors: AtomicU64,
}

/// HTTP fetcher with per-provider concurrency, a reserved share for live
/// requests, retries with backoff, and in-flight de-duplication.
pub struct Fetcher {
    client: reqwest::Client,
    provider: Arc<dyn Provider>,
    /// All requests take a permit here.
    total: Arc<Semaphore>,
    /// Background requests also take one here, so they can never use all
    /// permits and starve live reads.
    background: Arc<Semaphore>,
    inflight: Mutex<
        HashMap<TileCoord, futures::future::Shared<futures::future::BoxFuture<'static, Result<Fetched, String>>>>,
    >,
    pub stats: Arc<FetchStats>,
    retries: u32,
}

impl Fetcher {
    pub fn new(provider: Arc<dyn Provider>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("xporthofs/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(32)
            .build()?;
        let n = provider.max_concurrency().max(2);
        Ok(Self {
            client,
            total: Arc::new(Semaphore::new(n)),
            background: Arc::new(Semaphore::new((n * 3 / 4).max(1))),
            provider,
            inflight: Mutex::new(HashMap::new()),
            stats: Arc::new(FetchStats::default()),
            retries: 3,
        })
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// Fetch a tile; concurrent calls for the same tile share one request.
    pub async fn fetch(&self, t: TileCoord, prio: Priority) -> Result<Fetched, String> {
        use futures::FutureExt;
        let fut = {
            let mut map = self.inflight.lock();
            if let Some(f) = map.get(&t) {
                f.clone()
            } else {
                let f = Self::fetch_uncoalesced(
                    self.client.clone(),
                    self.provider.clone(),
                    self.total.clone(),
                    self.background.clone(),
                    self.stats.clone(),
                    self.retries,
                    t,
                    prio,
                )
                .boxed()
                .shared();
                map.insert(t, f.clone());
                f
            }
        };
        let res = fut.await;
        self.inflight.lock().remove(&t);
        res
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_uncoalesced(
        client: reqwest::Client,
        provider: Arc<dyn Provider>,
        total: Arc<Semaphore>,
        background: Arc<Semaphore>,
        stats: Arc<FetchStats>,
        retries: u32,
        t: TileCoord,
        prio: Priority,
    ) -> Result<Fetched, String> {
        if t.z > provider.max_zoom() || !t.is_valid() {
            return Ok(Fetched::NoData);
        }
        let _bg = match prio {
            Priority::Background => Some(background.acquire_owned().await.map_err(|e| e.to_string())?),
            Priority::Live => None,
        };
        let _permit = total.acquire_owned().await.map_err(|e| e.to_string())?;
        let url = provider.tile_url(t);
        let mut last_err = String::new();
        for attempt in 0..=retries {
            if attempt > 0 {
                let backoff = 250u64 << attempt.min(5);
                let jitter = rand::random::<u64>() % 200;
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
            }
            stats.requests.fetch_add(1, Ordering::Relaxed);
            match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers().clone();
                    if status == 429 || status >= 500 {
                        last_err = format!("HTTP {status}");
                        continue;
                    }
                    match resp.bytes().await {
                        Ok(body) => {
                            stats.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                            if provider.is_no_data(status, &headers, &body) {
                                stats.no_data.fetch_add(1, Ordering::Relaxed);
                                return Ok(Fetched::NoData);
                            }
                            if !(200..300).contains(&status) {
                                last_err = format!("HTTP {status}");
                                continue;
                            }
                            return Ok(Fetched::Data(body));
                        }
                        Err(e) => last_err = e.to_string(),
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        stats.errors.fetch_add(1, Ordering::Relaxed);
        Err(format!("{t}: {last_err}"))
    }
}

/// Decode an imagery tile (JPEG or PNG) to a 256×256 RGBA raster.
pub fn decode_tile(bytes: &[u8]) -> anyhow::Result<Tile> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (w, h) = img.dimensions();
    let rgba = Rgba { width: w, height: h, data: img.into_raw() };
    if (w, h) == (TILE_PX, TILE_PX) {
        Ok(rgba)
    } else {
        Ok(rgba.crop_scale(0.0, 0.0, w.min(h) as f32, TILE_PX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let t = TileCoord::new(16, 19344, 24560);
        assert!(Esri.tile_url(t).ends_with("/tile/16/24560/19344"));
        let b = Bing.tile_url(TileCoord::new(3, 3, 5));
        assert!(b.contains("/tiles/a213.jpeg"), "{b}");
    }

    #[test]
    fn decode_png() {
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(256, 256, image::Rgba([1, 2, 3, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let t = decode_tile(&buf).unwrap();
        assert_eq!(t.px(10, 10), [1, 2, 3, 255]);
    }
}
