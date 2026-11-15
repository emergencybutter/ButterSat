//! Web-Mercator ("slippy map") tile math.

use std::f64::consts::PI;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Edge length of an imagery tile in pixels.
pub const TILE_PX: u32 = 256;

/// A slippy-map tile at zoom `z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileCoord {
    pub const fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// The tile `levels` zoom levels up that contains this one.
    pub fn ancestor(self, levels: u8) -> Option<TileCoord> {
        if levels > self.z {
            return None;
        }
        Some(TileCoord::new(self.z - levels, self.x >> levels, self.y >> levels))
    }

    /// Bing-style quadkey.
    pub fn quadkey(self) -> String {
        let mut key = String::with_capacity(self.z as usize);
        for i in (1..=self.z).rev() {
            let mask = 1u32 << (i - 1);
            let mut digit = b'0';
            if self.x & mask != 0 {
                digit += 1;
            }
            if self.y & mask != 0 {
                digit += 2;
            }
            key.push(digit as char);
        }
        key
    }

    /// Latitude/longitude of the tile's north-west corner.
    pub fn nw_latlon(self) -> (f64, f64) {
        tile_to_latlon(self.x as f64, self.y as f64, self.z)
    }

    /// Latitude/longitude of the tile's centre.
    pub fn center_latlon(self) -> (f64, f64) {
        tile_to_latlon(self.x as f64 + 0.5, self.y as f64 + 0.5, self.z)
    }

    pub fn is_valid(self) -> bool {
        let n = 1u64 << self.z;
        (self.x as u64) < n && (self.y as u64) < n
    }
}

impl fmt::Display for TileCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.z, self.x, self.y)
    }
}

/// Fractional tile coordinates of a point at zoom `z`.
pub fn latlon_to_tile(lat: f64, lon: f64, z: u8) -> (f64, f64) {
    let n = (1u64 << z) as f64;
    let lat = lat.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    let x = (lon + 180.0) / 360.0 * n;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / PI) / 2.0 * n;
    (x, y)
}

/// Latitude/longitude of fractional tile coordinates at zoom `z`.
pub fn tile_to_latlon(x: f64, y: f64, z: u8) -> (f64, f64) {
    let n = (1u64 << z) as f64;
    let lon = x / n * 360.0 - 180.0;
    let lat = (PI * (1.0 - 2.0 * y / n)).sinh().atan().to_degrees();
    (lat, lon)
}

/// Ground resolution (meters per pixel) at latitude `lat` and zoom `z`.
pub fn meters_per_pixel(lat: f64, z: u8) -> f64 {
    const EARTH_CIRCUMFERENCE: f64 = 40_075_016.686;
    EARTH_CIRCUMFERENCE * lat.to_radians().cos() / ((TILE_PX as f64) * (1u64 << z) as f64)
}

/// A 1°×1° X-Plane DSF tile, identified by its south-west corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DsfTile {
    pub lat: i32,
    pub lon: i32,
}

impl DsfTile {
    pub fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }

    pub fn containing(lat: f64, lon: f64) -> Self {
        Self::new(lat.floor() as i32, lon.floor() as i32)
    }

    /// `+40-074`
    pub fn name(self) -> String {
        format!("{:+03}{:+04}", self.lat, self.lon)
    }

    /// `+40-080` — the 10°×10° folder in `Earth nav data`.
    pub fn folder(self) -> String {
        let lat = self.lat.div_euclid(10) * 10;
        let lon = self.lon.div_euclid(10) * 10;
        format!("{:+03}{:+04}", lat, lon)
    }

    /// `Earth nav data/+40-080/+40-074.dsf`, with `/` separators.
    pub fn dsf_rel_path(self) -> String {
        format!("Earth nav data/{}/{}.dsf", self.folder(), self.name())
    }

    /// Parse `+40-074`.
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != 7 {
            return None;
        }
        let (lat, lon) = s.split_at(3);
        let lat: i32 = lat.parse().ok()?;
        let lon: i32 = lon.parse().ok()?;
        if !(-90..90).contains(&lat) || !(-180..180).contains(&lon) {
            return None;
        }
        if !s.starts_with(['+', '-']) || !lon_str_ok(&s[3..]) {
            return None;
        }
        Some(Self::new(lat, lon))
    }

    /// The tiles in a square ring/area of `radius` tiles around this one.
    pub fn neighbourhood(self, radius: i32) -> impl Iterator<Item = DsfTile> {
        (-radius..=radius).flat_map(move |dy| {
            (-radius..=radius).filter_map(move |dx| {
                let lat = self.lat + dy;
                let lon = (self.lon + dx + 180).rem_euclid(360) - 180;
                (-90..90).contains(&lat).then_some(DsfTile::new(lat, lon))
            })
        })
    }
}

fn lon_str_ok(s: &str) -> bool {
    s.starts_with(['+', '-']) && s[1..].bytes().all(|b| b.is_ascii_digit())
}

impl fmt::Display for DsfTile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quadkey_matches_bing_docs() {
        // Example from Microsoft's "Bing Maps Tile System" article.
        assert_eq!(TileCoord::new(3, 3, 5).quadkey(), "213");
    }

    #[test]
    fn roundtrip_latlon() {
        let (x, y) = latlon_to_tile(40.6413, -73.7781, 16);
        let (lat, lon) = tile_to_latlon(x, y, 16);
        assert!((lat - 40.6413).abs() < 1e-9);
        assert!((lon + 73.7781).abs() < 1e-9);
    }

    #[test]
    fn ortho4xp_texture_load_center() {
        // From terrain/24560_19344_XO16.ter: LOAD_CENTER 41.01307 -73.69629 7382
        let (lat, lon) = tile_to_latlon(19344.0 + 8.0, 24560.0 + 8.0, 16);
        assert!((lat - 41.01307).abs() < 1e-5, "{lat}");
        assert!((lon + 73.69629).abs() < 1e-5, "{lon}");
        let size = (meters_per_pixel(lat, 16) * 4096.0) as i64;
        assert!((size - 7382).abs() <= 1, "{size}");
    }

    #[test]
    fn dsf_names() {
        let t = DsfTile::new(40, -74);
        assert_eq!(t.name(), "+40-074");
        assert_eq!(t.folder(), "+40-080");
        assert_eq!(t.dsf_rel_path(), "Earth nav data/+40-080/+40-074.dsf");
        assert_eq!(DsfTile::parse("+40-074"), Some(t));
        assert_eq!(DsfTile::new(-1, 5).name(), "-01+005");
        assert_eq!(DsfTile::new(-1, 5).folder(), "-10+000");
        assert_eq!(DsfTile::parse("-01+005"), Some(DsfTile::new(-1, 5)));
        assert_eq!(DsfTile::parse("40-074"), None);
        assert_eq!(DsfTile::containing(40.64, -73.78), t);
    }

    #[test]
    fn neighbourhood_wraps_antimeridian() {
        let n: Vec<_> = DsfTile::new(0, 179).neighbourhood(1).collect();
        assert_eq!(n.len(), 9);
        assert!(n.contains(&DsfTile::new(0, -180)));
    }
}
