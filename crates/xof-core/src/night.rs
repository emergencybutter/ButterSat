//! Night textures (design §5.6): the lights of a town, painted on a texture
//! X-Plane adds to the photo at night (`LIT_TEX_NOWRAP` in the `.ter`).
//!
//! X-Plane 12's own terrain glows at night where it is a town; an ortho photo
//! replaces that terrain, so without a lit texture of its own a town is black
//! but for the roads' and buildings' own lights. We draw, for each photo
//! texture, a 1024² BC1 texture holding:
//!
//! - **street lamps** along X-Plane's own roads (read from the user's Global
//!   Scenery DSFs, [`Roads`]), spaced by road class, mostly sodium orange and
//!   some white, a few out;
//! - **a soft glow** round each lamp;
//! - **windows** in a band along town streets (roads with sidewalks);
//! - and black everywhere else: X-Plane lights the photo itself at night.
//!
//! How built-up a place is comes from the roads themselves: road length per
//! km² on a world-aligned grid of zoom-16 tiles decides what share of the
//! lamps is lit and how many windows there are. No extra data is downloaded.
//!
//! Everything random is seeded from the position on Earth, so lamps match
//! across texture edges and zoom levels. Lights are splatted with their energy
//! defined in square metres, so a town is as bright on a ZL16 texture as on a
//! ZL18 one. Smaller mips are averaged in linear light.

use crate::bc1;
use crate::dds;
use crate::names::{TextureId, TEXTURE_TILES};
use crate::raster::Rgba;
use crate::tile::{latlon_to_tile, meters_per_pixel, tile_to_latlon, TILE_PX};

/// Edge length of a night texture.
pub const LIT_DIM: u32 = 1024;
/// Mip levels of a night texture (1024 .. 1).
pub const LIT_MIPS: u8 = 11;

/// Byte length of mip `level` of a night texture.
pub const fn lit_mip_len(level: u8) -> u64 {
    let d = LIT_DIM >> level;
    let b = if d / 4 == 0 { 1 } else { d / 4 } as u64;
    b * b * 8
}

/// Total size of a night texture file.
pub const LIT_FILE_LEN: u64 = {
    let mut n = dds::HEADER_LEN;
    let mut l = 0;
    while l < LIT_MIPS {
        n += lit_mip_len(l);
        l += 1;
    }
    n
};

/// Bump when the drawing changes: drawn textures cached on disk are keyed
/// on it.
pub const RENDER_VERSION: u32 = 2;

/// A night texture with no light: what is served when there are no roads (no
/// Global Scenery, open sea). BC1 zeros are opaque black.
pub fn black_dds() -> Vec<u8> {
    let mut out = vec![0u8; LIT_FILE_LEN as usize];
    out[..dds::HEADER_LEN as usize].copy_from_slice(&dds::header_for(LIT_DIM, LIT_MIPS));
    out
}

/// The road classes we light, from X-Plane 12's `roads.net` families.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RoadClass {
    Motorway = 0,
    Link = 1,
    Primary = 2,
    Secondary = 3,
    Local = 4,
    Lane = 5,
}

impl RoadClass {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Motorway,
            1 => Self::Link,
            2 => Self::Primary,
            3 => Self::Secondary,
            4 => Self::Local,
            5 => Self::Lane,
            _ => return None,
        })
    }

    /// Class of a DSF road subtype (`ROAD_TYPE` / 100 in `lib/g10/roads.net`
    /// and `roads_EU.net`), and whether it is a town street (has sidewalks).
    /// The tens are the family (1–2 primary, 3–4 secondary, 5–6 local, 7
    /// single lane, 10–11 highway, 12–13 ramps), the unit the variant: 0 is
    /// plain, 1–8 have sidewalks. Railways (15x), power lines (220) and
    /// junction stubs (1–3) lay no street: `None`.
    pub fn from_subtype(subtype: u32) -> Option<(Self, bool)> {
        let urban = subtype % 10 != 0;
        Some(match subtype {
            10..=29 => (Self::Primary, urban),
            30..=49 => (Self::Secondary, urban),
            50..=69 => (Self::Local, urban),
            70..=79 => (Self::Lane, false),
            100..=119 => (Self::Motorway, false),
            120..=139 => (Self::Link, false),
            _ => return None,
        })
    }

    /// (street width m, lamp brightness, lamp spacing m).
    fn style(self) -> (f64, f32, f64) {
        match self {
            Self::Motorway => (16.0, 1.00, 45.0),
            Self::Link => (8.0, 0.90, 35.0),
            Self::Primary => (12.0, 0.95, 35.0),
            Self::Secondary => (10.0, 0.85, 32.0),
            Self::Local => (6.0, 0.60, 28.0),
            Self::Lane => (4.0, 0.45, 40.0),
        }
    }

    /// Weight of a metre of this road in the built-up density.
    fn density_weight(self) -> f64 {
        match self {
            Self::Motorway | Self::Link => 0.3,
            _ => 1.0,
        }
    }

    fn has_windows(self) -> bool {
        matches!(self, Self::Primary | Self::Secondary | Self::Local)
    }
}

/// One road: a polyline of `pts[start..end]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Road {
    pub class: RoadClass,
    pub urban: bool,
    pub start: u32,
    pub end: u32,
}

/// The roads of one DSF tile: points are `[lon, lat]` in degrees.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Roads {
    pub roads: Vec<Road>,
    pub pts: Vec<[f64; 2]>,
}

const ROADS_MAGIC: &[u8; 8] = b"XORDS\x00\x00\x01";

impl Roads {
    pub fn push(&mut self, class: RoadClass, urban: bool, pts: impl IntoIterator<Item = [f64; 2]>) {
        let start = self.pts.len() as u32;
        self.pts.extend(pts);
        let end = self.pts.len() as u32;
        if end - start >= 2 {
            self.roads.push(Road { class, urban, start, end });
        } else {
            self.pts.truncate(start as usize);
        }
    }

    pub fn points(&self, r: &Road) -> &[[f64; 2]] {
        &self.pts[r.start as usize..r.end as usize]
    }

    /// Length of one road in metres.
    pub fn road_length_m(&self, r: &Road) -> f64 {
        polyline_len_m(self.points(r))
    }

    /// Total length in metres (diagnostics).
    pub fn length_m(&self) -> f64 {
        self.roads.iter().map(|r| self.road_length_m(r)).sum()
    }

    /// Compact binary form for the disk cache: points as f32 offsets from
    /// the south-west corner of their bounding box (sub-metre).
    pub fn to_bytes(&self) -> Vec<u8> {
        let (ox, oy) = self.pts.iter().fold((f64::MAX, f64::MAX), |(x, y), p| (x.min(p[0]), y.min(p[1])));
        let (ox, oy) = if self.pts.is_empty() { (0.0, 0.0) } else { (ox, oy) };
        let mut out = Vec::with_capacity(32 + self.roads.len() * 10 + self.pts.len() * 8);
        out.extend_from_slice(ROADS_MAGIC);
        out.extend_from_slice(&(self.roads.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.pts.len() as u32).to_le_bytes());
        out.extend_from_slice(&ox.to_le_bytes());
        out.extend_from_slice(&oy.to_le_bytes());
        for r in &self.roads {
            out.push(r.class as u8);
            out.push(r.urban as u8);
            out.extend_from_slice(&r.start.to_le_bytes());
            out.extend_from_slice(&r.end.to_le_bytes());
        }
        for p in &self.pts {
            out.extend_from_slice(&((p[0] - ox) as f32).to_le_bytes());
            out.extend_from_slice(&((p[1] - oy) as f32).to_le_bytes());
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let rest = b.strip_prefix(ROADS_MAGIC.as_slice())?;
        let u32_at =
            |s: &[u8], i: usize| -> Option<u32> { Some(u32::from_le_bytes(s.get(i..i + 4)?.try_into().ok()?)) };
        let f64_at =
            |s: &[u8], i: usize| -> Option<f64> { Some(f64::from_le_bytes(s.get(i..i + 8)?.try_into().ok()?)) };
        let nr = u32_at(rest, 0)? as usize;
        let np = u32_at(rest, 4)? as usize;
        let (ox, oy) = (f64_at(rest, 8)?, f64_at(rest, 16)?);
        let roads_at = 24;
        let pts_at = roads_at + nr * 10;
        if rest.len() != pts_at + np * 8 {
            return None;
        }
        let mut roads = Vec::with_capacity(nr);
        for i in 0..nr {
            let o = roads_at + i * 10;
            let class = RoadClass::from_u8(rest[o])?;
            let (start, end) = (u32_at(rest, o + 2)?, u32_at(rest, o + 6)?);
            if start >= end || end as usize > np {
                return None;
            }
            roads.push(Road { class, urban: rest[o + 1] != 0, start, end });
        }
        let pts = rest[pts_at..]
            .chunks_exact(8)
            .map(|c| {
                let x = f32::from_le_bytes(c[..4].try_into().unwrap()) as f64;
                let y = f32::from_le_bytes(c[4..].try_into().unwrap()) as f64;
                [x + ox, y + oy]
            })
            .collect();
        Some(Self { roads, pts })
    }
}

const M_PER_DEG_LAT: f64 = 110_574.0;
const M_PER_DEG_LON_EQ: f64 = 111_320.0;

/// Segment length in metres (equirectangular; fine at road scale).
fn seg_len_m(a: [f64; 2], b: [f64; 2]) -> f64 {
    let lat = ((a[1] + b[1]) * 0.5).to_radians();
    let dx = (b[0] - a[0]) * M_PER_DEG_LON_EQ * lat.cos();
    let dy = (b[1] - a[1]) * M_PER_DEG_LAT;
    (dx * dx + dy * dy).sqrt()
}

fn polyline_len_m(p: &[[f64; 2]]) -> f64 {
    p.windows(2).map(|w| seg_len_m(w[0], w[1])).sum()
}

/// splitmix64.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Deterministic randomness for a place on Earth (≈ 1 cm grid) and a salt.
fn seed_at(lon: f64, lat: f64, salt: u64) -> u64 {
    let x = (lon * 1e7).round() as i64 as u64;
    let y = (lat * 1e7).round() as i64 as u64;
    mix(x ^ mix(y ^ mix(salt)))
}

/// Successive uniform floats in [0, 1) from a seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = mix(self.0);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

const SODIUM: [f32; 3] = [1.00, 0.70, 0.36];
const WHITE: [f32; 3] = [0.95, 0.92, 0.85];
const WINDOW: [f32; 3] = [1.00, 0.80, 0.55];
const WHITE_SHARE: f32 = 0.35;
const LAMPS_OUT: f32 = 0.12;
/// Lamp core and glow (sigma in metres, relative intensity).
const CORE: (f64, f32) = (2.5, 1.0);
const GLOW: (f64, f32) = (10.0, 0.06);
const WINDOW_SIGMA_M: f64 = 3.0;
/// Narrowest light, in texels: wide enough for bilinear magnification to
/// draw a round glow rather than a square.
const MIN_SIGMA_PX: f64 = 1.2;
const WINDOW_STEP_M: f64 = 10.0;
/// Light fainter than this (of 255, brightest channel) is written black:
/// BC1 cannot hold faint values in their colour (purple/green blocks).
const FLOOR: u8 = 6;
/// Zoom of the world-aligned built-up density grid.
const DENSITY_ZOOM: u8 = 16;
const DENSITY_HALO: i64 = 2;

/// Built-up density (road km per km²) on a world-aligned grid of zoom-16
/// tiles covering the texture plus a halo, smoothed 3×3.
struct Density {
    x0: i64,
    y0: i64,
    n: i64,
    cells: Vec<f32>,
    /// Draw pixels → fractional zoom-16 tile coordinates.
    px_scale: f64,
    px_ox: f64,
    px_oy: f64,
}

impl Density {
    fn new(tex: &TextureId, roads: &[&Roads], draw_scale: f64) -> Self {
        let (x0t, y0t, cover) = if tex.zl <= DENSITY_ZOOM {
            let s = DENSITY_ZOOM - tex.zl;
            ((tex.col as i64) << s, (tex.row as i64) << s, (TEXTURE_TILES as i64) << s)
        } else {
            let s = tex.zl - DENSITY_ZOOM;
            ((tex.col as i64) >> s, (tex.row as i64) >> s, ((TEXTURE_TILES >> s).max(1)) as i64)
        };
        let (x0, y0, n) = (x0t - DENSITY_HALO, y0t - DENSITY_HALO, cover + 2 * DENSITY_HALO);
        let mut acc = vec![0f64; (n * n) as usize];
        let (lat_c, _) = tex.center_latlon();
        let cell_m = meters_per_pixel(lat_c, DENSITY_ZOOM) * TILE_PX as f64;
        let (nw_lat, nw_lon) = tile_to_latlon(x0 as f64, y0 as f64, DENSITY_ZOOM);
        let (se_lat, se_lon) = tile_to_latlon((x0 + n) as f64, (y0 + n) as f64, DENSITY_ZOOM);
        for rs in roads {
            for r in &rs.roads {
                let p = rs.points(r);
                if !bbox_hits(p, se_lat, nw_lat, nw_lon, se_lon) {
                    continue;
                }
                let w = r.class.density_weight() * if r.urban { 1.3 } else { 1.0 };
                for s in p.windows(2) {
                    let (mlon, mlat) = ((s[0][0] + s[1][0]) * 0.5, (s[0][1] + s[1][1]) * 0.5);
                    let (tx, ty) = latlon_to_tile(mlat, mlon, DENSITY_ZOOM);
                    let (cx, cy) = (tx.floor() as i64 - x0, ty.floor() as i64 - y0);
                    if (0..n).contains(&cx) && (0..n).contains(&cy) {
                        acc[(cy * n + cx) as usize] += seg_len_m(s[0], s[1]) * w;
                    }
                }
            }
        }
        // km of road per km² of cell, then a 3×3 box blur.
        let area_km2 = (cell_m / 1000.0).powi(2);
        let raw: Vec<f64> = acc.iter().map(|m| m / 1000.0 / area_km2).collect();
        let mut cells = vec![0f32; raw.len()];
        for y in 0..n {
            for x in 0..n {
                let (mut s, mut k) = (0.0, 0.0);
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (xx, yy) = (x + dx, y + dy);
                        if (0..n).contains(&xx) && (0..n).contains(&yy) {
                            s += raw[(yy * n + xx) as usize];
                            k += 1.0;
                        }
                    }
                }
                cells[(y * n + x) as usize] = (s / k) as f32;
            }
        }
        // Draw pixel p → texture tile (zl) col + p / draw_scale / 256 → zoom 16.
        let z = 2f64.powi(DENSITY_ZOOM as i32 - tex.zl as i32);
        Self {
            x0,
            y0,
            n,
            cells,
            px_scale: z / (draw_scale * TILE_PX as f64),
            px_ox: tex.col as f64 * z,
            px_oy: tex.row as f64 * z,
        }
    }

    /// Density at draw pixel (`x`, `y`), bilinear between cell centres.
    fn at(&self, x: f64, y: f64) -> f32 {
        let gx = self.px_ox + x * self.px_scale - self.x0 as f64 - 0.5;
        let gy = self.px_oy + y * self.px_scale - self.y0 as f64 - 0.5;
        let (ix, iy) = (gx.floor(), gy.floor());
        let (fx, fy) = ((gx - ix) as f32, (gy - iy) as f32);
        let c = |x: i64, y: i64| self.cells[(y.clamp(0, self.n - 1) * self.n + x.clamp(0, self.n - 1)) as usize];
        let (ix, iy) = (ix as i64, iy as i64);
        let top = c(ix, iy) * (1.0 - fx) + c(ix + 1, iy) * fx;
        let bot = c(ix, iy + 1) * (1.0 - fx) + c(ix + 1, iy + 1) * fx;
        top * (1.0 - fy) + bot * fy
    }
}

/// Share of a road's lamps that are lit at built-up density `d` (km/km²):
/// a fifth in open country, all from 10 km/km² (a village core) on.
fn lit_share(d: f32) -> f32 {
    (0.2 + 0.8 * (d - 2.0) / 8.0).clamp(0.2, 1.0)
}

/// How much of a town a place is, for windows: 0 below 3 km/km², 1 from 15.
fn townness(d: f32) -> f32 {
    ((d - 3.0) / 12.0).clamp(0.0, 1.0)
}

fn bbox_hits(p: &[[f64; 2]], s: f64, n: f64, w: f64, e: f64) -> bool {
    let (mut a, mut b, mut c, mut d) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for q in p {
        a = a.min(q[0]);
        b = b.max(q[0]);
        c = c.min(q[1]);
        d = d.max(q[1]);
    }
    b >= w && a <= e && d >= s && c <= n
}

/// Linear-light accumulation buffer.
struct Canvas {
    dim: usize,
    px: Vec<[f32; 3]>,
    m_per_px: f64,
}

impl Canvas {
    /// Add a gaussian of `sigma_m` metres whose peak would be `peak` if fully
    /// resolved. A light narrower than [`MIN_SIGMA_PX`] keeps its energy but
    /// is spread to that width: a one-texel light is magnified by X-Plane
    /// into a square at low altitude (in-sim, 2026-11-28).
    fn splat(&mut self, x: f64, y: f64, sigma_m: f64, peak: f32, rgb: [f32; 3]) {
        let s_true = sigma_m / self.m_per_px;
        let s = s_true.max(MIN_SIGMA_PX);
        let amp = peak * ((s_true / s) * (s_true / s)) as f32;
        let r = (3.0 * s).ceil() as i64;
        let (cx, cy) = (x.floor() as i64, y.floor() as i64);
        let d = self.dim as i64;
        if cx + r < 0 || cy + r < 0 || cx - r >= d || cy - r >= d {
            return;
        }
        let inv = 1.0 / (2.0 * s * s);
        for yy in (cy - r).max(0)..=(cy + r).min(d - 1) {
            let dy = yy as f64 + 0.5 - y;
            for xx in (cx - r).max(0)..=(cx + r).min(d - 1) {
                let dx = xx as f64 + 0.5 - x;
                let k = amp * (-(dx * dx + dy * dy) * inv).exp() as f32;
                if k < 1e-4 {
                    continue;
                }
                let p = &mut self.px[yy as usize * self.dim + xx as usize];
                p[0] += k * rgb[0];
                p[1] += k * rgb[1];
                p[2] += k * rgb[2];
            }
        }
    }
}

/// Draw the night texture of `tex` from the roads of the DSF tiles it
/// overlaps. Returns the whole DDS file (`LIT_FILE_LEN` bytes).
pub fn render(tex: &TextureId, roads: &[&Roads]) -> Vec<u8> {
    let canvas = draw(tex, roads);
    encode(&canvas)
}

fn draw(tex: &TextureId, roads: &[&Roads]) -> Canvas {
    let dim = LIT_DIM as usize;
    // Texture pixels (4096 per texture) → draw pixels.
    let draw_scale = LIT_DIM as f64 / (TEXTURE_TILES * TILE_PX) as f64;
    let (lat_c, _) = tex.center_latlon();
    let m_per_px = meters_per_pixel(lat_c, tex.zl) / draw_scale;
    let mut cv = Canvas { dim, px: vec![[0.0; 3]; dim * dim], m_per_px };
    let to_px = |p: [f64; 2]| -> (f64, f64) {
        let (tx, ty) = latlon_to_tile(p[1], p[0], tex.zl);
        ((tx - tex.col as f64) * TILE_PX as f64 * draw_scale, (ty - tex.row as f64) * TILE_PX as f64 * draw_scale)
    };
    // Texture bounds plus a margin for glows and windows, in degrees.
    let margin_m = 80.0;
    let (n_lat, w_lon) = tile_to_latlon(tex.col as f64, tex.row as f64, tex.zl);
    let (s_lat, e_lon) = tile_to_latlon((tex.col + TEXTURE_TILES) as f64, (tex.row + TEXTURE_TILES) as f64, tex.zl);
    let dlat = margin_m / M_PER_DEG_LAT;
    let dlon = margin_m / (M_PER_DEG_LON_EQ * lat_c.to_radians().cos());
    let (bs, bn, bw, be) = (s_lat - dlat, n_lat + dlat, w_lon - dlon, e_lon + dlon);
    let density = Density::new(tex, roads, draw_scale);
    let windows = m_per_px < 20.0;

    for rs in roads {
        for r in &rs.roads {
            let pts = rs.points(r);
            if !bbox_hits(pts, bs, bn, bw, be) {
                continue;
            }
            let (width, bright, spacing) = r.class.style();
            let side_off = width * 0.5 + 1.0;
            let first = pts[0];
            let mut s_next = Rng(seed_at(first[0], first[1], 1)).next() as f64 * spacing;
            let mut w_next = Rng(seed_at(first[0], first[1], 2)).next() as f64 * WINDOW_STEP_M;
            let mut s0 = 0.0;
            for seg in pts.windows(2) {
                let (a, b) = (seg[0], seg[1]);
                let len = seg_len_m(a, b);
                if len <= 0.0 {
                    continue;
                }
                let seg_in =
                    !(a[0].max(b[0]) < bw || a[0].min(b[0]) > be || a[1].max(b[1]) < bs || a[1].min(b[1]) > bn);
                if !seg_in {
                    // Keep the lamp/window rhythm without drawing.
                    while s_next < s0 + len {
                        s_next += spacing;
                    }
                    while w_next < s0 + len {
                        w_next += WINDOW_STEP_M;
                    }
                    s0 += len;
                    continue;
                }
                let (pa, pb) = (to_px(a), to_px(b));
                let (ux, uy) = {
                    let (dx, dy) = (pb.0 - pa.0, pb.1 - pa.1);
                    let l = (dx * dx + dy * dy).sqrt().max(1e-9);
                    (dx / l, dy / l)
                };
                let at = |s: f64| -> ([f64; 2], (f64, f64)) {
                    let t = ((s - s0) / len).clamp(0.0, 1.0);
                    let ll = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                    (ll, (pa.0 + (pb.0 - pa.0) * t, pa.1 + (pb.1 - pa.1) * t))
                };
                while s_next < s0 + len {
                    let (ll, (x, y)) = at(s_next);
                    s_next += spacing;
                    let d = density.at(x, y);
                    let mut rng = Rng(seed_at(ll[0], ll[1], 3 + r.class as u64));
                    let share = lit_share(d) * if r.urban || r.class == RoadClass::Motorway { 1.0 } else { 0.6 };
                    if rng.next() >= share || rng.next() < LAMPS_OUT {
                        continue;
                    }
                    let rgb = if rng.next() < WHITE_SHARE { WHITE } else { SODIUM };
                    let level = bright * (0.75 + 0.5 * rng.next()) * (0.85 + 0.3 * townness(d));
                    let side = if rng.next() < 0.5 { -1.0 } else { 1.0 };
                    let o = side * side_off / m_per_px;
                    let (lx, ly) = (x - uy * o, y + ux * o);
                    cv.splat(lx, ly, CORE.0, CORE.1 * level, rgb);
                    cv.splat(lx, ly, GLOW.0, GLOW.1 * level, rgb);
                }
                if windows && r.urban && r.class.has_windows() {
                    while w_next < s0 + len {
                        let (ll, (x, y)) = at(w_next);
                        w_next += WINDOW_STEP_M;
                        let town = townness(density.at(x, y));
                        if town <= 0.0 {
                            continue;
                        }
                        let mut rng = Rng(seed_at(ll[0], ll[1], 17));
                        for side in [-1.0, 1.0] {
                            if rng.next() >= 0.10 + 0.40 * town {
                                continue;
                            }
                            let off = (width * 0.5 + 8.0 + 35.0 * rng.next() as f64) / m_per_px;
                            let along = (rng.next() as f64 - 0.5) * WINDOW_STEP_M / m_per_px;
                            let (wx, wy) = (x + ux * along - uy * off * side, y + uy * along + ux * off * side);
                            let level = (0.10 + 0.25 * rng.next() * rng.next()) * (0.5 + 0.5 * town);
                            cv.splat(wx, wy, WINDOW_SIGMA_M, level, WINDOW);
                        }
                    }
                } else {
                    while w_next < s0 + len {
                        w_next += WINDOW_STEP_M;
                    }
                }
                s0 += len;
            }
        }
    }
    cv
}

fn linear_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0 + 0.5) as u8
}

/// sRGB bytes of a linear level: faint light black, never greener or bluer
/// than red (all our lights are warm).
fn to_rgba(px: &[[f32; 3]], dim: u32) -> Rgba {
    let mut img = Rgba::new(dim, dim);
    for (i, p) in px.iter().enumerate() {
        let (r, g, b) = (linear_to_srgb(p[0]), linear_to_srgb(p[1]), linear_to_srgb(p[2]));
        let out = if r.max(g).max(b) < FLOOR { [0, 0, 0, 255] } else { [r, g.min(r), b.min(r), 255] };
        img.data[i * 4..i * 4 + 4].copy_from_slice(&out);
    }
    img
}

fn encode(cv: &Canvas) -> Vec<u8> {
    let mut out = Vec::with_capacity(LIT_FILE_LEN as usize);
    out.extend_from_slice(&dds::header_for(LIT_DIM, LIT_MIPS));
    let mut level: Vec<[f32; 3]> = cv.px.clone();
    let mut dim = cv.dim;
    for mip in 0..LIT_MIPS {
        if level.iter().all(|p| p[0] < 1e-3 && p[1] < 1e-3 && p[2] < 1e-3) {
            out.resize(out.len() + lit_mip_len(mip) as usize, 0);
        } else {
            let mut blocks = bc1::encode(&to_rgba(&level, dim as u32));
            warm_bc1(&mut blocks);
            debug_assert_eq!(blocks.len() as u64, lit_mip_len(mip));
            out.extend_from_slice(&blocks);
        }
        if dim > 1 {
            let h = dim / 2;
            let mut next = vec![[0f32; 3]; h * h];
            for y in 0..h {
                for x in 0..h {
                    let mut s = [0f32; 3];
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let p = level[(2 * y + dy) * dim + 2 * x + dx];
                        s[0] += p[0];
                        s[1] += p[1];
                        s[2] += p[2];
                    }
                    next[y * h + x] = [s[0] * 0.25, s[1] * 0.25, s[2] * 0.25];
                }
            }
            level = next;
            dim = h;
        }
    }
    debug_assert_eq!(out.len() as u64, LIT_FILE_LEN);
    out
}

/// Make BC1 endpoints no greener or bluer than they are red, keeping each
/// block in 4-colour mode: the 565 quantization of faint orange light
/// otherwise comes out green or purple.
fn warm_bc1(blocks: &mut [u8]) {
    let ex5 = |v: u16| (v << 3) | (v >> 2);
    let ex6 = |v: u16| (v << 2) | (v >> 4);
    let fix = |c: u16| -> u16 {
        let (r, mut g, b) = (c >> 11, (c >> 5) & 63, c & 31);
        let r8 = ex5(r);
        while g > 0 && ex6(g) > r8 {
            g -= 1;
        }
        (r << 11) | (g << 5) | b.min(r)
    };
    for b in blocks.chunks_exact_mut(8) {
        let c0 = u16::from_le_bytes([b[0], b[1]]);
        let c1 = u16::from_le_bytes([b[2], b[3]]);
        if c0 <= c1 {
            continue; // 3-colour mode (or flat): leave as encoded
        }
        let (n0, n1) = (fix(c0), fix(c1));
        let mut idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let (n0, n1) = if n0 > n1 {
            (n0, n1)
        } else if n0 < n1 {
            idx ^= 0x5555_5555; // swap endpoints: 0↔1, 2↔3
            (n1, n0)
        } else {
            idx = 0;
            (n0, n1)
        };
        b[0..2].copy_from_slice(&n0.to_le_bytes());
        b[2..4].copy_from_slice(&n1.to_le_bytes());
        b[4..8].copy_from_slice(&idx.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        assert_eq!(lit_mip_len(0), 524_288);
        assert_eq!(lit_mip_len(10), 8);
        assert_eq!(LIT_FILE_LEN, 128 + 699_064);
        let b = black_dds();
        assert_eq!(b.len() as u64, LIT_FILE_LEN);
        assert_eq!(&b[0..4], b"DDS ");
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 1024);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 11);
    }

    #[test]
    fn subtypes() {
        assert_eq!(RoadClass::from_subtype(50), Some((RoadClass::Local, false)));
        assert_eq!(RoadClass::from_subtype(54), Some((RoadClass::Local, true)));
        assert_eq!(RoadClass::from_subtype(41), Some((RoadClass::Secondary, true)));
        assert_eq!(RoadClass::from_subtype(110), Some((RoadClass::Motorway, false)));
        assert_eq!(RoadClass::from_subtype(121), Some((RoadClass::Link, false)));
        assert_eq!(RoadClass::from_subtype(152), None);
        assert_eq!(RoadClass::from_subtype(220), None);
        assert_eq!(RoadClass::from_subtype(2), None);
    }

    #[test]
    fn roads_roundtrip() {
        let mut r = Roads::default();
        r.push(RoadClass::Local, true, [[-73.9, 40.7], [-73.89, 40.71], [-73.88, 40.705]]);
        r.push(RoadClass::Motorway, false, [[-73.5, 40.2], [-73.4, 40.3]]);
        r.push(RoadClass::Lane, false, [[-73.5, 40.2]]); // too short: dropped
        assert_eq!(r.roads.len(), 2);
        let back = Roads::from_bytes(&r.to_bytes()).unwrap();
        assert_eq!(back.roads, r.roads);
        for (a, b) in back.pts.iter().zip(&r.pts) {
            assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
        }
        assert!(Roads::from_bytes(b"junk").is_none());
        let mut bytes = r.to_bytes();
        bytes.pop();
        assert!(Roads::from_bytes(&bytes).is_none());
    }

    /// A grid of town streets across a ZL16 texture in Manhattan.
    fn town(tex: &TextureId) -> Roads {
        let (n, w) = tile_to_latlon(tex.col as f64, tex.row as f64, tex.zl);
        let (s, e) = tile_to_latlon((tex.col + 16) as f64, (tex.row + 16) as f64, tex.zl);
        let mut r = Roads::default();
        let k = 60;
        for i in 0..=k {
            let f = i as f64 / k as f64;
            r.push(RoadClass::Local, true, (0..=20).map(|j| [w + (e - w) * j as f64 / 20.0, s + (n - s) * f]));
            r.push(RoadClass::Local, true, (0..=20).map(|j| [w + (e - w) * f, s + (n - s) * j as f64 / 20.0]));
        }
        r
    }

    #[test]
    fn empty_is_black() {
        let tex = TextureId::new(24624, 19296, 16, "XO");
        let out = render(&tex, &[]);
        assert_eq!(out, black_dds());
    }

    #[test]
    fn town_is_lit_warm_and_consistent_across_zooms() {
        let tex = TextureId::new(24624, 19296, 16, "XO");
        let roads = town(&tex);
        let out = render(&tex, &[&roads]);
        assert_eq!(out.len() as u64, LIT_FILE_LEN);
        let m0 = bc1::decode(&out[128..128 + lit_mip_len(0) as usize], 1024, 1024);
        let mean = |img: &Rgba, c: usize| {
            img.data.chunks(4).map(|p| p[c] as f64).sum::<f64>() / (img.width * img.height) as f64
        };
        let (r, g, b) = (mean(&m0, 0), mean(&m0, 1), mean(&m0, 2));
        assert!(r > 3.0, "town too dark: {r}");
        assert!(r >= g && g >= b, "not warm: {r} {g} {b}");
        // No texel greener or bluer than red.
        assert!(m0.data.chunks(4).all(|p| p[1] <= p[0] && p[2] <= p[0]));

        // A ZL17 texture inside it shows about the same light per area.
        let sub = TextureId::new(tex.row * 2 + 16, tex.col * 2 + 16, 17, "XO");
        let out17 = render(&sub, &[&roads]);
        let m17 = bc1::decode(&out17[128..128 + lit_mip_len(0) as usize], 1024, 1024);
        let lin = |img: &Rgba| {
            img.data
                .chunks(4)
                .map(|p| {
                    let v = p[0] as f64 / 255.0;
                    if v <= 0.04045 {
                        v / 12.92
                    } else {
                        ((v + 0.055) / 1.055).powf(2.4)
                    }
                })
                .sum::<f64>()
                / (img.width * img.height) as f64
        };
        let (l16, l17) = (lin(&m0), lin(&m17));
        assert!((l17 / l16 - 1.0).abs() < 0.35, "zl16 {l16} vs zl17 {l17}");
    }

    #[test]
    fn warm_fix_keeps_four_colour_mode() {
        // Greenish endpoints.
        let c0: u16 = (4 << 11) | (40 << 5) | 2;
        let c1: u16 = (2 << 11) | (10 << 5) | 1;
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&c0.to_le_bytes());
        b[2..4].copy_from_slice(&c1.to_le_bytes());
        b[4..8].copy_from_slice(&0x1b1b_1b1bu32.to_le_bytes());
        warm_bc1(&mut b);
        let n0 = u16::from_le_bytes([b[0], b[1]]);
        let n1 = u16::from_le_bytes([b[2], b[3]]);
        assert!(n0 > n1 || (n0 == n1 && b[4..8] == [0; 4]));
        let px = bc1::decode(&b, 4, 4);
        assert!(px.data.chunks(4).all(|p| p[1] <= p[0] && p[2] <= p[0] && p[3] == 255));
    }
}
