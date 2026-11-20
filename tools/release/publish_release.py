#!/usr/bin/env python3
"""Publish a ButterSat release for every platform to the update feed.

    python tools/release/publish_release.py 0.1.5 [--run RUN_ID] [--go]

Inputs:
- Windows: the NSIS installer and its .sig, built and signed locally
  (docs/release.md, target/release/bundle/nsis/).
- macOS and Linux: the artifacts of a successful "release bundles" GitHub
  Actions run (.github/workflows/release-bundles.yml) for the same version;
  the latest successful run unless --run is given.

What it does: downloads the CI artifacts, signs the update files (macOS
.app.tar.gz, Linux AppImage and .deb) with the updater key, which never
leaves this machine, uploads everything to buttersat.butr.cc/updates/, then
writes latest.json last (all platforms) and checks every public URL against
the local SHA-256. Without --go it only shows the plan.
"""

import argparse
import datetime
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.request

REPO = "emergencybutter/ButterSat"
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NSIS = os.path.join(ROOT, "target", "release", "bundle", "nsis")
KEY = r"C:\Users\me\Desktop\Projects\xporthofs-build\keys\updater.key"
HOST = "user@update-server"
REMOTE = "/var/www/buttersat/updates"
BASE = "https://buttersat.butr.cc/updates"
# Pre-0.1.8 apps read the feed here (same files on selfhost); checked too.
OLD_FEED = "https://satsrv.butr.cc/updates/latest.json"
TAURI = ["npx", "--yes", "@tauri-apps/cli@2.12.4"]


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, **kw)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def latest_run():
    out = subprocess.run(
        ["gh", "run", "list", "--repo", REPO, "--workflow", "release-bundles.yml", "--status", "success",
         "--limit", "1", "--json", "databaseId,headSha,createdAt"],
        check=True, capture_output=True, text=True).stdout
    runs = json.loads(out)
    if not runs:
        sys.exit("no successful release-bundles run")
    return runs[0]


def sign(path):
    """minisign signature with the updater key (via the Tauri CLI)."""
    if os.path.exists(path + ".sig"):
        os.remove(path + ".sig")
    run(TAURI + ["signer", "sign", "-f", KEY, "-p", "", path], shell=(os.name == "nt"),
        stdout=subprocess.DEVNULL)
    return open(path + ".sig", encoding="utf-8").read().strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("version")
    ap.add_argument("--run", help="GitHub Actions run id of release-bundles (default: latest successful)")
    ap.add_argument("--go", action="store_true")
    ap.add_argument("--notes", default="")
    a = ap.parse_args()
    v = a.version

    win = os.path.join(NSIS, f"ButterSat_{v}_x64-setup.exe")
    if not (os.path.exists(win) and os.path.exists(win + ".sig")):
        sys.exit(f"missing {win} (+ .sig): build the signed Windows installer first (docs/release.md)")

    run_id = a.run or str(latest_run()["databaseId"])
    work = tempfile.mkdtemp(prefix="buttersat-release-")
    print(f"downloading artifacts of run {run_id}")
    run(["gh", "run", "download", run_id, "--repo", REPO, "-D", work])
    found = {}
    for d, _, files in os.walk(work):
        for f in files:
            found[f] = os.path.join(d, f)

    # Published name -> (local path, update target keys or None for download-only)
    out = {f"ButterSat_{v}_x64-setup.exe": (win, ["windows-x86_64"])}
    want = {
        f"ButterSat_{v}_aarch64.dmg": (f"ButterSat_{v}_aarch64.dmg", None),
        f"ButterSat_{v}_x64.dmg": (f"ButterSat_{v}_x64.dmg", None),
        f"ButterSat_{v}_aarch64.app.tar.gz": ("ButterSat_aarch64.app.tar.gz", ["darwin-aarch64"]),
        f"ButterSat_{v}_x64.app.tar.gz": ("ButterSat_x64.app.tar.gz", ["darwin-x86_64"]),
        f"ButterSat_{v}_amd64.AppImage": (f"ButterSat_{v}_amd64.AppImage", ["linux-x86_64-appimage", "linux-x86_64"]),
        f"ButterSat_{v}_amd64.deb": (f"ButterSat_{v}_amd64.deb", ["linux-x86_64-deb"]),
    }
    missing = [src for src, _ in want.values() if src not in found]
    if missing:
        sys.exit(f"run {run_id} lacks {missing} (version mismatch? have {sorted(found)})")
    for name, (src, keys) in want.items():
        dst = os.path.join(work, name)
        if found[src] != dst:
            shutil.move(found[src], dst)
        out[name] = (dst, keys)

    for name, (path, keys) in out.items():
        print(f"  {name:42} {os.path.getsize(path) / 1e6:7.1f} MB  {'update: ' + ', '.join(keys) if keys else 'download'}")
    if not a.go:
        print("dry run: add --go to sign, upload and switch the feed")
        return

    platforms = {}
    for name, (path, keys) in out.items():
        if not keys:
            continue
        sig = open(path + ".sig", encoding="utf-8").read().strip() if name.endswith("-setup.exe") else sign(path)
        for k in keys:
            platforms[k] = {"signature": sig, "url": f"{BASE}/{name}"}
    feed = {
        "version": v,
        "notes": a.notes or f"ButterSat {v}",
        "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    feed_path = os.path.join(work, "latest.json")
    with open(feed_path, "w") as f:
        json.dump(feed, f, indent=2)

    files = [p for p, _ in out.values()] + [p + ".sig" for n, (p, k) in out.items() if k]
    print(f"uploading {len(files)} files")
    run(["scp", "-q", "-o", "BatchMode=yes", *files, f"{HOST}:{REMOTE}/"])
    run(["scp", "-q", "-o", "BatchMode=yes", feed_path, f"{HOST}:{REMOTE}/latest.json.new"])
    run(["ssh", "-o", "BatchMode=yes", HOST, f"cd {REMOTE} && mv latest.json.new latest.json && chmod 644 *"])

    print("verifying public URLs")
    for name, (path, _) in out.items():
        req = urllib.request.Request(f"{BASE}/{name}", headers={"User-Agent": "ButterSat-release"})
        h = hashlib.sha256()
        with urllib.request.urlopen(req, timeout=600) as r:
            for chunk in iter(lambda: r.read(1 << 20), b""):
                h.update(chunk)
        ok = h.hexdigest() == sha256(path)
        print(f"  {'OK ' if ok else 'BAD'} {name}")
        if not ok:
            sys.exit("checksum mismatch")
    req = urllib.request.Request(f"{BASE}/latest.json", headers={"User-Agent": "ButterSat-release", "Cache-Control": "no-cache"})
    live = json.load(urllib.request.urlopen(req, timeout=60))
    print(f"feed: {live['version']} for {', '.join(sorted(live['platforms']))}")
    req = urllib.request.Request(OLD_FEED, headers={"User-Agent": "ButterSat-release", "Cache-Control": "no-cache"})
    old = json.load(urllib.request.urlopen(req, timeout=60))
    print(f"old feed address: {old['version']}")
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
