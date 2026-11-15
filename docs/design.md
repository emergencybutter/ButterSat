# xporthofs — Design

Status: draft, pre-implementation (2026-11-14)

xporthofs is a virtual filesystem that presents X-Plane 12 with orthophoto
scenery. Both the terrain mesh and the textures are streamed: meshes from a
hosted catalog we build, textures generated on demand from online
satellite/aerial imagery. The core is written in Rust, the control UI uses
Tauri, and it runs on Windows, Linux and macOS.

Prior art: [AutoOrtho](https://github.com/kubilus1/autoortho) and
[AutoOrtho Continued](https://programmingdinosaur.github.io/autoortho4xplane)
(Python, FUSE/Dokan/WinFsp), [OrthoStreamXP](https://github.com/flysimdev/OrthoStreamXP)
(.NET, WinFsp, Windows only).

---

## 1. Goals and non-goals

### Guiding principle: zero setup
Install, launch, fly. No side downloads, no manual configuration, no manual.
The single accepted exception is installing macFUSE on macOS (the UI detects
it and links to the installer).

### Goals (v1)
- Serve X-Plane 12 a read-only scenery pack in which **meshes are streamed**
  from a hosted catalog and **textures are synthesized lazily** from imagery
  tiles. Nothing is pre-downloaded by the user.
- Never stall X-Plane on textures: every texture read completes within a time
  budget, degrading to lower resolution rather than blocking. Mesh reads block
  but are prefetched aggressively.
- Run on Windows 10/11 (WinFsp, bundled), Linux (libfuse3) and macOS 26+
  (macFUSE, FSKit mode — no kext).
- Bounded caches with LRU eviction.
- Prefetch ahead of the aircraft using X-Plane 12's Web API.
- A Tauri control UI that can be closed without affecting a running flight.
- Locally imported Ortho4XP / AutoOrtho packs supported as an optional
  override for power users.

### Non-goals (v1)
- Generating or transforming meshes on the user's machine (see §17).
- X-Plane 11.
- BC7 / any texture format other than BC1.
- Native FSKit backend (future add-on).
- Per-region provider overrides.

---

## 2. Background: what an ortho pack looks like

An Ortho4XP-style pack:

```
zOrtho4XP_+40-074/
  Earth nav data/+40-080/+40-074.dsf     # mesh, 7z-compressed (X-Plane reads 7z DSFs natively)
  terrain/24832_19008_XO16.ter           # terrain defs → point at textures (tiny, formulaic)
  textures/24832_19008_XO16.dds          # 4096² BC1 textures  ← the 90%+ by size
  textures/*.png                         # water masks, coastal tiles only
```

Texture/terrain name: `{row}_{col}_{provider}{zl}`

- `row`, `col`: Web-Mercator (slippy) tile Y/X of the **top-left** 256px tile at
  zoom `zl`. Both are multiples of 16.
- The texture covers a 16×16 block of 256px tiles → 4096×4096 pixels.
- `provider`: short code. Our hosted meshes use a neutral code (`XO`); imported
  Ortho4XP packs carry real codes (`BI`, …). Either way the VFS treats the code
  as a hint — the user's configured provider is what's fetched. Exact naming,
  `.ter` contents and mask naming are verified in spike S3.

Because geometry, zoom and provider are encoded in the name, `.ter` contents
and `.dds` size/contents are fully determined by the path — no network needed
to answer `stat`/`readdir`.

---

## 3. Architecture

```
┌──────────────┐  JSON-RPC (socket / pipe)   ┌──────────────────────────────┐
│ xof-ui       │◄───────────────────────────►│ xof-daemon                   │
│ (Tauri)      │   requests + pushed events  │  ├─ vfs: winfsp | fuser      │
└──────────────┘                             │  ├─ texture engine (DDS)     │
                                             │  ├─ mesh streamer + catalog  │
X-Plane 12 ──read()──► mountpoint ─────────► │  ├─ imagery providers        │
X-Plane 12 ──Web API (ws, :8086)───────────► │  ├─ caches (disk + RAM)      │
                                             │  └─ prefetcher               │
                                             └──────┬──────────────┬────────┘
                                                    │ HTTPS        │ HTTPS
                                       mesh origin (R2 / self-host)  imagery providers
```

- **Daemon owns the data plane.** The UI is a pure control plane; closing or
  crashing it never unmounts. The UI spawns the daemon as a detached helper if
  none is running; the daemon only unmounts on explicit `shutdown`.
- Later: optional start-at-login (Windows: per-user Task Scheduler task;
  Linux: systemd user unit; macOS: launchd agent).

### Cargo workspace

| Crate | Responsibility | OS deps |
|---|---|---|
| `xof-core` | tile math, DDS layout, providers, fetch, decode, mip generation, BC1 encode, texture cache | none |
| `xof-mesh` | catalog (fetch, verify, refresh), mesh download/verify, mesh cache, `.ter` synthesis | none |
| `xof-vfs` | `VfsBackend` trait, shared path → node mapping and PID filter; `fuser` impl (Linux/macOS) and `winfsp` impl (Windows) behind `cfg(target_os)` | libfuse3 / macFUSE / WinFsp |
| `xof-daemon` | binary: config, mount lifecycle, IPC server, prefetcher, XP12 Web API client | — |
| `xof-proto` | IPC request/response/event types (serde), shared with UI | none |
| `xof-ui` | Tauri app (Rust backend + web frontend) | — |
| `xof-cli` | headless control/debug CLI | — |
| `xof-publish` | build-side tool: package Ortho4XP output, hash, sign manifest, upload to an origin (§10) | none |

The engines expose "give me bytes `[off, off+len)` of logical file `F` with
deadline `D`", so the filesystem layer knows nothing about imagery or meshes,
and a future FSKit backend reuses everything.

---

## 4. Virtual filesystem

### 4.1 Backends
- **Windows 10/11:** WinFsp via the `winfsp` crate (winfsp-rs). The WinFsp MSI
  is bundled in our installer and installed silently. Mounts directly onto
  `Custom Scenery\z_xporthofs` (the directory must not pre-exist — WinFsp
  creates it as a reparse point) without admin rights. Build needs the WinFsp
  SDK and libclang (bindgen).
  - Rejected: ProjFS / Cloud Files API (Microsoft-native, but they hydrate
    whole files onto NTFS — degraded textures can't be upgraded cleanly and
    every read consumes disk); NFS via Windows NFS client (quirky, absent on
    Home editions); Dokan (one Windows backend is enough; WinFsp is better
    maintained).
- **Linux:** `fuser` against libfuse3, unprivileged mount via `fusermount3`.
  libfuse3 is present on mainstream distros; the health check reports if not.
- **macOS 26+:** same `fuser` code against macFUSE 5 in FSKit mode (no kext,
  no Recovery-mode step). The one manual install. FUSE-T compatibility checked
  in S1 as a possible alternative.
- **Future:** native FSKit backend behind the same trait (requires paid Apple
  Developer Program for the entitlement + notarization).

### 4.2 Mount layout and sources
One mountpoint, `Custom Scenery/z_xporthofs/`, presenting a merged view.
For each 1°×1° tile, sources in priority order:

1. **Imported packs** (optional, power users) in the backing dir
   (`%LOCALAPPDATA%\xporthofs\packs\*`, `~/.local/share/xporthofs/packs/*`,
   `~/Library/Application Support/xporthofs/packs/*`). Passthrough, including
   any real `.dds` files they contain.
2. **Streamed catalog** (§6), filtered by the user's coverage setting
   (worldwide by default).
3. Neither → the tile isn't listed; X-Plane uses its default scenery there.

Synthesized files:
- `terrain/*.ter` — generated from the name (texture path, load centre, etc.;
  exact format per S3). Small, cheap, never touches the network.
- `textures/*.dds` — texture engine (§5), unless an imported pack has a real
  file.

Path handling is normalised in one place: `/` and `\` separators, and
case-insensitive lookup on Windows and macOS (case-preserving names returned).

The mount is read-only. The `scenery_packs.ini` entry is created automatically
on first run (below overlays, above Global Scenery), backing up the file first.

### 4.3 Metadata
- `lookup`/`getattr`/`readdir` are answered from the catalog, the backing dir
  and filename parsing. They must **never** trigger fetches — X-Plane stats
  every file at scenery load.
- Inodes: stable 64-bit hash of the virtual path (collision-checked in a table).
- Synthesized DDS size is constant (§5.1); DSF size comes from the catalog.

### 4.4 Threading
- `fuser` dispatches on one thread. `read` handlers hand the `ReplyData` (which
  is `Send`) to the engine and return immediately; the engine replies from a
  worker.
- WinFsp dispatches on its own thread pool and supports asynchronous
  completion (`STATUS_PENDING` + `FspFileSystemSendResponse`).
- Either way the backend adapts to the same engine call:
  `read(file, off, len, deadline) -> future<bytes>`. Metadata ops are answered
  inline.

### 4.5 Process filter
FUSE and WinFsp both supply the requesting PID. Reads are classified:
- **X-Plane** (process name/exe matches, configurable): full pipeline.
- **Other** (Windows Defender `MsMpEng.exe`, Windows Search, Spotlight `mds`,
  QuickLook, Linux indexers, backup tools): textures served only from cache or
  the cheap low-res path; mesh reads served only if cached (never trigger mesh
  downloads).

Platform extras:
- **Windows:** the installer (already elevated for WinFsp) offers a checkbox,
  on by default, to exclude the mountpoint and caches from Defender real-time
  scanning. Never done silently. Volume marked not indexed.
- **macOS:** mount with `nobrowse`, `noappledouble`, `noapplexattr`; disable
  Spotlight (`mdutil -i off`) on the mountpoint.

### 4.6 Page cache
- Fully-resolved textures and all DSFs: kernel caching enabled
  (`FOPEN_KEEP_CACHE` / WinFsp `FileInfoTimeout`).
- Textures served with any degraded band: caching disabled for that open so a
  later open sees the upgraded content. Semantics verified per backend in S1.

---

## 5. Texture synthesis

### 5.1 DDS layout (BC1, 4096², full mip chain)

128-byte header (`DDS ` magic + `DDS_HEADER`, FourCC `DXT1`, `MIPMAPCOUNT=13`),
then mip levels back-to-back. BC1 level size = `max(1,w/4) * max(1,h/4) * 8`.

| Mip | Size | Bytes | Source |
|---|---|---|---|
| 0 | 4096 | 8,388,608 | ZL Z, 16×16 tiles |
| 1 | 2048 | 2,097,152 | ZL Z-1, 8×8 tiles |
| 2 | 1024 | 524,288 | ZL Z-2, 4×4 tiles |
| 3 | 512 | 131,072 | ZL Z-3, 2×2 tiles |
| 4 | 256 | 32,768 | ZL Z-4, 1 tile |
| 5–12 | 128…1 | 11,064 total | downsampled from mip 4 |

Total file size: **11,184,952 bytes** (128 header + 11,184,824 data).

Mips 0–4 are built directly from the provider's own lower zoom levels instead
of downsampling — fewer downloads for distant terrain and better source quality.

### 5.2 Offset → work mapping
Within mip `m` of width `W`, BC1 blocks are row-major with stride `W/4*8` bytes.
One 256-px source tile row = 64 block rows. For mip 0 that is 512 KiB per tile
row. So a read `[off, off+len)` maps to: mip level(s) → band(s) of 64 block
rows → the source tiles in that band. A band is the unit of work and caching.

The header and mips ≥ 4 (~44 KB total, needs 1 tile at Z-4) are produced eagerly
on first open of any texture.

X-Plane at reduced texture quality never reads mip 0; that ZL is then simply
never fetched.

### 5.3 Band pipeline
1. Resolve source tiles for the band; check RAM → disk cache; enqueue misses
   with the read's deadline as priority.
2. Decode JPEG (`zune-jpeg`; libjpeg-turbo bindings if faster in S2).
3. Compose band into a linear-light RGB buffer (sRGB → linear on decode).
4. Encode BC1 (`intel_tex_2`, ISPC):
   - **on-demand** (X-Plane waiting): fast profile;
   - **background** (prefetch / upgrade): high-quality profile.
5. Store encoded band in RAM cache, reply.

Mips 5–12: downsample mip 4 in linear light (`fast_image_resize`), convert back
to sRGB, encode.

CPU work runs on a rayon pool sized `cores - 1`; network on tokio.

### 5.4 Deadlines and degradation
Each read carries a deadline (default from config, tuned in M1; order of 1 s).
If the band's tiles aren't all available by the deadline:
- missing tiles are filled by upscaling the best cached lower-ZL ancestor
  (worst case: the Z-4 tile, which is fetched first on open);
- the band is marked **degraded**, the fetch continues in the background, and
  the band is rebuilt when tiles arrive (§4.6 for visibility).

Degraded-band counts are a first-class metric in the UI.

### 5.5 Dynamic zoom (M3)
Above a configurable AGL threshold, mip 0 of a ZL Z texture is built from ZL
Z-1 tiles (upscaled) instead of ZL Z — ~75% fewer downloads at cruise. Below
the threshold, real ZL Z. File size and names never change, so X-Plane is
unaffected; bands built at reduced zoom are tracked like degraded bands and
upgraded on descent.

---

## 6. Mesh streaming

### 6.1 Catalog (manifest)
A single signed manifest describes all hosted tiles:

```jsonc
{
  "format": 1,
  "catalog_version": "2026.11.1",
  "generated_at": "2026-11-20T00:00:00Z",
  "build_config": "blake3:…",          // hash of the Ortho4XP config used
  "mirrors": ["https://mesh.example.org/v1/", "https://r2.example.org/v1/"],
  "attribution": ["© OpenStreetMap contributors", "Copernicus DEM …"],
  "tiles": {
    "+40-074": {
      "dsf":   { "object": "tiles/+40-080/+40-074.3f9a….dsf", "size": 41234567, "blake3": "…" },
      "masks": [ { "name": "…png", "object": "masks/….png", "size": 12345, "blake3": "…" } ],
      "zl": { "base": 16, "airports": 17 }
    }
  }
}
```

- Delivered as `manifest.json.zst` + detached ed25519 signature
  `manifest.json.zst.sig`. The public key(s) are compiled into the app, so
  mirrors are untrusted.
- Fetched at daemon start, refreshed daily with `ETag`. Last good copy kept
  for offline use.
- Bootstrap URLs (where to find the manifest) are built in and overridable in
  config; after that the manifest's own `mirrors` list is used, tried in order
  with failover.

### 6.2 Read path
- DSF read, cached and hash-valid → serve.
- Not cached → download the full object (from first healthy mirror), verify
  BLAKE3, write to mesh cache, then serve. DSFs are 7z-compressed and served
  byte-for-byte — X-Plane decompresses them itself.
- **No degraded fallback**: mesh reads block. XP12 loads scenery on background
  threads; prefetch (§9) keeps this rare.
- Concurrent reads of the same DSF coalesce onto one download.

### 6.3 Offline and failure behaviour
- Origin unreachable at startup → only cached tiles are listed; uncovered
  areas fall back cleanly to X-Plane's default scenery.
- Download failure mid-flight → `EIO`. X-Plane's handling of a failed or slow
  DSF read is measured in spike S4; if it's ugly, prefetch radius grows and
  uncached-tile listing becomes conservative.

### 6.4 Updates
A tile whose hash changed in a new catalog version is downloaded in the
background; the new file is swapped in only at the next simulator start
(detected via Web API disconnect/reconnect or daemon restart), never under a
running X-Plane.

### 6.5 Mesh cache
Plain files on disk (few, large), separate size cap and LRU from the imagery
cache. Tiles around frequently used airports can be pinned.

---

## 7. Imagery providers

```rust
trait Provider {
    fn id(&self) -> &str;                 // "esri", "bing"
    fn filename_codes(&self) -> &[&str];  // codes seen in imported Ortho4XP names
    fn max_zoom(&self) -> u8;
    fn tile_request(&self, z: u8, x: u32, y: u32) -> Request; // URL+headers
    fn rate_limit(&self) -> RateLimit;
}
```

v1 providers:
- **Esri World Imagery** — XYZ scheme. Default.
- **Bing** — quadkey scheme (exercises the non-XYZ path).

Fetcher: reqwest/HTTP2, per-provider concurrency + rate limit, retries with
backoff, generic User-Agent. "No tile at this zoom" responses (blank/placeholder
images, 404) are cached as negative entries and trigger ancestor fallback.

Provider terms of service vary; the default provider is used after a one-time
notice in the UI; others are switchable in settings.

---

## 8. Caches

| Tier | Content | Store | Eviction |
|---|---|---|---|
| M | streamed DSFs + masks | plain files on disk | LRU by tile, own cap, pinning |
| T1 | raw JPEG tiles keyed `(provider, z, x, y)` | single embedded DB (`redb`), not one file per tile | LRU by **area** (texture block), size cap 25 GB–1 TB |
| T2 | encoded BC1 bands + header/small mips | RAM | LRU by bytes, configurable budget |
| T3 *(later)* | complete DDS files for pinned regions | files on disk | manual |

T1 tracks last-access per 1°×1° tile for the UI cache map. Negative entries
live in T1 with a TTL.

---

## 9. Prefetch

- **Position source:** X-Plane 12 Web API (localhost:8086). Resolve dataref IDs
  via REST, then subscribe over the websocket to latitude, longitude,
  groundspeed, true track, elevation AGL.
- **Priority** (highest first):
  1. live mesh reads;
  2. live texture reads;
  3. mesh prefetch — current tile + 8 neighbours, plus 2–3 tiles ahead along
     the track, plus the SimBrief route when available (M5);
  4. texture prefetch — Z-4/Z-3 tiles near the aircraft first, a cone ahead
     sized by groundspeed (≈5–10 min), nearer before farther. Fills T1 only;
     encoding happens on read, except background upgrade of degraded bands.
- **Prepare departure** (UI): pick an airport before launching X-Plane; its
  surrounding meshes and low-ZL imagery are fetched in advance, so the first
  load in a new area doesn't wait.
- If the Web API is unreachable, prefetch is idle; streaming still works.

---

## 10. Mesh build pipeline and hosting

### 10.1 Build
1. **Inputs:** OSM (water, coastline, airports — ODbL, attribution required)
   and Copernicus GLO-30 or SRTM elevation (clear licences; avoid sources
   with unclear terms).
2. **Ortho4XP headless batch, mesh steps only** (no imagery downloaded at
   build time). Config versioned in this repo for reproducibility:
   - ZL16 base, **ZL17 within a few km of airports**;
   - neutral provider code `XO`;
   - water masks on.
3. Runs on preemptible cloud VMs or the self-hosted server; minutes per tile.
4. `xof-publish`: 7z-compress DSFs, compute BLAKE3, write content-addressed
   objects, generate and sign the manifest, upload.
5. Automated checks per tile: DSF parses, expected terrain names, size sanity;
   a sample loaded in X-Plane for each release.

### 10.2 Origin contract
Any static HTTPS server satisfying:
- serves the layout below, with `ETag` and HTTP range support;
- objects are **content-addressed and immutable** (`Cache-Control: immutable`),
  so any CDN/proxy can cache them forever; only the manifest changes, and a
  release is atomic by swapping it.

```
/v1/manifest.json.zst
/v1/manifest.json.zst.sig
/v1/tiles/+40-080/+40-074.<blake3-prefix>.dsf
/v1/masks/<name>.<blake3-prefix>.png
```

### 10.3 Supported hosting setups
Both are first-class; the client cannot tell them apart.

| Setup | How | Notes |
|---|---|---|
| **Cloudflare R2** | `xof-publish --target s3` (R2 is S3-compatible), public bucket on a custom domain | Zero egress fees; ~$0.015/GB-month storage |
| **Self-hosted dedicated server** | `xof-publish --target dir` writes the tree locally, then rsync; served by Caddy/nginx | Optionally fronted by Cloudflare CDN; can also run the build |

Both can be listed as mirrors in the same manifest for failover. Signing
happens on the publishing machine; the private key never lives on an origin.

### 10.4 Coverage rollout
1. **Pilot:** New York City — the 3×3 block `+39-075` … `+41-073` (KJFK,
   KLGA, KEWR, KTEB all in `+40-074`).
2. US Northeast.
3. North America + Europe.
4. Worldwide.

Uncovered tiles fall back to default scenery, so partial coverage is seamless.
Global storage estimate (few hundred GB – ~1 TB) is confirmed by the pilot.

---

## 11. Installation and first run (zero setup)

| Concern | Handling |
|---|---|
| Filesystem driver | Windows: WinFsp MSI bundled, silent install. Linux: system libfuse3. macOS: macFUSE — the one manual step, detected and linked by the UI |
| X-Plane location | Read `x-plane_install_12.txt` from X-Plane's per-user preferences location on each OS; ask only if absent or ambiguous |
| `scenery_packs.ini` | Entry added automatically on first run, with backup |
| Defender | Installer checkbox (see §4.5) |
| Imagery provider | Esri by default after a one-time notice |
| Mesh | Streamed (§6); nothing to download |
| Attribution | Shown in an About screen (OSM, DEM, imagery provider) |

Packaging: MSI/NSIS (Windows), DMG (macOS), AppImage/deb (Linux) via the Tauri
bundler. Unsigned initially (SmartScreen / Gatekeeper prompt once).

### 11.1 Licensing obligations
xporthofs is licensed under **MIT**. Bundling WinFsp relies on its FLOSS
exception, which requires that we:
- ship only **unmodified** official WinFsp installer releases;
- show "WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos"
  with a link to the WinFsp repository **in the UI (About screen) and in
  user-facing docs**;
- **not link or distribute xporthofs with proprietary software.** Consequence:
  the Windows installer must not embed Microsoft's WebView2 runtime/bootstrapper
  — use Tauri's `downloadBootstrapper` mode (or rely on the WebView2 runtime
  preinstalled on Windows 10/11). All Rust/JS dependencies must be FLOSS
  (enforced with `cargo-deny` licence checks in CI).

Data attribution (OSM, DEM, imagery provider) is shown alongside in the About
screen.

---

## 12. Daemon, IPC, configuration

- Transport via the `interprocess` crate:
  - Windows: named pipe `\\.\pipe\xporthofs-<user SID>` (ACL: current user only);
  - Linux: `$XDG_RUNTIME_DIR/xporthofs.sock`;
  - macOS: `~/Library/Application Support/xporthofs/daemon.sock`.
- Protocol: newline-delimited JSON-RPC 2.0. Requests: `status`, `mount`,
  `unmount`, `shutdown`, `get_config`/`set_config`, `cache_stats`,
  `cache_evict_area`, `catalog_status`, `prepare_departure`, `packs_list`,
  `packs_import`. Events (pushed): `stats` (1 Hz), `aircraft`, `log`, `health`.
- Config: TOML in the platform config dir; the UI edits it via IPC, the daemon
  owns the file. Defaults must work without any edits.
- Single instance enforced via the socket/pipe + lockfile.

---

## 13. Tauri UI

- **Map** (MapLibre GL): catalog coverage, cached meshes and imagery
  (size/age), aircraft and prefetch area.
- **Live stats:** tiles/s, read latency histograms (mesh and texture), degraded
  bands, cache usage, encode queue depth, mesh downloads in flight.
- **Prepare departure:** airport search → pre-fetch.
- **Settings:** cache sizes, deadlines, provider, coverage, dynamic zoom.
- **Packs (advanced):** import Ortho4XP output.
- **Health checks:** filesystem driver present, X-Plane found, Web API
  reachable, catalog reachable and signature valid, `scenery_packs.ini`
  ordering, mountpoint state, Defender exclusion (Windows).
- **About:** data attribution.

---

## 14. Testing

- **Unit:** tile math, filename parsing, `.ter` synthesis, DDS size/offset →
  band mapping (property tests), quadkeys, cache eviction, manifest
  parsing/signature verification.
- **Golden images:** synthesize a DDS from fixture tiles, decode with a BC1
  decoder, compare PSNR against reference.
- **Network:** mocked imagery server and mocked mesh origin; injected latency,
  failures, bad hashes, mirror failover, bad signatures.
- **Integration (Windows + Linux CI):** mount with a fixture catalog (WinFsp on
  hosted Windows runners, FUSE on Linux), read every file with random offsets,
  validate DDS headers/decodability and DSF hashes.
- **macOS:** manual/nightly on a real Mac.
- **Flight tests:** scripted XP12 flights around NYC recording degraded-band %,
  mesh-read waits and stutter.

---

## 15. Roadmap

### Spikes (before M1)
- **S1** — Filesystem backends: (a) `winfsp` crate on Windows — build, mount on
  a directory, async read completion, cache control; (b) macFUSE (FSKit mode,
  macOS 26) + `fuser` — build/link, read throughput for 11 MB files,
  keep-cache semantics; FUSE-T compatibility.
- **S2** — BC1 encode benchmark (`intel_tex_2` fast vs HQ), JPEG decode
  (`zune-jpeg` vs turbojpeg) on a 4096² band.
- **S3** — Inspect real Ortho4XP output: naming, `.ter` format, mask naming,
  ZL distribution.
- **S4** — X-Plane 12 behaviour on slow (multi-second) and failing DSF reads.
- **S5** — Pilot mesh build: Ortho4XP headless, NYC 3×3, our config. Measures
  build time and size per tile → global hosting estimate. Its output also
  feeds M1.

### Milestones
- **M1 — Headless Windows textures:** `xof-cli mount` the S5 NYC pack as an
  imported pack via WinFsp, Esri provider, T1 + T2 cache, DDS synthesis with
  deadlines/degradation, `.ter` synthesis. Fly NYC in XP12. Linux FUSE backend
  kept compiling and covered by CI.
- **M2 — Mesh streaming:** catalog + signing, `xof-publish` (dir and s3
  targets), mesh cache, offline behaviour, update swapping. NYC published to
  both a local static server and R2. Fly NYC from a clean machine with no
  imported packs.
- **M3 — Prefetch & platforms:** XP12 Web API prefetch with mesh-first
  priorities, Bing provider, dynamic zoom, PID filter, Linux + macOS flight
  tests, daemon + IPC.
- **M4 — UI & installer:** Tauri app, bundled WinFsp, X-Plane auto-detect,
  automatic `scenery_packs.ini`, Defender checkbox, prepare departure, health
  checks, attribution.
- **M5 — Coverage & polish:** US Northeast → NA + Europe builds, cache map,
  SimBrief route prefetch, HQ background re-encode, start-at-login.

### Future
- Worldwide coverage.
- Native FSKit backend (macOS).
- BC7 "Ultra" option — after verifying XP12 loads DX10-header DDS.
- Colour adjustments, per-region provider overrides, T3 pinned regions.

---

## 16. Risks

| Risk | Mitigation |
|---|---|
| macFUSE FSKit mode / WinFsp semantically different from Linux FUSE | S1 before M1; backend behind trait; shared conformance tests |
| X-Plane handles slow/failed DSF reads badly | S4; aggressive mesh prefetch; prepare departure; conservative listing when offline |
| We operate a mesh service (build + hosting) | Static immutable files only, no servers to run; two interchangeable origin setups; small predictable cost |
| Ortho4XP batch quality/failures at scale | Pilot (S5) first; automated per-tile checks; versioned config |
| Data licences (OSM ODbL, DEM) | OSM + Copernicus/SRTM only; attribution in app |
| WinFsp licence: GPLv3 with a FLOSS exception; closed-source distribution needs a commercial licence | Decided: xporthofs is MIT-licensed (satisfies the OSD, so the exception applies). Exception conditions we must keep — see §11.1 |
| Provider blocks / rate limits / ToS | Per-provider rate limits, one-time notice, multiple providers |
| X-Plane read patterns differ from assumptions | Instrument reads in M1; band design still works, only budgets change |
| Defender / indexers trigger mass fetches | PID filter + installer exclusion (§4.5) |
| Ortho4XP naming variants | S3; config-driven parser |

---

## 17. Rejected alternatives

- **Bring-your-own mesh only** — violates zero setup.
- **Transforming X-Plane's default DSFs in the VFS** (re-texturing the installed
  global mesh) — no hosting needed, but requires a full DSF reader/writer and
  triangle clipping with uncertain load-time cost; judged too risky for v1.
- **Native Rust mesh generator on the user's machine** — a project in itself
  (DEM, OSM, triangulation, DSF writing); minutes per tile.
- **Reusing AutoOrtho's published meshes** — external dependency, unverified
  redistribution terms, packaged as large region archives.
- **ProjFS / Cloud Files API, Dokan, NFS** for Windows — see §4.1.
