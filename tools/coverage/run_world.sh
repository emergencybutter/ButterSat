#!/usr/bin/env bash
# Unattended world build (Git Bash on the Windows build machine).
#   1. cut Europe's tiles from the planet, then build Europe
#   2. meanwhile cut the rest of the world
#   3. build the remaining regions in priority order, then a final world pass
# Resumable: rerun after an interruption; finished cuts and tiles are skipped.
# Once every tile is cut, osm/tiles/.cut_done is written and later runs skip
# the cutting stages (the planet and intermediate cuts may then be deleted).
# overpass_lite.py must be running in WSL. Never run two builders at once.
#
#   WORKERS=12 run_world.sh PLANET_YYMMDD
set -uo pipefail
day=$1
workers=${WORKERS:-12}
root=/d/ButterSat-build
tools=/c/Users/me/Desktop/Projects/ButterSat-wt-coverage/tools/coverage
py=/c/Users/me/Desktop/Projects/xporthofs-build/venv/Scripts/python.exe
wosm=/mnt/d/ButterSat-build/osm
wtools=/mnt/c/Users/me/Desktop/Projects/ButterSat-wt-coverage/tools/coverage
log() { echo "$(date -u +%FT%TZ) [world] $*" | tee -a "$root/logs/progress.log"; }

split() {  # LAT0 LAT1 LON0 LON1 LOGNAME
  MSYS_NO_PATHCONV=1 wsl bash -c "bash $wtools/split_world.sh $wosm $wosm/planet-$day.osm.pbf $1 $2 $3 $4" \
    >> "$root/logs/$5.log" 2>&1
}
build() {  # NAME LAT0 LAT1 LON0 LON1
  log "build $1 box $2 $3 $4 $5 ($workers workers)"
  (cd "$root" && "$py" -u "$tools/build_tiles.py" --box "$2" "$3" "$4" "$5" --workers "$workers" --retry-failed \
     >> "$root/logs/build-world-$1.out" 2>&1)
}

if [ -e "$root/osm/tiles/.cut_done" ]; then
  log "tiles already cut; building"
  build europe 35 71 -25 44
else
  log "split Europe"
  split 35 71 -25 44 split-europe || { log "Europe split failed"; exit 1; }
  log "Europe cut; building Europe while the rest of the world is cut"
  split -60 83 -180 179 split-world &
  world_split=$!
  build europe 35 71 -25 44
  wait $world_split || { log "world split failed"; exit 1; }
  touch "$root/osm/tiles/.cut_done"
  log "world cut"
fi

build north-america 7 83 -170 -52   # Canada, Mexico, Central America, US border rebuild
build oceania -48 -10 112 179       # Australia, New Zealand
build east-asia 20 46 120 146       # Japan, Korea, Taiwan, eastern China
build south-america -56 13 -82 -34
build africa -35 37 -18 52
build asia -11 83 44 179
build world -60 83 -180 179         # islands and everything left
log "world build finished"
