//! End-to-end: mount the scenery with the platform backend and read it
//! through the OS. Skipped when the filesystem driver is unavailable.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use xof_core::cache::TileStore;
use xof_core::dds;
use xof_core::engine::{EngineConfig, TextureEngine};
use xof_core::names::TerParams;
use xof_core::provider::Provider;
use xof_core::{DsfTile, TileCoord};
use xof_mesh::catalog::{self, DsfEntry, Manifest, Object, TileEntry, ZoomInfo};
use xof_mesh::{Coverage, MeshConfig, MeshStreamer};
use xof_vfs::{Scenery, SceneryConfig};

struct Mock(String);
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
        format!("{}/tiles/{}/{}/{}", self.0, t.z, t.x, t.y)
    }
}

fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(256, 256, image::Rgba([90, 120, 60, 255]));
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

/// Serves `/tiles/...` as PNG and everything else from `root`.
async fn serve(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let tile = Arc::new(png());
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let (root, tile) = (root.clone(), tile.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                let body =
                    if path.starts_with("tiles/") { Some(tile.to_vec()) } else { std::fs::read(root.join(&path)).ok() };
                let resp = match body {
                    Some(b) => {
                        let mut r =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len())
                                .into_bytes();
                        r.extend_from_slice(&b);
                        r
                    }
                    None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = s.write_all(&resp).await;
            });
        }
    });
    format!("http://{addr}")
}

fn stripped_dsf() -> Vec<u8> {
    use md5::{Digest, Md5};
    fn atom(out: &mut Vec<u8>, tag: &[u8; 4], p: &[u8]) {
        out.extend_from_slice(tag);
        out.extend_from_slice(&((p.len() + 8) as u32).to_le_bytes());
        out.extend_from_slice(p);
    }
    let mut defn = Vec::new();
    atom(&mut defn, b"TRET", b"terrain/24560_19344_XO16.ter\0");
    atom(&mut defn, b"NMED", b"");
    let mut out = b"XPLNEDSF".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    atom(&mut out, b"DAEH", b"p");
    atom(&mut out, b"NFED", &defn);
    atom(&mut out, b"SDMC", &[3; 100]);
    let sum = Md5::digest(&out);
    out.extend_from_slice(&sum);
    out
}

fn publish(root: &Path, key: &SigningKey, t: DsfTile, dsf: &[u8]) {
    let v1 = root.join("v1");
    let z = zstd::encode_all(dsf, 3).unwrap();
    let (obj, hash) = catalog::object_name(&format!("tiles/{}", t.folder()), &t.name(), "dsf.zst", &z);
    std::fs::create_dir_all(v1.join(Path::new(&obj).parent().unwrap())).unwrap();
    std::fs::write(v1.join(&obj), &z).unwrap();
    let mut m = Manifest::empty();
    m.catalog_version = "test".into();
    m.tiles.insert(
        t.name(),
        TileEntry {
            dsf: DsfEntry {
                object: Object { object: obj, size: z.len() as u64, blake3: hash },
                stripped_size: dsf.len() as u64,
            },
            masks: vec![],
            zl: ZoomInfo { base: 16, airports: None },
        },
    );
    let zst = m.to_zst().unwrap();
    std::fs::write(v1.join(catalog::MANIFEST_FILE), &zst).unwrap();
    std::fs::write(v1.join(catalog::SIGNATURE_FILE), catalog::sign(key, &zst)).unwrap();
}

#[test]
fn mount_and_read_through_os() {
    if !xof_vfs::driver_available() {
        eprintln!("skipped: {} not available", xof_vfs::driver_name());
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let work = tempfile::tempdir().unwrap();
    let origin = work.path().join("origin");
    let pack = work.path().join("pack");
    std::fs::create_dir_all(pack.join("Earth nav data/+50+000")).unwrap();
    std::fs::write(pack.join("Earth nav data/+50+000/+50+005.dsf"), b"imported").unwrap();
    std::fs::write(pack.join("library.txt"), b"A\n800\nLIBRARY\n").unwrap();

    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let tile = DsfTile::new(40, -74);
    let dsf = stripped_dsf();
    publish(&origin, &key, tile, &dsf);
    let base = rt.block_on(serve(origin.clone()));

    let mesh = MeshStreamer::new(
        MeshConfig {
            bootstrap_mirrors: vec![format!("{base}/v1")],
            cache_dir: work.path().join("mesh"),
            global_scenery: None,
            coverage: Coverage::World,
            pinned: HashSet::new(),
            night: false,
        },
        vec![key.verifying_key()],
    )
    .unwrap();
    rt.block_on(mesh.refresh()).unwrap();
    let store = TileStore::open(&work.path().join("tiles.redb")).unwrap();
    let engine = {
        let _g = rt.enter();
        TextureEngine::new(EngineConfig { deadline_ms: 10_000, ..Default::default() }, Arc::new(Mock(base)), store)
            .unwrap()
    };
    // Treat this test binary as "X-Plane" so it may trigger downloads.
    let me = std::env::current_exe().unwrap();
    let me = me.file_stem().unwrap().to_string_lossy().into_owned();
    let scenery = Scenery::new(
        SceneryConfig { packs: vec![pack], ter: TerParams::default(), xplane_processes: vec![me] },
        Some(mesh),
        engine,
        rt.handle().clone(),
    );
    let mnt = work.path().join("mnt");
    let mounted = match xof_vfs::mount(scenery, &mnt) {
        Ok(m) => m,
        // CI runners where the driver is installed but can't be loaded
        // (macFUSE needs interactive approval).
        Err(e) if std::env::var_os("XOF_MOUNT_TEST_OPTIONAL").is_some() => {
            eprintln!("skipped: mount failed: {e:#}");
            return;
        }
        Err(e) => panic!("mount: {e:#}"),
    };

    // Root listing.
    let mut names: Vec<String> =
        std::fs::read_dir(&mnt).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["Earth nav data", "README.txt", "library.txt", "terrain", "textures"]);
    let readme = std::fs::read_to_string(mnt.join("README.txt")).unwrap();
    assert!(readme.contains("OpenStreetMap") && readme.contains("ODbL"), "{readme}");
    let mut folders: Vec<String> = std::fs::read_dir(mnt.join("Earth nav data"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    folders.sort();
    assert_eq!(folders, ["+40-080", "+50+000"]);

    // Imported pack passthrough, case-insensitive.
    assert_eq!(std::fs::read(mnt.join("earth nav data/+50+000/+50+005.DSF")).unwrap(), b"imported");
    // Streamed DSF (no Global Scenery: served stripped).
    let got = std::fs::read(mnt.join("Earth nav data/+40-080/+40-074.dsf")).unwrap();
    assert_eq!(got, dsf);
    // Synthesized terrain.
    let ter = std::fs::read_to_string(mnt.join("terrain/24560_19344_XO16.ter")).unwrap();
    assert!(ter.contains("BASE_TEX_NOWRAP ../textures/24560_19344_XO16.dds"));
    // Texture: size from metadata, header + a mip-0 range through seeks.
    let tex = mnt.join("textures/24560_19344_XO16.dds");
    assert_eq!(std::fs::metadata(&tex).unwrap().len(), dds::FILE_LEN);
    let mut f = std::fs::File::open(&tex).unwrap();
    let mut head = [0u8; 128];
    f.read_exact(&mut head).unwrap();
    assert_eq!(&head[..], &dds::header()[..]);
    f.seek(SeekFrom::Start(dds::FILE_LEN - 100)).unwrap();
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).unwrap();
    assert_eq!(tail.len(), 100);
    // Water transition and unknown files.
    assert_eq!(std::fs::read(mnt.join("textures/water_transition.png")).unwrap()[..4], [0x89, b'P', b'N', b'G']);
    assert!(std::fs::metadata(mnt.join("textures/nope.dds")).is_err());
    assert!(std::fs::write(mnt.join("textures/x.txt"), b"no").is_err());

    drop(mounted);
    assert!(std::fs::read_dir(&mnt).map(|r| r.count()).unwrap_or(0) == 0);
}
