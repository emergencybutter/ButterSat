#!/usr/bin/env bash
# Download one Geofabrik extract as a dated (immutable) snapshot, resumable,
# and verify its MD5. "-latest" files are replaced daily, so resuming one
# across an update splices two versions into a corrupt file.
#
#   download_extract.sh OUT_DIR REGION [YYMMDD]     e.g. us-west 261115
set -euo pipefail
out=$1; region=$2; day=${3:-$(date -u -d yesterday +%y%m%d)}
base="https://download.geofabrik.de/north-america"
f="$region-$day.osm.pbf"
cd "$out"
want=$(curl -sfL "$base/$f.md5" | cut -d' ' -f1)
[ -n "$want" ] || { echo "no snapshot $f" >&2; exit 1; }
until curl -sfL --retry 10 -C - -o "$f.part" "$base/$f"; do sleep 30; done
got=$(md5sum "$f.part" | cut -d' ' -f1)
if [ "$got" != "$want" ]; then echo "md5 mismatch for $f ($got != $want)" >&2; mv "$f.part" "$f.bad"; exit 1; fi
mv "$f.part" "$f"
ln -sf "$f" "$region-latest.osm.pbf" 2>/dev/null || cp "$f" "$region-latest.osm.pbf"
echo "$(date -u +%FT%TZ) downloaded $f (md5 ok)" >> ../logs/download.log
