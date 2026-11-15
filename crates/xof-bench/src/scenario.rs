//! Scenario files (TOML): where to look, how to get there, and which
//! scenery variants to compare.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default = "default_aircraft")]
    pub aircraft: String,
    /// Fixed sun: day of year (0 = 1 Jan) and local time, reset at every spot.
    #[serde(default = "default_day")]
    pub day_of_year: u32,
    #[serde(default = "default_time")]
    pub local_time_h: f64,
    /// Slew speed between spots.
    #[serde(default = "default_speed")]
    pub slew_speed_kt: f64,
    /// Camera field of view (degrees).
    #[serde(default = "default_fov")]
    pub fov_deg: f64,
    /// Screenshot times after arriving at a spot (seconds).
    #[serde(default = "default_shots")]
    pub shots_s: Vec<f64>,
    /// Local mesh catalog folder served over HTTP for `xof` variants (the
    /// folder that contains `v1/`). Relative to the scenario file.
    #[serde(default)]
    pub origin: Option<PathBuf>,
    /// Extra trusted catalog keys (hex).
    #[serde(default)]
    pub public_keys: Vec<String>,
    /// Folder names in Custom Scenery to disable during the run (e.g. other
    /// ortho packs covering the same area).
    #[serde(default)]
    pub disable_packs: Vec<String>,
    /// Variant whose final screenshots are the similarity reference.
    #[serde(default)]
    pub compare_to: Option<String>,
    #[serde(rename = "spot")]
    pub spots: Vec<Spot>,
    #[serde(rename = "variant")]
    pub variants: Vec<Variant>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spot {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Camera altitude, feet MSL.
    pub alt_ft: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Fresh, empty daemon caches for every run.
    #[default]
    Cold,
    /// Caches kept between runs (`<out>/homes/<variant>`): run twice for warm.
    Persistent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Mount xporthofs for this variant.
    #[serde(default)]
    pub xof: bool,
    /// Static scenery packs (folders) linked into Custom Scenery.
    #[serde(default)]
    pub packs: Vec<PathBuf>,
    #[serde(default)]
    pub cache: CacheMode,
    /// Daemon config overrides, merged into the generated config.toml.
    #[serde(default)]
    pub config: toml::Table,
}

fn default_aircraft() -> String {
    "Aircraft/Laminar Research/Cessna 172 SP/Cessna_172SP.acf".into()
}
fn default_day() -> u32 {
    172
}
fn default_time() -> f64 {
    12.5
}
fn default_speed() -> f64 {
    250.0
}
fn default_fov() -> f64 {
    60.0
}
fn default_shots() -> Vec<f64> {
    vec![0.0, 2.0, 5.0, 10.0, 20.0, 40.0]
}

impl Scenario {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut s: Scenario = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        let abs = |p: &PathBuf| if p.is_absolute() { p.clone() } else { base.join(p) };
        s.origin = s.origin.as_ref().map(abs);
        for v in &mut s.variants {
            v.packs = v.packs.iter().map(abs).collect();
        }
        s.validate()?;
        Ok(s)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.spots.is_empty(), "scenario has no [[spot]]");
        anyhow::ensure!(!self.variants.is_empty(), "scenario has no [[variant]]");
        anyhow::ensure!(!self.shots_s.is_empty(), "shots_s is empty");
        anyhow::ensure!(self.slew_speed_kt > 0.0, "slew_speed_kt must be positive");
        for (i, v) in self.variants.iter().enumerate() {
            anyhow::ensure!(
                !v.name.is_empty() && v.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "variant name '{}' must be [A-Za-z0-9_-]",
                v.name
            );
            anyhow::ensure!(!self.variants[..i].iter().any(|o| o.name == v.name), "duplicate variant '{}'", v.name);
            for p in &v.packs {
                anyhow::ensure!(p.is_dir(), "variant {}: pack {} not found", v.name, p.display());
            }
        }
        if let Some(c) = &self.compare_to {
            anyhow::ensure!(self.variants.iter().any(|v| &v.name == c), "compare_to '{c}' is not a variant");
        }
        if self.variants.iter().any(|v| v.xof) {
            if let Some(o) = &self.origin {
                anyhow::ensure!(o.join("v1").is_dir(), "origin {} has no v1/ folder", o.display());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let s: Scenario = toml::from_str(
            r#"
name = "t"
[[spot]]
name = "JFK"
lat = 40.64
lon = -73.78
alt_ft = 3000
[[variant]]
name = "default"
[[variant]]
name = "xof"
xof = true
cache = "persistent"
[variant.config.imagery]
deadline_ms = 800
"#,
        )
        .unwrap();
        s.validate().unwrap();
        assert_eq!(s.variants[1].cache, CacheMode::Persistent);
        assert_eq!(s.variants[1].config["imagery"]["deadline_ms"].as_integer(), Some(800));
    }

    #[test]
    fn rejects_bad_names() {
        let mut s: Scenario =
            toml::from_str("name='t'\n[[spot]]\nname='a'\nlat=0\nlon=0\nalt_ft=1\n[[variant]]\nname='a b'\n").unwrap();
        assert!(s.validate().is_err());
        s.variants[0].name = "ok".into();
        s.compare_to = Some("nope".into());
        assert!(s.validate().is_err());
    }
}
