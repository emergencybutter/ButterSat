//! Night textures served to X-Plane (design §5.6): drawn on first read from
//! the roads of the DSF tiles a photo texture overlaps, kept in memory and
//! in the mesh cache (`lit/`). X-Plane reads every night texture its terrain
//! names while the flight loads, by day too, and some again as it pages
//! mips: the disk cache makes each one a single drawing.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use xof_core::names::TEXTURE_TILES;
use xof_core::night::{self, Roads, LIT_FILE_LEN, RENDER_VERSION};
use xof_core::tile::tile_to_latlon;
use xof_core::{DsfTile, TextureId};

use crate::MeshStreamer;

type Cell = Arc<OnceLock<Arc<Vec<u8>>>>;

/// Night textures kept in memory (≈ 0.7 MB each).
const KEEP: usize = 96;

#[derive(Debug, Default)]
pub struct LitStats {
    /// Drawn from roads.
    pub drawn: AtomicU64,
    pub draw_ms: AtomicU64,
    /// Read back from the disk cache.
    pub from_disk: AtomicU64,
}

pub struct LitTextures {
    mesh: Arc<MeshStreamer>,
    cache: Mutex<lru::LruCache<TextureId, Cell>>,
    pub stats: LitStats,
}

impl LitTextures {
    pub fn new(mesh: Arc<MeshStreamer>) -> Arc<Self> {
        Arc::new(Self {
            mesh,
            cache: Mutex::new(lru::LruCache::new(NonZeroUsize::new(KEEP).unwrap())),
            stats: LitStats::default(),
        })
    }

    /// The DSF tiles a texture overlaps.
    pub fn tiles_of(tex: &TextureId) -> Vec<DsfTile> {
        let (n, w) = tile_to_latlon(tex.col as f64, tex.row as f64, tex.zl);
        let (s, e) = tile_to_latlon((tex.col + TEXTURE_TILES) as f64, (tex.row + TEXTURE_TILES) as f64, tex.zl);
        let mut out = Vec::new();
        for lat in s.floor() as i32..=(n - 1e-9).floor() as i32 {
            for lon in w.floor() as i32..=(e - 1e-9).floor() as i32 {
                out.push(DsfTile::new(lat, lon));
            }
        }
        out
    }

    /// Disk cache file of `tex`: keyed on the drawing version and on the
    /// Global Scenery files its roads come from.
    fn disk_path(&self, tex: &TextureId) -> PathBuf {
        let mut h = blake3::Hasher::new();
        h.update(&RENDER_VERSION.to_le_bytes());
        for t in Self::tiles_of(tex) {
            h.update(self.mesh.roads_key(t).as_bytes());
            h.update(b";");
        }
        let key = &h.finalize().to_hex()[..16];
        self.mesh.cache_dir().join("lit").join(format!("{}.{key}.dds", tex.stem()))
    }

    fn load_or_draw(&self, tex: &TextureId) -> Vec<u8> {
        let path = self.disk_path(tex);
        if let Ok(b) = std::fs::read(&path) {
            if b.len() as u64 == LIT_FILE_LEN {
                self.stats.from_disk.fetch_add(1, Ordering::Relaxed);
                return b;
            }
        }
        let started = std::time::Instant::now();
        let roads: Vec<Arc<Option<Roads>>> = Self::tiles_of(tex).into_iter().map(|t| self.mesh.roads(t)).collect();
        let refs: Vec<&Roads> = roads.iter().filter_map(|r| r.as_ref().as_ref()).collect();
        let out = if refs.is_empty() { night::black_dds() } else { night::render(tex, &refs) };
        self.stats.drawn.fetch_add(1, Ordering::Relaxed);
        self.stats.draw_ms.fetch_add(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        tracing::debug!("night texture {tex} in {:?}", started.elapsed());
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, &out).and_then(|_| std::fs::rename(&tmp, &path)) {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!("night texture cache {}: {e}", path.display());
        }
        out
    }

    /// The whole drawn night texture of `tex`. Blocking: may read Global Scenery (first time per DSF tile)
    /// and draw (≈50–100 ms). Concurrent readers share one drawing.
    fn drawn(&self, tex: &TextureId) -> Arc<Vec<u8>> {
        let cell = self.cache.lock().get_or_insert(tex.clone(), Cell::default).clone();
        cell.get_or_init(|| Arc::new(self.load_or_draw(tex))).clone()
    }

    /// Read part of the night texture of `tex`.
    pub fn read(&self, tex: &TextureId, off: u64, len: u64) -> Vec<u8> {
        let data = self.drawn(tex);
        let start = (off as usize).min(data.len());
        let end = (off.saturating_add(len) as usize).min(data.len());
        data[start..end].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_of_texture() {
        // Manhattan ZL16 starts at 74.003° W: it straddles two DSF tiles.
        assert_eq!(
            LitTextures::tiles_of(&TextureId::new(24624, 19296, 16, "XO")),
            vec![DsfTile::new(40, -75), DsfTile::new(40, -74)]
        );
        assert_eq!(LitTextures::tiles_of(&TextureId::new(24624, 19312, 16, "XO")), vec![DsfTile::new(40, -74)]);
        // A ZL12 texture spans several degrees.
        let t = LitTextures::tiles_of(&TextureId::new(1536, 1200, 12, "XO"));
        assert!(t.len() >= 2 && t.contains(&DsfTile::new(40, -74)), "{t:?}");
    }
}
