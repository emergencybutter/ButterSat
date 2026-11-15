//! Daemon configuration (TOML). Every field has a working default: the
//! daemon must run with no config file at all (design §11).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use xof_mesh::Coverage;

/// Default mesh origins. The manifest's own `mirrors` list is used after the
/// first successful fetch.
pub const DEFAULT_MESH_MIRRORS: &[&str] = &["https://mesh.xporthofs.org/v1"];

/// Trusted manifest signing keys (ed25519, hex). Release builds embed the
/// project key here; additional keys can be added in config.
pub const DEFAULT_MESH_KEYS: &[&str] = &[];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DaemonConfig {
    pub xplane: XPlaneConfig,
    pub mount: MountConfig,
    pub imagery: ImageryConfig,
    pub mesh: MeshSection,
    pub prefetch: PrefetchConfig,
    pub packs: PacksConfig,
    pub simbrief: SimbriefConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            xplane: XPlaneConfig::default(),
            mount: MountConfig::default(),
            imagery: ImageryConfig::default(),
            mesh: MeshSection::default(),
            prefetch: PrefetchConfig::default(),
            packs: PacksConfig::default(),
            simbrief: SimbriefConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct XPlaneConfig {
    /// X-Plane 12 folder; auto-detected when unset.
    pub path: Option<PathBuf>,
    /// Extra executable names treated as X-Plane by the process filter.
    pub process_names: Vec<String>,
    /// Web API port.
    pub web_api_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MountConfig {
    /// Mount point; default `<X-Plane>/Custom Scenery/z_xporthofs`.
    pub path: Option<PathBuf>,
    /// Mount automatically when the daemon starts.
    pub auto_mount: bool,
    /// Add/maintain our entry in `scenery_packs.ini`.
    pub manage_scenery_packs: bool,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self { path: None, auto_mount: true, manage_scenery_packs: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ImageryConfig {
    /// `esri` or `bing`.
    pub provider: String,
    /// The user acknowledged the provider's terms notice.
    pub provider_notice_accepted: bool,
    pub deadline_ms: u64,
    pub cache_size_gb: u64,
    pub segment_ram_mb: usize,
    pub decoded_ram_mb: usize,
}

impl Default for ImageryConfig {
    fn default() -> Self {
        Self {
            provider: "esri".into(),
            provider_notice_accepted: false,
            deadline_ms: 1200,
            cache_size_gb: 100,
            segment_ram_mb: 1024,
            decoded_ram_mb: 256,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MeshSection {
    pub enabled: bool,
    pub mirrors: Vec<String>,
    /// Extra trusted manifest keys (hex).
    pub public_keys: Vec<String>,
    pub coverage: Coverage,
    pub cache_size_gb: u64,
    /// DSF tiles never evicted (`+40-074`).
    pub pinned: Vec<String>,
}

impl Default for MeshSection {
    fn default() -> Self {
        Self {
            enabled: true,
            mirrors: DEFAULT_MESH_MIRRORS.iter().map(|s| s.to_string()).collect(),
            public_keys: vec![],
            coverage: Coverage::World,
            cache_size_gb: 50,
            pinned: vec![],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PrefetchConfig {
    pub enabled: bool,
    /// DSF tiles around the aircraft kept ready (radius).
    pub mesh_radius: i32,
    /// Minutes of flight ahead covered by imagery prefetch.
    pub lookahead_minutes: f64,
    /// Dynamic zoom (design §5.5).
    pub dynamic_zoom: bool,
    /// Above this height (feet AGL) mip 0 is built from ZL-1.
    pub dynamic_zoom_agl_ft: f64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mesh_radius: 1,
            lookahead_minutes: 8.0,
            dynamic_zoom: true,
            dynamic_zoom_agl_ft: 12_000.0,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PacksConfig {
    /// Additional imported pack folders (the packs dir is scanned too).
    pub extra: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SimbriefConfig {
    /// SimBrief username or pilot ID for route prefetch.
    pub user: Option<String>,
}

/// Standard per-user locations.
pub struct Dirs {
    pub config_file: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl Dirs {
    pub fn standard() -> Self {
        // XOF_HOME relocates everything (tests, portable installs).
        if let Some(home) = std::env::var_os("XOF_HOME") {
            let h = PathBuf::from(home);
            return Self { config_file: h.join("config.toml"), data: h.join("data"), cache: h.join("cache") };
        }
        let base_cfg = dirs::config_dir().unwrap_or_else(std::env::temp_dir).join("xporthofs");
        let data = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("xporthofs");
        let cache = dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("xporthofs");
        Self { config_file: base_cfg.join("config.toml"), data, cache }
    }

    pub fn packs(&self) -> PathBuf {
        self.data.join("packs")
    }
}

impl DaemonConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_default_and_roundtrips() {
        let c: DaemonConfig = toml::from_str("").unwrap();
        assert_eq!(c, DaemonConfig::default());
        let s = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<DaemonConfig>(&s).unwrap(), c);
        let partial: DaemonConfig = toml::from_str("[imagery]\nprovider = \"bing\"\n").unwrap();
        assert_eq!(partial.imagery.provider, "bing");
        assert_eq!(partial.imagery.deadline_ms, 1200);
    }
}
