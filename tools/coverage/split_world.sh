#!/usr/bin/env bash
# Planet -> Ortho4XP tags -> 10x10 degree blocks -> 1x1 degree tile cuts for
# overpass_lite.py. Run in WSL (osmium-tool). One planet means no region
# borders: every tile has complete data (unlike per-region Geofabrik cuts).
#
#   split_world.sh OSM_DIR PLANET.osm.pbf [LAT0 LAT1 LON0 LON1]
#
# Resumable: existing block and tile files are skipped. Header-only tile cuts
# (open water) are deleted at the end; build_tiles.py ignores them meanwhile.
set -euo pipefail
osm=$1; planet=$2
lat0=${3:--60}; lat1=${4:-83}; lon0=${5:--180}; lon1=${6:-179}
mkdir -p "$osm/filtered" "$osm/blocks" "$osm/tiles"

# Everything Ortho4XP's vector step asks Overpass for (O4_Vector_Map.py).
FILTER=(
  nwr/aeroway
  w/highway=motorway,trunk,primary,secondary,tertiary,unclassified,residential,service,track
  w/railway=rail,narrow_gauge
  w/natural=coastline
  nwr/natural=water
  nwr/waterway=riverbank,dock
)

world="$osm/filtered/world.filtered.osm.pbf"
if [ ! -s "$world" ] || [ "$planet" -nt "$world" ]; then
  echo "filter $(basename "$planet")"
  osmium tags-filter "$planet" "${FILTER[@]}" -o "$world" --overwrite
fi

# extract SRC DIR "name lat lon dlat dlon"...: one osmium pass, smart strategy
# (complete ways and multipolygons across the cut edges). Outputs land in a
# temporary folder and move into place only if the pass succeeds, so an
# interrupted pass never leaves a partial file that a rerun would skip.
extract() {
  local src=$1 dir=$2; shift 2
  [ $# -gt 0 ] || return 0
  local tmp="$dir/.pass.$$"; rm -rf "$tmp"; mkdir -p "$tmp"
  local cfg; cfg=$(mktemp --suffix .json)
  {
    echo '{"directory": "'"$tmp"'", "extracts": ['
    local first=1
    for t in "$@"; do
      read -r name lat lon dlat dlon <<< "$t"
      [ $first -eq 1 ] || echo ','
      first=0
      printf '{"output": "%s.osm.pbf", "bbox": [%d, %d, %d, %d]}'         "$name" "$lon" "$lat" $((lon + dlon)) $((lat + dlat))
    done
    echo ']}'
  } > "$cfg"
  osmium extract -c "$cfg" -s smart "$src" --overwrite
  rm -f "$cfg"
  mv "$tmp"/*.osm.pbf "$dir"/
  rmdir "$tmp"
}

floor10() { echo $(( ($1 >= 0 ? $1 : $1 - 9) / 10 * 10 )); }

floor30() { echo $(( ($1 >= 0 ? $1 : $1 - 29) / 30 * 30 )); }
floor60() { echo $(( ($1 + 180) / 60 * 60 - 180 )); }

# 1. Blocks covering the box. Every pass over the 32 GB world file reads it
#    ~3 times and the smart strategy's id sets cap a pass at ~4 outputs
#    (15 GB RSS), so 10-degree blocks come from 30x60-degree super-blocks:
#    4 super-blocks per world pass, then 6 blocks per (much smaller) pass.
mkdir -p "$osm/superblocks"
blocks=()
for blat in $(seq "$(floor10 "$lat0")" 10 "$(floor10 "$lat1")"); do
  for blon in $(seq "$(floor10 "$lon0")" 10 "$(floor10 "$lon1")"); do
    name=$(printf 'B%+03d%+04d' "$blat" "$blon")
    blocks+=("$name $blat $blon")
  done
done
declare -A sb_blocks sb_corner
for b in "${blocks[@]}"; do
  read -r name blat blon <<< "$b"
  [ -s "$osm/blocks/$name.osm.pbf" ] && continue
  slat=$(floor30 "$blat"); slon=$(floor60 "$blon")
  sb=$(printf 'S%+03d%+04d' "$slat" "$slon")
  sb_blocks[$sb]+="$name $blat $blon 10 10|"
  sb_corner[$sb]="$slat $slon"
done
pending=()
for sb in $(printf '%s
' "${!sb_blocks[@]}" | sort); do
  [ -s "$osm/superblocks/$sb.osm.pbf" ] && continue
  read -r slat slon <<< "${sb_corner[$sb]}"
  pending+=("$sb $slat $slon 30 60")
  if [ ${#pending[@]} -ge 4 ]; then
    echo "superblocks ${pending[*]%% *}"; extract "$world" "$osm/superblocks" "${pending[@]}"; pending=()
  fi
done
[ ${#pending[@]} -eq 0 ] || { echo "superblocks ${pending[*]%% *}"; extract "$world" "$osm/superblocks" "${pending[@]}"; }
for sb in $(printf '%s
' "${!sb_blocks[@]}" | sort); do
  IFS='|' read -r -a todo <<< "${sb_blocks[$sb]}"
  pending=()
  for t in "${todo[@]}"; do
    [ -n "$t" ] || continue
    pending+=("$t")
    if [ ${#pending[@]} -ge 6 ]; then
      echo "blocks ${pending[*]%% *}"; extract "$osm/superblocks/$sb.osm.pbf" "$osm/blocks" "${pending[@]}"; pending=()
    fi
  done
  [ ${#pending[@]} -eq 0 ] || { echo "blocks ${pending[*]%% *}"; extract "$osm/superblocks/$sb.osm.pbf" "$osm/blocks" "${pending[@]}"; }
done

# 2. Tiles from each block (small input: 25 tiles per pass).
for b in "${blocks[@]}"; do
  read -r name blat blon <<< "$b"
  src="$osm/blocks/$name.osm.pbf"
  ways=$(osmium fileinfo -e -g data.count.ways "$src")
  [ "$ways" != "0" ] || continue
  pending=()
  for lat in $(seq "$blat" $((blat + 9))); do
    for lon in $(seq "$blon" $((blon + 9))); do
      { [ "$lat" -lt "$lat0" ] || [ "$lat" -gt "$lat1" ] || [ "$lon" -lt "$lon0" ] || [ "$lon" -gt "$lon1" ]; } && continue
      t=$(printf '%+03d%+04d' "$lat" "$lon")
      [ -s "$osm/tiles/$t.osm.pbf" ] && continue
      pending+=("$t $lat $lon 1 1")
      if [ ${#pending[@]} -ge 25 ]; then extract "$src" "$osm/tiles" "${pending[@]}"; pending=(); fi
    done
  done
  [ ${#pending[@]} -eq 0 ] || extract "$src" "$osm/tiles" "${pending[@]}"
  echo "tiles from $name done"
done

# 3. Drop header-only tile cuts (no ways: open water, empty land).
dropped=0
for f in "$osm"/tiles/*.osm.pbf; do
  [ "$(stat -c %s "$f")" -gt 512 ] && continue
  rm -f "$f"; dropped=$((dropped + 1))
done
echo "tiles: $(ls "$osm"/tiles/*.osm.pbf | wc -l) kept, $dropped empty dropped"
