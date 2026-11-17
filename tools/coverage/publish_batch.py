#!/usr/bin/env python3
"""Publish newly built tiles to the live catalog (superset, incremental).

    python publish_batch.py [--only complete.json]        # dry run: the plan
    python publish_batch.py [--only complete.json] --go   # build, upload, verify

- Tiles: state.json entries with status "done" that are not yet in
  <root>/published.json. The staging catalog (<root>/publish/v1) holds the
  previous release, so `xof-publish build` only compresses the new tiles and
  the manifest stays a superset.
- Two mirrors share one signed manifest: Cloudflare R2 (tiles.butr.cc) holds
  everything; selfhost (satsrv.butr.cc) holds the continental-US tiles as a second
  mirror. The manifest lists R2 first; clients try mirrors in order and a 404
  moves on to the next.
- `--sync` uploads whatever either host lacks for the current staged catalog
  (no build), e.g. after adding a host.
- Upload: only objects each host lacks, objects first, then LICENSE,
  ATTRIBUTION, the signature and the manifest last on both hosts (atomic
  switch for clients).
- Safety: refuses if selfhost would have < 20 GB free afterwards.
"""

import argparse
import datetime
import json
import os
import random
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor

ROOT = r"D:\ButterSat-build"
O4_TILES = os.path.join(ROOT, "Ortho4XP", "Tiles")
STAGE = os.path.join(ROOT, "publish")
XOF_PUBLISH = r"C:\Users\me\Desktop\Projects\xporthofs\target\release\xof-publish.exe"
KEYS = r"C:\Users\me\Desktop\Projects\xporthofs-build\keys"
KEY = os.path.join(KEYS, "release-signing.key")
R2_ENV = os.path.join(KEYS, "r2.env")
PUBKEY = "4d80119c5ec547ea0335d62a8ebbaf9a7831bb08849e4379ffdeda0b42757bfb"
CFG = os.path.join(ROOT, "Ortho4XP", "Ortho4XP.cfg")
SELFHOST_URL = "https://satsrv.butr.cc/v1"
HOST = "user@update-server"
REMOTE = "/var/www/buttersat/v1"
MIN_FREE = 20 << 30
# Tiles inside this box are served by selfhost; all others by R2.
US_BOX = (24, 49, -125, -67)
ATTRIBUTION = [
    "Mesh data © OpenStreetMap contributors (ODbL 1.0)",
    "Elevation: USGS 3DEP (public domain); Copernicus DEM GLO-30 © DLR e.V. 2010-2014 and "
    "© Airbus Defence and Space GmbH 2014-2018, provided under COPERNICUS by the European Union and ESA; "
    "viewfinderpanoramas.org (J. de Ferranti)",
    "Built with Ortho4XP",
]
LAST = ["LICENSE", "ATTRIBUTION", "manifest.json.zst.sig", "manifest.json.zst"]
IMMUTABLE = "public, max-age=31536000, immutable"


def ssh(cmd, **kw):
    return subprocess.run(["ssh", "-o", "BatchMode=yes", HOST, cmd], check=True, capture_output=True, text=True, **kw).stdout


def r2_env():
    return dict(line.strip().split("=", 1) for line in open(R2_ENV, encoding="utf-8") if "=" in line)


def r2_client(env):
    import boto3

    return boto3.client(
        "s3",
        endpoint_url=env["R2_ENDPOINT"],
        aws_access_key_id=env["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=env["R2_SECRET_ACCESS_KEY"],
        region_name="auto",
    )


def r2_url(env):
    return env["R2_PUBLIC_URL"].rstrip("/") + "/v1"


def in_us(tile):
    lat, lon = int(tile[:3]), int(tile[3:])
    return US_BOX[0] <= lat <= US_BOX[1] and US_BOX[2] <= lon <= US_BOX[3]


def local_files():
    v1 = os.path.join(STAGE, "v1")
    out = {}
    for d, _, fs in os.walk(v1):
        for f in fs:
            p = os.path.join(d, f)
            out[os.path.relpath(p, v1).replace("\\", "/")] = os.path.getsize(p)
    return out


def staged_manifest():
    import zstandard

    with open(os.path.join(STAGE, "v1", "manifest.json.zst"), "rb") as f:
        return json.loads(zstandard.ZstdDecompressor().stream_reader(f).read())


def objects_by_host(manifest):
    """{"selfhost": {rel: (size, blake3)}, "r2": {...}}: R2 gets every object,
    selfhost the continental-US ones."""
    hosts = {"selfhost": {}, "r2": {}}
    for tile, e in manifest["tiles"].items():
        objs = {e["dsf"]["object"]: (e["dsf"]["size"], e["dsf"]["blake3"])}
        for m in e.get("masks", []):
            objs[m["object"]] = (m["size"], m["blake3"])
        hosts["r2"].update(objs)
        if in_us(tile):
            hosts["selfhost"].update(objs)
    return hosts


def selfhost_have():
    have = {}
    for line in ssh(f"cd {REMOTE} && find . -type f -printf '%P %s\\n'").splitlines():
        rel, size = line.rsplit(" ", 1)
        have[rel] = int(size)
    return have


def selfhost_upload(rels):
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


def r2_have(s3, bucket):
    have = {}
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=bucket, Prefix="v1/"):
        for o in page.get("Contents", []):
            have[o["Key"][3:]] = o["Size"]
    return have


def r2_upload(s3, bucket, rels, cache_control, workers=16):
    v1 = os.path.join(STAGE, "v1")

    def put(rel):
        with open(os.path.join(v1, rel), "rb") as f:
            s3.put_object(
                Bucket=bucket,
                Key="v1/" + rel,
                Body=f,
                CacheControl=cache_control,
                ContentType="text/plain; charset=utf-8" if rel in ("LICENSE", "ATTRIBUTION", "manifest.json.zst.sig")
                else "application/octet-stream",
            )

    done = 0
    with ThreadPoolExecutor(max_workers=workers) as ex:
        for _ in ex.map(put, rels):
            done += 1
            if done % 2000 == 0:
                print(f"  R2: {done}/{len(rels)}", flush=True)


def check_objects(base, objs, n=20):
    """Fetch a sample through the public URL and compare BLAKE3."""
    import blake3

    sample = random.sample(sorted(objs), min(n, len(objs)))
    for rel in sample:
        size, digest = objs[rel]
        req = urllib.request.Request(f"{base}/{rel}", headers={"User-Agent": "ButterSat-publish"})
        with urllib.request.urlopen(req, timeout=60) as r:
            data = r.read()
        if len(data) != size or blake3.blake3(data).hexdigest() != digest:
            sys.exit(f"verification failed: {base}/{rel}")
    print(f"  {len(sample)} sampled objects OK on {base}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--go", action="store_true")
    ap.add_argument("--box", nargs=4, type=int, metavar=("LAT0", "LAT1", "LON0", "LON1"),
                    help="only publish tiles in this inclusive range")
    ap.add_argument("--only", help="JSON from classify_tiles.py: publish only its 'complete' tiles")
    ap.add_argument("--sync", action="store_true",
                    help="no build: upload what either host lacks for the current staged catalog")
    a = ap.parse_args()
    if a.sync:
        return sync(a.go)

    state = json.load(open(os.path.join(ROOT, "state.json")))
    pub_path = os.path.join(ROOT, "published.json")
    published = set(json.load(open(pub_path))) if os.path.exists(pub_path) else set()

    def in_box(t):
        if not a.box:
            return True
        lat, lon = int(t[:3]), int(t[3:])
        return a.box[0] <= lat <= a.box[1] and a.box[2] <= lon <= a.box[3]

    allowed = set(json.load(open(a.only))["complete"]) if a.only else None
    new = sorted(
        t
        for t, v in state.items()
        if v.get("status") == "done" and t not in published and in_box(t) and (allowed is None or t in allowed)
    )
    dirs = [os.path.join(O4_TILES, f"zOrtho4XP_{t}") for t in new]
    missing = [d for d in dirs if not os.path.isdir(d)]
    if missing:
        sys.exit(f"missing tile folders: {missing[:5]}")
    us = sum(in_us(t) for t in new)
    print(f"{len(new)} new tiles to publish (all to R2, {us} of them also to selfhost; {len(published)} already published)")
    if not new or not a.go:
        return

    env = r2_env()
    version = datetime.datetime.now(datetime.timezone.utc).strftime("%Y.%m.%d.%H%M")
    cmd = [XOF_PUBLISH, "build", "--tiles", *dirs, "--out", STAGE, "--key", KEY, "--version", version,
           "--mirror", r2_url(env), "--mirror", SELFHOST_URL, "--build-config", CFG, "--strip-rasters"]
    for line in ATTRIBUTION:
        cmd += ["--attribution", line]
    print("building catalog", version)
    subprocess.run(cmd, check=True)

    upload_and_verify()
    published |= set(new)
    with open(pub_path + ".tmp", "w") as f:
        json.dump(sorted(published), f, indent=1)
    os.replace(pub_path + ".tmp", pub_path)
    print(f"published {version}: {len(published)} built tiles in total (plus the pilot)")


def sync(go):
    hosts = objects_by_host(staged_manifest())
    print(f"staged catalog: {len(hosts['r2'])} objects (R2), {len(hosts['selfhost'])} (selfhost)")
    upload_and_verify(dry_run=not go)


def upload_and_verify(dry_run=False):
    env = r2_env()
    s3, bucket = r2_client(env), env["R2_BUCKET"]
    local = local_files()
    hosts = objects_by_host(staged_manifest())

    have = selfhost_have()
    to_selfhost = [r for r in hosts["selfhost"] if have.get(r) != local[r]]
    need = sum(local[r] for r in to_selfhost)
    free = int(ssh("df -B1 --output=avail / | tail -1").strip())
    print(f"selfhost: upload {len(to_selfhost)} objects, {need / 1e6:.0f} MB; server free {free / 1e9:.1f} GB")
    if free - need < MIN_FREE:
        sys.exit("refusing: selfhost would drop below 20 GB free")

    r2have = r2_have(s3, bucket)
    to_r2 = [r for r in hosts["r2"] if r2have.get(r) != local[r]]
    print(f"R2: upload {len(to_r2)} objects, {sum(local[r] for r in to_r2) / 1e9:.1f} GB "
          f"(bucket holds {len(r2have)} objects)")
    if dry_run:
        return

    selfhost_upload(to_selfhost)
    r2_upload(s3, bucket, to_r2, IMMUTABLE)
    # Signature before manifest on each host: a client never sees a manifest
    # without its signature.
    for rel in LAST:
        r2_upload(s3, bucket, [rel], "no-cache", workers=1)
        selfhost_upload([rel])

    print("verifying")
    for base in (r2_url(env), SELFHOST_URL):
        subprocess.run([XOF_PUBLISH, "verify", "--origin", base, "--pubkey", PUBKEY, "--sample", "0"], check=True)
    check_objects(SELFHOST_URL, hosts["selfhost"])
    check_objects(r2_url(env), hosts["r2"])
    us_r2 = {r: v for r, v in hosts["r2"].items() if r in hosts["selfhost"]}
    check_objects(r2_url(env), us_r2)


if __name__ == "__main__":
    main()
