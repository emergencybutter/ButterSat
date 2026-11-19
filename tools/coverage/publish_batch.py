#!/usr/bin/env python3
"""Publish newly built tiles to the live catalog (superset, incremental).

    python publish_batch.py [--only complete.json]        # dry run: the plan
    python publish_batch.py [--only complete.json] --go   # build, upload, verify

- Tiles: state.json entries with status "done" that are not yet in
  <root>/published.json. The staging catalog (<root>/publish/v1) holds the
  previous release, so `xof-publish build` only compresses the new tiles and
  the manifest stays a superset.
- Host: Cloudflare R2 (tiles.butr.cc) holds the whole catalog and is the
  manifest's only mirror. (selfhost / satsrv.butr.cc/v1 used to mirror the US;
  it now redirects to R2 for old clients.)
- `--sync` uploads whatever R2 lacks for the current staged catalog (no build).
- `--remirror` re-signs the staged manifest under a new version with the
  current mirror list (no new tiles) and publishes it.
- Upload: only objects R2 lacks, objects first, then LICENSE, ATTRIBUTION,
  the signature and the manifest last (atomic switch for clients).
"""

import argparse
import datetime
import json
import os
import random
import subprocess
import sys
import tempfile
import urllib.request
from concurrent.futures import ThreadPoolExecutor

ROOT = r"D:\ButterSat-build"
# Tiles build on the SSD (Ortho4XP is a junction to E:); tiles built before
# the move stay in Ortho4XP-D on the hard disk. A tile's folder is whichever
# location holds its DSF.
O4_TILE_ROOTS = [os.path.join(ROOT, "Ortho4XP", "Tiles"), os.path.join(ROOT, "Ortho4XP-D", "Tiles")]


def tile_dir(t):
    for root in O4_TILE_ROOTS:
        d = os.path.join(root, f"zOrtho4XP_{t}")
        end = os.path.join(d, "Earth nav data")
        if os.path.isdir(end) and any(f.endswith(".dsf") for _, _, fs in os.walk(end) for f in fs):
            return d
    return os.path.join(O4_TILE_ROOTS[0], f"zOrtho4XP_{t}")
STAGE = os.path.join(ROOT, "publish")
XOF_PUBLISH = r"C:\Users\me\Desktop\Projects\xporthofs\target\release\xof-publish.exe"
KEYS = r"C:\Users\me\Desktop\Projects\xporthofs-build\keys"
KEY = os.path.join(KEYS, "release-signing.key")
R2_ENV = os.path.join(KEYS, "r2.env")
PUBKEY = "4d80119c5ec547ea0335d62a8ebbaf9a7831bb08849e4379ffdeda0b42757bfb"
CFG = os.path.join(ROOT, "Ortho4XP", "Ortho4XP.cfg")
ATTRIBUTION = [
    "Mesh data © OpenStreetMap contributors (ODbL 1.0)",
    "Elevation: USGS 3DEP (public domain); Copernicus DEM GLO-30 © DLR e.V. 2010-2014 and "
    "© Airbus Defence and Space GmbH 2014-2018, provided under COPERNICUS by the European Union and ESA; "
    "viewfinderpanoramas.org (J. de Ferranti)",
    "Built with Ortho4XP",
]
LAST = ["LICENSE", "ATTRIBUTION", "manifest.json.zst.sig", "manifest.json.zst"]
IMMUTABLE = "public, max-age=31536000, immutable"
R2_WORKERS = 16




def r2_env():
    return dict(line.strip().split("=", 1) for line in open(R2_ENV, encoding="utf-8") if "=" in line)


def r2_client(env):
    import boto3
    from botocore.config import Config

    return boto3.client(
        "s3",
        endpoint_url=env["R2_ENDPOINT"],
        aws_access_key_id=env["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=env["R2_SECRET_ACCESS_KEY"],
        region_name="auto",
        config=Config(retries={"max_attempts": 10, "mode": "adaptive"}, max_pool_connections=32),
    )


def r2_url(env):
    return env["R2_PUBLIC_URL"].rstrip("/") + "/v1"


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


def catalog_objects(manifest):
    """{rel: (size, blake3)} of every tile object in the manifest."""
    objs = {}
    for e in manifest["tiles"].values():
        objs[e["dsf"]["object"]] = (e["dsf"]["size"], e["dsf"]["blake3"])
        for m in e.get("masks", []):
            objs[m["object"]] = (m["size"], m["blake3"])
    return objs


def r2_have(s3, bucket):
    have = {}
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=bucket, Prefix="v1/"):
        for o in page.get("Contents", []):
            have[o["Key"][3:]] = o["Size"]
    return have


def r2_upload(s3, bucket, rels, cache_control, workers=16):
    v1 = os.path.join(STAGE, "v1")

    def put(rel):
        # Connection drops (SSL EOF, resets) are not all retried by botocore:
        # retry the whole put a few times before giving up.
        import time
        from botocore.exceptions import BotoCoreError, ClientError

        for attempt in range(6):
            try:
                with open(os.path.join(v1, rel), "rb") as f:
                    s3.put_object(
                        Bucket=bucket,
                        Key="v1/" + rel,
                        Body=f,
                        CacheControl=cache_control,
                        ContentType="text/plain; charset=utf-8"
                        if rel in ("LICENSE", "ATTRIBUTION", "manifest.json.zst.sig")
                        else "application/octet-stream",
                    )
                return
            except (BotoCoreError, ClientError, OSError) as e:
                if attempt == 5:
                    raise
                print(f"  retry {rel}: {e}", flush=True)
                time.sleep(2 ** attempt)

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
    ap.add_argument("--prune", action="store_true",
                    help="after a verified publish, delete the raw DSFs of the tiles just made live "
                         "(the compressed copy stays in the staging catalog; frees ~2x their size)")
    ap.add_argument("--r2-workers", type=int, default=16,
                    help="parallel R2 uploads; fewer leaves upstream room for other traffic "
                         "(a saturated upstream also slows every download on the link)")
    ap.add_argument("--sync", action="store_true",
                    help="no build: upload what R2 lacks for the current staged catalog")
    ap.add_argument("--remirror", action="store_true",
                    help="no new tiles: re-sign the staged manifest with the current mirror list and publish it")
    a = ap.parse_args()
    global R2_WORKERS
    R2_WORKERS = a.r2_workers
    if a.sync:
        return sync(a.go)
    if a.remirror:
        return remirror(a.go)

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
    dirs = [tile_dir(t) for t in new]
    missing = [d for d in dirs if not os.path.isdir(os.path.join(d, "Earth nav data"))]
    if missing:
        sys.exit(f"missing tile folders: {missing[:5]}")
    print(f"{len(new)} new tiles to publish ({len(published)} already published)")
    if not new or not a.go:
        return

    env = r2_env()
    version = new_version()
    # Windows caps a command line at 32,767 characters (~600 tile paths):
    # build in batches; each one adds to the staged catalog (superset).
    print("building catalog", version)
    for i in range(0, len(dirs), 300):
        cmd = [XOF_PUBLISH, "build", "--tiles", *dirs[i:i + 300], "--out", STAGE, "--key", KEY, "--version",
               version, *mirror_args(env), "--build-config", CFG, "--strip-rasters"]
        for line in ATTRIBUTION:
            cmd += ["--attribution", line]
        subprocess.run(cmd, check=True)

    upload_and_verify()
    published |= set(new)
    with open(pub_path + ".tmp", "w") as f:
        json.dump(sorted(published), f, indent=1)
    os.replace(pub_path + ".tmp", pub_path)
    print(f"published {version}: {len(published)} built tiles in total (plus the pilot)")
    if a.prune:
        prune(new)


def new_version():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y.%m.%d.%H%M")


def mirror_args(env):
    return ["--mirror", r2_url(env)]


def remirror(go):
    """New manifest version, same tiles, current mirror list."""
    env = r2_env()
    before = staged_manifest()
    print(f"staged {before['catalog_version']}: mirrors {before.get('mirrors')} -> {[r2_url(env)]}")
    if not go:
        return
    version = new_version()
    with tempfile.TemporaryDirectory() as empty:
        # A tile folder with no DSFs: xof-publish rewrites the manifest only.
        os.makedirs(os.path.join(empty, "Earth nav data"))
        cmd = [XOF_PUBLISH, "build", "--tiles", empty, "--out", STAGE, "--key", KEY, "--version", version,
               *mirror_args(env), "--build-config", CFG, "--strip-rasters"]
        for line in ATTRIBUTION:
            cmd += ["--attribution", line]
        subprocess.run(cmd, check=True)
    after = staged_manifest()
    if set(after["tiles"]) != set(before["tiles"]):
        sys.exit("refusing: the tile set changed while re-signing")
    upload_and_verify()
    print(f"published {version} with mirrors {after['mirrors']}")


def prune(tiles):
    """Delete the raw DSFs of verified-live tiles. Only called after
    upload_and_verify succeeded (it exits on any failure)."""
    freed = 0
    for t in tiles:
        d = os.path.join(tile_dir(t), "Earth nav data")
        for root, _, files in os.walk(d):
            for f in files:
                if f.endswith(".dsf"):
                    p = os.path.join(root, f)
                    freed += os.path.getsize(p)
                    os.remove(p)
    print(f"pruned raw DSFs of {len(tiles)} published tiles: {freed / 1e9:.1f} GB freed")


def sync(go):
    manifest = staged_manifest()
    print(f"staged catalog: {len(catalog_objects(manifest))} objects")
    upload_and_verify(dry_run=not go)
    if go:
        # Everything in the verified staged catalog is live now (e.g. resuming
        # a publish that died mid-upload): record it so it is not rebuilt.
        pub_path = os.path.join(ROOT, "published.json")
        published = set(json.load(open(pub_path))) if os.path.exists(pub_path) else set()
        state = json.load(open(os.path.join(ROOT, "state.json")))
        live = {t for t in manifest["tiles"] if state.get(t, {}).get("status") == "done"}
        # Tiles being rebuilt (US border tiles recut from the planet) are in
        # the staged catalog with their old version: leave them unpublished
        # so the rebuilt version goes out with the next publish.
        border_path = os.path.join(ROOT, "border_rebuild.json")
        if os.path.exists(border_path):
            live -= set(json.load(open(border_path))) - published
        added = live - published
        with open(pub_path + ".tmp", "w") as f:
            json.dump(sorted(published | live), f, indent=1)
        os.replace(pub_path + ".tmp", pub_path)
        print(f"catalog {manifest['catalog_version']} live; {len(added)} tiles newly recorded as published")


def upload_and_verify(dry_run=False):
    env = r2_env()
    s3, bucket = r2_client(env), env["R2_BUCKET"]
    local = local_files()
    objs = catalog_objects(staged_manifest())

    r2have = r2_have(s3, bucket)
    to_r2 = [r for r in objs if r2have.get(r) != local[r]]
    print(f"R2: upload {len(to_r2)} objects, {sum(local[r] for r in to_r2) / 1e9:.1f} GB "
          f"(bucket holds {len(r2have)} objects)")
    if dry_run:
        return

    r2_upload(s3, bucket, to_r2, IMMUTABLE, workers=R2_WORKERS)
    # Signature before manifest: a client never sees a manifest without its
    # signature.
    for rel in LAST:
        r2_upload(s3, bucket, [rel], "no-cache", workers=1)

    print("verifying")
    subprocess.run([XOF_PUBLISH, "verify", "--origin", r2_url(env), "--pubkey", PUBKEY, "--sample", "0"], check=True)
    check_objects(r2_url(env), objs)


if __name__ == "__main__":
    main()
