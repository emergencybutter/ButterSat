# Spike findings: S3 (Ortho4XP output), S5 (NYC pilot build), S6 (raster splice), S2 (performance)

Date: 2026-11-15. Machine: Windows 11, 16 threads. Ortho4XP at commit
`26ec00a` (2026-03-14), Python 3.12 venv, GDAL 3.13.3.

## S5 — NYC pilot mesh build

Config (`xporthofs-build/Ortho4XP/Ortho4XP.cfg`): `skip_downloads=True`,
`default_website=XO`, `default_zl=16`, `cover_airports_with_highres=True`,
`cover_zl=17`, `cover_extent=2`, `imprint_masks_to_dds=False`,
`custom_dem=NED 1/3"` (USGS, public domain), no Global Scenery path.

| Tile | Triangles | DSF (stripped) | zstd-19 | 7z (one sample) | Masks | Build time (of which compute) |
|---|---|---|---|---|---|---|
| +40-074 (JFK/LGA/EWR/TEB) | 2.39 M | 43.0 MB | 27.3 MB | 24.7 MB | 84 | 26 min (≈2.7 min) |
| +40-073 | 0.85 M | 15.8 MB | 10.0 MB | | 53 | 19 min (≈1.1 min) |
| +40-075 | 2.82 M | 56.8 MB | 34.9 MB | | 49 | 15 min (≈2.9 min) |
| +41-073 | 2.85 M | 71.0 MB | 42.6 MB | | 68 | 15 min (≈2.6 min) |
| +41-074 | 2.84 M | 63.9 MB | 40.1 MB | | 20 | 16 min (≈2.7 min) |
| +41-075 | 2.66 M | 35.3 MB | 29.1 MB | | 0 | 18 min (≈1.7 min) |
| +39-075 | 2.91 M | 66.5 MB | 36.9 MB | | 66 | 22 min (≈2.7 min) |
| +39-074 | — | — | — | | — | Ortho4XP crash: all-ocean tile (0 edges) |
| +39-073 | — | — | — | | — | all-ocean; stuck in OSM retries |

**Findings**

1. **Public Overpass is the bottleneck.** 85–90 % of wall time was waiting on
   OSM servers. `overpass-api.de`, `kumi.systems`, `private.coffee` timed out;
   `openstreetmap.fr` refused; only `maps.mail.ru` answered, with frequent
   rate-limit rejections. The pipeline must use a local Overpass loaded from
   Geofabrik extracts (Docker is available on the dev machine).
   Compute alone is ~1–3 min per dense tile.
2. **GDAL is mandatory** for any DEM other than Viewfinderpanoramas (whose
   terms are unclear). NED 1/3" via GDAL works out of the box for the US.
3. **All-ocean tiles** make Ortho4XP crash ("Number of edges: 0"). They need no
   mesh (X-Plane's default ocean); the build driver should skip tiles with no
   land and not treat this as an error.
4. **Hosting estimate.** Dense land tiles ≈ 30–40 MB zstd; mean of the 7 pilot
   tiles 31.6 MB. With ~19k land tiles worldwide and most being far less dense
   than NYC, a rough worldwide estimate is **250–500 GB** — within the
   design's "few hundred GB – 1 TB" range (≈ $4–8/month on R2).
5. **zstd vs 7z**: zstd-19 is ~10 % larger than 7z for DSFs but decompresses an
   order of magnitude faster. Since the daemon always decompresses (to splice),
   hosted meshes use zstd (design §6.1 updated).

## S3 — Ortho4XP output format (confirmed)

- Texture/terrain: `{row}_{col}_{code}{zl}` with row/col = slippy Y/X of the
  top-left 256-px tile, multiples of 16. ZL17 textures around airports.
- Terrain variants: plain (land), `_water_overlay`, `_sea_overlay` (with our
  config, which uses "XP11 + bathy" water). `_water`/`_sea` (non-overlay) occur
  with the XP12 water tech.
- `.ter` contents are fully reproducible from the name; `xof-core` generates
  byte-identical files (golden tests).
- Masks: `{row}_{col}_ZL{zl}.png` (no provider code), 1024 px at `mask_zl` 14.
- `water_transition.png`: a 64×128 greyscale vertical ramp; ButterSat generates
  its own equivalent.
- DDS: 4096², DXT1, 13 mips, 11,184,952 bytes; header flags identical to ours.

## S6 — Laminar raster splice

- X-Plane 12 Global Scenery DSFs carry 11 raster layers: `elevation`,
  `sea_level` (1201² int16), `soundscape` (1333² u8), `spr1`…`win2` (128² u8).
- Python prototype and then the Rust implementation (`xof_mesh::dsf::splice`)
  produce output **byte-identical** to Ortho4XP's own Global-Scenery-enabled
  build of `+40-074`. `strip_rasters` is the exact inverse.
- Cost: 0.58 s (release, incl. 7z decompression of the 10 MB Global Scenery
  file); cached per tile in the raster index.
- End to end through the daemon + WinFsp mount: published catalog → download →
  verify → decompress → splice → served DSF identical to the reference.

## S2 — Texture performance (release build)

- Full 4096² texture with imagery already cached (341 JPEG decodes + BC1 for
  all mips): **131 ms**.
- Same with live Esri fetches: ~2.6–3.1 s for 341 requests; no degraded bands
  at the default 1.2 s per-read budget (reads arrive in 128 KiB chunks, so each
  read only needs one band).
- Synthesized texture vs Ortho4XP/nvcompress texture from the same Esri
  imagery: **33.2 dB PSNR** at mip 0, mean colour within 0.5 levels — confirms
  geo-referencing, orientation and mip layout.

## Environment notes

- `x-plane_install_12.txt` on the dev machine lists a stale `C:\X-Plane 12/`
  before the real Steam install: entries must be validated.
- WinFsp developer files are not needed: `winfsp-sys` 0.12 bundles headers and
  the import library; `winfsp-sys` 0.2 (bindgen 0.68) generates broken
  bindings with LLVM 22.
- `intel_tex_2` needs the C++ runtime linked explicitly on Linux/macOS.
