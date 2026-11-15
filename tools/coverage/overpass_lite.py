#!/usr/bin/env python3
"""Minimal local stand-in for the Overpass API, for Ortho4XP builds.

Ortho4XP only sends queries of the form

    (way["k"="v"](S,W,N,E);node["k"](S,W,N,E);...);(._;>>;);out meta;

and parses the XML line by line. We answer them from pre-filtered,
pre-split 1x1 degree OSM extracts (``tiles/{+40-074}.osm.pbf``, made by
``split_tiles.sh``) with ``osmium tags-filter``, which adds referenced
nodes and relation members just like ``(._;>>;)``.

Run inside WSL (needs osmium-tool):
    python3 overpass_lite.py --tiles /mnt/d/ButterSat-build/osm/tiles --port 8099
"""

import argparse
import math
import os
import re
import subprocess
import tempfile
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Ortho4XP writes the bbox as str(tuple): "(40, -74, 41, -73)", with spaces.
N = r"\s*([-\d.]+)\s*"
CLAUSE = re.compile(r'(node|way|rel)\["([^"]+)"(?:="([^"]*)")?\]\(' + N + "," + N + "," + N + "," + N + r"\)")
EMPTY = b'<?xml version="1.0" encoding="UTF-8"?>\n<osm version="0.6" generator="overpass-lite">\n</osm>\n'
TYPE = {"node": "n", "way": "w", "rel": "r"}


def tile_name(lat, lon):
    return f"{lat:+03d}{lon:+04d}"


def tiles_for_bbox(s, w, n, e):
    out = []
    for lat in range(math.floor(s), max(math.ceil(n), math.floor(s) + 1)):
        for lon in range(math.floor(w), max(math.ceil(e), math.floor(w) + 1)):
            out.append(tile_name(lat, lon))
    return out


def parse(query):
    clauses = CLAUSE.findall(query)
    if not clauses:
        raise ValueError("unsupported query")
    exprs, bboxes = [], set()
    for typ, k, v, s, w, n, e in clauses:
        exprs.append(f"{TYPE[typ]}/{k}" + (f"={v}" if v else ""))
        bboxes.add((float(s), float(w), float(n), float(e)))
    if len(bboxes) != 1:
        raise ValueError("queries with several bboxes are not supported")
    return exprs, bboxes.pop()


class Handler(BaseHTTPRequestHandler):
    tiles_dir = "."
    stats = {"queries": 0, "bytes": 0, "errors": 0}
    lock = threading.Lock()

    def log_message(self, fmt, *args):  # quiet; we log our own lines
        pass

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        if url.path == "/status":
            body = repr(self.stats).encode()
            return self.reply(200, body, "text/plain")
        if not url.path.endswith("/interpreter"):
            return self.reply(404, b"not found", "text/plain")
        q = urllib.parse.parse_qs(url.query).get("data", [""])[0]
        t0 = time.time()
        try:
            exprs, (s, w, n, e) = parse(q)
            files = [f for f in (os.path.join(self.tiles_dir, t + ".osm.pbf") for t in tiles_for_bbox(s, w, n, e)) if os.path.exists(f)]
            body = self.run(files, exprs) if files else EMPTY
            with self.lock:
                self.stats["queries"] += 1
                self.stats["bytes"] += len(body)
            print(f"{time.strftime('%H:%M:%S')} {len(body) >> 10:>7} KB {time.time() - t0:5.1f}s {exprs} {s},{w},{n},{e}", flush=True)
            self.reply(200, body, "application/osm3s+xml")
        except Exception as ex:  # noqa: BLE001 - report to the client
            with self.lock:
                self.stats["errors"] += 1
            print(f"error: {ex} for {q[:200]}", flush=True)
            self.reply(400, f"<error>{ex}</error>".encode(), "text/xml")

    def run(self, files, exprs):
        with tempfile.TemporaryDirectory() as tmp:
            src = files[0]
            if len(files) > 1:
                src = os.path.join(tmp, "merged.osm.pbf")
                subprocess.run(["osmium", "merge", *files, "-o", src, "--overwrite"], check=True, capture_output=True)
            out = subprocess.run(
                ["osmium", "tags-filter", src, *exprs, "-f", "osm,add_metadata=false", "-o", "-"],
                check=True,
                capture_output=True,
            )
            return out.stdout

    def reply(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tiles", required=True)
    ap.add_argument("--port", type=int, default=8099)
    ap.add_argument("--bind", default="0.0.0.0")
    a = ap.parse_args()
    Handler.tiles_dir = a.tiles
    print(f"overpass-lite on {a.bind}:{a.port}, tiles in {a.tiles}", flush=True)
    ThreadingHTTPServer((a.bind, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
