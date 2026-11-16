#!/usr/bin/env python3
"""Run only Ortho4XP's mesh step (Triangle4XP) for a tile whose step 1 files
(.poly/.node/.alt) exist, verbosely, to diagnose Triangle4XP failures. Step 2
does not read neighbouring tiles, so this is safe while other builds run.
Run with Ortho4XP's directory as the working dir:

    python o4_mesh_only.py 47 -123 [key=value ...]
"""

import os
import sys

# Output goes to a log file, which Windows Python writes as cp1252: an
# airport name with e.g. U+02BC (Navajo) crashed the build mid-print.
sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")
sys.path.append(os.path.join(".", "src"))

import O4_File_Names as FNAMES  # noqa: E402

sys.path.append(FNAMES.Provider_dir)
import O4_Imagery_Utils as IMG  # noqa: E402,F401
import O4_UI_Utils as UI  # noqa: E402
import O4_Mesh_Utils as MESH  # noqa: E402
import O4_Config_Utils as CFG  # noqa: E402


def main():
    lat, lon = int(sys.argv[1]), int(sys.argv[2])
    tile = CFG.Tile(lat, lon, "")
    for x in sys.argv[3:]:
        k, v = x.split("=", 1)
        old = getattr(tile, k)
        setattr(tile, k, type(old)(v) if old is not None else v)
        print(f"override {k}: {old} -> {getattr(tile, k)}", flush=True)
    UI.verbosity = 2
    MESH.build_mesh(tile)
    print("mesh exists:", os.path.exists(FNAMES.mesh_file(tile.build_dir, lat, lon)))


if __name__ == "__main__":
    main()
