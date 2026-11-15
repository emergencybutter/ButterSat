//! Ortho4XP-style file names: textures, terrain files and water masks.
//!
//! Texture: `{row}_{col}_{code}{zl}.dds` where `row`/`col` are the slippy tile
//! Y/X of the top-left 256px tile at zoom `zl` (multiples of 16) and the texture
//! covers 16×16 tiles. Terrain: same stem + optional `_water`/`_sea` and
//! `_overlay` suffixes + `.ter`. Masks: `{row}_{col}_ZL{zl}.png`.

use std::fmt;

use crate::tile::{meters_per_pixel, tile_to_latlon, TileCoord};

/// Tiles per texture edge.
pub const TEXTURE_TILES: u32 = 16;
/// Lowest/highest texture zoom levels we accept.
pub const MIN_ZL: u8 = 12;
pub const MAX_ZL: u8 = 19;

/// Identifies one 4096² ortho texture.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextureId {
    pub row: u32,
    pub col: u32,
    pub zl: u8,
    /// Provider code from the file name (`XO`, `BI`, ...). A hint only.
    pub code: String,
}

impl TextureId {
    pub fn new(row: u32, col: u32, zl: u8, code: impl Into<String>) -> Self {
        Self { row, col, zl, code: code.into() }
    }

    /// Parse a texture file name (`24560_19344_XO16.dds`). Case-insensitive
    /// extension; the stem must match exactly.
    pub fn parse_dds(name: &str) -> Option<Self> {
        let stem = strip_ext(name, "dds")?;
        Self::parse_stem(stem)
    }

    /// Parse `24560_19344_XO16`.
    pub fn parse_stem(stem: &str) -> Option<Self> {
        let mut parts = stem.splitn(3, '_');
        let row: u32 = parse_digits(parts.next()?)?;
        let col: u32 = parse_digits(parts.next()?)?;
        let rest = parts.next()?;
        if rest.len() < 3 || !rest.is_ascii() {
            return None;
        }
        let (code, zl) = rest.split_at(rest.len() - 2);
        let zl: u8 = parse_digits(zl)?;
        if code.is_empty() || code.contains('_') || code.eq_ignore_ascii_case("ZL") {
            return None;
        }
        if !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'@') {
            return None;
        }
        let id = Self::new(row, col, zl, code);
        id.is_valid().then_some(id)
    }

    pub fn is_valid(&self) -> bool {
        (MIN_ZL..=MAX_ZL).contains(&self.zl)
            && self.row % TEXTURE_TILES == 0
            && self.col % TEXTURE_TILES == 0
            && (self.row as u64) < (1u64 << self.zl)
            && (self.col as u64) < (1u64 << self.zl)
    }

    pub fn stem(&self) -> String {
        format!("{}_{}_{}{}", self.row, self.col, self.code, self.zl)
    }

    pub fn dds_name(&self) -> String {
        format!("{}.dds", self.stem())
    }

    /// Top-left tile of the texture's block at zoom `zl - levels_up`.
    pub fn origin_at(&self, levels_up: u8) -> TileCoord {
        TileCoord::new(self.zl - levels_up, self.col >> levels_up, self.row >> levels_up)
    }

    /// Tiles covering this texture at `zl - levels_up` (row-major).
    pub fn tiles_at(&self, levels_up: u8) -> impl Iterator<Item = TileCoord> {
        let o = self.origin_at(levels_up);
        let n = (TEXTURE_TILES >> levels_up).max(1);
        (0..n).flat_map(move |dy| (0..n).map(move |dx| TileCoord::new(o.z, o.x + dx, o.y + dy)))
    }

    /// Centre of the texture.
    pub fn center_latlon(&self) -> (f64, f64) {
        tile_to_latlon(self.col as f64 + 8.0, self.row as f64 + 8.0, self.zl)
    }

    /// Approximate edge length in meters, as Ortho4XP writes it in `LOAD_CENTER`.
    pub fn approx_size_m(&self) -> u32 {
        let (lat, _) = self.center_latlon();
        (meters_per_pixel(lat, self.zl) * 4096.0) as u32
    }
}

impl fmt::Display for TextureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.stem())
    }
}

/// Kind of terrain file, from its suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerKind {
    Land,
    Water,
    Sea,
}

/// A terrain (`.ter`) file name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TerName {
    pub texture: TextureId,
    pub kind: TerKind,
    pub overlay: bool,
}

impl TerName {
    pub fn parse(name: &str) -> Option<Self> {
        let mut stem = strip_ext(name, "ter")?;
        let mut overlay = false;
        if let Some(s) = stem.strip_suffix("_overlay") {
            stem = s;
            overlay = true;
        }
        let mut kind = TerKind::Land;
        if let Some(s) = stem.strip_suffix("_water") {
            stem = s;
            kind = TerKind::Water;
        } else if let Some(s) = stem.strip_suffix("_sea") {
            stem = s;
            kind = TerKind::Sea;
        }
        let texture = TextureId::parse_stem(stem)?;
        Some(Self { texture, kind, overlay })
    }

    pub fn file_name(&self) -> String {
        let mut s = self.texture.stem();
        match self.kind {
            TerKind::Land => {}
            TerKind::Water => s.push_str("_water"),
            TerKind::Sea => s.push_str("_sea"),
        }
        if self.overlay {
            s.push_str("_overlay");
        }
        s.push_str(".ter");
        s
    }
}

/// A water mask PNG name (`24560_19344_ZL16.png`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MaskName {
    pub row: u32,
    pub col: u32,
    pub zl: u8,
}

impl MaskName {
    pub fn parse(name: &str) -> Option<Self> {
        let stem = strip_ext(name, "png")?;
        let mut parts = stem.splitn(3, '_');
        let row = parse_digits(parts.next()?)?;
        let col = parse_digits(parts.next()?)?;
        let zl = parse_digits(parts.next()?.strip_prefix("ZL")?)?;
        Some(Self { row, col, zl })
    }

    pub fn for_texture(t: &TextureId) -> Self {
        Self { row: t.row, col: t.col, zl: t.zl }
    }

    pub fn file_name(&self) -> String {
        format!("{}_{}_ZL{}.png", self.row, self.col, self.zl)
    }
}

/// Inputs Ortho4XP uses when writing a `.ter` file that we need to reproduce.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerParams {
    /// Zoom of the water masks (Ortho4XP `mask_zl`, default 14).
    pub mask_zl: u8,
    /// Whether land terrain gets the decal directive (`use_decal_on_terrain`).
    pub decal: bool,
    /// Whether land casts shadows (`terrain_casts_shadows`).
    pub land_casts_shadows: bool,
}

impl Default for TerParams {
    fn default() -> Self {
        Self { mask_zl: 14, decal: false, land_casts_shadows: true }
    }
}

/// Generate the contents of a terrain file exactly as Ortho4XP does
/// (`O4_DSF_Utils.create_terrain_file`, "XP11 + bathy" water tech, external
/// masks). `texture_name` is the DDS name the terrain points to.
pub fn ter_contents(ter: &TerName, p: &TerParams) -> String {
    let t = &ter.texture;
    let (lat, lon) = t.center_latlon();
    let size = t.approx_size_m();
    let mut s = String::with_capacity(256);
    s.push_str("A\n800\nTERRAIN\n\n");
    s.push_str(&format!("LOAD_CENTER {lat:.5} {lon:.5} {size} 4096\n"));
    s.push_str(&format!("BASE_TEX_NOWRAP ../textures/{}\n", t.dds_name()));
    let wet = ter.kind != TerKind::Land;
    if wet && !ter.overlay {
        s.push_str("WATER_COLOR_MASK\n");
    } else if ter.kind == TerKind::Water {
        s.push_str("BORDER_TEX ../textures/water_transition.png\n");
    } else if ter.kind == TerKind::Sea {
        let border = 4096 >> (t.zl.saturating_sub(p.mask_zl));
        s.push_str(&format!("LOAD_CENTER_BORDER {lat:.5} {lon:.5} {size} {border}\n"));
        s.push_str(&format!("BORDER_TEX ../textures/{}\n", MaskName::for_texture(t).file_name()));
    }
    if ter.kind != TerKind::Water && p.decal {
        s.push_str("DECAL_LIB lib/g10/decals/maquify_2_green_key.dcl\n");
    }
    if wet {
        s.push_str("WET\n");
    } else {
        s.push_str("NO_ALPHA\n");
    }
    if wet || !p.land_casts_shadows {
        s.push_str("NO_SHADOW\n");
    }
    s
}

fn strip_ext<'a>(name: &'a str, ext: &str) -> Option<&'a str> {
    let dot = name.rfind('.')?;
    let (stem, e) = name.split_at(dot);
    e[1..].eq_ignore_ascii_case(ext).then_some(stem)
}

fn parse_digits<T: std::str::FromStr>(s: &str) -> Option<T> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_texture() {
        let t = TextureId::parse_dds("24560_19344_XO16.dds").unwrap();
        assert_eq!(t, TextureId::new(24560, 19344, 16, "XO"));
        assert_eq!(t.dds_name(), "24560_19344_XO16.dds");
        assert_eq!(TextureId::parse_dds("49216_38896_XO17.DDS").unwrap().zl, 17);
        assert_eq!(TextureId::parse_dds("24560_19344_GO216.dds").unwrap().code, "GO2");
        assert!(TextureId::parse_dds("24561_19344_XO16.dds").is_none()); // not /16
        assert!(TextureId::parse_dds("24560_19344_ZL16.dds").is_none());
        assert!(TextureId::parse_dds("water_transition.png").is_none());
        assert!(TextureId::parse_dds("24560_19344_XO16.png").is_none());
    }

    #[test]
    fn tiles_at_levels() {
        let t = TextureId::new(24560, 19344, 16, "XO");
        assert_eq!(t.tiles_at(0).count(), 256);
        assert_eq!(t.tiles_at(4).collect::<Vec<_>>(), vec![TileCoord::new(12, 1209, 1535)]);
        assert_eq!(t.tiles_at(1).next().unwrap(), TileCoord::new(15, 9672, 12280));
    }

    #[test]
    fn ter_names() {
        for n in [
            "24560_19344_XO16.ter",
            "24560_19344_XO16_sea_overlay.ter",
            "24560_19296_XO16_water_overlay.ter",
            "24560_19296_XO16_water.ter",
            "24560_19296_XO16_sea.ter",
        ] {
            assert_eq!(TerName::parse(n).unwrap().file_name(), n);
        }
        assert!(TerName::parse("foo.ter").is_none());
    }

    #[test]
    fn mask_names() {
        let m = MaskName::parse("24560_19344_ZL16.png").unwrap();
        assert_eq!(m, MaskName { row: 24560, col: 19344, zl: 16 });
        assert_eq!(m.file_name(), "24560_19344_ZL16.png");
        assert!(MaskName::parse("water_transition.png").is_none());
    }

    // Golden outputs copied from the NYC pilot build (Ortho4XP, our config).
    #[test]
    fn ter_land_matches_ortho4xp() {
        let ter = TerName::parse("24560_19344_XO16.ter").unwrap();
        assert_eq!(
            ter_contents(&ter, &TerParams::default()),
            "A\n800\nTERRAIN\n\nLOAD_CENTER 41.01307 -73.69629 7382 4096\n\
             BASE_TEX_NOWRAP ../textures/24560_19344_XO16.dds\nNO_ALPHA\n"
        );
        let ter = TerName::parse("49216_38896_XO17.ter").unwrap();
        assert_eq!(
            ter_contents(&ter, &TerParams::default()),
            "A\n800\nTERRAIN\n\nLOAD_CENTER 40.83044 -73.14697 3701 4096\n\
             BASE_TEX_NOWRAP ../textures/49216_38896_XO17.dds\nNO_ALPHA\n"
        );
    }

    #[test]
    fn ter_sea_overlay_matches_ortho4xp() {
        let ter = TerName::parse("24560_19344_XO16_sea_overlay.ter").unwrap();
        assert_eq!(
            ter_contents(&ter, &TerParams::default()),
            "A\n800\nTERRAIN\n\nLOAD_CENTER 41.01307 -73.69629 7382 4096\n\
             BASE_TEX_NOWRAP ../textures/24560_19344_XO16.dds\n\
             LOAD_CENTER_BORDER 41.01307 -73.69629 7382 1024\n\
             BORDER_TEX ../textures/24560_19344_ZL16.png\nWET\nNO_SHADOW\n"
        );
    }

    #[test]
    fn ter_water_overlay_matches_ortho4xp() {
        let ter = TerName::parse("24560_19296_XO16_water_overlay.ter").unwrap();
        assert_eq!(
            ter_contents(&ter, &TerParams::default()),
            "A\n800\nTERRAIN\n\nLOAD_CENTER 41.01307 -73.95996 7382 4096\n\
             BASE_TEX_NOWRAP ../textures/24560_19296_XO16.dds\n\
             BORDER_TEX ../textures/water_transition.png\nWET\nNO_SHADOW\n"
        );
    }
}
