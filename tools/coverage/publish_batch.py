#!/usr/bin/env python3
"""Publish newly built tiles to the live catalog (superset, incremental).

    python publish_batch.py            # dry run: what would be published
    python publish_batch.py --go       # build, upload, verify

- Tiles: state.json entries with status "done" that are not yet in
  <root>/published.json. The staging catalog (<root>/publish/v1) already holds
  the previous release (seeded from the pilot), so `xof-publish build` only
  compresses the new tiles and the manifest stays a superset.
- Upload: only files the server lacks, objects first, then LICENSE,
  ATTRIBUTION and the signed manifest last (atomic switch for clients).
- Safety: refuses if the server would have < 20 GB free afterwards.
"""

import argparse
import datetime
import json
import os
import subprocess
import sys
import tarfile
import tempfile

ROOT = r"D:\ButterSat-build"
O4_TILES = os.path.join(ROOT, "Ortho4XP", "Tiles")
STAGE = os.path.join(ROOT, "publish")
XOF_PUBLISH = r"C:\Users\me\Desktop\Projects\xporthofs\target\release\xof-publish.exe"
KEY = r"C:\Users\me\Desktop\Projects\xporthofs-build\keys\release-signing.key"
PUBKEY = "4d80119c5ec547ea0335d62a8ebbaf9a7831bb08849e4379ffdeda0b42757bfb"
CFG = os.path.join(ROOT, "Ortho4XP", "Ortho4XP.cfg")
MIRROR = "https://satsrv.butr.cc/v1"
HOST = "user@update-server"
REMOTE = "/var/www/buttersat/v1"
MIN_FREE = 20 << 30
ATTRIBUTION = [
    "Mesh data © OpenStreetMap contributors (ODbL 1.0)",
    "Elevation: USGS 3DEP (public domain)",
    "Built with Ortho4XP",
]
LAST = {"manifest.json.zst", "manifest.json.zst.sig", "LICENSE", "ATTRIBUTION"}


def ssh(cmd, **kw):
    return subprocess.run(["ssh", "-o", "BatchMode=yes", HOST, cmd], check=True, capture_output=True, text=True, **kw).stdout


def local_files():
    v1 = os.path.join(STAGE, "v1")
    out = {}
    for d, _, fs in os.walk(v1):
        for f in fs:
            p = os.path.join(d, f)
            out[os.path.relpath(p, v1).replace("\\", "/")] = os.path.getsize(p)
    return out


def upload(rels):
    if not rels:
        return
    v1 = os.path.join(STAGE, "v1")
    with tempfile.NamedTemporaryFile(suffix=".tar", delete=False) as tmp:
        with tarfile.open(fileobj=tmp, mode="w") as tar:
            for r in rels:
                tar.add(os.path.join(v1, r), arcname=r)
        name = tmp.name
    try:
        with open(name, "rb") as f:
            subprocess.run(
                ["ssh", "-o", "BatchMode=yes", HOST,
                 f"tar -C {REMOTE} -xf - && chown -R root:root {REMOTE} && chmod -R a+rX {REMOTE}"],
                stdin=f, check=True,
            )
    finally:
        os.remove(name)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--go", action="store_true")
    ap.add_argument("--box", nargs=4, type=int, metavar=("LAT0", "LAT1", "LON0", "LON1"),
                    help="only publish tiles in this inclusive range (e.g. tiles whose OSM data is complete)")
    a = ap.parse_args()

    state = json.load(open(os.path.join(ROOT, "state.json")))
    pub_path = os.path.join(ROOT, "published.json")
    published = set(json.load(open(pub_path))) if os.path.exists(pub_path) else set()
    def in_box(t):
        if not a.box:
            return True
        lat, lon = int(t[:3]), int(t[3:])
        return a.box[0] <= lat <= a.box[1] and a.box[2] <= lon <= a.box[3]

    new = sorted(t for t, v in state.items() if v.get("status") == "done" and t not in published and in_box(t))
    dirs = [os.path.join(O4_TILES, f"zOrtho4XP_{t}") for t in new]
    missing = [d for d in dirs if not os.path.isdir(d)]
    if missing:
        sys.exit(f"missing tile folders: {missing[:5]}")
    print(f"{len(new)} new tiles to publish ({len(published)} already published)")
    if not new or not a.go:
        print(" ".join(new))
        return

    version = datetime.datetime.now(datetime.timezone.utc).strftime("%Y.%m.%d.%H%M")
    cmd = [XOF_PUBLISH, "build", "--tiles", *dirs, "--out", STAGE, "--key", KEY, "--version", version,
           "--mirror", MIRROR, "--build-config", CFG, "--strip-rasters"]
    for line in ATTRIBUTION:
        cmd += ["--attribution", line]
    print("building catalog", version)
    subprocess.run(cmd, check=True)

    have = {}
    for line in ssh(f"cd {REMOTE} && find . -type f -printf '%P %s\\n'").splitlines():
        rel, size = line.rsplit(" ", 1)
        have[rel] = int(size)
    local = local_files()
    objects = [r for r, s in local.items() if r not in LAST and have.get(r) != s]
    last = [r for r in sorted(LAST) if r in local and r != "manifest.json.zst" and r != "manifest.json.zst.sig"]
    need = sum(local[r] for r in objects)
    free = int(ssh("df -B1 --output=avail / | tail -1").strip())
    print(f"upload {len(objects)} objects, {need / 1e6:.0f} MB; server free {free / 1e9:.1f} GB")
    if free - need < MIN_FREE:
        sys.exit("refusing: server would drop below 20 GB free")
    upload(objects)
    upload(last)
    # Signature first then manifest: a client never sees a manifest without its signature.
    upload(["manifest.json.zst.sig"])
    upload(["manifest.json.zst"])
    print("verifying through", MIRROR)
    subprocess.run([XOF_PUBLISH, "verify", "--origin", MIRROR, "--pubkey", PUBKEY, "--sample", "20"], check=True)
    published |= set(new)
    with open(pub_path + ".tmp", "w") as f:
        json.dump(sorted(published), f, indent=1)
    os.replace(pub_path + ".tmp", pub_path)
    print(f"published {version}: {len(published)} built tiles in total (plus the pilot)")


if __name__ == "__main__":
    main()
