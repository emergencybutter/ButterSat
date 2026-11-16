#!/usr/bin/env bash
# Download a dated OSM planet snapshot from a mirror (resumable) and verify
# its MD5 against planet.openstreetmap.org.
#
#   download_planet.sh OUT_DIR YYMMDD
set -euo pipefail
out=$1; day=$2
f="planet-$day.osm.pbf"
mirror="https://ftp5.gwdg.de/pub/misc/openstreetmap/planet.openstreetmap.org/pbf"
cd "$out"
want=$(curl -sfL "https://planet.openstreetmap.org/pbf/$f.md5" | cut -d' ' -f1)
[ -n "$want" ] || { echo "no snapshot $f" >&2; exit 1; }
until curl -sfL --retry 20 -C - -o "$f.part" "$mirror/$f"; do sleep 30; done
got=$(md5sum "$f.part" | cut -d' ' -f1)
if [ "$got" != "$want" ]; then echo "md5 mismatch for $f" >&2; mv "$f.part" "$f.bad"; exit 1; fi
mv "$f.part" "$f"
echo "$(date -u +%FT%TZ) downloaded $f (md5 ok)" >> ../logs/download.log
