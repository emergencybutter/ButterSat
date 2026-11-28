//! Night textures through the scenery tree (no mount needed): `.ter` lines,
//! `textures_night` paths, roads read from a Global Scenery DSF and cached.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use xof_core::cache::TileStore;
use xof_core::engine::{EngineConfig, ReaderClass, TextureEngine};
use xof_core::names::TerParams;
use xof_core::night::{black_dds, lit_mip_len, LIT_FILE_LEN};
use xof_core::provider::Provider;
use xof_core::{bc1, TileCoord};
use xof_mesh::{Coverage, MeshConfig, MeshStreamer};
use xof_vfs::{Node, Scenery, SceneryConfig};

struct Mock;
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
    fn tile_url(&self, _: TileCoord) -> String {
        "http://127.0.0.1:9/none".into()
    }
}

fn atom(out: &mut Vec<u8>, tag: &[u8; 4], p: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&((p.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(p);
}

/// A Global Scenery DSF for +40-074 with a grid of town streets over
/// [40.72, 40.78] × [-73.91, -73.85] (all points in one 32-bit pool).
fn gs_dsf() -> Vec<u8> {
    use md5::{Digest, Md5};
    let (lat0, lon0) = (40.0f64, -74.0f64);
    let q = |v: f64| (v * 4_294_967_295.0) as u32;
    let mut lon = Vec::new();
    let mut lat = Vec::new();
    let mut cmds = vec![1, 0, 0, 3, 0, 6, 54]; // pool 0, net 0, local with sidewalks
    let k = 30;
    for i in 0..=k {
        let f = 0.72 + 0.06 * i as f64 / k as f64;
        let g = 0.09 + 0.06 * i as f64 / k as f64;
        for (a, b) in [((0.09, f), (0.15, f)), ((g, 0.72), (g, 0.78))] {
            let first = lon.len() as u16;
            lon.extend([q(a.0), q(b.0)]);
            lat.extend([q(a.1), q(b.1)]);
            cmds.push(10);
            cmds.extend(first.to_le_bytes());
            cmds.extend((first + 2).to_le_bytes());
        }
    }
    let mut po = (lon.len() as u32).to_le_bytes().to_vec();
    po.push(2);
    for plane in [&lon, &lat] {
        po.push(0);
        plane.iter().for_each(|v| po.extend(v.to_le_bytes()));
    }
    let mut sc = Vec::new();
    for v in [1.0f32, lon0 as f32, 1.0, lat0 as f32] {
        sc.extend(v.to_le_bytes());
    }
    let mut geod = Vec::new();
    atom(&mut geod, b"23OP", &po);
    atom(&mut geod, b"23CS", &sc);
    let mut defn = Vec::new();
    atom(&mut defn, b"WTEN", b"lib/g10/roads.net\0");
    let mut out = b"XPLNEDSF".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    atom(&mut out, b"DAEH", b"gs");
    atom(&mut out, b"NFED", &defn);
    atom(&mut out, b"DOEG", &geod);
    atom(&mut out, b"SDMC", &cmds);
    let sum = Md5::digest(&out);
    out.extend_from_slice(&sum);
    out
}

fn scenery(work: &Path, rt: &tokio::runtime::Runtime, lum: Option<u16>, gs: bool) -> Arc<Scenery> {
    let gs_root = work.join("gs");
    let gs_file = gs_root.join("Earth nav data/+40-080/+40-074.dsf");
    if gs && !gs_file.exists() {
        // Written once: caches are keyed on its size and mtime.
        std::fs::create_dir_all(gs_file.parent().unwrap()).unwrap();
        std::fs::write(&gs_file, gs_dsf()).unwrap();
    }
    let mesh = MeshStreamer::new(
        MeshConfig {
            bootstrap_mirrors: vec![],
            cache_dir: work.join("mesh"),
            global_scenery: gs.then_some(gs_root),
            coverage: Coverage::World,
            pinned: HashSet::new(),
            night: lum.is_some(),
        },
        vec![],
    )
    .unwrap();
    let store = TileStore::open(&work.join("tiles.redb")).unwrap();
    let engine = {
        let _g = rt.enter();
        TextureEngine::new(EngineConfig::default(), Arc::new(Mock), store).unwrap()
    };
    Scenery::new(
        SceneryConfig {
            packs: vec![],
            ter: TerParams { lit_luminance: lum, ..Default::default() },
            xplane_processes: vec![],
        },
        Some(mesh),
        engine,
        rt.handle().clone(),
    )
}

fn read_all(s: &Scenery, path: &str) -> Vec<u8> {
    let node = s.resolve(path).unwrap_or_else(|| panic!("{path} not found"));
    let len = s.attr(&node).unwrap().size;
    s.read_blocking(&node, 0, len, ReaderClass::XPlane).unwrap()
}

// A ZL16 texture inside +40-074: [40.714, 40.781] × [-73.916, -73.828].
const TER: &str = "terrain/24624_19312_XO16.ter";
const LIT: &str = "textures_night/24624_19312_XO16_LIT.dds";

#[test]
fn night_textures_served_and_named() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let work = tempfile::tempdir().unwrap();
    let s = scenery(work.path(), &rt, Some(20), true);
    assert!(s.lit().is_some());

    let ter = String::from_utf8(read_all(&s, TER)).unwrap();
    assert!(ter.contains("LIT_TEX_NOWRAP ../textures_night/24624_19312_XO16_LIT.dds\nLUMINANCE 20\n"), "{ter}");
    let water = String::from_utf8(read_all(&s, "terrain/24624_19312_XO16_water.ter")).unwrap();
    assert!(!water.contains("LIT_TEX"));

    assert!(s.list("").iter().any(|(n, _)| n == "textures_night"));
    assert!(matches!(s.resolve("textures_night"), Some(Node::Dir(_))));
    assert!(matches!(s.resolve("TEXTURES_NIGHT/24624_19312_xo16_lit.dds"), Some(Node::Lit(_))));
    assert!(s.resolve("textures_night/24624_19312_XO16.dds").is_none());

    let lit = read_all(&s, LIT);
    assert_eq!(lit.len() as u64, LIT_FILE_LEN);
    assert_ne!(lit, black_dds(), "town should be lit");
    let m0 = bc1::decode(&lit[128..128 + lit_mip_len(0) as usize], 1024, 1024);
    assert!(m0.data.chunks(4).any(|p| p[0] > 100), "no bright lamp");
    // A partial read matches the whole.
    let node = s.resolve(LIT).unwrap();
    assert_eq!(s.read_blocking(&node, 1000, 64, ReaderClass::XPlane).unwrap(), lit[1000..1064]);
    // Roads and the drawn texture were cached on disk.
    let cached: Vec<_> = std::fs::read_dir(work.path().join("mesh/roads")).unwrap().flatten().collect();
    assert_eq!(cached.len(), 1);
    let lit_files: Vec<_> = std::fs::read_dir(work.path().join("mesh/lit")).unwrap().flatten().collect();
    assert_eq!(lit_files.len(), 1);
    assert_eq!(s.lit().unwrap().stats.drawn.load(std::sync::atomic::Ordering::Relaxed), 1);

    // A fresh daemon serves the same texture from the disk cache.
    drop(s);
    let again = scenery(work.path(), &rt, Some(20), true);
    assert_eq!(read_all(&again, LIT), lit);
    let st = &again.lit().unwrap().stats;
    assert_eq!(st.drawn.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(st.from_disk.load(std::sync::atomic::Ordering::Relaxed), 1);
    // Without the disk cache it is drawn again, identically (same roads).
    drop(again);
    std::fs::remove_dir_all(work.path().join("mesh/lit")).unwrap();
    let third = scenery(work.path(), &rt, Some(20), true);
    assert_eq!(read_all(&third, LIT), lit);
}

#[test]
fn night_off_or_without_global_scenery() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    for (lum, gs) in [(None, true), (Some(20), false)] {
        let work = tempfile::tempdir().unwrap();
        let s = scenery(work.path(), &rt, lum, gs);
        assert!(s.lit().is_none());
        let ter = String::from_utf8(read_all(&s, TER)).unwrap();
        assert!(!ter.contains("LIT_TEX"), "{ter}");
        assert!(s.resolve(LIT).is_none());
        assert!(!s.list("").iter().any(|(n, _)| n == "textures_night"));
    }
}

#[test]
fn no_roads_is_black() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let work = tempfile::tempdir().unwrap();
    let s = scenery(work.path(), &rt, Some(12), true);
    // Far from the roads (west Pacific): no Global Scenery DSF there.
    let lit = read_all(&s, "textures_night/25008_7008_XO16_LIT.dds");
    assert_eq!(lit, black_dds());
}
