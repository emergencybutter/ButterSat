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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub catalog_version: String,
    pub generated_at: String,
    #[serde(default)]
    pub build_config: String,
    #[serde(default)]
    pub mirrors: Vec<String>,
    #[serde(default)]
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
            mirrors: vec![],
            attribution: vec![],
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
}

/// Outcome of a refresh.
pub enum Refresh {
    /// A newly verified manifest.
    Updated(Manifest),
    /// The origin says nothing changed.
    Unchanged,
}

impl CatalogClient {
    pub fn new(keys: Vec<VerifyingKey>, cache_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cache_dir)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("xporthofs/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { http, keys, cache_dir: cache_dir.to_path_buf() })
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
        for base in mirrors {
            match self.fetch_from(base, etag.as_deref()).await {
                Ok(None) => return Ok(Refresh::Unchanged),
                Ok(Some((zst, sig, new_etag))) => {
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
    pub async fn fetch_object(&self, mirrors: &[String], obj: &Object) -> anyhow::Result<Vec<u8>> {
        let mut last_err = anyhow::anyhow!("no mirrors configured");
        for base in mirrors {
            let res: anyhow::Result<Vec<u8>> = async {
                let data = self.http.get(join_url(base, &obj.object)).send().await?.error_for_status()?.bytes().await?;
                anyhow::ensure!(data.len() as u64 == obj.size, "size mismatch for {}", obj.object);
                let h = blake3::hash(&data).to_hex();
                anyhow::ensure!(h.as_str() == obj.blake3, "hash mismatch for {}", obj.object);
                Ok(data.to_vec())
            }
            .await;
            match res {
                Ok(d) => return Ok(d),
                Err(e) => {
                    tracing::warn!("object {} from {base}: {e}", obj.object);
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
