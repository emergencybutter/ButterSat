# Building the mesh catalog

How the continental-US catalog was built on one Windows machine (WSL for
osmium, spinning disk `D:\ButterSat-build`), and how to extend or rebuild it.
First full run: 2026-11-15/16, about 15 hours wall clock, 970 tiles.

## Pipeline

```
Geofabrik regions ──download_extract.sh──▶ dated .osm.pbf (MD5-checked)
        │
        ▼ split_tiles.sh (WSL, osmium)
filter to Ortho4XP's tags → merge regions → dedupe (time-filter) → 1° tile cuts
        │                                                  osm/tiles/+LA-LON.osm.pbf
        ▼ overpass_lite.py (answers Ortho4XP's Overpass queries from the cuts)
build_tiles.py ──▶ o4_build.py (Ortho4XP steps 1–3, 6 parallel, never adjacent)
        │                                                  Ortho4XP/Tiles/zOrtho4XP_*
        ▼ classify_tiles.py (publish only tiles whose OSM data is complete)
publish_batch.py ──▶ xof-publish build (superset) ──▶ upload to R2 ──▶ verify
```

- **No Overpass server.** A full planet Overpass import is 100+ GB and days of
  work on a HDD. Ortho4XP only asks for a fixed set of tags per tile, so
  `split_tiles.sh` pre-filters Geofabrik extracts to those tags and cuts 1°
  tiles; `overpass_lite.py` (local HTTP, Ortho4XP server choice `LO`) answers
  each query from the tile's cut. +40-074 rebuilt this way matched the
  public-Overpass pilot (same input vertices, same DSF size) in 3 min vs 26.
- **Elevation:** USGS NED 1" (US, Canada, Mexico), ~30 m.
- **Throughput:** ~6 tiles in parallel; 1–6 min per land tile, 20 s for water.

## Running it

```sh
# 1. Data (Geofabrik caps bandwidth per address; dated snapshots never change)
tools/coverage/download_extract.sh D:/ButterSat-build/osm us-west 261115

# 2. Cut tiles (WSL); pass every region whose data the box touches
wsl bash tools/coverage/split_tiles.sh /mnt/d/ButterSat-build/osm 31 49 -125 -102 \
    .../us-northeast-latest.osm.pbf .../us-midwest-latest.osm.pbf \
    .../us-south-latest.osm.pbf .../us-west-261115.osm.pbf
#    then record each new cut's source regions in osm/tiles/sources.json

# 3. Build (resumable; state.json; per-tile logs in logs/tiles/)
python tools/coverage/build_tiles.py --box 24 49 -125 -67 --workers 6 [--retry-failed]

# 4. Publish complete tiles (dry run without --go)
python tools/coverage/classify_tiles.py > complete.json
python tools/coverage/publish_batch.py --only complete.json --go
```

`overpass_lite.py` must be running (WSL) during step 3. The publisher uploads to
Cloudflare R2 (`tiles.butr.cc`, credentials in `keys/r2.env`): objects first,
then the signature, then the manifest, so clients never see a partial catalog.
`--remirror` re-signs the manifest with the current mirror list.

## Rules that matter

- **Never build adjacent tiles at once.** Ortho4XP reads already-built
  neighbours' meshes to stitch edges; a half-written neighbour crashed the
  reader. The builder schedules around the 8 neighbours of every running tile,
  and only one builder may run at a time.
- **A tile is published only if its OSM data is complete.** A tile near a
  region border cut from one region's extract is missing roads and water on
  the other side. `classify_tiles.py` samples each tile against Geofabrik's
  region polygons and compares with the regions recorded for its cut.
- **Header-only cuts are open water.** The splitter deletes them only at the
  end; the builder ignores files under 512 bytes so it can run during a cut.
- **Edit `state.json` only when no builder runs**, or rely on the builder
  re-reading it before each write (it used to overwrite manual resets).

## Problems found, and fixes

| Symptom | Cause | Fix |
|---|---|---|
| Border tile: `cannot convert float NaN to integer` | NED 1/3" is US-only; NED marks voids as NaN, which Ortho4XP's nodata fill never matched | NED 1" everywhere; patched void fill (nearest valid) — `ortho4xp-patches/dem-void-fill.patch` |
| `could not broadcast input array from shape (0,)` | neighbour mesh read while being written | adjacency-aware scheduler |
| `Triangle4XP really couldn't make it` on any setting (Louisiana marsh, Puget Sound) | stack overflow (0xC00000FD) in Triangle's recursion, 2 MB stack | `editbin /STACK:268435456 Triangle4XP.exe` |
| Tile hung for hours at 100 % CPU in the DSF step (FL panhandle) | XP12 water re-cut piles ~40k points within 1e-7°; DSF quadtree looped forever | quadtree patch (`dsf-quadtree-overflow.patch`) + automatic retry with `water_simplification=1 min_area=0.01` |
| `failed to uncompress data` reading an extract | `-latest` download resumed across Geofabrik's daily update (spliced versions) | dated snapshots + MD5 (`download_extract.sh`) |
| `Way ID twice in input` | extracts from different days hold two versions of border objects | `osmium time-filter` after merge |
| `'charmap' codec can't encode` | airport name with U+02BC, log written as cp1252 | UTF-8 stdout in `o4_build.py` |

`o4_mesh_only.py LAT LON` reruns only the mesh step, verbosely (safe during
other builds), and prints Triangle4XP's command for running it by hand.

## Extending beyond the US

Same pipeline with other Geofabrik regions. Outside North America, replace
the DEM (`custom_dem`) with Copernicus GLO-30 and check Ortho4XP's per-region
defaults. Storage: CONUS is ~10 GB compressed; the whole catalog lives on R2.
