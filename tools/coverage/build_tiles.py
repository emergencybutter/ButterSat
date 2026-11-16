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
import os
import subprocess
import sys
import threading
import time
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

    def set(self, tile, **kw):
        with self.lock:
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


def clear_intermediates(o4, lat, lon):
    """The elevation is baked into Ortho4XP's intermediate files: drop them."""
    t = tile_name(lat, lon)
    d = os.path.join(o4, "Tiles", f"zOrtho4XP_{t}")
    for ext in ("alt", "mesh", "node", "poly", "apt"):
        f = os.path.join(d, f"Data{t}.{ext}")
        if os.path.exists(f):
            os.remove(f)


def build(o4, lat, lon, timeout_s):
    status, reason, secs = build_once(o4, lat, lon, timeout_s)
    if status == "failed" and "NaN" in reason_text(lat, lon):
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


def build_once(o4, lat, lon, timeout_s, dem=None):
    t = tile_name(lat, lon)
    logf = os.path.join(ROOT, "logs", "tiles", f"{t}.log")
    t0 = time.time()
    with open(logf, "w", encoding="utf-8", errors="replace") as out:
        p = subprocess.Popen(
            # o4_build.py (copied into the Ortho4XP folder) is Ortho4XP.py's CLI
            # plus an optional per-tile DEM, which the CLI can't do.
            [PYTHON, "o4_build.py", str(lat), str(lon)] + ([dem] if dem else []),
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
    for lat in range(a.box[0], a.box[1] + 1):
        for lon in range(a.box[2], a.box[3] + 1):
            t = tile_name(lat, lon)
            st = state.data.get(t, {}).get("status")
            if st in ("done", "skipped") or (st == "failed" and not a.retry_failed):
                continue
            if t not in have_osm:
                state.set(t, status="skipped", reason="no OSM data (ocean / outside extracts)")
                continue
            if os.path.exists(dsf_path(a.o4, lat, lon)):
                state.set(t, status="done", reason="already built", seconds=0,
                          dsf_bytes=os.path.getsize(dsf_path(a.o4, lat, lon)))
                continue
            todo.append((lat, lon))
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
