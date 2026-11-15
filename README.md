# xporthofs

Streamed orthophoto scenery for **X-Plane 12** on Windows, Linux and macOS.

xporthofs mounts a virtual scenery pack in `Custom Scenery`. When X-Plane
reads it:

- **meshes** (DSF) are downloaded on demand from a signed catalog, then
  completed locally with the water and season layers from your own X-Plane
  Global Scenery;
- **textures** (DDS) are built on the fly from satellite imagery (Esri or
  Bing), one band at a time, within a time budget so X-Plane never stalls.

Nothing to pre-download: install, launch, fly. Design: [`docs/design.md`](docs/design.md).

## Components

| Crate | What |
|---|---|
| `xof-core` | tile math, Ortho4XP names, DDS layout, BC1, imagery providers, caches, texture engine |
| `xof-mesh` | DSF raster splice, signed catalog, mesh streaming |
| `xof-vfs` | the virtual filesystem: WinFsp (Windows), FUSE (Linux, macOS via macFUSE) |
| `xof-daemon` | background service: mount, prefetch, X-Plane Web API, IPC |
| `xof-cli` | command-line control; `xof-cli run` for a standalone test mount |
| `xof-publish` | build, sign and upload the mesh catalog |
| `xof-ui` | Tauri control app (`xporthofs`) |
| `xof-bench` | automated in-sim A/B benchmark: drives X-Plane, slews between spots, scores nadir screenshots ([`docs/bench.md`](docs/bench.md)) |

## Requirements

- X-Plane 12 (Web API enabled — the default — for prefetch).
- Windows 10/11: WinFsp (bundled with the installer).
- Linux: FUSE 3 (`fuse3` package).
- macOS 26+: [macFUSE](https://macfuse.github.io) — the one manual install.

## Build

```sh
cargo build --release          # all crates
cargo test --workspace --exclude xof-ui
```

Run the app from source: `cargo run -p xof-ui` (it starts `xof-daemon` from the
same target folder).

Installers: `pwsh scripts/stage-release.ps1`, then
`cd crates/xof-ui && cargo tauri build --config tauri.release.conf.json`.

## Using the CLI

```sh
xof-cli status            # mount, X-Plane, imagery, mesh, prefetch
xof-cli health            # what's wrong and how to fix it
xof-cli prepare KJFK      # warm meshes + imagery around an airport
xof-cli route             # prefetch along your SimBrief plan
xof-cli set imagery.provider bing
xof-cli run --pack path/to/zOrtho4XP_+40-074 --mount X:/mnt   # standalone test mount
```

Config lives in the per-user config dir (`xporthofs/config.toml`); every key
has a working default. `XOF_HOME` relocates config, data and caches.

## Publishing a mesh catalog

```sh
xof-publish keygen --out signing.key            # prints the public key
xof-publish build --tiles Ortho4XP/Tiles --out publish --key signing.key \
    --version 2026.11.1 --mirror https://mesh.example.org/v1
xof-publish upload --dir publish --target s3 \
    --endpoint https://<account>.r2.cloudflarestorage.com --bucket mesh   # or --target dir --dest /srv/mesh/v1
xof-publish verify --origin https://mesh.example.org/v1 --pubkey <hex>
```

Clients trust catalogs signed by keys in `[mesh].public_keys`. Hosted DSFs
never contain Laminar data: `build` refuses DSFs with raster layers unless
`--strip-rasters` is given.

## Licence and notices

MIT — see [`LICENSE`](LICENSE).

- WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos —
  <https://github.com/winfsp/winfsp> (bundled unmodified under its FLOSS exception).
- MapLibre GL JS (BSD-3-Clause), vendored in `crates/xof-ui/ui/vendor`.
- Mesh data © OpenStreetMap contributors (ODbL); elevation: USGS 3DEP / Copernicus DEM.
- Imagery belongs to the selected provider; use it according to their terms.
