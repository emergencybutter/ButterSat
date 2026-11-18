#!/usr/bin/env python3
"""Resumable batch builder for ButterSat meshes (Ortho4XP CLI).

Builds every 1x1 degree tile that has OSM data (a file in the overpass_lite
tiles dir) inside a lat/lon box, a few at a time at low priority, and keeps
a state file so runs can be interrupted and resumed.

    python build_tiles.py --box 40 43 -80 -70 --workers 6

Tiles without OSM data are open ocean / outside the extracts: never built.
State: <root>/state.json  {tile: {status, reason, seconds, dsf_bytes, at}}
Logs:  <root>/logs/tiles/<tile>.log, <root>/logs/progress.log
"""

import argparse
import glob
import json
import shutil
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

ROOT = r"D:\ButterSat-build"
PYTHON = r"C:\Users\me\Desktop\Projects\xporthofs-build\venv\Scripts\python.exe"
BELOW_NORMAL = 0x00004000
NO_WINDOW = 0x08000000


def tile_name(lat, lon):
    return f"{lat:+03d}{lon:+04d}"


def folder(lat, lon):
    return f"{(lat // 10) * 10:+03d}{(lon // 10) * 10:+04d}"


def dsf_path(o4, lat, lon):
    t = tile_name(lat, lon)
    return os.path.join(o4, "Tiles", f"zOrtho4XP_{t}", "Earth nav data", folder(lat, lon), f"{t}.dsf")


class State:
    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.data = json.load(open(path)) if os.path.exists(path) else {}

    def set_many(self, entries):
        """Record several tiles in one read-modify-write (startup marks
        thousands of empty squares; one write each took minutes)."""
        if not entries:
            return
        with self.lock:
            if os.path.exists(self.path):
                self.data = json.load(open(self.path))
            at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            for tile, kw in entries.items():
                self.data[tile] = {**kw, "at": at}
            tmp = self.path + ".tmp"
            with open(tmp, "w") as f:
                json.dump(self.data, f, indent=1, sort_keys=True)
            os.replace(tmp, self.path)

    def set(self, tile, **kw):
        with self.lock:
            # Re-read before writing: the file is also edited by hand and by
            # other tools while a build runs; writing our stale in-memory copy
            # silently reverted those edits.
            if os.path.exists(self.path):
                self.data = json.load(open(self.path))
            self.data[tile] = {**kw, "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
            tmp = self.path + ".tmp"
            with open(tmp, "w") as f:
                json.dump(self.data, f, indent=1, sort_keys=True)
            os.replace(tmp, self.path)


def log(msg):
    line = f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {msg}"
    print(line, flush=True)
    with open(os.path.join(ROOT, "logs", "progress.log"), "a") as f:
        f.write(line + "\n")


# NED 1/3" covers the US only: tiles on the Canadian/Mexican border end up
# with nodata elevation and Ortho4XP's DSF step fails ("cannot convert float
# NaN to integer"). NED 1" also covers Canada and Mexico.
DEM_FALLBACK = 'NED 1" (from USGS) - USA, Canada, Mexico'


# Elevation by region: NED 1" (Ortho4XP.cfg default) in North America;
# elsewhere Copernicus GLO-30 (30 m, free, attribution in the app's About),
# fetched per tile from the public AWS bucket and deleted after the build;
# viewfinderpanoramas (3") where Copernicus has no tile.
COP30_URL = "https://copernicus-dem-30m.s3.amazonaws.com/{n}/{n}.tif"
VIEW_DEM = "Viewfinderpanoramas (J. de Ferranti) - mostly worldwide"


def in_ned(lat, lon):
    return 14 <= lat <= 71 and -170 <= lon <= -52


def hem(lat, lon):
    return f"{'N' if lat >= 0 else 'S'}{abs(lat):02d}{'E' if lon >= 0 else 'W'}{abs(lon):03d}"


def cop30_path(o4, lat, lon):
    return os.path.join(o4, "Elevation_data", "COP30", hem(lat, lon) + ".tif")


def dem_for(o4, lat, lon):
    """None = Ortho4XP.cfg's NED 1"; else a DEM file path or source name."""
    if in_ned(lat, lon):
        return None
    path = cop30_path(o4, lat, lon)
    if os.path.exists(path) and os.path.getsize(path) > 0:
        return os.path.abspath(path)
    ns = f"{'N' if lat >= 0 else 'S'}{abs(lat):02d}_00"
    ew = f"{'E' if lon >= 0 else 'W'}{abs(lon):03d}_00"
    url = COP30_URL.format(n=f"Copernicus_DSM_COG_10_{ns}_{ew}_DEM")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".part"
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=120) as r, open(tmp, "wb") as f:
                shutil.copyfileobj(r, f)
            os.replace(tmp, path)
            return os.path.abspath(path)
        except urllib.error.HTTPError as e:
            if e.code in (403, 404):  # no Copernicus tile here (open sea, gaps)
                return VIEW_DEM
            time.sleep(10 * (attempt + 1))
        except OSError:
            time.sleep(10 * (attempt + 1))
    return VIEW_DEM


def tidy(o4, osm_tiles, state, t, done):
    """Free disk as the build goes (~200 MB per tile otherwise, far too much
    for the world): a built tile's .alt/.node/.poly/.weight/.apt are never
    read again; its .mesh is read only by neighbours still to be built (edge
    stitching), so it goes once all 8 neighbours are settled. Called for each
    finished tile and its neighbours."""
    lat, lon = int(t[:3]), int(t[3:])

    def settled(lt, ln):
        n = tile_name(lt, ln)
        st = state.get(n, {}).get("status")
        cut = os.path.join(osm_tiles, f"{n}.osm.pbf")
        no_data = not os.path.exists(cut) or os.path.getsize(cut) <= 512
        return st in ("done", "skipped") or no_data

    def files(n, exts):
        d = os.path.join(o4, "Tiles", f"zOrtho4XP_{n}")
        return [os.path.join(d, f"Data{n}.{e}") for e in exts]

    freed = 0
    if done:
        for f in files(t, ("alt", "node", "poly", "weight", "apt")):
            if os.path.exists(f):
                freed += os.path.getsize(f)
                os.remove(f)
    for a in (-1, 0, 1):
        for b in (-1, 0, 1):
            lt, ln = lat + a, lon + b
            n = tile_name(lt, ln)
            if state.get(n, {}).get("status") != "done":
                continue
            if all(settled(lt + c, ln + d) for c in (-1, 0, 1) for d in (-1, 0, 1) if c or d):
                for f in files(n, ("mesh",)):
                    if os.path.exists(f):
                        freed += os.path.getsize(f)
                        os.remove(f)
    return freed


def clear_intermediates(o4, lat, lon):
    """The elevation is baked into Ortho4XP's intermediate files: drop them."""
    t = tile_name(lat, lon)
    d = os.path.join(o4, "Tiles", f"zOrtho4XP_{t}")
    for ext in ("alt", "mesh", "node", "poly", "apt"):
        f = os.path.join(d, f"Data{t}.{ext}")
        if os.path.exists(f):
            os.remove(f)


# Dense, defective water geometry breaks Ortho4XP in two ways: Triangle4XP
# fails even after its own min_angle=0 retry (Louisiana marsh), or the XP12
# water re-cut degenerates (+30-085). Coarser water first, then a looser
# curvature tolerance.
TRIANGLE_LADDER = [
    ["water_simplification=1", "min_area=0.01"],
    ["water_simplification=2", "min_area=0.05", "curvature_tol=3"],
]


def build(o4, lat, lon, timeout_s):
    dem = dem_for(o4, lat, lon)
    try:
        return build_with(o4, lat, lon, timeout_s, dem)
    finally:
        if dem and dem.endswith(".tif"):
            try:
                os.remove(dem)
            except OSError:
                pass


def build_with(o4, lat, lon, timeout_s, dem):
    status, reason, secs = build_once(o4, lat, lon, timeout_s, dem)
    for extra in TRIANGLE_LADDER:
        text = reason_text(lat, lon) if status == "failed" else ""
        # Triangle4XP crash, or a degenerate XP12 water re-cut that piles
        # thousands of points onto one spot (DSF quadtree runs out of bits).
        if not ("really couldn't make it" in text or "invalid literal for int() with base 2" in text):
            break
        log(f"retry {tile_name(lat, lon)} with {' '.join(extra)} (mesh/water geometry failure)")
        clear_intermediates(o4, lat, lon)
        status, reason, s2 = build_once(o4, lat, lon, timeout_s, dem, extra=extra)
        secs += s2
        if status == "done":
            reason = "retried with " + " ".join(extra)
    if status == "failed" and dem is None and "NaN" in reason_text(lat, lon):
        log(f"retry {tile_name(lat, lon)} with {DEM_FALLBACK} (nodata elevation, border tile)")
        clear_intermediates(o4, lat, lon)
        status, reason, secs2 = build_once(o4, lat, lon, timeout_s, DEM_FALLBACK)
        secs += secs2
        if status == "done":
            reason = "DEM fallback: NED 1\""
    return status, reason, secs


def reason_text(lat, lon):
    p = os.path.join(ROOT, "logs", "tiles", f"{tile_name(lat, lon)}.log")
    return open(p, encoding="utf-8", errors="replace").read() if os.path.exists(p) else ""


def build_once(o4, lat, lon, timeout_s, dem=None, extra=()):
    t = tile_name(lat, lon)
    logf = os.path.join(ROOT, "logs", "tiles", f"{t}.log")
    t0 = time.time()
    with open(logf, "w", encoding="utf-8", errors="replace") as out:
        p = subprocess.Popen(
            # o4_build.py (copied into the Ortho4XP folder) is Ortho4XP.py's CLI
            # plus an optional per-tile DEM, which the CLI can't do.
            [PYTHON, "o4_build.py", str(lat), str(lon)] + ([dem] if dem else []) + list(extra),
            cwd=o4,
            stdout=out,
            stderr=subprocess.STDOUT,
            creationflags=BELOW_NORMAL | NO_WINDOW,
        )
        try:
            p.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            p.kill()
            return "failed", f"timeout after {timeout_s}s", time.time() - t0
    text = open(logf, encoding="utf-8", errors="replace").read()
    dsf = dsf_path(o4, lat, lon)
    secs = time.time() - t0
    if os.path.exists(dsf) and os.path.getsize(dsf) > 0 and "Crash!" not in text:
        return "done", "", secs
    if "Number of edges: 0" in text or "no land" in text.lower():
        return "skipped", "no land (Ortho4XP: 0 edges)", secs
    tail = [l for l in text.strip().splitlines() if l.strip()][-3:]
    return "failed", " | ".join(tail)[-300:], secs


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--box", nargs=4, type=int, metavar=("LAT0", "LAT1", "LON0", "LON1"), required=True,
                    help="inclusive tile range: lat0..lat1, lon0..lon1 (south-west corners)")
    ap.add_argument("--workers", type=int, default=6)
    ap.add_argument("--timeout", type=int, default=4 * 3600)
    ap.add_argument("--retry-failed", action="store_true")
    ap.add_argument("--o4", default=os.path.join(ROOT, "Ortho4XP"))
    ap.add_argument("--osm-tiles", default=os.path.join(ROOT, "osm", "tiles"))
    a = ap.parse_args()
    os.makedirs(os.path.join(ROOT, "logs", "tiles"), exist_ok=True)
    state = State(os.path.join(ROOT, "state.json"))

    # A header-only file (~92 bytes) is a cut with no data (open water); the
    # splitter deletes those at the very end, so a build started while it
    # runs must not take them for land.
    have_osm = {
        os.path.basename(f)[:7]
        for f in glob.glob(os.path.join(a.osm_tiles, "*.osm.pbf"))
        if os.path.getsize(f) > 512
    }
    todo = []
    marks = {}
    for lat in range(a.box[0], a.box[1] + 1):
        for lon in range(a.box[2], a.box[3] + 1):
            t = tile_name(lat, lon)
            st = state.data.get(t, {}).get("status")
            if st in ("done", "skipped") or (st == "failed" and not a.retry_failed):
                continue
            if t not in have_osm:
                marks[t] = {"status": "skipped", "reason": "no OSM data (ocean / outside extracts)"}
                continue
            if os.path.exists(dsf_path(a.o4, lat, lon)):
                marks[t] = {"status": "done", "reason": "already built", "seconds": 0,
                            "dsf_bytes": os.path.getsize(dsf_path(a.o4, lat, lon))}
                continue
            todo.append((lat, lon))
    state.set_many(marks)
    # Inland first is no better than any order; go north-east to south-west
    # so the region around the pilot tiles finishes first.
    todo.sort(key=lambda p: (-p[0], -p[1]))
    log(f"box {a.box}: {len(todo)} tiles to build with {a.workers} workers")

    # Ortho4XP reads the meshes of already-built neighbours to stitch tile
    # edges, so two adjacent tiles must never build at the same time (a
    # half-written neighbour mesh crashes the reader: "could not broadcast
    # input array from shape (0,)"). Workers take the first pending tile with
    # no neighbour in progress.
    cond = threading.Condition()
    pending = list(todo)
    running = set()

    def adjacent(p, q):
        return abs(p[0] - q[0]) <= 1 and abs(p[1] - q[1]) <= 1

    def next_tile():
        with cond:
            while True:
                if not pending:
                    return None
                for i, p in enumerate(pending):
                    if not any(adjacent(p, r) for r in running):
                        running.add(p)
                        return pending.pop(i)
                cond.wait()

    def work(p):
        lat, lon = p
        t = tile_name(lat, lon)
        log(f"start {t}")
        try:
            status, reason, secs = build(a.o4, lat, lon, a.timeout)
            size = os.path.getsize(dsf_path(a.o4, lat, lon)) if status == "done" else 0
            state.set(t, status=status, reason=reason, seconds=round(secs), dsf_bytes=size)
            log(f"{status} {t} in {secs / 60:.1f} min {size >> 20} MB {reason}")
            with cond:  # never while a neighbour of a deleted mesh is building
                busy = {tile_name(*r) for r in running if r != p}
                snapshot = dict(state.data)
                for n in busy:  # a running tile counts as unsettled
                    snapshot[n] = {"status": "building"}
                tidy(a.o4, a.osm_tiles, snapshot, t, status == "done")
        finally:
            with cond:
                running.discard(p)
                cond.notify_all()

    def worker():
        while (p := next_tile()) is not None:
            work(p)

    with ThreadPoolExecutor(max_workers=a.workers) as ex:
        for f in [ex.submit(worker) for _ in range(a.workers)]:
            f.result()
    counts = {}
    for v in state.data.values():
        counts[v["status"]] = counts.get(v["status"], 0) + 1
    log(f"finished box {a.box}: {counts}")


if __name__ == "__main__":
    sys.exit(main())
