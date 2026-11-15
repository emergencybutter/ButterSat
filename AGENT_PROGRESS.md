# Agent Progress — Coverage (CONUS)

- `2026-11-15T20:55:00Z` — Started coverage task; read common.md, coverage.md, design.md, bench.md, findings-s3-s5.md.
- `2026-11-15T21:05:00Z` — agy agent blocked by its sandbox (git/docker/wsl denied); coverage work taken over by the main Claude Code session (user's option 3).
- `2026-11-15T21:20:00Z` — Approach: no full Overpass import (≈100 GB+, days on HDD). `tools/coverage/overpass_lite.py` answers Ortho4XP's Overpass queries from pre-filtered 1° OSM tile extracts (`split_tiles.sh`, osmium in WSL). Data on D:\ButterSat-build.
- `2026-11-15T21:25:00Z` — Downloading Geofabrik us-northeast/midwest/south/west extracts sequentially (Geofabrik caps ~700 KB/s per address; ≈5 h total, Northeast first ≈1 h).
- `2026-11-15T21:30:00Z` — Ortho4XP copy at D:\ButterSat-build\Ortho4XP (overpass server LO = local), resumable builder `tools/coverage/build_tiles.py`, publish dir seeded with the live pilot catalog (incremental, superset).
- `2026-11-15T22:20:00Z` — Northeast extract (1.8 GB) downloaded; filtered to Ortho4XP's tags (1.0 GB); split into 1° tiles (osmium 'smart' extract, max 5 tiles per pass: 15 per pass ran out of 30 GB RAM).
- `2026-11-15T22:45:00Z` — First pilot rebuild failed: Ortho4XP formats bboxes as "(40, -74, 41, -73)" (spaces); overpass_lite only parsed the no-space form. Fixed; rebuilding +40-074.
