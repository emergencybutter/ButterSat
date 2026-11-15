//! Imagery providers and the HTTP tile fetcher.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::raster::{Rgba, Tile};
use crate::tile::{TileCoord, TILE_PX};

/// Static description of an imagery source.
pub trait Provider: Send + Sync {
    /// Stable id used in config and cache keys (`usgs`, `ign`, `swisstopo`, `pdok`, `pnoa`, `basemap_at`, `esri`, `bing`).
    fn id(&self) -> &str;
    /// Human-readable name.
    fn name(&self) -> &str;
    /// Required copyright / source attribution shown in the UI and pack README.
    fn attribution(&self) -> &str {
        self.name()
    }
    /// Short licence summary.
    fn license(&self) -> &str {
        "Open data"
    }
    /// Whether this provider is openly licensed (true) or requires explicit
    /// commercial-terms opt-in (false: Esri, Bing).
    fn is_open(&self) -> bool {
        true
    }
    /// Bounding polygons (`(lat, lon)` vertices) where this regional provider
    /// has ortho coverage. Empty means global coverage.
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        &[]
    }
    /// Does this provider cover `(lat, lon)`?
    fn covers(&self, lat: f64, lon: f64) -> bool {
        let polys = self.coverage_polygons();
        polys.is_empty() || polys.iter().any(|poly| point_in_polygon(lat, lon, poly))
    }
    /// Provider codes found in Ortho4XP file names that map to this provider.
    fn filename_codes(&self) -> &[&str];
    fn max_zoom(&self) -> u8;
    /// URL for a tile.
    fn tile_url(&self, t: TileCoord) -> String;
    /// Upper bound on concurrent requests (the fetcher's limit moves below
    /// it: higher while X-Plane loads, lower in flight). Requests are
    /// latency-bound (~130 ms each for uncached Esri tiles), so throughput
    /// scales with concurrency: measured 125/194/330 tiles/s at 16/32/64.
    fn max_concurrency(&self) -> usize {
        64
    }
    /// Is this response a "no imagery here" placeholder?
    fn is_no_data(&self, status: u16, headers: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        let _ = headers;
        status == 404 || status == 204 || body.is_empty()
    }
    /// For composite/coverage-aware providers: the concrete provider to use for tile `t`.
    fn resolve(&self, _t: TileCoord) -> Option<Arc<dyn Provider>> {
        None
    }
    /// Secondary fallback provider if the primary regional provider has no data at `t`.
    fn fallback_for(&self, _t: TileCoord) -> Option<Arc<dyn Provider>> {
        None
    }
    /// Whether network fetches are permitted for tile `t` (enforces opt-in on Esri/Bing).
    fn can_fetch(&self, _t: TileCoord) -> bool {
        true
    }
}

/// Ray-casting point-in-polygon test for `(lat, lon)`.
pub fn point_in_polygon(lat: f64, lon: f64, poly: &[(f64, f64)]) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (yi, xi) = poly[i];
        let (yj, xj) = poly[j];
        if ((yi > lat) != (yj > lat)) && (lon < (xj - xi) * (lat - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

const US_CONUS_POLY: &[(f64, f64)] = &[
    (48.50, -124.85),
    (49.00, -123.00),
    (49.00, -95.15),
    (48.00, -89.50),
    (46.50, -84.35),
    (43.00, -82.40),
    (42.25, -83.10),
    (42.00, -80.50),
    (43.30, -79.05),
    (45.00, -74.70),
    (45.02, -71.50),
    (47.46, -69.20),
    (44.80, -66.90),
    (41.20, -69.90),
    (35.20, -75.50),
    (25.10, -80.00),
    (24.50, -82.00),
    (29.00, -83.00),
    (28.90, -89.40),
    (25.83, -97.15),
    (29.30, -101.00),
    (31.78, -106.50),
    (31.33, -111.00),
    (32.53, -117.15),
    (34.45, -120.50),
    (40.44, -124.45),
];
const US_ALASKA_POLY: &[(f64, f64)] =
    &[(51.20, -179.90), (71.50, -179.90), (71.50, -141.00), (60.00, -141.00), (54.65, -130.00), (51.20, -130.00)];
const US_HAWAII_POLY: &[(f64, f64)] = &[(18.80, -160.50), (22.40, -160.50), (22.40, -154.60), (18.80, -154.60)];
const US_PR_POLY: &[(f64, f64)] = &[(17.60, -67.40), (18.60, -67.40), (18.60, -64.50), (17.60, -64.50)];
const US_POLYS: &[&[(f64, f64)]] = &[US_CONUS_POLY, US_ALASKA_POLY, US_HAWAII_POLY, US_PR_POLY];

/// USGS The National Map `USGSImageryOnly` (US public domain, 1m NAIP + HRO).
pub struct Usgs;

impl Provider for Usgs {
    fn id(&self) -> &str {
        "usgs"
    }
    fn name(&self) -> &str {
        "USGS The National Map (US)"
    }
    fn attribution(&self) -> &str {
        "USGS The National Map (U.S. Geological Survey, public domain)"
    }
    fn license(&self) -> &str {
        "Public domain (17 U.S.C. § 105)"
    }
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        US_POLYS
    }
    fn filename_codes(&self) -> &[&str] {
        &["USGS", "NAIP"]
    }
    fn max_zoom(&self) -> u8 {
        16
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/tile/{}/{}/{}",
            t.z, t.y, t.x
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || status == 204 || body.len() < 1500
    }
}

const FR_MAINLAND_POLY: &[(f64, f64)] = &[
    (43.35, -1.80),
    (46.00, -1.50),
    (48.40, -4.85),
    (49.75, -1.95),
    (51.09, 2.55),
    (50.10, 4.25),
    (49.50, 5.80),
    (49.00, 8.23),
    (47.58, 7.60),
    (46.15, 5.95),
    (45.80, 6.85),
    (44.95, 6.65),
    (43.75, 7.55),
    (43.00, 6.40),
    (43.30, 3.50),
    (42.43, 3.18),
    (42.65, 0.50),
];
const FR_CORSICA_POLY: &[(f64, f64)] = &[(41.33, 8.50), (43.05, 8.50), (43.05, 9.60), (41.33, 9.60)];
const FR_POLYS: &[&[(f64, f64)]] = &[FR_MAINLAND_POLY, FR_CORSICA_POLY];

/// France IGN Géoplateforme BD ORTHO 20 cm (Etalab Open Licence 2.0).
pub struct Ign;

impl Provider for Ign {
    fn id(&self) -> &str {
        "ign"
    }
    fn name(&self) -> &str {
        "IGN Géoplateforme Ortho (FR)"
    }
    fn attribution(&self) -> &str {
        "© IGN — Géoplateforme BD ORTHO (Licence Ouverte / Open Licence 2.0)"
    }
    fn license(&self) -> &str {
        "Etalab Open Licence 2.0"
    }
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        FR_POLYS
    }
    fn filename_codes(&self) -> &[&str] {
        &["IGN", "FR"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://data.geopf.fr/wmts?SERVICE=WMTS&VERSION=1.0.0&REQUEST=GetTile&LAYER=HR.ORTHOIMAGERY.ORTHOPHOTOS&STYLE=normal&FORMAT=image/jpeg&TILEMATRIXSET=PM&TILEMATRIX={}&TILEROW={}&TILECOL={}",
            t.z, t.y, t.x
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || status == 400 || status == 204 || body.len() < 1500
    }
}

const CH_POLY: &[(f64, f64)] = &[
    (46.13, 5.95),
    (46.45, 6.05),
    (47.05, 6.45),
    (47.50, 6.90),
    (47.60, 7.55),
    (47.81, 8.55),
    (47.55, 9.55),
    (47.05, 9.50),
    (47.00, 10.49),
    (46.20, 10.15),
    (45.82, 9.02),
    (46.10, 8.00),
    (45.85, 7.05),
    (46.13, 6.80),
];
const CH_POLYS: &[&[(f64, f64)]] = &[CH_POLY];

/// Switzerland Federal Office of Topography swisstopo SWISSIMAGE (Open Government Data).
pub struct Swisstopo;

impl Provider for Swisstopo {
    fn id(&self) -> &str {
        "swisstopo"
    }
    fn name(&self) -> &str {
        "swisstopo SWISSIMAGE (CH)"
    }
    fn attribution(&self) -> &str {
        "© swisstopo (Swiss Open Government Data)"
    }
    fn license(&self) -> &str {
        "Swiss Open Government Data (OGD)"
    }
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        CH_POLYS
    }
    fn filename_codes(&self) -> &[&str] {
        &["CH", "SWISS"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://wmts.geo.admin.ch/1.0.0/ch.swisstopo.swissimage/default/current/3857/{}/{}/{}.jpeg",
            t.z, t.x, t.y
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || status == 204 || body.len() < 1500
    }
}

const NL_POLY: &[(f64, f64)] = &[
    (51.25, 3.35),
    (52.00, 3.95),
    (53.00, 4.60),
    (53.55, 6.10),
    (53.45, 7.23),
    (52.60, 7.08),
    (52.30, 7.05),
    (51.85, 6.15),
    (50.75, 6.05),
    (50.75, 5.65),
    (51.35, 5.10),
    (51.40, 4.20),
];
const NL_POLYS: &[&[(f64, f64)]] = &[NL_POLY];

/// Netherlands PDOK Luchtfoto Actueel Ortho HR (CC BY 4.0).
pub struct Pdok;

impl Provider for Pdok {
    fn id(&self) -> &str {
        "pdok"
    }
    fn name(&self) -> &str {
        "PDOK Luchtfoto (NL)"
    }
    fn attribution(&self) -> &str {
        "© Beeldmateriaal Nederland / PDOK (CC BY 4.0)"
    }
    fn license(&self) -> &str {
        "CC BY 4.0"
    }
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        NL_POLYS
    }
    fn filename_codes(&self) -> &[&str] {
        &["PDOK", "NL"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://service.pdok.nl/hwh/luchtfotorgb/wmts/v1_0/Actueel_orthoHR/EPSG:3857/{:02}/{}/{}.jpeg",
            t.z, t.x, t.y
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || status == 204 || body.len() < 1500
    }
}

const ES_MAINLAND_POLY: &[(f64, f64)] = &[
    (36.00, -5.65),
    (36.50, -6.35),
    (37.15, -7.40),
    (38.80, -7.00),
    (41.90, -6.50),
    (41.85, -8.90),
    (43.80, -9.35),
    (43.35, -1.78),
    (42.42, 3.35),
    (38.70, 4.40),
    (36.60, -2.00),
];
const ES_CANARY_POLY: &[(f64, f64)] = &[(27.60, -18.25), (29.50, -18.25), (29.50, -13.30), (27.60, -13.30)];
const ES_POLYS: &[&[(f64, f64)]] = &[ES_MAINLAND_POLY, ES_CANARY_POLY];

/// Spain IGN Plan Nacional de Ortofotografía Aérea (PNOA Máxima Actualidad, CC BY 4.0).
pub struct Pnoa;

impl Provider for Pnoa {
    fn id(&self) -> &str {
        "pnoa"
    }
    fn name(&self) -> &str {
        "IGN PNOA (ES)"
    }
    fn attribution(&self) -> &str {
        "© Instituto Geográfico Nacional — PNOA (CC BY 4.0)"
    }
    fn license(&self) -> &str {
        "CC BY 4.0"
    }
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        ES_POLYS
    }
    fn filename_codes(&self) -> &[&str] {
        &["PNOA", "ES"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://www.ign.es/wmts/pnoa-ma?SERVICE=WMTS&VERSION=1.0.0&REQUEST=GetTile&LAYER=OI.OrthoimageCoverage&STYLE=default&FORMAT=image/jpeg&TILEMATRIXSET=GoogleMapsCompatible&TILEMATRIX={}&TILEROW={}&TILECOL={}",
            t.z, t.y, t.x
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || status == 400 || status == 204 || body.len() < 1500
    }
}

const AT_POLY: &[(f64, f64)] = &[
    (47.55, 9.55),
    (47.55, 10.65),
    (47.70, 12.15),
    (48.10, 12.80),
    (48.60, 13.75),
    (49.02, 15.05),
    (48.75, 16.95),
    (48.00, 17.16),
    (46.85, 16.00),
    (46.50, 14.50),
    (46.60, 12.50),
    (46.85, 11.00),
    (46.85, 9.90),
    (47.25, 9.53),
];
const AT_POLYS: &[&[(f64, f64)]] = &[AT_POLY];

/// Austria basemap.at Orthofoto 30 cm (CC BY 4.0 / OGD Austria).
pub struct BasemapAt;

impl Provider for BasemapAt {
    fn id(&self) -> &str {
        "basemap_at"
    }
    fn name(&self) -> &str {
        "basemap.at Orthofoto (AT)"
    }
    fn attribution(&self) -> &str {
        "Data source: basemap.at (CC BY 4.0)"
    }
    fn license(&self) -> &str {
        "CC BY 4.0 (OGD Austria)"
    }
    fn coverage_polygons(&self) -> &[&[(f64, f64)]] {
        AT_POLYS
    }
    fn filename_codes(&self) -> &[&str] {
        &["AT", "BMAP"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!("https://mapsneu.wien.gv.at/basemap/bmaporthofoto30cm/normal/google3857/{}/{}/{}.jpeg", t.z, t.y, t.x)
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || status == 204 || body.len() < 1500
    }
}

/// Esri World Imagery (XYZ, commercial opt-in).
pub struct Esri;

impl Provider for Esri {
    fn id(&self) -> &str {
        "esri"
    }
    fn name(&self) -> &str {
        "Esri World Imagery"
    }
    fn attribution(&self) -> &str {
        "Imagery © Esri, Maxar, Earthstar Geographics, and the GIS User Community"
    }
    fn license(&self) -> &str {
        "Proprietary (Esri Terms of Use; opt-in required)"
    }
    fn is_open(&self) -> bool {
        false
    }
    fn filename_codes(&self) -> &[&str] {
        &["ARC", "Arc", "XO"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        format!(
            "https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{}/{}/{}",
            t.z, t.y, t.x
        )
    }
    fn is_no_data(&self, status: u16, _h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        // Esri answers missing tiles with 404 or a tiny "Map data not yet
        // available" image.
        status == 404 || body.len() < 1500
    }
}

/// Bing aerial (quadkey, commercial opt-in).
pub struct Bing;

impl Provider for Bing {
    fn id(&self) -> &str {
        "bing"
    }
    fn name(&self) -> &str {
        "Bing Maps Aerial"
    }
    fn attribution(&self) -> &str {
        "Imagery © Microsoft Bing Maps"
    }
    fn license(&self) -> &str {
        "Proprietary (Microsoft Bing Maps Terms of Use; opt-in required)"
    }
    fn is_open(&self) -> bool {
        false
    }
    fn filename_codes(&self) -> &[&str] {
        &["BI"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        let server = (t.x + t.y) % 4;
        format!("https://ecn.t{server}.tiles.virtualearth.net/tiles/a{}.jpeg?g=136", t.quadkey())
    }
    fn is_no_data(&self, status: u16, h: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404 || body.is_empty() || h.get("X-VE-Tile-Info").is_some_and(|v| v.as_bytes() == b"no-tile")
    }
}

/// Open regional providers in priority order (smaller/more specific countries first).
pub fn open_providers() -> Vec<Arc<dyn Provider>> {
    vec![Arc::new(Swisstopo), Arc::new(Pdok), Arc::new(BasemapAt), Arc::new(Ign), Arc::new(Pnoa), Arc::new(Usgs)]
}

/// Select the open regional provider whose coverage polygon contains `(lat, lon)`.
pub fn open_for_latlon(lat: f64, lon: f64) -> Option<Arc<dyn Provider>> {
    open_providers().into_iter().find(|p| p.covers(lat, lon))
}

/// Select the open regional provider for tile `t`'s area (`cache::area_of(t)`),
/// so all tiles and mip levels of a 4096² texture block share one provider.
pub fn open_for_tile(t: TileCoord) -> Option<Arc<dyn Provider>> {
    let (lat, lon) = crate::cache::area_of(t).center_latlon();
    open_for_latlon(lat, lon)
}

/// Coverage-aware provider selector: prefers regional open imagery where available,
/// falling back to the user's chosen global provider (`fallback`), and enforcing
/// `notice_accepted` before fetching from non-open providers (Esri/Bing).
pub struct CoverageProvider {
    fallback: Option<Arc<dyn Provider>>,
    prefer_open: bool,
    notice_accepted: bool,
    label: String,
    attrib: String,
}

impl CoverageProvider {
    pub fn new(fallback: Option<Arc<dyn Provider>>, prefer_open: bool, notice_accepted: bool) -> Self {
        let open_list = "USGS (US), IGN (FR), swisstopo (CH), PDOK (NL), IGN PNOA (ES), basemap.at (AT)";
        let (label, attrib) = match (&fallback, prefer_open, notice_accepted) {
            (None, _, _) => ("Open imagery (US, FR, CH, NL, ES, AT)".to_string(), format!("Open imagery: {open_list}")),
            (Some(fb), true, true) => (
                format!("Open imagery + {}", fb.name()),
                format!("Open imagery ({open_list}); fallback: {}", fb.attribution()),
            ),
            (Some(fb), true, false) if !fb.is_open() => {
                (format!("Open imagery ({} opt-in pending)", fb.name()), format!("Open imagery ({open_list})"))
            }
            (Some(fb), true, _) => {
                (format!("Open imagery + {}", fb.name()), format!("Open imagery ({open_list}); {}", fb.attribution()))
            }
            (Some(fb), false, true) => (fb.name().to_string(), fb.attribution().to_string()),
            (Some(fb), false, false) if !fb.is_open() => {
                (format!("{} (disabled — accept terms in Settings)", fb.name()), fb.attribution().to_string())
            }
            (Some(fb), false, false) => (fb.name().to_string(), fb.attribution().to_string()),
        };
        Self { fallback, prefer_open, notice_accepted, label, attrib }
    }

    fn select(&self, t: TileCoord) -> Option<Arc<dyn Provider>> {
        if self.prefer_open {
            if let Some(open) = open_for_tile(t) {
                return Some(open);
            }
        }
        self.fallback.clone()
    }
}

impl Provider for CoverageProvider {
    fn id(&self) -> &str {
        match (&self.fallback, self.prefer_open) {
            (Some(fb), false) => fb.id(),
            _ => "open",
        }
    }
    fn name(&self) -> &str {
        &self.label
    }
    fn attribution(&self) -> &str {
        &self.attrib
    }
    fn license(&self) -> &str {
        match &self.fallback {
            Some(fb) if !self.prefer_open => fb.license(),
            _ => "Open data (Public domain / CC BY 4.0 / Etalab 2.0 / OGD)",
        }
    }
    fn is_open(&self) -> bool {
        self.fallback.as_ref().is_none_or(|fb| fb.is_open())
    }
    fn filename_codes(&self) -> &[&str] {
        &["XO", "USGS", "NAIP", "IGN", "CH", "PDOK", "PNOA", "AT", "ARC", "BI"]
    }
    fn max_zoom(&self) -> u8 {
        19
    }
    fn tile_url(&self, t: TileCoord) -> String {
        self.select(t).map(|p| p.tile_url(t)).unwrap_or_default()
    }
    fn is_no_data(&self, status: u16, headers: &reqwest::header::HeaderMap, body: &[u8]) -> bool {
        status == 404
            || status == 204
            || body.len() < 1500
            || headers.get("X-VE-Tile-Info").is_some_and(|v| v.as_bytes() == b"no-tile")
    }
    fn resolve(&self, t: TileCoord) -> Option<Arc<dyn Provider>> {
        self.select(t)
    }
    fn fallback_for(&self, t: TileCoord) -> Option<Arc<dyn Provider>> {
        let fb = self.fallback.as_ref()?;
        if !fb.is_open() && !self.notice_accepted {
            return None;
        }
        let primary = self.select(t)?;
        (primary.id() != fb.id()).then(|| fb.clone())
    }
    fn can_fetch(&self, t: TileCoord) -> bool {
        match self.select(t) {
            Some(p) => p.is_open() || self.notice_accepted,
            None => false,
        }
    }
}

/// Registry of built-in providers.
pub fn builtin(id: &str) -> Option<Arc<dyn Provider>> {
    match id {
        "usgs" | "naip" => Some(Arc::new(Usgs)),
        "ign" | "fr" => Some(Arc::new(Ign)),
        "swisstopo" | "ch" => Some(Arc::new(Swisstopo)),
        "pdok" | "nl" => Some(Arc::new(Pdok)),
        "pnoa" | "es" => Some(Arc::new(Pnoa)),
        "basemap_at" | "at" => Some(Arc::new(BasemapAt)),
        "esri" => Some(Arc::new(Esri)),
        "bing" => Some(Arc::new(Bing)),
        "open" | "auto" => Some(Arc::new(CoverageProvider::new(None, true, false))),
        _ => None,
    }
}

/// Build the configured provider with coverage-aware open selection and commercial opt-in gating.
pub fn configured(id: &str, prefer_open: bool, notice_accepted: bool) -> Option<Arc<dyn Provider>> {
    if id == "open" || id == "auto" {
        return Some(Arc::new(CoverageProvider::new(None, true, notice_accepted)));
    }
    let fb = builtin(id)?;
    Some(Arc::new(CoverageProvider::new(Some(fb), prefer_open, notice_accepted)))
}

pub fn builtin_ids() -> &'static [&'static str] {
    &["open", "usgs", "ign", "swisstopo", "pdok", "pnoa", "basemap_at", "esri", "bing"]
}

/// Result of fetching one tile.
#[derive(Clone, Debug)]
pub enum Fetched {
    Data(Bytes),
    NoData,
}

/// Request priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Priority {
    /// Prefetch / background upgrade.
    Background,
    /// X-Plane is waiting.
    Live,
}

#[derive(Debug, Default)]
pub struct FetchStats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    pub no_data: AtomicU64,
    pub errors: AtomicU64,
}

/// HTTP fetcher with per-provider concurrency, a reserved share for live
/// requests, retries with backoff, and in-flight de-duplication.
pub struct Fetcher {
    client: reqwest::Client,
    provider: Arc<dyn Provider>,
    /// All requests take a permit here.
    total: Arc<Semaphore>,
    /// Background requests also take one here, so they can never use all
    /// permits and starve live reads.
    background: Arc<Semaphore>,
    /// Semaphores are sized for `max`; permits parked here lower the
    /// effective limit (see [`Fetcher::set_limit`]).
    max: usize,
    limit: std::sync::atomic::AtomicUsize,
    parked: Mutex<(Vec<tokio::sync::OwnedSemaphorePermit>, Vec<tokio::sync::OwnedSemaphorePermit>)>,
    inflight: Mutex<
        HashMap<TileCoord, futures::future::Shared<futures::future::BoxFuture<'static, Result<Fetched, String>>>>,
    >,
    pub stats: Arc<FetchStats>,
    retries: u32,
}

impl Fetcher {
    pub fn new(provider: Arc<dyn Provider>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("ButterSat/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(32)
            .build()?;
        let n = provider.max_concurrency().max(2);
        let f = Self {
            client,
            total: Arc::new(Semaphore::new(n)),
            background: Arc::new(Semaphore::new(Self::bg_share(n))),
            max: n,
            limit: std::sync::atomic::AtomicUsize::new(n),
            parked: Mutex::new((Vec::new(), Vec::new())),
            provider,
            inflight: Mutex::new(HashMap::new()),
            stats: Arc::new(FetchStats::default()),
            retries: 3,
        };
        f.set_limit(n / 2);
        Ok(f)
    }

    fn bg_share(n: usize) -> usize {
        (n * 3 / 4).max(1)
    }

    /// Change the number of concurrent requests (clamped to 2..=max). Lowering
    /// parks free permits; permits still in use are parked by later calls
    /// (the daemon re-applies the limit several times a second).
    pub fn set_limit(&self, n: usize) {
        let n = n.clamp(2, self.max);
        self.limit.store(n, Ordering::Relaxed);
        let mut parked = self.parked.lock();
        let (pt, pb) = &mut *parked;
        Self::park(&self.total, pt, self.max - n);
        Self::park(&self.background, pb, Self::bg_share(self.max) - Self::bg_share(n));
    }

    fn park(sem: &Arc<Semaphore>, parked: &mut Vec<tokio::sync::OwnedSemaphorePermit>, target: usize) {
        parked.truncate(target);
        while parked.len() < target {
            match sem.clone().try_acquire_owned() {
                Ok(p) => parked.push(p),
                Err(_) => break,
            }
        }
    }

    /// Current limit and how many permits are actually parked (diagnostics).
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    /// Fetch a tile; concurrent calls for the same tile share one request.
    pub async fn fetch(&self, t: TileCoord, prio: Priority) -> Result<Fetched, String> {
        use futures::FutureExt;
        let fut = {
            let mut map = self.inflight.lock();
            if let Some(f) = map.get(&t) {
                f.clone()
            } else {
                let f = Self::fetch_uncoalesced(
                    self.client.clone(),
                    self.provider.clone(),
                    self.total.clone(),
                    self.background.clone(),
                    self.stats.clone(),
                    self.retries,
                    t,
                    prio,
                )
                .boxed()
                .shared();
                map.insert(t, f.clone());
                f
            }
        };
        let res = fut.await;
        self.inflight.lock().remove(&t);
        res
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_uncoalesced(
        client: reqwest::Client,
        provider: Arc<dyn Provider>,
        total: Arc<Semaphore>,
        background: Arc<Semaphore>,
        stats: Arc<FetchStats>,
        retries: u32,
        t: TileCoord,
        prio: Priority,
    ) -> Result<Fetched, String> {
        if !t.is_valid() {
            return Ok(Fetched::NoData);
        }
        if !provider.can_fetch(t) {
            return Err("provider opt-in notice not accepted".to_string());
        }
        let eff = provider.resolve(t).unwrap_or_else(|| provider.clone());
        if t.z > eff.max_zoom() {
            return Ok(Fetched::NoData);
        }
        let _bg = match prio {
            Priority::Background => Some(background.acquire_owned().await.map_err(|e| e.to_string())?),
            Priority::Live => None,
        };
        let _permit = total.acquire_owned().await.map_err(|e| e.to_string())?;
        let res = Self::fetch_from_provider(&client, &eff, &stats, retries, t).await;
        if matches!(res, Ok(Fetched::NoData)) {
            if let Some(fb) = provider.fallback_for(t) {
                if t.z <= fb.max_zoom() {
                    return Self::fetch_from_provider(&client, &fb, &stats, retries, t).await;
                }
            }
        }
        res
    }

    async fn fetch_from_provider(
        client: &reqwest::Client,
        eff: &Arc<dyn Provider>,
        stats: &Arc<FetchStats>,
        retries: u32,
        t: TileCoord,
    ) -> Result<Fetched, String> {
        let url = eff.tile_url(t);
        if url.is_empty() {
            return Ok(Fetched::NoData);
        }
        let mut last_err = String::new();
        for attempt in 0..=retries {
            if attempt > 0 {
                let backoff = 250u64 << attempt.min(5);
                let jitter = rand::random::<u64>() % 200;
                tokio::time::sleep(Duration::from_millis(backoff + jitter)).await;
            }
            stats.requests.fetch_add(1, Ordering::Relaxed);
            match client.get(&url).send().await {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers().clone();
                    if status == 429 || status >= 500 {
                        last_err = format!("HTTP {status}");
                        continue;
                    }
                    match resp.bytes().await {
                        Ok(body) => {
                            stats.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                            if eff.is_no_data(status, &headers, &body) {
                                stats.no_data.fetch_add(1, Ordering::Relaxed);
                                return Ok(Fetched::NoData);
                            }
                            if !(200..300).contains(&status) {
                                last_err = format!("HTTP {status}");
                                continue;
                            }
                            return Ok(Fetched::Data(body));
                        }
                        Err(e) => last_err = e.to_string(),
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        stats.errors.fetch_add(1, Ordering::Relaxed);
        Err(format!("{t}: {last_err}"))
    }
}

/// Decode an imagery tile (JPEG or PNG) to a 256×256 RGBA raster.
pub fn decode_tile(bytes: &[u8]) -> anyhow::Result<Tile> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (w, h) = img.dimensions();
    let rgba = Rgba { width: w, height: h, data: img.into_raw() };
    if (w, h) == (TILE_PX, TILE_PX) {
        Ok(rgba)
    } else {
        Ok(rgba.crop_scale(0.0, 0.0, w.min(h) as f32, TILE_PX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limit_parks_and_releases_permits() {
        let f = Fetcher::new(Arc::new(Esri)).unwrap();
        assert_eq!(f.max, 64);
        assert_eq!(f.limit(), 32, "starts at half the maximum");
        assert_eq!(f.total.available_permits(), 32);
        assert_eq!(f.background.available_permits(), 48 - 24);
        f.set_limit(64);
        assert_eq!(f.total.available_permits(), 64);
        assert_eq!(f.background.available_permits(), 48);
        // Permits in use can't be parked yet; a later call catches up.
        let held: Vec<_> = (0..60).map(|_| f.total.clone().try_acquire_owned().unwrap()).collect();
        f.set_limit(16);
        assert_eq!(f.total.available_permits(), 0);
        drop(held);
        f.set_limit(16);
        assert_eq!(f.total.available_permits(), 16);
        f.set_limit(1);
        assert_eq!(f.limit(), 2, "clamped");
    }

    #[test]
    fn urls() {
        let t = TileCoord::new(16, 19344, 24560);
        assert!(Esri.tile_url(t).ends_with("/tile/16/24560/19344"));
        assert!(Usgs.tile_url(t).ends_with("/USGSImageryOnly/MapServer/tile/16/24560/19344"));
        assert!(Ign.tile_url(t).contains("LAYER=HR.ORTHOIMAGERY.ORTHOPHOTOS&STYLE=normal&FORMAT=image/jpeg&TILEMATRIXSET=PM&TILEMATRIX=16&TILEROW=24560&TILECOL=19344"));
        assert!(Swisstopo.tile_url(t).ends_with("/3857/16/19344/24560.jpeg"));
        assert!(Pdok.tile_url(t).ends_with("/EPSG:3857/16/19344/24560.jpeg"));
        assert!(Pnoa.tile_url(t).contains("LAYER=OI.OrthoimageCoverage&STYLE=default&FORMAT=image/jpeg&TILEMATRIXSET=GoogleMapsCompatible&TILEMATRIX=16&TILEROW=24560&TILECOL=19344"));
        assert!(BasemapAt.tile_url(t).ends_with("/google3857/16/24560/19344.jpeg"));
        let b = Bing.tile_url(TileCoord::new(3, 3, 5));
        assert!(b.contains("/tiles/a213.jpeg"), "{b}");
    }

    #[test]
    fn coverage_selection_and_opt_in() {
        // NYC (JFK): 40.6413, -73.7781 -> USGS
        assert_eq!(open_for_latlon(40.6413, -73.7781).unwrap().id(), "usgs");
        // Paris (CDG): 49.0097, 2.5479 -> IGN
        assert_eq!(open_for_latlon(49.0097, 2.5479).unwrap().id(), "ign");
        // Zurich (ZRH): 47.4647, 8.5492 -> swisstopo
        assert_eq!(open_for_latlon(47.4647, 8.5492).unwrap().id(), "swisstopo");
        // Amsterdam (AMS): 52.3105, 4.7683 -> PDOK
        assert_eq!(open_for_latlon(52.3105, 4.7683).unwrap().id(), "pdok");
        // Madrid (MAD): 40.4983, -3.5676 -> PNOA
        assert_eq!(open_for_latlon(40.4983, -3.5676).unwrap().id(), "pnoa");
        // Vienna (VIE): 48.1103, 16.5697 -> basemap.at
        assert_eq!(open_for_latlon(48.1103, 16.5697).unwrap().id(), "basemap_at");
        // Tokyo (HND): 35.5494, 139.7798 -> None (no open regional provider)
        assert!(open_for_latlon(35.5494, 139.7798).is_none());

        let nyc_tile = TileCoord::from_latlon(40.6413, -73.7781, 16);
        let tokyo_tile = TileCoord::from_latlon(35.5494, 139.7798, 16);

        // Configured with Esri fallback, prefer_open = true, notice_accepted = false:
        let p_unaccepted = configured("esri", true, false).unwrap();
        assert_eq!(p_unaccepted.resolve(nyc_tile).unwrap().id(), "usgs");
        assert!(p_unaccepted.can_fetch(nyc_tile), "open provider works without opt-in");
        assert_eq!(p_unaccepted.resolve(tokyo_tile).unwrap().id(), "esri");
        assert!(!p_unaccepted.can_fetch(tokyo_tile), "Esri blocked without opt-in");

        // Once notice_accepted = true, Esri is allowed for Tokyo while NYC still uses USGS:
        let p_accepted = configured("esri", true, true).unwrap();
        assert_eq!(p_accepted.resolve(nyc_tile).unwrap().id(), "usgs");
        assert!(p_accepted.can_fetch(nyc_tile));
        assert_eq!(p_accepted.resolve(tokyo_tile).unwrap().id(), "esri");
        assert!(p_accepted.can_fetch(tokyo_tile));
    }

    #[test]
    fn decode_png() {
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(256, 256, image::Rgba([1, 2, 3, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let t = decode_tile(&buf).unwrap();
        assert_eq!(t.px(10, 10), [1, 2, 3, 255]);
    }
}
