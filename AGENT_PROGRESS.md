# Agent Progress — Coverage (CONUS)

- `2026-11-15T20:55:00Z` — Started coverage task; read common.md, coverage.md, design.md, bench.md, findings-s3-s5.md.
- `2026-11-15T21:05:00Z` — agy agent blocked by its sandbox (git/docker/wsl denied); coverage work taken over by the main Claude Code session (user's option 3).
- `2026-11-15T21:20:00Z` — Approach: no full Overpass import (≈100 GB+, days on HDD). `tools/coverage/overpass_lite.py` answers Ortho4XP's Overpass queries from pre-filtered 1° OSM tile extracts (`split_tiles.sh`, osmium in WSL). Data on D:\ButterSat-build.
- `2026-11-15T21:25:00Z` — Downloading Geofabrik us-northeast/midwest/south/west extracts sequentially (Geofabrik caps ~700 KB/s per address; ≈5 h total, Northeast first ≈1 h).
- `2026-11-15T21:30:00Z` — Ortho4XP copy at D:\ButterSat-build\Ortho4XP (overpass server LO = local), resumable builder `tools/coverage/build_tiles.py`, publish dir seeded with the live pilot catalog (incremental, superset).
- `2026-11-15T22:20:00Z` — Northeast extract (1.8 GB) downloaded; filtered to Ortho4XP's tags (1.0 GB); split into 1° tiles (osmium 'smart' extract, max 5 tiles per pass: 15 per pass ran out of 30 GB RAM).
- `2026-11-15T22:45:00Z` — First pilot rebuild failed: Ortho4XP formats bboxes as "(40, -74, 41, -73)" (spaces); overpass_lite only parsed the no-space form. Fixed; rebuilding +40-074.
- `2026-11-15T22:48:30Z` — Validation: +40-074 rebuilt from local OSM in 3.0 min (pilot with public Overpass: 26 min); identical input vertices (257,378), mesh vertices 1,201,869 vs 1,201,838, same DSF size.
- `2026-11-15T23:50:00Z` — Northeast built: 79 done, 70 skipped (ocean / outside), 1 failed (+44-075: NED 1/3" nodata on the Canadian border).
- `2026-11-16T00:03:00Z` — +44-075 rebuilt with NED 1" (US+Canada+Mexico) via tools/coverage/o4_build.py (Ortho4XP's CLI ignores per-tile configs).
- `2026-11-16T00:15:00Z` — Published catalog 2026.11.15.2351: 69 tiles (68 Northeast tiles fully inside NE states + pilot +39-075), 1.7 GB uploaded, verified through Cloudflare; selfhost 99 GB free. Region-edge tiles (lat 38-39 MD/DE/WV, lon -81 OH) held back until all extracts are merged.
- `2026-11-16T00:14:00Z` — Midwest: extract downloaded and split (300 tiles with data); build started (6 workers). South/West downloads continue.
