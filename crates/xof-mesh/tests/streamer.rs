//! MeshStreamer against a local static origin.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xof_core::DsfTile;
use xof_mesh::catalog::{self, DsfEntry, Manifest, MaskEntry, Object, TileEntry, ZoomInfo};
use xof_mesh::dsf;
use xof_mesh::{Coverage, MeshConfig, MeshStreamer};

fn atom(out: &mut Vec<u8>, tag: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(tag);
    out.extend_from_slice(&((payload.len() + 8) as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

fn make_dsf(atoms: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = b"XPLNEDSF".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    for (t, p) in atoms {
        atom(&mut out, t, p);
    }
    let sum = Md5::digest(&out);
    out.extend_from_slice(&sum);
    out
}

fn stripped(seed: u8) -> Vec<u8> {
    let mut defn = Vec::new();
    atom(&mut defn, b"TRET", b"terrain/24560_19344_XO16.ter\0");
    atom(&mut defn, b"NMED", b"");
    make_dsf(&[(b"DAEH", b"p".to_vec()), (b"NFED", defn), (b"DOEG", vec![seed; 1000]), (b"SDMC", vec![1; 50])])
}

fn global() -> Vec<u8> {
    let mut defn = Vec::new();
    atom(&mut defn, b"NMED", b"elevation\0sea_level\0");
    let mut dems = Vec::new();
    atom(&mut dems, b"IMED", &[0; 20]);
    atom(&mut dems, b"DMED", &[5; 200]);
    atom(&mut dems, b"IMED", &[0; 20]);
    atom(&mut dems, b"DMED", &[6; 200]);
    make_dsf(&[(b"DAEH", b"gs".to_vec()), (b"NFED", defn), (b"SMED", dems)])
}

/// Minimal static file server rooted at `root`. Returns base URL.
async fn serve(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let root = root.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                let resp = match std::fs::read(root.join(&path)) {
                    Ok(body) => {
                        let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                        r.extend_from_slice(&body);
                        r
                    }
                    Err(_) => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = s.write_all(&resp).await;
            });
        }
    });
    format!("http://{addr}/v1")
}

/// Publish a catalog of `tiles` into `root/v1`.
fn publish(root: &Path, key: &SigningKey, version: &str, tiles: &[(DsfTile, Vec<u8>)], mask: Option<&[u8]>) -> Manifest {
    let v1 = root.join("v1");
    let mut m = Manifest::empty();
    m.catalog_version = version.into();
    for (t, data) in tiles {
        let z = zstd::encode_all(&data[..], 3).unwrap();
        let (obj, hash) = catalog::object_name(&format!("tiles/{}", t.folder()), &t.name(), "dsf.zst", &z);
        std::fs::create_dir_all(v1.join(Path::new(&obj).parent().unwrap())).unwrap();
        std::fs::write(v1.join(&obj), &z).unwrap();
        let mut masks = vec![];
        if let Some(png) = mask {
            let (mobj, mhash) = catalog::object_name("masks", "24560_19344_ZL16", "png", png);
            std::fs::create_dir_all(v1.join("masks")).unwrap();
            std::fs::write(v1.join(&mobj), png).unwrap();
            masks.push(MaskEntry {
                name: "24560_19344_ZL16.png".into(),
                object: Object { object: mobj, size: png.len() as u64, blake3: mhash },
            });
        }
        m.tiles.insert(
            t.name(),
            TileEntry {
                dsf: DsfEntry { object: Object { object: obj, size: z.len() as u64, blake3: hash }, stripped_size: data.len() as u64 },
                masks,
                zl: ZoomInfo { base: 16, airports: Some(17) },
            },
        );
    }
    let zst = m.to_zst().unwrap();
    std::fs::write(v1.join(catalog::MANIFEST_FILE), &zst).unwrap();
    std::fs::write(v1.join(catalog::SIGNATURE_FILE), catalog::sign(key, &zst)).unwrap();
    m
}

fn streamer(cache: &Path, gs: Option<PathBuf>, mirrors: Vec<String>, key: &SigningKey) -> Arc<MeshStreamer> {
    MeshStreamer::new(
        MeshConfig {
            bootstrap_mirrors: mirrors,
            cache_dir: cache.to_path_buf(),
            global_scenery: gs,
            coverage: Coverage::World,
            pinned: HashSet::new(),
        },
        vec![key.verifying_key()],
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_splice_and_sizes() {
    let origin = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let gs_root = tempfile::tempdir().unwrap();
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let t = DsfTile::new(40, -74);
    let s = stripped(1);
    publish(origin.path(), &key, "v1", &[(t, s.clone()), (DsfTile::new(41, -74), stripped(2))], Some(b"PNGDATA"));
    // Global Scenery only for +40-074.
    let gs_file = gs_root.path().join(t.dsf_rel_path());
    std::fs::create_dir_all(gs_file.parent().unwrap()).unwrap();
    std::fs::write(&gs_file, global()).unwrap();

    let base = serve(origin.path().to_path_buf()).await;
    let ms = streamer(cache.path(), Some(gs_root.path().to_path_buf()), vec![base], &key);
    assert!(ms.refresh().await.unwrap());
    let mut listed = ms.listed_tiles();
    listed.sort();
    assert_eq!(listed, vec![DsfTile::new(40, -74), DsfTile::new(41, -74)]);

    // Advertised size before any download includes the local rasters.
    let size = ms.dsf_size(t).unwrap();
    let rasters = dsf::extract_rasters(&global()).unwrap().unwrap();
    assert_eq!(size, s.len() as u64 + rasters.extra_len());
    let all = ms.read_dsf(t, 0, size + 100).await.unwrap();
    assert_eq!(all.len() as u64, size);
    assert!(dsf::md5_ok(&all));
    assert!(dsf::has_raster_data(&all).unwrap());
    assert_eq!(all, dsf::splice(&s, &rasters).unwrap());
    // Partial read.
    assert_eq!(ms.read_dsf(t, 10, 20).await.unwrap(), all[10..30].to_vec());

    // No Global Scenery for the neighbour: served stripped.
    let n = DsfTile::new(41, -74);
    assert_eq!(ms.dsf_size(n).unwrap(), stripped(2).len() as u64);
    assert_eq!(ms.read_dsf(n, 0, 1 << 20).await.unwrap(), stripped(2));

    // Masks.
    assert_eq!(ms.read_mask("24560_19344_ZL16.png", 0, 100).await.unwrap(), b"PNGDATA");
    assert_eq!(ms.status().downloads, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_lists_only_cached_and_updates_are_deferred() {
    let origin = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let a = DsfTile::new(40, -74);
    let b = DsfTile::new(40, -73);
    publish(origin.path(), &key, "v1", &[(a, stripped(1)), (b, stripped(2))], None);
    let base = serve(origin.path().to_path_buf()).await;
    {
        let ms = streamer(cache.path(), None, vec![base.clone()], &key);
        ms.refresh().await.unwrap();
        ms.prefetch(a).await.unwrap();
    }
    // Restart offline (unreachable mirror): cached manifest, only `a` listed.
    let ms = streamer(cache.path(), None, vec!["http://127.0.0.1:9/v1".into()], &key);
    assert!(ms.refresh().await.is_err());
    assert_eq!(ms.listed_tiles(), vec![a]);
    assert_eq!(ms.read_dsf(a, 0, 8).await.unwrap(), b"XPLNEDSF");

    // Online with a new catalog version: kept pending until promoted.
    let ms = streamer(cache.path(), None, vec![base], &key);
    publish(origin.path(), &key, "v2", &[(a, stripped(9))], None);
    ms.refresh().await.unwrap();
    assert_eq!(ms.manifest().catalog_version, "v1");
    assert_eq!(ms.status().pending_update.as_deref(), Some("v2"));
    assert!(ms.promote_pending());
    assert_eq!(ms.manifest().catalog_version, "v2");
    assert_eq!(ms.read_dsf(a, 0, 1 << 20).await.unwrap(), stripped(9));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_bad_signature_and_tampered_objects() {
    let origin = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let a = DsfTile::new(40, -74);
    let m = publish(origin.path(), &key, "v1", &[(a, stripped(1))], None);
    let base = serve(origin.path().to_path_buf()).await;

    let other = SigningKey::generate(&mut rand::rngs::OsRng);
    let ms = streamer(cache.path(), None, vec![base.clone()], &other);
    assert!(ms.refresh().await.is_err());
    assert!(ms.listed_tiles().is_empty());

    // Tamper with the object after publishing.
    let obj = &m.tiles[&a.name()].dsf.object.object;
    let p = origin.path().join("v1").join(obj);
    let mut d = std::fs::read(&p).unwrap();
    d[3] ^= 0xff;
    std::fs::write(&p, d).unwrap();
    let ms = streamer(cache.path(), None, vec![base], &key);
    ms.refresh().await.unwrap();
    assert!(ms.read_dsf(a, 0, 8).await.is_err());
    let _ = BTreeMap::<u8, u8>::new();
}
