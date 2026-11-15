//! Engine tests against a local mock tile server.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xof_core::bc1;
use xof_core::cache::TileStore;
use xof_core::dds::{self, Segment};
use xof_core::engine::{EngineConfig, ReadCtx, ReaderClass, TextureEngine};
use xof_core::provider::Provider;
use xof_core::{TextureId, TileCoord};

/// Colour served for a zoom level.
fn color(z: u8) -> [u8; 3] {
    match z {
        16 => [200, 40, 40],
        15 => [40, 200, 40],
        14 => [40, 40, 200],
        13 => [200, 200, 40],
        12 => [40, 200, 200],
        _ => [128, 128, 128],
    }
}

fn png(c: [u8; 3]) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(256, 256, image::Rgba([c[0], c[1], c[2], 255]));
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

struct Mock {
    base: String,
    hits: Arc<AtomicU64>,
}

impl Provider for Mock {
    fn id(&self) -> &str {
        "mock"
    }
    fn name(&self) -> &str {
        "mock"
    }
    fn filename_codes(&self) -> &[&str] {
        &["XO"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        self.hits.fetch_add(0, Ordering::Relaxed);
        format!("{}/{}/{}/{}", self.base, t.z, t.x, t.y)
    }
    fn is_no_data(&self, status: u16, _: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || body.is_empty()
    }
}

/// Serve PNGs; requests for zoom `slow_z` are delayed by `delay`.
async fn serve(slow_z: u8, delay: Duration) -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicU64::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let h = h.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/");
                let z: u8 = path.split('/').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                h.fetch_add(1, Ordering::Relaxed);
                if z == slow_z {
                    tokio::time::sleep(delay).await;
                }
                let body = png(color(z));
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    (format!("http://{addr}"), hits)
}

fn engine(base: String, hits: Arc<AtomicU64>, dir: &std::path::Path, deadline_ms: u64) -> TextureEngine {
    let store = TileStore::open(&dir.join("tiles.redb")).unwrap();
    // Band-level behaviour; read-ahead has its own test.
    let cfg = EngineConfig { deadline_ms, read_ahead: false, ..Default::default() };
    TextureEngine::new(cfg, Arc::new(Mock { base, hits }), store).unwrap()
}

fn tex() -> TextureId {
    TextureId::new(24560, 19344, 16, "XO")
}

/// Decode the first pixel of a banded segment from a full-file buffer.
fn first_px(file: &[u8], seg: Segment) -> [u8; 4] {
    let off = seg.offset() as usize;
    let img = bc1::decode(&file[off..off + 8], 4, 4);
    img.px(0, 0)
}

fn close(a: [u8; 4], b: [u8; 3]) -> bool {
    (0..3).all(|i| (a[i] as i32 - b[i] as i32).abs() <= 8)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_read_uses_matching_zoom_per_mip() {
    let (base, hits) = serve(255, Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(base, hits.clone(), dir.path(), 10_000);
    let ctx = e.live_ctx();
    let file = e.read(&tex(), 0, dds::FILE_LEN, ctx).await.unwrap();
    assert_eq!(file.len() as u64, dds::FILE_LEN);
    assert_eq!(&file[..128], &dds::header()[..]);
    assert!(close(first_px(&file, Segment::Band { mip: 0, band: 0 }), color(16)));
    assert!(close(first_px(&file, Segment::Band { mip: 1, band: 3 }), color(15)));
    assert!(close(first_px(&file, Segment::Band { mip: 2, band: 0 }), color(14)));
    assert!(close(first_px(&file, Segment::Band { mip: 3, band: 1 }), color(13)));
    assert!(close(first_px(&file, Segment::Tail), color(12)));
    // 256 + 64 + 16 + 4 + 1 tiles.
    assert_eq!(hits.load(Ordering::Relaxed), 341);
    assert!(!e.is_degraded(&tex()));

    // Second read is served from RAM: no new requests.
    let again = e.read(&tex(), 1000, 5000, e.live_ctx()).await.unwrap();
    assert_eq!(&again[..], &file[1000..6000]);
    assert_eq!(hits.load(Ordering::Relaxed), 341);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_tiles_degrade_then_upgrade() {
    let (base, hits) = serve(16, Duration::from_millis(1500)).await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(base, hits, dir.path(), 300);
    // Warm the fallback (ZL15 parent) by reading mip1's first band.
    let m1 = Segment::Band { mip: 1, band: 0 };
    e.read(&tex(), m1.offset(), 16, ReadCtx::xplane(Duration::from_secs(10))).await.unwrap();

    let m0 = Segment::Band { mip: 0, band: 0 };
    let t0 = std::time::Instant::now();
    let data = e.read(&tex(), m0.offset(), m0.len(), e.live_ctx()).await.unwrap();
    let took = t0.elapsed();
    assert!(took < Duration::from_millis(1200), "read took {took:?}");
    // Filled from the green ZL15 parent.
    let px = bc1::decode(&data[..8], 4, 4).px(0, 0);
    assert!(close(px, color(15)), "{px:?}");
    assert!(e.is_degraded(&tex()));

    // The background upgrade replaces it with real ZL16 imagery.
    for _ in 0..50 {
        if !e.is_degraded(&tex()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!e.is_degraded(&tex()));
    let data = e.read(&tex(), m0.offset(), 8, e.live_ctx()).await.unwrap();
    assert!(close(bc1::decode(&data, 4, 4).px(0, 0), color(16)));
    assert!(e.stats().upgrades >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_ahead_follows_the_mip_being_read() {
    let (base, hits) = serve(255, Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let store = TileStore::open(&dir.path().join("tiles.redb")).unwrap();
    let e = TextureEngine::new(
        EngineConfig { deadline_ms: 5000, ..Default::default() },
        Arc::new(Mock { base, hits: hits.clone() }),
        store,
    )
    .unwrap();
    // Open + X-Plane's 64 KiB probe: the tail and mip 0 band 0 only.
    e.on_open(&tex(), ReaderClass::XPlane);
    e.read(&tex(), 0, 65_536, e.live_ctx()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::Relaxed), 1 + 16);
    // A distant texture: mip 3 only -> mip 3's two bands, nothing larger.
    let m3 = Segment::Band { mip: 3, band: 0 };
    e.read(&tex(), m3.offset(), 16, e.live_ctx()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::Relaxed), 1 + 16 + 4);
    // Read past the probe in mip 0: everything is built in the background.
    e.read(&tex(), 65_536, 65_536, e.live_ctx()).await.unwrap();
    for _ in 0..100 {
        if hits.load(Ordering::Relaxed) >= 341 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(hits.load(Ordering::Relaxed), 341);
    // Later reads are served from RAM.
    let last = Segment::Band { mip: 2, band: 3 };
    e.read(&tex(), last.offset(), last.len(), e.live_ctx()).await.unwrap();
    assert_eq!(hits.load(Ordering::Relaxed), 341);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_readers_never_fetch_high_zoom() {
    let (base, hits) = serve(255, Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(base, hits.clone(), dir.path(), 2000);
    let ctx = ReadCtx { deadline: tokio::time::Instant::now() + Duration::from_secs(5), class: ReaderClass::Other };
    let m0 = Segment::Band { mip: 0, band: 0 };
    e.read(&tex(), m0.offset(), m0.len(), ctx).await.unwrap();
    // Only allowed zooms (<= ZL-2) may be fetched; the band's 16 ZL16 tiles
    // must not be.
    assert_eq!(hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reduced_zoom_builds_mip0_from_parent() {
    let (base, hits) = serve(255, Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let e = engine(base, hits.clone(), dir.path(), 5000);
    e.set_reduced_zoom(true);
    let m0 = Segment::Band { mip: 0, band: 2 };
    let data = e.read(&tex(), m0.offset(), 8, e.live_ctx()).await.unwrap();
    assert!(close(bc1::decode(&data, 4, 4).px(0, 0), color(15)));
    // 8 parent tiles for the band, no ZL16 requests.
    assert_eq!(hits.load(Ordering::Relaxed), 8);
    e.set_reduced_zoom(false);
    for _ in 0..50 {
        if !e.is_degraded(&tex()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let data = e.read(&tex(), m0.offset(), 8, e.live_ctx()).await.unwrap();
    assert!(close(bc1::decode(&data, 4, 4).px(0, 0), color(16)));
}

#[test]
fn texture_tiles_cover_all_mips() {
    let t = xof_core::TextureId::new(24560, 19344, 16, "XO");
    let tiles = TextureEngine::texture_tiles(&t);
    assert_eq!(tiles.len(), 256 + 64 + 16 + 4 + 1);
    assert_eq!(tiles.iter().filter(|c| c.z == 16).count(), 256);
    assert_eq!(tiles.iter().filter(|c| c.z == 12).count(), 1);
    assert!(tiles.contains(&xof_core::TileCoord::new(16, 19344, 24560)));
    assert!(tiles.contains(&xof_core::TileCoord::new(16, 19359, 24575)));
}
