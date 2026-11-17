//! The signed mesh catalog (design §6.1, §10.2).
//!
//! Origin layout:
//! ```text
//! /v1/manifest.json.zst
//! /v1/manifest.json.zst.sig        (base64 ed25519 signature of the .zst bytes)
//! /v1/tiles/+40-080/+40-074.<blake3-prefix>.dsf.zst
//! /v1/masks/<name>.<blake3-prefix>.png
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use xof_core::DsfTile;

pub const FORMAT: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json.zst";
pub const SIGNATURE_FILE: &str = "manifest.json.zst.sig";
pub const LICENSE_FILE: &str = "LICENSE";
pub const ATTRIBUTION_FILE: &str = "ATTRIBUTION";

/// One hosted object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Object {
    /// Path relative to the origin base URL.
    pub object: String,
    /// Size of the hosted (compressed) object.
    pub size: u64,
    /// BLAKE3 of the hosted object, hex.
    pub blake3: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsfEntry {
    #[serde(flatten)]
    pub object: Object,
    /// Size of the decompressed DSF (without Laminar rasters).
    pub stripped_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaskEntry {
    pub name: String,
    #[serde(flatten)]
    pub object: Object,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoomInfo {
    pub base: u8,
    #[serde(default)]
    pub airports: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileEntry {
    pub dsf: DsfEntry,
    #[serde(default)]
    pub masks: Vec<MaskEntry>,
    #[serde(default)]
    pub zl: ZoomInfo,
}

fn default_license() -> String {
    "ODbL-1.0".into()
}

fn default_attribution() -> Vec<String> {
    vec![
        "Mesh data © OpenStreetMap contributors (ODbL 1.0)".into(),
        "Elevation data: USGS 3DEP (public domain) / Copernicus DEM".into(),
    ]
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub catalog_version: String,
    pub generated_at: String,
    #[serde(default)]
    pub build_config: String,
    #[serde(default = "default_license")]
    pub license: String,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default = "default_attribution")]
    pub attribution: Vec<String>,
    /// Keyed by DSF tile name (`+40-074`).
    pub tiles: BTreeMap<String, TileEntry>,
}

impl Manifest {
    pub fn empty() -> Self {
        Self {
            format: FORMAT,
            catalog_version: "empty".into(),
            generated_at: String::new(),
            build_config: String::new(),
            license: default_license(),
            mirrors: vec![],
            attribution: default_attribution(),
            tiles: BTreeMap::new(),
        }
    }

    pub fn tile(&self, t: DsfTile) -> Option<&TileEntry> {
        self.tiles.get(&t.name())
    }

    pub fn dsf_tiles(&self) -> impl Iterator<Item = DsfTile> + '_ {
        self.tiles.keys().filter_map(|k| DsfTile::parse(k))
    }

    /// Serialize + compress (what gets signed and hosted).
    pub fn to_zst(&self) -> anyhow::Result<Vec<u8>> {
        let json = serde_json::to_vec(self)?;
        Ok(zstd::encode_all(&json[..], 19)?)
    }

    pub fn from_zst(data: &[u8]) -> anyhow::Result<Self> {
        let json = zstd::decode_all(data)?;
        let m: Manifest = serde_json::from_slice(&json)?;
        anyhow::ensure!(m.format == FORMAT, "unsupported manifest format {}", m.format);
        Ok(m)
    }
}

/// Signature over the compressed manifest bytes, base64.
pub fn sign(key: &SigningKey, manifest_zst: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(key.sign(manifest_zst).to_bytes())
}

/// Verify against any of the trusted keys.
pub fn verify(keys: &[VerifyingKey], manifest_zst: &[u8], sig_b64: &str) -> anyhow::Result<()> {
    let raw = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim())?;
    let sig = Signature::from_slice(&raw)?;
    for k in keys {
        if k.verify(manifest_zst, &sig).is_ok() {
            return Ok(());
        }
    }
    anyhow::bail!("manifest signature does not match any trusted key")
}

pub fn parse_public_key(hex_str: &str) -> anyhow::Result<VerifyingKey> {
    let bytes: [u8; 32] =
        hex::decode(hex_str.trim())?.try_into().map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

/// Content-addressed object name: `<stem>.<first 16 hex of blake3>.<ext>`.
pub fn object_name(dir: &str, stem: &str, ext: &str, data: &[u8]) -> (String, String) {
    let hash = blake3::hash(data).to_hex().to_string();
    (format!("{dir}/{stem}.{}.{ext}", &hash[..16]), hash)
}

/// Joins a mirror base URL and a relative object path.
pub fn join_url(base: &str, rel: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rel.trim_start_matches('/'))
}

/// Fetches and verifies the manifest from a list of mirrors, keeping the last
/// good copy on disk for offline starts.
pub struct CatalogClient {
    http: reqwest::Client,
    keys: Vec<VerifyingKey>,
    cache_dir: PathBuf,
    /// The mirror that last served us: tried first, so a dead mirror early
    /// in the list costs one failure, not one per object.
    preferred: std::sync::Mutex<Option<String>>,
    /// Object bytes received so far, counted as they stream in (so the
    /// activity chart shows a slow 60 s download as 60 s of traffic, not one
    /// spike at the end).
    received: std::sync::atomic::AtomicU64,
}

/// Outcome of a refresh.
pub enum Refresh {
    /// A newly verified manifest.
    Updated(Manifest),
    /// The origin says nothing changed.
    Unchanged,
}

/// How long a mesh object fetch keeps retrying transient failures before the
/// read fails (X-Plane waits meanwhile; a read error crashes it). Stays well
/// under the VFS request timeout (WinFsp IrpTimeout, set to 10 min).
pub const OBJECT_RETRY_WINDOW: Duration = Duration::from_secs(240);

impl CatalogClient {
    pub fn new(keys: Vec<VerifyingKey>, cache_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("ButterSat/", env!("CARGO_PKG_VERSION")))
            // Idle timeout, not a total one: a 60 s cap cut off big DSFs on
            // a slow link while data was still flowing, X-Plane got a read
            // error and crashed (2026-11-17, KTVC).
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            keys,
            cache_dir: cache_dir.to_path_buf(),
            preferred: Default::default(),
            received: Default::default(),
        })
    }

    /// Object bytes received since start (streamed, including retries).
    pub fn bytes_received(&self) -> u64 {
        self.received.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `mirrors` with the last good one first.
    fn ordered(&self, mirrors: &[String]) -> Vec<String> {
        let mut v = mirrors.to_vec();
        if let Some(p) = self.preferred.lock().unwrap().as_ref() {
            if let Some(i) = v.iter().position(|m| m == p) {
                let m = v.remove(i);
                v.insert(0, m);
            }
        }
        v
    }

    fn mark(&self, base: &str, ok: bool) {
        let mut p = self.preferred.lock().unwrap();
        if ok {
            *p = Some(base.to_string());
        } else if p.as_deref() == Some(base) {
            *p = None;
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The last verified manifest, if any.
    pub fn load_cached(&self) -> Option<Manifest> {
        let zst = std::fs::read(self.cache_dir.join(MANIFEST_FILE)).ok()?;
        let sig = std::fs::read_to_string(self.cache_dir.join(SIGNATURE_FILE)).ok()?;
        verify(&self.keys, &zst, &sig).ok()?;
        Manifest::from_zst(&zst).ok()
    }

    /// Try each mirror in order. Uses `If-None-Match` with the cached ETag.
    pub async fn refresh(&self, mirrors: &[String]) -> anyhow::Result<Refresh> {
        let etag_path = self.cache_dir.join("manifest.etag");
        let etag = std::fs::read_to_string(&etag_path).ok();
        let mut last_err = anyhow::anyhow!("no mirrors configured");
        for base in &self.ordered(mirrors) {
            match self.fetch_from(base, etag.as_deref()).await {
                Ok(None) => {
                    self.mark(base, true);
                    return Ok(Refresh::Unchanged);
                }
                Ok(Some((zst, sig, new_etag))) => {
                    self.mark(base, true);
                    verify(&self.keys, &zst, &sig)?;
                    let m = Manifest::from_zst(&zst)?;
                    std::fs::write(self.cache_dir.join(MANIFEST_FILE), &zst)?;
                    std::fs::write(self.cache_dir.join(SIGNATURE_FILE), &sig)?;
                    match new_etag {
                        Some(e) => std::fs::write(&etag_path, e)?,
                        None => {
                            let _ = std::fs::remove_file(&etag_path);
                        }
                    }
                    return Ok(Refresh::Updated(m));
                }
                Err(e) => {
                    tracing::warn!("catalog mirror {base} failed: {e}");
                    self.mark(base, false);
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn fetch_from(
        &self,
        base: &str,
        etag: Option<&str>,
    ) -> anyhow::Result<Option<(Vec<u8>, String, Option<String>)>> {
        let mut req = self.http.get(join_url(base, MANIFEST_FILE));
        if let Some(e) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, e);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        let new_etag = resp.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let zst = resp.bytes().await?.to_vec();
        let sig = self.http.get(join_url(base, SIGNATURE_FILE)).send().await?.error_for_status()?.text().await?;
        Ok(Some((zst, sig, new_etag)))
    }

    /// Download an object from the first mirror that has it, verifying BLAKE3.
    ///
    /// Mirrors may be partial (e.g. the US on one host, the rest of the world
    /// on another, all sharing one signed manifest): a 404 means "not here",
    /// so it moves on quietly without demoting the mirror. Other errors
    /// (unreachable, 5xx, bad hash) count as mirror failures.
    pub async fn fetch_object(&self, mirrors: &[String], obj: &Object) -> anyhow::Result<Vec<u8>> {
        // A failed read makes X-Plane crash, so transient failures (network,
        // timeouts, cut transfers, 5xx/429) are retried across all mirrors
        // with backoff for up to OBJECT_RETRY_WINDOW; X-Plane waits instead.
        // A 404 everywhere or a corrupt copy is final.
        let started = std::time::Instant::now();
        let mut delay = Duration::from_secs(2);
        loop {
            match self.fetch_object_once(mirrors, obj).await {
                Ok(d) => return Ok(d),
                Err((e, transient)) => {
                    if !transient || started.elapsed() + delay > OBJECT_RETRY_WINDOW {
                        return Err(e);
                    }
                    tracing::warn!("{}: {e:#}; retrying in {}s", obj.object, delay.as_secs());
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    /// One pass over the mirrors. Err carries whether a retry could help
    /// (some mirror failed transiently).
    async fn fetch_object_once(&self, mirrors: &[String], obj: &Object) -> Result<Vec<u8>, (anyhow::Error, bool)> {
        let mut last_err = anyhow::anyhow!("no mirrors configured");
        let mut transient = false;
        for base in &self.ordered(mirrors) {
            let t0 = std::time::Instant::now();
            let res: anyhow::Result<Option<Vec<u8>>> = async {
                let resp = self.http.get(join_url(base, &obj.object)).send().await?;
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                let resp = resp.error_for_status()?;
                let cache =
                    resp.headers().get("cf-cache-status").and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
                let mut resp = resp;
                let mut data = Vec::with_capacity(obj.size.min(256 << 20) as usize);
                while let Some(chunk) = resp.chunk().await? {
                    self.received.fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
                    data.extend_from_slice(&chunk);
                }
                anyhow::ensure!(data.len() as u64 == obj.size, "size mismatch for {}", obj.object);
                let h = blake3::hash(&data).to_hex();
                anyhow::ensure!(h.as_str() == obj.blake3, "hash mismatch for {}", obj.object);
                log_download(&obj.object, base, data.len(), t0.elapsed(), &cache);
                Ok(Some(data))
            }
            .await;
            match res {
                Ok(Some(d)) => {
                    return Ok(d);
                }
                Ok(None) => {
                    tracing::debug!("object {} not on {base}", obj.object);
                    if !transient {
                        last_err = anyhow::anyhow!("{} not found on any mirror", obj.object);
                    }
                }
                Err(e) => {
                    transient |= is_transient(&e);
                    tracing::warn!("object {} from {base}: {e}", obj.object);
                    self.mark(base, false);
                    last_err = e;
                }
            }
        }
        Err((last_err, transient))
    }
}

/// One line per mesh download (DSFs at info, the many small masks at debug);
/// a warning when a sizeable object comes in below 1 MB/s.
fn log_download(object: &str, base: &str, bytes: usize, took: Duration, cache: &str) {
    let secs = took.as_secs_f64().max(0.001);
    let mbps = bytes as f64 / 1e6 / secs;
    if bytes >= 1_000_000 && mbps < 1.0 {
        tracing::warn!(
            "slow download: {object} {:.1} MB in {secs:.1} s ({mbps:.2} MB/s) from {base} (cdn {cache})",
            bytes as f64 / 1e6
        );
    } else if object.starts_with("tiles/") {
        tracing::info!(
            "downloaded {object} {:.1} MB in {secs:.1} s ({mbps:.1} MB/s) from {base} (cdn {cache})",
            bytes as f64 / 1e6
        );
    } else {
        tracing::debug!("downloaded {object} {bytes} B in {secs:.2} s from {base} (cdn {cache})");
    }
}

/// Network trouble, timeouts, cut transfers, overload: worth retrying.
fn is_transient(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<reqwest::Error>() {
        Some(r) => match r.status() {
            Some(s) => s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS,
            None => true,
        },
        None => false, // size/hash mismatch: a corrupt copy stays corrupt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_good_mirror_goes_first() {
        let dir = std::env::temp_dir().join(format!("xof-catalog-order-{}", std::process::id()));
        let c = CatalogClient::new(vec![], &dir).unwrap();
        let m: Vec<String> = ["http://dead/v1", "http://live/v1", "http://other/v1"].map(String::from).to_vec();
        assert_eq!(c.ordered(&m), m, "configured order until something works");
        c.mark("http://dead/v1", false);
        c.mark("http://live/v1", true);
        assert_eq!(c.ordered(&m)[0], "http://live/v1");
        assert_eq!(c.ordered(&m)[1..], ["http://dead/v1".to_string(), "http://other/v1".to_string()]);
        // A failure of the preferred mirror resets to the configured order.
        c.mark("http://live/v1", false);
        assert_eq!(c.ordered(&m), m);
        // A preferred mirror absent from the list is ignored.
        c.mark("http://gone/v1", true);
        assert_eq!(c.ordered(&m), m);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn sample() -> Manifest {
        let mut m = Manifest::empty();
        m.catalog_version = "2026.11.1".into();
        m.tiles.insert(
            "+40-074".into(),
            TileEntry {
                dsf: DsfEntry {
                    object: Object {
                        object: "tiles/+40-080/+40-074.abc.dsf.zst".into(),
                        size: 10,
                        blake3: "00".into(),
                    },
                    stripped_size: 42_987_139,
                },
                masks: vec![],
                zl: ZoomInfo { base: 16, airports: Some(17) },
            },
        );
        m
    }

    #[test]
    fn sign_and_verify() {
        let key = SigningKey::generate(&mut rand::rngs::OsRng);
        let zst = sample().to_zst().unwrap();
        let sig = sign(&key, &zst);
        verify(&[key.verifying_key()], &zst, &sig).unwrap();
        let other = SigningKey::generate(&mut rand::rngs::OsRng);
        assert!(verify(&[other.verifying_key()], &zst, &sig).is_err());
        let mut tampered = zst.clone();
        tampered[5] ^= 1;
        assert!(verify(&[key.verifying_key()], &tampered, &sig).is_err());
        let back = Manifest::from_zst(&zst).unwrap();
        assert_eq!(back, sample());
        assert_eq!(back.dsf_tiles().collect::<Vec<_>>(), vec![DsfTile::new(40, -74)]);
    }

    #[test]
    fn content_addressed_names() {
        let (name, hash) = object_name("tiles/+40-080", "+40-074", "dsf.zst", b"abc");
        assert!(name.starts_with("tiles/+40-080/+40-074."));
        assert!(name.ends_with(".dsf.zst"));
        assert_eq!(hash.len(), 64);
        assert_eq!(join_url("https://x/v1/", "/tiles/a"), "https://x/v1/tiles/a");
    }
}
