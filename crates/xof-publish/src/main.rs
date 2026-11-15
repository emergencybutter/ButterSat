//! xof-publish: build, sign and upload the mesh catalog (design §10).
//!
//! ```text
//! xof-publish keygen  --out signing.key
//! xof-publish build   --tiles Tiles/ --out publish/ --key signing.key --version 2026.11.1 --mirror https://mesh.example.org/v1
//! xof-publish upload  --dir publish/ --target dir --dest /srv/mesh
//! xof-publish upload  --dir publish/ --target s3 --endpoint https://<acct>.r2.cloudflarestorage.com --bucket mesh
//! xof-publish verify  --origin https://mesh.example.org/v1 --pubkey <hex>
//! ```

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use ed25519_dalek::SigningKey;
use xof_core::names::{MaskName, TerName};
use xof_core::DsfTile;
use xof_mesh::catalog::{self, DsfEntry, Manifest, MaskEntry, Object, TileEntry, ZoomInfo};
use xof_mesh::dsf;

#[derive(Parser)]
#[command(name = "xof-publish", version, about = "Build, sign and upload the xporthofs mesh catalog")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Target {
    Dir,
    S3,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate an ed25519 signing key.
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Print the public key for a signing key.
    Pubkey {
        #[arg(long)]
        key: PathBuf,
    },
    /// Package Ortho4XP tile folders into a signed catalog tree (`<out>/v1`).
    Build {
        /// Ortho4XP tile folders (`zOrtho4XP_+40-074`) or folders containing them.
        #[arg(long, required = true, num_args = 1..)]
        tiles: Vec<PathBuf>,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        version: String,
        /// Mirror base URLs written into the manifest (keeps existing if omitted).
        #[arg(long)]
        mirror: Vec<String>,
        #[arg(long)]
        attribution: Vec<String>,
        /// Ortho4XP config used for the build (hashed into the manifest).
        #[arg(long)]
        build_config: Option<PathBuf>,
        /// Remove Laminar raster layers instead of refusing such DSFs.
        #[arg(long)]
        strip_rasters: bool,
        /// zstd level.
        #[arg(long, default_value_t = 19)]
        level: i32,
    },
    /// Upload a built tree. Objects first, manifest last (atomic release).
    Upload {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, value_enum)]
        target: Target,
        /// Destination folder for `--target dir`.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// S3 endpoint (R2: https://<account>.r2.cloudflarestorage.com).
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long, default_value = "auto")]
        region: String,
        /// Key prefix in the bucket.
        #[arg(long, default_value = "v1")]
        prefix: String,
    },
    /// Fetch and verify a published catalog.
    Verify {
        #[arg(long)]
        origin: String,
        #[arg(long)]
        pubkey: String,
        /// Number of objects to download and hash-check.
        #[arg(long, default_value_t = 3)]
        sample: usize,
    },
}

fn read_key(p: &Path) -> anyhow::Result<SigningKey> {
    let hexs = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    let bytes: [u8; 32] = hex::decode(hexs.trim())?.try_into().map_err(|_| anyhow::anyhow!("key must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Find Ortho4XP tile folders under the given paths.
fn tile_dirs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in inputs {
        if p.join("Earth nav data").is_dir() {
            out.push(p.clone());
        } else if let Ok(rd) = std::fs::read_dir(p) {
            let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|q| q.join("Earth nav data").is_dir()).collect();
            v.sort();
            out.extend(v);
        }
    }
    out
}

fn dsfs_in(tile_dir: &Path) -> Vec<(DsfTile, PathBuf)> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(tile_dir.join("Earth nav data")) {
        for f in rd.flatten() {
            if let Ok(rd2) = std::fs::read_dir(f.path()) {
                for d in rd2.flatten() {
                    let name = d.file_name().to_string_lossy().into_owned();
                    if let Some(t) = name.strip_suffix(".dsf").and_then(DsfTile::parse) {
                        v.push((t, d.path()));
                    }
                }
            }
        }
    }
    v
}

/// Base/airport zoom levels from the terrain file names.
fn zoom_info(tile_dir: &Path) -> ZoomInfo {
    let mut counts: HashMap<u8, usize> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(tile_dir.join("terrain")) {
        for e in rd.flatten() {
            if let Some(t) = TerName::parse(&e.file_name().to_string_lossy()) {
                *counts.entry(t.texture.zl).or_default() += 1;
            }
        }
    }
    let base = counts.iter().max_by_key(|(_, c)| **c).map(|(z, _)| *z).unwrap_or(16);
    let max = counts.keys().copied().max().unwrap_or(base);
    ZoomInfo { base, airports: (max > base).then_some(max) }
}

fn write_object(v1: &Path, rel: &str, data: &[u8]) -> anyhow::Result<()> {
    let p = v1.join(rel);
    if p.exists() {
        return Ok(()); // content-addressed: identical
    }
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, data)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build(tiles: &[PathBuf], out: &Path, key: &Path, version: &str, mirrors: &[String], attribution: &[String], build_config: Option<&Path>, strip: bool, level: i32) -> anyhow::Result<()> {
    let key = read_key(key)?;
    let v1 = out.join("v1");
    std::fs::create_dir_all(&v1)?;
    let mut manifest = std::fs::read(v1.join(catalog::MANIFEST_FILE))
        .ok()
        .and_then(|z| Manifest::from_zst(&z).ok())
        .unwrap_or_else(Manifest::empty);
    let dirs = tile_dirs(tiles);
    anyhow::ensure!(!dirs.is_empty(), "no Ortho4XP tile folders found");
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    for dir in dirs {
        for (tile, path) in dsfs_in(&dir) {
            let raw = std::fs::read(&path)?;
            anyhow::ensure!(dsf::md5_ok(&raw), "{}: bad DSF checksum", path.display());
            let data = if dsf::has_raster_data(&raw)? {
                anyhow::ensure!(strip, "{}: contains raster data (Laminar layers); rebuild without Global Scenery or pass --strip-rasters", path.display());
                dsf::strip_rasters(&raw)?
            } else {
                raw
            };
            let z = zstd::encode_all(&data[..], level)?;
            let (obj, hash) = catalog::object_name(&format!("tiles/{}", tile.folder()), &tile.name(), "dsf.zst", &z);
            write_object(&v1, &obj, &z)?;
            let mut masks = Vec::new();
            if let Ok(rd) = std::fs::read_dir(dir.join("textures")) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if MaskName::parse(&name).is_none() {
                        continue;
                    }
                    let png = std::fs::read(e.path())?;
                    let (mobj, mhash) = catalog::object_name("masks", name.trim_end_matches(".png"), "png", &png);
                    write_object(&v1, &mobj, &png)?;
                    masks.push(MaskEntry { name, object: Object { object: mobj, size: png.len() as u64, blake3: mhash } });
                }
            }
            masks.sort_by(|a, b| a.name.cmp(&b.name));
            total_in += data.len() as u64;
            total_out += z.len() as u64;
            println!("{}  {:>6.1} MB -> {:>6.1} MB  ({} masks)", tile, data.len() as f64 / 1e6, z.len() as f64 / 1e6, masks.len());
            manifest.tiles.insert(
                tile.name(),
                TileEntry {
                    dsf: DsfEntry { object: Object { object: obj, size: z.len() as u64, blake3: hash }, stripped_size: data.len() as u64 },
                    masks,
                    zl: zoom_info(&dir),
                },
            );
        }
    }
    manifest.catalog_version = version.into();
    manifest.generated_at = humantime_now();
    if !mirrors.is_empty() {
        manifest.mirrors = mirrors.to_vec();
    }
    if !attribution.is_empty() {
        manifest.attribution = attribution.to_vec();
    } else if manifest.attribution.is_empty() {
        manifest.attribution = vec!["Mesh data © OpenStreetMap contributors (ODbL)".into(), "Elevation: USGS 3DEP / Copernicus DEM".into()];
    }
    if let Some(cfg) = build_config {
        manifest.build_config = format!("blake3:{}", blake3::hash(&std::fs::read(cfg)?).to_hex());
    }
    let zst = manifest.to_zst()?;
    std::fs::write(v1.join(catalog::MANIFEST_FILE), &zst)?;
    std::fs::write(v1.join(catalog::SIGNATURE_FILE), catalog::sign(&key, &zst))?;
    println!(
        "catalog {}: {} tiles, this run {:.1} MB -> {:.1} MB; signed with {}",
        version,
        manifest.tiles.len(),
        total_in as f64 / 1e6,
        total_out as f64 / 1e6,
        hex::encode(key.verifying_key().to_bytes())
    );
    Ok(())
}

fn humantime_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // Civil-from-days (Howard Hinnant), UTC.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Files under `v1`, objects first and the manifest + signature last.
fn upload_order(v1: &Path) -> anyhow::Result<Vec<String>> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                walk(base, &e.path(), out)?;
            } else {
                out.push(e.path().strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(v1, v1, &mut files)?;
    files.retain(|f| f != catalog::MANIFEST_FILE && f != catalog::SIGNATURE_FILE);
    files.sort();
    files.push(catalog::SIGNATURE_FILE.into());
    files.push(catalog::MANIFEST_FILE.into());
    Ok(files)
}

async fn upload_s3(v1: &Path, endpoint: &str, bucket: &str, region: &str, prefix: &str) -> anyhow::Result<()> {
    use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
    let key = std::env::var("AWS_ACCESS_KEY_ID").context("AWS_ACCESS_KEY_ID not set")?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").context("AWS_SECRET_ACCESS_KEY not set")?;
    let creds = Credentials::new(key, secret);
    let bucket = Bucket::new(endpoint.parse()?, UrlStyle::Path, bucket.to_string(), region.to_string())?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(600)).build()?;
    let ttl = Duration::from_secs(3600);
    let (mut sent, mut skipped) = (0, 0);
    for rel in upload_order(v1)? {
        let obj_key = format!("{}/{rel}", prefix.trim_matches('/'));
        let immutable = rel != catalog::MANIFEST_FILE && rel != catalog::SIGNATURE_FILE;
        if immutable {
            let head = bucket.head_object(Some(&creds), &obj_key).sign(ttl);
            if http.head(head).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
                skipped += 1;
                continue;
            }
        }
        let data = std::fs::read(v1.join(&rel))?;
        let mut action = bucket.put_object(Some(&creds), &obj_key);
        let cache = if immutable { "public, max-age=31536000, immutable" } else { "public, max-age=300" };
        action.headers_mut().insert("cache-control", cache);
        let url = action.sign(ttl);
        http.put(url).header("cache-control", cache).body(data).send().await?.error_for_status().with_context(|| format!("PUT {obj_key}"))?;
        sent += 1;
        println!("uploaded {obj_key}");
    }
    println!("done: {sent} uploaded, {skipped} already present");
    Ok(())
}

fn upload_dir(v1: &Path, dest: &Path) -> anyhow::Result<()> {
    let (mut sent, mut skipped) = (0, 0);
    for rel in upload_order(v1)? {
        let to = dest.join(&rel);
        let immutable = rel != catalog::MANIFEST_FILE && rel != catalog::SIGNATURE_FILE;
        if immutable && to.exists() {
            skipped += 1;
            continue;
        }
        std::fs::create_dir_all(to.parent().unwrap())?;
        let tmp = to.with_extension("partial");
        std::fs::copy(v1.join(&rel), &tmp)?;
        std::fs::rename(tmp, &to)?;
        sent += 1;
    }
    println!("done: {sent} copied, {skipped} already present → {}", dest.display());
    Ok(())
}

async fn verify(origin: &str, pubkey: &str, sample: usize) -> anyhow::Result<()> {
    let key = catalog::parse_public_key(pubkey)?;
    let http = reqwest::Client::new();
    let zst = http.get(catalog::join_url(origin, catalog::MANIFEST_FILE)).send().await?.error_for_status()?.bytes().await?;
    let sig = http.get(catalog::join_url(origin, catalog::SIGNATURE_FILE)).send().await?.error_for_status()?.text().await?;
    catalog::verify(&[key], &zst, &sig)?;
    let m = Manifest::from_zst(&zst)?;
    println!("signature OK — catalog {} ({} tiles, generated {})", m.catalog_version, m.tiles.len(), m.generated_at);
    let objs: BTreeMap<&String, &TileEntry> = m.tiles.iter().take(sample).collect();
    for (name, e) in objs {
        let d = http.get(catalog::join_url(origin, &e.dsf.object.object)).send().await?.error_for_status()?.bytes().await?;
        anyhow::ensure!(blake3::hash(&d).to_hex().as_str() == e.dsf.object.blake3, "{name}: hash mismatch");
        let raw = zstd::decode_all(&d[..])?;
        anyhow::ensure!(raw.len() as u64 == e.dsf.stripped_size && !dsf::has_raster_data(&raw)?, "{name}: bad content");
        println!("  {name}: OK ({:.1} MB)", d.len() as f64 / 1e6);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Keygen { out } => {
            anyhow::ensure!(!out.exists(), "{} already exists", out.display());
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            std::fs::write(&out, hex::encode(key.to_bytes()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("public key: {}", hex::encode(key.verifying_key().to_bytes()));
            println!("keep {} secret; add the public key to [mesh].public_keys", out.display());
        }
        Cmd::Pubkey { key } => println!("{}", hex::encode(read_key(&key)?.verifying_key().to_bytes())),
        Cmd::Build { tiles, out, key, version, mirror, attribution, build_config, strip_rasters, level } => {
            build(&tiles, &out, &key, &version, &mirror, &attribution, build_config.as_deref(), strip_rasters, level)?
        }
        Cmd::Upload { dir, target, dest, endpoint, bucket, region, prefix } => {
            let v1 = dir.join("v1");
            anyhow::ensure!(v1.join(catalog::MANIFEST_FILE).exists(), "{} has no built catalog", dir.display());
            match target {
                Target::Dir => upload_dir(&v1, &dest.context("--dest required")?)?,
                Target::S3 => upload_s3(&v1, &endpoint.context("--endpoint required")?, &bucket.context("--bucket required")?, &region, &prefix).await?,
            }
        }
        Cmd::Verify { origin, pubkey, sample } => verify(&origin, &pubkey, sample).await?,
    }
    Ok(())
}
