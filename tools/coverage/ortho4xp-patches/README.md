# Ortho4XP changes for the ButterSat build (D:\ButterSat-build\Ortho4XP)

Base: Ortho4XP at commit 26ec00a, as used for the NYC pilot.

- `src/O4_OSM_Utils.py`: Overpass server `LO` = `http://127.0.0.1:8099/api/interpreter`
  (tools/coverage/overpass_lite.py); User-Agent `ButterSat-build/0.1`.
- `Ortho4XP.cfg`: `overpass_server_choice=LO`,
  `custom_dem=NED 1" (from USGS) - USA, Canada, Mexico` (from 2026-11-16; the
  first 80 Northeast tiles used NED 1/3"). NED 1/3" covers the US only, so
  border tiles had no elevation on the Canadian/Mexican side; at ~30 m NED 1"
  is still finer than the mesh's triangle spacing.
- `dem-void-fill.patch` (needs scipy in the build venv): USGS NED marks voids
  (Great Lakes, outside coverage) as NaN, which Ortho4XP's nodata handling
  never matched, crashing the DSF step ("cannot convert float NaN to
  integer"). Voids are filled from the nearest valid elevation.
- `tools/coverage/o4_build.py` is copied into the Ortho4XP folder: Ortho4XP's
  CLI with an optional per-tile DEM.
- `dsf-quadtree-overflow.patch`: `QuadTree.insert` (DSF point pools) looped
  forever when more than `bucket_size` mesh points share identical
  coordinates (seen on +30-085, Florida panhandle coast, after the XP12 water
  re-cut): splitting can't separate them and the key stops growing past the
  last quadkey digit. The deepest bucket now overflows instead.
- `build_tiles.py` retries Triangle4XP failures ("really couldn't make it",
  dense marsh such as +29-092) with `water_simplification`/`min_area` and
  then `curvature_tol=3`, via `o4_build.py key=value` overrides.
