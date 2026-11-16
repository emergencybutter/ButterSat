#!/usr/bin/env python3
"""Which built tiles have complete OSM data?

A tile is cut from one or more Geofabrik extracts. If the tile also overlaps
a region whose extract wasn't in that cut (e.g. a Pennsylvania tile reaching
into Maryland, cut before us-south arrived), roads and water are missing
there. We sample points over each tile, find the Geofabrik regions they fall
in (region .poly files), and call the tile complete when all of them were in
its cut. Points outside every region (ocean, Canada, Mexico) don't count.

    python classify_tiles.py --cut us-northeast:38,47,-81,-67 \\
        --cut us-northeast,us-midwest:35,49,-105,-82 > complete.json
"""

import argparse
import glob
import json
import os
import sys

ROOT = r"D:\ButterSat-build"


def read_poly(path):
    """Osmosis .poly: list of (rings, holes) as lists of (lon, lat)."""
    rings, holes, cur, hole = [], [], None, False
    lines = [l.strip() for l in open(path) if l.strip()]
    for l in lines[1:]:
        if l == "END":
            if cur is not None:
                (holes if hole else rings).append(cur)
                cur = None
            continue
        if cur is None:
            hole = l.startswith("!")
            cur = []
            continue
        x, y = l.split()[:2]
        cur.append((float(x), float(y)))
    return rings, holes


def inside(ring, x, y):
    c = False
    j = len(ring) - 1
    for i in range(len(ring)):
        xi, yi = ring[i]
        xj, yj = ring[j]
        if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
            c = not c
        j = i
    return c


def in_region(poly, x, y):
    rings, holes = poly
    return any(inside(r, x, y) for r in rings) and not any(inside(h, x, y) for h in holes)


def regions_of_tile(polys, lat, lon, n=9):
    found = set()
    for i in range(n):
        for j in range(n):
            x = lon + j / (n - 1)
            y = lat + i / (n - 1)
            for name, p in polys.items():
                if in_region(p, x, y):
                    found.add(name)
    return found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cut", action="append", default=[],
                    help="REGIONS:LAT0,LAT1,LON0,LON1 - tiles in this box were cut from these extracts "
                    "(fallback for tiles missing from osm/tiles/sources.json)")
    a = ap.parse_args()
    polys = {os.path.basename(p)[:-5]: read_poly(p) for p in glob.glob(os.path.join(ROOT, "osm", "poly", "*.poly"))}
    cuts = []
    for c in a.cut:
        regs, box = c.split(":")
        lat0, lat1, lon0, lon1 = map(int, box.split(","))
        cuts.append((set(regs.split(",")), lat0, lat1, lon0, lon1))
    sources_path = os.path.join(ROOT, "osm", "tiles", "sources.json")
    sources = json.load(open(sources_path)) if os.path.exists(sources_path) else {}
    state = json.load(open(os.path.join(ROOT, "state.json")))
    complete, incomplete = [], {}
    for t, v in sorted(state.items()):
        if v.get("status") != "done":
            continue
        lat, lon = int(t[:3]), int(t[3:])
        src = set(sources[t]) if t in sources else None
        for regs, la0, la1, lo0, lo1 in cuts:  # first matching box wins
            if src is None and la0 <= lat <= la1 and lo0 <= lon <= lo1:
                src = regs
                break
        if src is None:
            incomplete[t] = "unknown cut"
            continue
        need = regions_of_tile(polys, lat, lon)
        missing = need - src
        if missing:
            incomplete[t] = "missing " + ",".join(sorted(missing))
        else:
            complete.append(t)
    json.dump({"complete": complete, "incomplete": incomplete}, sys.stdout, indent=1)
    print(f"\n{len(complete)} complete, {len(incomplete)} incomplete", file=sys.stderr)


if __name__ == "__main__":
    main()
