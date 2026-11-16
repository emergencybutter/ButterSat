#!/usr/bin/env bash
# Filter Geofabrik extracts to what Ortho4XP queries, merge them, and cut
# 1x1 degree tile files for overpass_lite.py. Run in WSL (osmium-tool).
#
#   split_tiles.sh OSM_DIR LAT_MIN LAT_MAX LON_MIN LON_MAX extract.osm.pbf...
#
# Output: OSM_DIR/tiles/{+40-074}.osm.pbf (tiles without data are dropped).
set -euo pipefail
osm=$1; lat0=$2; lat1=$3; lon0=$4; lon1=$5; shift 5
mkdir -p "$osm/filtered" "$osm/tiles"

# Everything Ortho4XP's vector step asks Overpass for (O4_Vector_Map.py).
FILTER=(
  nwr/aeroway
  w/highway=motorway,trunk,primary,secondary,tertiary,unclassified,residential,service,track
  w/railway=rail,narrow_gauge
  w/natural=coastline
  nwr/natural=water
  nwr/waterway=riverbank,dock
)

filtered=()
for src in "$@"; do
  out="$osm/filtered/$(basename "${src%.osm.pbf}").filtered.osm.pbf"
  if [ ! -s "$out" ] || [ "$src" -nt "$out" ]; then
    echo "filter $(basename "$src")"
    osmium tags-filter "$src" "${FILTER[@]}" -o "$out" --overwrite
  fi
  filtered+=("$out")
done

merged="$osm/filtered/merged.osm.pbf"
if [ ${#filtered[@]} -gt 1 ]; then
  echo "merge ${#filtered[@]} extracts"
  osmium merge "${filtered[@]}" -o "$merged.multi" --overwrite
  # Extracts from different days can carry two versions of an object that
  # crosses a region border; extract rejects duplicate ids. Read the merge as
  # a history and keep the newest version of each object.
  osmium time-filter "$merged.multi" -o "$merged" --overwrite
  rm -f "$merged.multi"
else
  cp "${filtered[0]}" "$merged"
fi

# osmium extract handles several bboxes per pass, but the 'smart' strategy
# keeps large id sets per extract: more than ~5 per pass ran out of memory
# (30 GB) on the US Northeast. Tiles already cut are skipped (resumable).
BATCH=5
pending=()
flush() {
  [ ${#pending[@]} -gt 0 ] || return 0
  cfg=$(mktemp --suffix .json)
  {
    echo '{"directory": "'"$osm/tiles"'", "extracts": ['
    local first=1
    for t in "${pending[@]}"; do
      read -r name lat lon <<< "$t"
      [ $first -eq 1 ] || echo ','
      first=0
      printf '{"output": "%s.osm.pbf", "bbox": [%d, %d, %d, %d]}' "$name" "$lon" "$lat" $((lon + 1)) $((lat + 1))
    done
    echo ']}'
  } > "$cfg"
  echo "extract ${pending[*]%% *}"
  osmium extract -c "$cfg" -s smart "$merged" --overwrite
  rm -f "$cfg"
  pending=()
}
for lat in $(seq "$lat0" "$lat1"); do
  for lon in $(seq "$lon0" "$lon1"); do
    name=$(printf '%+03d%+04d' "$lat" "$lon")
    [ -s "$osm/tiles/$name.osm.pbf" ] && continue
    pending+=("$name $lat $lon")
    [ ${#pending[@]} -ge $BATCH ] && flush
  done
done
flush

# Drop tiles with no ways at all (open ocean, outside the extracts).
dropped=0
for f in "$osm"/tiles/*.osm.pbf; do
  ways=$(osmium fileinfo -e -g data.count.ways "$f")
  if [ "$ways" = "0" ]; then rm -f "$f"; dropped=$((dropped + 1)); fi
done
echo "tiles: $(ls "$osm"/tiles/*.osm.pbf | wc -l) kept, $dropped empty dropped"
