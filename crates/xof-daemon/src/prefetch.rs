//! Prefetcher (design §9): keeps meshes around/ahead of the aircraft ready,
//! warms low-zoom imagery in a corridor ahead, drives dynamic zoom, and
//! seeds departure areas and SimBrief routes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{mpsc, watch, Semaphore};
use xof_core::engine::TextureEngine;
use xof_core::tile::{latlon_to_tile, meters_per_pixel, TILE_PX};
use xof_core::{DsfTile, TileCoord};
use xof_mesh::MeshStreamer;
use xof_proto::config::PrefetchConfig;
use xof_proto::{AircraftState, PrefetchStatus};

/// Base texture zoom we prefetch for (imagery at base-4 ..= base-1).
const BASE_ZL: u8 = 16;

#[derive(Debug)]
enum Job {
    Mesh(DsfTile),
    Imagery(TileCoord),
}

pub struct Prefetcher {
    cfg: Mutex<PrefetchConfig>,
    engine: Mutex<TextureEngine>,
    mesh: Mutex<Option<Arc<MeshStreamer>>>,
    tx: mpsc::UnboundedSender<Job>,
    seen: Mutex<HashSet<TileCoord>>,
    seen_mesh: Mutex<HashSet<DsfTile>>,
    queued: AtomicUsize,
    mesh_done: AtomicU64,
    imagery_done: AtomicU64,
    reduced: AtomicBool,
    area: Mutex<Vec<(f64, f64)>>,
}

impl Prefetcher {
    pub fn spawn(
        cfg: PrefetchConfig,
        engine: TextureEngine,
        mesh: Option<Arc<MeshStreamer>>,
        aircraft: watch::Receiver<Option<AircraftState>>,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::new(Self {
            cfg: Mutex::new(cfg),
            engine: Mutex::new(engine),
            mesh: Mutex::new(mesh),
            tx,
            seen: Mutex::new(HashSet::new()),
            seen_mesh: Mutex::new(HashSet::new()),
            queued: AtomicUsize::new(0),
            mesh_done: AtomicU64::new(0),
            imagery_done: AtomicU64::new(0),
            reduced: AtomicBool::new(false),
            area: Mutex::new(Vec::new()),
        });
        tokio::spawn(this.clone().worker(rx));
        tokio::spawn(this.clone().planner(aircraft));
        this
    }

    /// Swap components after a config change / remount.
    pub fn reconfigure(&self, cfg: PrefetchConfig, engine: TextureEngine, mesh: Option<Arc<MeshStreamer>>) {
        *self.cfg.lock() = cfg;
        *self.engine.lock() = engine;
        *self.mesh.lock() = mesh;
        self.seen.lock().clear();
        self.seen_mesh.lock().clear();
    }

    pub fn status(&self) -> PrefetchStatus {
        PrefetchStatus {
            enabled: self.cfg.lock().enabled,
            queued: self.queued.load(Ordering::Relaxed),
            mesh_tiles_done: self.mesh_done.load(Ordering::Relaxed),
            imagery_tiles_done: self.imagery_done.load(Ordering::Relaxed),
            reduced_zoom: self.reduced.load(Ordering::Relaxed),
            area: self.area.lock().clone(),
        }
    }

    fn push_mesh(&self, t: DsfTile) -> bool {
        // Only tiles the catalog presents and that aren't on disk yet;
        // uncovered tiles (ocean, outside coverage) are never queued.
        let wanted = match self.mesh.lock().as_ref() {
            Some(m) => m.is_listed(t) && !m.is_dsf_ready(t),
            None => false,
        };
        if !wanted || !self.seen_mesh.lock().insert(t) {
            return false;
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Job::Mesh(t));
        true
    }

    fn push_imagery(&self, t: TileCoord) -> bool {
        {
            let mut seen = self.seen.lock();
            if seen.len() > 200_000 {
                seen.clear();
            }
            if !seen.insert(t) {
                return false;
            }
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Job::Imagery(t));
        true
    }

    async fn worker(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Job>) {
        let mesh_sem = Arc::new(Semaphore::new(2));
        // The fetcher's background share is the real limit; this only bounds
        // spawned tasks.
        let img_sem = Arc::new(Semaphore::new(64));
        while let Some(job) = rx.recv().await {
            let this = self.clone();
            match job {
                Job::Mesh(t) => {
                    let permit = mesh_sem.clone().acquire_owned().await.unwrap();
                    let mesh = self.mesh.lock().clone();
                    tokio::spawn(async move {
                        if let Some(m) = mesh {
                            match m.prefetch(t).await {
                                Ok(()) => {
                                    this.mesh_done.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    tracing::debug!("mesh prefetch {t}: {e}");
                                    this.seen_mesh.lock().remove(&t);
                                }
                            }
                        }
                        this.queued.fetch_sub(1, Ordering::Relaxed);
                        drop(permit);
                    });
                }
                Job::Imagery(t) => {
                    let permit = img_sem.clone().acquire_owned().await.unwrap();
                    let engine = self.engine.lock().clone();
                    tokio::spawn(async move {
                        if engine.warm_tile(t).await {
                            this.imagery_done.fetch_add(1, Ordering::Relaxed);
                        } else {
                            this.seen.lock().remove(&t);
                        }
                        this.queued.fetch_sub(1, Ordering::Relaxed);
                        drop(permit);
                    });
                }
            }
        }
    }

    async fn planner(self: Arc<Self>, mut aircraft: watch::Receiver<Option<AircraftState>>) {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tick.tick().await;
            let Some(a) = aircraft.borrow_and_update().clone() else { continue };
            let cfg = self.cfg.lock().clone();
            self.update_dynamic_zoom(&cfg, &a);
            if cfg.enabled {
                self.plan(&cfg, &a);
            }
        }
    }

    fn update_dynamic_zoom(&self, cfg: &PrefetchConfig, a: &AircraftState) {
        let agl_ft = a.agl_m * 3.28084;
        let on = self.reduced.load(Ordering::Relaxed);
        let want = cfg.dynamic_zoom
            && if on { agl_ft > cfg.dynamic_zoom_agl_ft - 500.0 } else { agl_ft > cfg.dynamic_zoom_agl_ft + 500.0 };
        if want != on {
            self.reduced.store(want, Ordering::Relaxed);
            self.engine.lock().set_reduced_zoom(want);
            tracing::info!("dynamic zoom {}", if want { "on (high altitude)" } else { "off" });
        }
    }

    fn plan(&self, cfg: &PrefetchConfig, a: &AircraftState) {
        // Meshes: neighbourhood now + tiles along the track ahead.
        let here = DsfTile::containing(a.lat, a.lon);
        for t in here.neighbourhood(cfg.mesh_radius) {
            self.push_mesh(t);
        }
        let ahead_m = (a.groundspeed_ms * cfg.lookahead_minutes * 60.0).max(5_000.0);
        let path = sample_path(a.lat, a.lon, a.track_deg, ahead_m, 2_000.0);
        let mesh_ahead_m = (a.groundspeed_ms * 60.0 * 15.0).max(30_000.0);
        for (lat, lon) in sample_path(a.lat, a.lon, a.track_deg, mesh_ahead_m, 10_000.0) {
            self.push_mesh(DsfTile::containing(lat, lon));
        }
        // Imagery: corridor ahead, wider and longer at low zoom.
        for (levels_up, half_width_m) in [(4u8, 12_000.0), (3, 8_000.0), (2, 5_000.0), (1, 3_000.0)] {
            let z = BASE_ZL - levels_up;
            for &(lat, lon) in &path {
                for t in tiles_around(lat, lon, z, half_width_m) {
                    self.push_imagery(t);
                }
            }
        }
        *self.area.lock() = corridor_outline(a.lat, a.lon, a.track_deg, ahead_m, 12_000.0);
    }

    /// Pre-fetch meshes and low-zoom imagery around a point (prepare departure).
    pub fn prepare(self: &Arc<Self>, lat: f64, lon: f64) -> (usize, usize) {
        self.prewarm(lat, lon);
        let mut m = 0;
        for t in DsfTile::containing(lat, lon).neighbourhood(1) {
            m += self.push_mesh(t) as usize;
        }
        let mut i = 0;
        for (levels_up, r) in [(4u8, 20_000.0), (3, 15_000.0), (2, 10_000.0), (1, 6_000.0), (0, 3_000.0)] {
            for t in tiles_around(lat, lon, BASE_ZL - levels_up, r) {
                i += self.push_imagery(t) as usize;
            }
        }
        (m, i)
    }

    /// Pre-warm what X-Plane loads when a flight starts here: the textures
    /// listed by the DSFs around the point (downloading those DSFs), nearest
    /// first, within `prewarm_radius_km`. Runs in the background.
    pub fn prewarm(self: &Arc<Self>, lat: f64, lon: f64) {
        let this = self.clone();
        tokio::spawn(async move {
            let Some(mesh) = this.mesh.lock().clone() else { return };
            let radius_m = this.cfg.lock().prewarm_radius_km * 1000.0;
            let started = std::time::Instant::now();
            let mut texs = Vec::new();
            for t in DsfTile::containing(lat, lon).neighbourhood(1) {
                if !mesh.is_listed(t) {
                    continue;
                }
                match mesh.textures(t).await {
                    Ok(v) => texs.extend(v),
                    Err(e) => tracing::debug!("prewarm {t}: {e}"),
                }
            }
            let dist = |x: &xof_core::TextureId| {
                let (la, lo) = x.center_latlon();
                haversine(lat, lon, la, lo)
            };
            texs.retain(|x| dist(x) <= radius_m);
            texs.sort_by(|a, b| dist(a).total_cmp(&dist(b)));
            let mut tiles = 0;
            for tex in &texs {
                for t in TextureEngine::texture_tiles(tex) {
                    tiles += this.push_imagery(t) as usize;
                }
            }
            tracing::info!(
                "prewarm around {lat:.3},{lon:.3}: {} textures, {tiles} imagery tiles queued ({} ms to list)",
                texs.len(),
                started.elapsed().as_millis()
            );
        });
    }

    /// Seed meshes and the lowest imagery zooms along a route.
    pub fn route(&self, pts: &[(f64, f64)]) -> (usize, usize) {
        let (mut m, mut i) = (0, 0);
        for w in pts.windows(2) {
            let ((la, lo), (lb, lob)) = (w[0], w[1]);
            let dist = haversine(la, lo, lb, lob);
            let steps = (dist / 5_000.0).ceil().max(1.0) as usize;
            for s in 0..=steps {
                let f = s as f64 / steps as f64;
                let (lat, lon) = (la + (lb - la) * f, lo + (lob - lo) * f);
                m += self.push_mesh(DsfTile::containing(lat, lon)) as usize;
                for t in tiles_around(lat, lon, BASE_ZL - 4, 8_000.0) {
                    i += self.push_imagery(t) as usize;
                }
            }
        }
        (m, i)
    }
}

const EARTH_R: f64 = 6_371_000.0;

fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_R * a.sqrt().asin()
}

/// Point at `dist` meters along `bearing` (degrees) from (lat, lon).
pub fn destination(lat: f64, lon: f64, bearing: f64, dist: f64) -> (f64, f64) {
    let (p1, l1, b, d) = (lat.to_radians(), lon.to_radians(), bearing.to_radians(), dist / EARTH_R);
    let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * b.cos()).asin();
    let l2 = l1 + (b.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
    (p2.to_degrees(), (l2.to_degrees() + 540.0) % 360.0 - 180.0)
}

fn sample_path(lat: f64, lon: f64, track: f64, length: f64, step: f64) -> Vec<(f64, f64)> {
    let n = (length / step).ceil() as usize;
    (0..=n).map(|i| destination(lat, lon, track, i as f64 * step)).collect()
}

/// Tiles at zoom `z` within roughly `radius_m` of a point.
pub fn tiles_around(lat: f64, lon: f64, z: u8, radius_m: f64) -> Vec<TileCoord> {
    let tile_m = meters_per_pixel(lat, z) * TILE_PX as f64;
    let r = (radius_m / tile_m).ceil() as i64;
    let (x, y) = latlon_to_tile(lat, lon, z);
    let (cx, cy) = (x.floor() as i64, y.floor() as i64);
    let n = 1i64 << z;
    let mut out = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r * r + r {
                continue;
            }
            let (tx, ty) = ((cx + dx).rem_euclid(n), cy + dy);
            if (0..n).contains(&ty) {
                out.push(TileCoord::new(z, tx as u32, ty as u32));
            }
        }
    }
    out
}

fn corridor_outline(lat: f64, lon: f64, track: f64, length: f64, half_width: f64) -> Vec<(f64, f64)> {
    let left = destination(lat, lon, track - 90.0, half_width);
    let right = destination(lat, lon, track + 90.0, half_width);
    let (el, er) = {
        let end = destination(lat, lon, track, length);
        (destination(end.0, end.1, track - 90.0, half_width), destination(end.0, end.1, track + 90.0, half_width))
    };
    vec![left, el, er, right]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry() {
        let (lat, lon) = destination(40.0, -74.0, 90.0, 10_000.0);
        assert!((lat - 40.0).abs() < 0.01 && lon > -74.0);
        assert!((haversine(40.0, -74.0, lat, lon) - 10_000.0).abs() < 1.0);
        let t = tiles_around(40.64, -73.78, 12, 12_000.0);
        assert!(t.len() >= 9 && t.len() < 60, "{}", t.len());
        assert!(t.iter().all(|c| c.z == 12));
    }
}
