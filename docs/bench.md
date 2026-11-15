# xof-bench: automated in-sim A/B testing

`xof-bench` measures what a pilot sees, without a pilot. It drives X-Plane 12.4+
through the local Web API and compares scenery **variants** at a list of
**spots**:

1. For each variant: kill X-Plane, set up `scenery_packs.ini`, start an
   isolated xporthofs daemon if the variant needs one, then launch X-Plane.
2. Start a flight at the first spot (`POST /api/v3/flight`, clear calm weather,
   fixed day and time) and freeze the aircraft (`override_planepath`).
3. Point the camera straight down: pilot's head 4 m below the fuselage
   (outside the 3D cockpit), pitch –90°, fixed FOV, north up.
4. Slew to each spot at a fixed speed by writing `local_x/y/z` in a closed
   loop on true lat/lon. X-Plane loads scenery as it would in flight.
5. At each spot, reset the time of day, then take screenshots at fixed
   offsets after arrival (`sim/operation/screenshot`).
6. Record frame times all along, and the daemon's engine counters per spot.
7. Restore `scenery_packs.ini`, remove links, and score the screenshots.

```sh
cargo build --release -p xof-bench -p xof-daemon
target/release/xof-bench run bench/nyc.toml                      # all variants
target/release/xof-bench run bench/nyc.toml --variants reference,xof
target/release/xof-bench run bench/nyc.toml --variants xof-warm --repeat 2
target/release/xof-bench report bench-runs/nyc-<stamp>           # re-score
```

Results land in `bench-runs/<scenario>-<unix time>/`: `run.json` (raw record),
`report.html`, `shots/` (JPEG + thumbnails; `--keep-png` keeps the 7 MB
originals) and `homes/` (daemon logs, config and caches of cold runs).

## Scenario file

See `bench/nyc.toml`. Top level: `name`, `aircraft`, `day_of_year`,
`local_time_h`, `slew_speed_kt`, `fov_deg`, `shots_s`, `origin` (a local
catalog folder containing `v1/`, served on 127.0.0.1 for `xof` variants),
`public_keys`, `disable_packs`, `isolate_packs` (default `true`: every other
Custom Scenery pack is disabled, including folders missing from the ini, so
only X-Plane's base scenery and global airports remain), `compare_to`.
`[[spot]]` has `name`, `lat`, `lon`, `alt_ft` (MSL) and an optional
`slew_speed_kt` for the leg that ends there. `[[variant]]` has:

| key | meaning |
|---|---|
| `xof` | mount xporthofs (isolated `XOF_HOME`, bench-generated config) |
| `packs` | static scenery folders, linked into Custom Scenery as `z_xofbench_*` |
| `cache` | `cold` (fresh caches every run) or `persistent` (`bench-runs/homes/<variant>`) |
| `[variant.config]` | daemon config overrides, e.g. `[variant.config.imagery] deadline_ms = 600` |

## Metrics

- **Similarity**: SSIM of the centre 70 % (edges can carry X-Plane
  notifications) against the `compare_to` variant's final shot of the same
  spot. Similarity at t=0 means "what you see on arrival"; final means
  "what it converges to".
- **Time to sharp**: first shot whose sharpness (mean squared Laplacian:
  blurry fallback bands lose high frequencies) reaches 90 % of the run's final
  shot.
- **Frames**: `sim/time/framerate_period` sampled every 50 ms. **Stalls**
  are Web API round trips over 100 ms: X-Plane serves requests between frames,
  so a slow answer is a hitch even when the period is averaged.
- **Daemon**: DDS reads, degraded bands, upgrades, reads slower than 1 s,
  imagery fetches (deltas over the run and per spot in `run.json`).

## X-Plane facts this relies on (verified on 12.4.4)

- Datarefs exist only once a flight is loaded: at the main menu the count is
  0, and every lookup fails.
- Weather presets have no clear sky; the bench passes a custom definition.
- `sim/operation/quit` asks for confirmation, so the bench kills the process.
  X-Plane is only reading during a run.
- Use `127.0.0.1`: `localhost` costs about 200 ms per request (IPv6 fallback).
- A REST request is answered in about one frame (16–33 ms).

## Caveats

- Runs take over the machine: X-Plane is killed and relaunched per variant.
  Don't run while flying. Close the xporthofs app first, since the bench runs
  its own daemon.
- X-Plane plugins still run; notifications and traffic can appear in shots,
  which is one reason for the centre crop.
- The reference pack must use the same mesh as the catalog, or similarity
  measures mesh differences as well as texture differences.
