#!/usr/bin/env python3
"""Build one Ortho4XP tile like `Ortho4XP.py lat lon`, with an optional DEM.

Ortho4XP's command line builds from the global Ortho4XP.cfg only (it never
reads the tile's own config), so per-tile settings such as the elevation
source need this wrapper. Run with Ortho4XP's directory as the working dir:

    python o4_build.py 44 -75 ['NED 1" (from USGS) - USA, Canada, Mexico'] [key=value ...]

key=value overrides any tile setting (e.g. water_simplification=1).
"""

import os
import sys

sys.path.append(os.path.join(".", "src"))

import O4_File_Names as FNAMES  # noqa: E402

sys.path.append(FNAMES.Provider_dir)
import O4_Imagery_Utils as IMG  # noqa: E402
import O4_Vector_Map as VMAP  # noqa: E402
import O4_Mesh_Utils as MESH  # noqa: E402
import O4_Mask_Utils as MASK  # noqa: E402
import O4_Tile_Utils as TILE  # noqa: E402
import O4_Config_Utils as CFG  # noqa: E402  (last: it patches other modules)


def main():
    lat, lon = int(sys.argv[1]), int(sys.argv[2])
    extra = sys.argv[3:]
    dem = next((x for x in extra if "=" not in x), None)
    overrides = dict(x.split("=", 1) for x in extra if "=" in x)
    for d in (FNAMES.OSM_dir, FNAMES.Mask_dir, FNAMES.Elevation_dir, FNAMES.Tile_dir, FNAMES.Tmp_dir):
        os.makedirs(d, exist_ok=True)
    IMG.initialize_extents_dict()
    IMG.initialize_color_filters_dict()
    IMG.initialize_providers_dict()
    IMG.initialize_combined_providers_dict()
    tile = CFG.Tile(lat, lon, "")
    if dem:
        tile.custom_dem = dem
        print("custom_dem override:", dem, flush=True)
    for k, v in overrides.items():
        old = getattr(tile, k)
        typ = type(old) if old is not None else str
        val = (v.lower() in ("1", "true", "yes")) if typ is bool else typ(v)
        setattr(tile, k, val)
        print(f"override {k}: {old} -> {val}", flush=True)
    try:
        VMAP.build_poly_file(tile)
        MESH.build_mesh(tile)
        MASK.build_masks(tile)
        TILE.build_tile(tile)
        print("Bon vol!")
    except Exception as e:  # noqa: BLE001 - same contract as Ortho4XP.py
        print("Crash!", e)


if __name__ == "__main__":
    main()
