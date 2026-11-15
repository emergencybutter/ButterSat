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


def build(o4, lat, lon, timeout_s):
    t = tile_name(lat, lon)
    logf = os.path.join(ROOT, "logs", "tiles", f"{t}.log")
    t0 = time.time()
    with open(logf, "w", encoding="utf-8", errors="replace") as out:
        p = subprocess.Popen(
            [PYTHON, "Ortho4XP.py", str(lat), str(lon)],
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

    have_osm = {os.path.basename(f)[:7] for f in glob.glob(os.path.join(a.osm_tiles, "*.osm.pbf"))}
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

    def work(p):
        lat, lon = p
        t = tile_name(lat, lon)
        log(f"start {t}")
        status, reason, secs = build(a.o4, lat, lon, a.timeout)
        size = os.path.getsize(dsf_path(a.o4, lat, lon)) if status == "done" else 0
        state.set(t, status=status, reason=reason, seconds=round(secs), dsf_bytes=size)
        log(f"{status} {t} in {secs / 60:.1f} min {size >> 20} MB {reason}")

    with ThreadPoolExecutor(max_workers=a.workers) as ex:
        list(ex.map(work, todo))
    counts = {}
    for v in state.data.values():
        counts[v["status"]] = counts.get(v["status"], 0) + 1
    log(f"finished box {a.box}: {counts}")


if __name__ == "__main__":
    sys.exit(main())
