# AGENT_REPORT — Windows Release (`agent/release`)

## 1. What Was Done

### 1.1 CI Green Across Windows, Linux, and macOS
- **Fixed `other_readers_never_fetch_high_zoom` failure on Linux (`xof-core`)**:
  - In [`crates/xof-core/src/engine.rs`](crates/xof-core/src/engine.rs), `TextureEngine::build_segment` previously scheduled background high-zoom upgrades (`self.schedule_upgrade(key)`) whenever `buf.degraded` was true, even when `ctx.class == ReaderClass::Other`. Gated both degraded-segment RAM caching and background upgrade scheduling on `ctx.class == ReaderClass::XPlane`.
- **Fixed `full_read_uses_matching_zoom_per_mip` timeout on Windows (`xof-core`)**:
  - In [`crates/xof-core/tests/engine.rs`](crates/xof-core/tests/engine.rs), `serve()` previously re-encoded a 256×256 PNG in debug mode on every HTTP tile request (682 times across the test suite). Pre-encoded the per-zoom test PNGs once at server startup and raised the test deadline from `10_000` to `60_000` ms, bringing `cargo test -p xof-core` runtime down to `1.76 s`.
- **Workflow & Warnings**:
  - Updated [`.github/workflows/ci.yml`](.github/workflows/ci.yml) push trigger to include `'agent/**'`.
  - Silenced unused `const NAME` warning on non-Windows targets in [`crates/xof-daemon/src/autostart.rs`](crates/xof-daemon/src/autostart.rs) and unused `thiserror` dependency in [`crates/xof-core/Cargo.toml`](crates/xof-core/Cargo.toml).

### 1.2 Windows NSIS Installer & Clean Uninstall
- **Pinned WinFsp MSI (`scripts/stage-release.ps1`)**:
  - Staged `xof-daemon.exe` and `xof-cli.exe` sidecars into `crates/xof-ui/binaries/` and downloaded the unmodified WinFsp v2.1 MSI (`winfsp-2.1.25156.msi`) to `crates/xof-ui/installer/winfsp.msi`.
  - Pinned its SHA-256 (`073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A`) in [`scripts/stage-release.ps1`](scripts/stage-release.ps1).
- **Per-user Install & NSIS Hooks (`crates/xof-ui/tauri.conf.json`, `crates/xof-ui/installer/hooks.nsh`)**:
  - Configured `"installMode": "currentUser"` (`RequestExecutionLevel user`) so ButterSat installs into `%LOCALAPPDATA%\ButterSat` without requiring administrator privileges when WinFsp is already present.
  - Fixed `"category": "Simulation Game"` in [`crates/xof-ui/tauri.conf.json`](crates/xof-ui/tauri.conf.json) to satisfy Tauri v2 bundler validation.
  - `NSIS_HOOK_PREINSTALL`: stops a running daemon via `"$INSTDIR\xof-cli.exe" shutdown` before overwriting binaries during an upgrade.
  - `NSIS_HOOK_POSTINSTALL`: checks `HKLM\SOFTWARE\WOW6432Node\WinFsp` and `HKLM\SOFTWARE\WinFsp` (`InstallDir`); only if WinFsp is missing does it elevate `msiexec.exe` (`-Verb RunAs`) to install the bundled unmodified `resources\winfsp.msi`. Prompts interactively (never during silent/passive updates) to exclude `%LOCALAPPDATA%\ButterSat` from Microsoft Defender real-time scanning.
  - `NSIS_HOOK_PREUNINSTALL` & `NSIS_HOOK_POSTUNINSTALL`: distinguishes `$UpdateMode = 1` (stops daemon only, preserving `scenery_packs.ini` and caches) from full uninstall (`xof-cli uninstall`, or `xof-cli uninstall --remove-cache` + deleting `%LOCALAPPDATA%\ButterSat` and `%APPDATA%\ButterSat` when the uninstaller's "Delete app data" checkbox is ticked).
- **Clean Uninstall CLI & Library Support (`crates/xof-proto/src/xplane.rs`, `crates/xof-cli/src/main.rs`)**:
  - Added `remove_pack_entry`, `remove_named_pack_entry`, and `remove_scenery_pack` (with timestamped `.bak` backup and empty `Custom Scenery/z_buttersat` removal) in [`crates/xof-proto/src/xplane.rs`](crates/xof-proto/src/xplane.rs).
  - Updated `xof-cli shutdown` to succeed cleanly if the daemon is not running and wait until the IPC socket and `daemon.lock` are released.
  - Added `xof-cli uninstall [--remove-cache]` in [`crates/xof-cli/src/main.rs`](crates/xof-cli/src/main.rs) to stop the daemon, unmount, remove the `HKCU\...\Run\ButterSat` login entry, remove `SCENERY_PACK Custom Scenery/z_buttersat/` from `scenery_packs.ini`, and (only when `--remove-cache` is passed) delete user caches (including relocated `storage.cache_path`), data, and config.

### 1.3 Auto-Updater (`tauri-plugin-updater`)
- **Minisign Keypair**:
  - Generated Tauri updater Minisign keypair at:
    - Private key: `C:\Users\me\Desktop\Projects\xporthofs-build\keys\updater.key` (kept outside the repo; never logged or committed)
    - Public key: `C:\Users\me\Desktop\Projects\xporthofs-build\keys\updater.key.pub` (`dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDhCODhDNzVDRDRFMjcxNEMKUldSTWNlTFVYTWVJaTJMTGNJck8zY2lOdzBYWFFSU2w5ZlArUUl4azg1dEhIQm5yT2ZrNkp1ekkK`)
  - Embedded the public key and endpoint `https://satsrv.butr.cc/updates/latest.json` in [`crates/xof-ui/tauri.conf.json`](crates/xof-ui/tauri.conf.json) with `"createUpdaterArtifacts": true`.
- **UI & Backend Integration (`crates/xof-ui`)**:
  - Added `tauri-plugin-updater = "2"` to [`crates/xof-ui/Cargo.toml`](crates/xof-ui/Cargo.toml) and `"updater:default"` to [`crates/xof-ui/capabilities/default.json`](crates/xof-ui/capabilities/default.json).
  - Added `update_check` and `update_install` commands in [`crates/xof-ui/src/main.rs`](crates/xof-ui/src/main.rs). `update_install` verifies `sim_connected == false` before and after downloading the update, stops `xof-daemon` cleanly so files are unlocked, and runs the installer.
  - Updated [`crates/xof-ui/ui/index.html`](crates/xof-ui/ui/index.html), [`crates/xof-ui/ui/app.js`](crates/xof-ui/ui/app.js), and [`crates/xof-ui/ui/app.css`](crates/xof-ui/ui/app.css) to check for updates automatically at startup and on demand in the About tab, showing an **Install update** banner on Overview that disables itself whenever X-Plane is connected.
- **Nginx & Release Docs**:
  - Updated [`deploy/nginx/buttersat.conf`](deploy/nginx/buttersat.conf) with `/updates/latest.json` (`max-age=300`) and `/updates/` (`max-age=31536000, immutable`).
  - Wrote [`docs/release.md`](docs/release.md) documenting versioning, keys, building, staging, publishing to `selfhost`, and rolling back.

---

## 2. Produced Installer & Update Artifacts

All release artifacts were built locally in the worktree (`C:\Users\me\Desktop\Projects\ButterSat-wt-release\target\release\bundle\nsis\`):

| Artifact | Path | Size | SHA-256 |
|---|---|---|---|
| **NSIS Installer** | `target\release\bundle\nsis\ButterSat_0.1.0_x64-setup.exe` | `13,247,035` bytes (12.63 MiB) | `856ca69301e4c0dcd90247f59a5e5292677028313eb162379e1f33b9d3e1f8a2` |
| **Minisign Signature** | `target\release\bundle\nsis\ButterSat_0.1.0_x64-setup.exe.sig` | `358` bytes | — |
| **Update Feed JSON** | `target\release\bundle\nsis\latest.json` | `583` bytes | — |
| **Bundled WinFsp MSI** | `crates\xof-ui\installer\winfsp.msi` (`winfsp-2.1.25156.msi`) | `2,953,216` bytes | `073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A` |

---

## 3. What Was Verified & How

1. **GitHub Actions CI (`43764318742` on `agent/release`)**:
   - All 5 jobs passed on GitHub Actions:
     - `fmt` (16s)
     - `licences and advisories` / `cargo-deny` (48s)
     - `test (macos-latest)` (2m43s)
     - `test (ubuntu-latest)` (8m04s)
     - `test (windows-latest)` (13m24s)
2. **Local Workspace Test Suite**:
   - `cargo test --workspace --exclude xof-ui --no-fail-fast`: all 66 unit and integration tests across `xof-bench`, `xof-cli`, `xof-core`, `xof-daemon`, `xof-mesh`, `xof-proto`, `xof-publish`, and `xof-vfs` (including `mount_and_read_through_os` and the new `removes_pack_entry_and_is_idempotent` test) passed.
3. **Static NSIS Script Verification**:
   - Inspected generated `target\release\nsis\x64\installer.nsi` to verify:
     - `RequestExecutionLevel user` is set (`INSTALLMODE = "currentUser"`).
     - `buttersat.exe`, `xof-daemon.exe`, `xof-cli.exe`, and `resources\winfsp.msi` are packaged into `$INSTDIR` and deleted on uninstall.
     - `hooks.nsh` macros (`NSIS_HOOK_PREINSTALL`, `NSIS_HOOK_POSTINSTALL`, `NSIS_HOOK_PREUNINSTALL`, `NSIS_HOOK_POSTUNINSTALL`) are included and invoked in the right sections.
4. **Isolated Dry-Run Verification (without touching the user's running ButterSat)**:
   - Windows Sandbox (`C:\Windows\System32\WindowsSandbox.exe`) is not installed on this machine.
   - Tested the staged release binaries (`xof-daemon-x86_64-pc-windows-msvc.exe` and `xof-cli-x86_64-pc-windows-msvc.exe`) in complete isolation by overriding `USERNAME=buttersat-dryrun` (so the named pipe `buttersat-buttersat-dryrun` never collided with the user's running `buttersat-arnau` daemon) and `XOF_HOME` pointing to a temporary directory with a mock X-Plane 12 installation and `scenery_packs.ini`:
     1. Started `xof-daemon --no-mount` and verified `xof-cli status` connected and reported the mock X-Plane 12 install.
     2. Ran `xof-cli uninstall` (without `--remove-cache`): verified the daemon exited cleanly with code `0`, `SCENERY_PACK Custom Scenery/z_buttersat/` and the empty `Custom Scenery/z_buttersat` folder were removed (with a `.bak` file created), and the `cache` directory was preserved.
     3. Ran `xof-cli uninstall --remove-cache`: verified `cache`, `data`, and `config.toml` were deleted.

---

## 4. For the User to Run

### 4.1 Deploy the `/updates/` Endpoint and Update Feed to `selfhost`

Run the following commands from a terminal with SSH access to `update-server`:

```powershell
# 1. Create the updates directory on selfhost (note: check whether nginx root on selfhost is /srv/buttersat or /var/www/buttersat;
#    deploy/nginx/buttersat.conf uses root /srv/buttersat, so create /srv/buttersat/updates and symlink /var/www/buttersat/updates if needed)
ssh user@update-server "mkdir -p /srv/buttersat/updates /var/www/buttersat && ln -sfn /srv/buttersat/updates /var/www/buttersat/updates"

# 2. Upload the updated nginx config and reload nginx
scp C:\Users\me\Desktop\Projects\ButterSat-wt-release\deploy\nginx\buttersat.conf user@update-server:/etc/nginx/conf.d/buttersat.conf
ssh user@update-server "nginx -t && systemctl reload nginx"

# 3. Upload the signed installer, .sig, and latest.json to /srv/buttersat/updates/
scp C:\Users\me\Desktop\Projects\ButterSat-wt-release\target\release\bundle\nsis\ButterSat_0.1.0_x64-setup.exe `
    C:\Users\me\Desktop\Projects\ButterSat-wt-release\target\release\bundle\nsis\ButterSat_0.1.0_x64-setup.exe.sig `
    C:\Users\me\Desktop\Projects\ButterSat-wt-release\target\release\bundle\nsis\latest.json `
    user@update-server:/srv/buttersat/updates/

# 4. Verify the update feed and installer over HTTPS from outside
curl.exe -fsSL https://satsrv.butr.cc/updates/latest.json
curl.exe -fsSI https://satsrv.butr.cc/updates/ButterSat_0.1.0_x64-setup.exe
```

### 4.2 Manual End-to-End Installer / Uninstaller / Auto-Update Verification Steps

When you are ready to test the full installer UI on your host (or in a VM):

1. **Close your currently running ButterSat UI and daemon** (`xof-cli shutdown`).
2. **Run the installer**:
   ```powershell
   C:\Users\me\Desktop\Projects\ButterSat-wt-release\target\release\bundle\nsis\ButterSat_0.1.0_x64-setup.exe
   ```
   - Verify it installs per-user into `%LOCALAPPDATA%\ButterSat` without prompting for UAC (since WinFsp is already installed).
   - Verify it offers the Microsoft Defender exclusion prompt (`Yes`/`No`).
   - Launch ButterSat from the Finish page; verify the UI opens, starts `xof-daemon.exe`, mounts `Custom Scenery/z_buttersat`, and reports `Up to date (0.1.0)` when clicking **Check for updates** in the **About** tab.
3. **Test auto-update detection**:
   - On `selfhost`, temporarily edit `/srv/buttersat/updates/latest.json` to set `"version": "0.1.1"` (leaving `url` and `signature` pointing to `ButterSat_0.1.0_x64-setup.exe`).
   - Click **Check for updates** in ButterSat **About** (or restart the UI): verify the blue update banner appears on **Overview** (`ButterSat 0.1.1 is available`) with the **Install update** button, and verify the button disables itself if X-Plane 12 is running.
   - Restore `"version": "0.1.0"` in `/srv/buttersat/updates/latest.json` afterward.
4. **Test clean uninstall**:
   - Run `%LOCALAPPDATA%\ButterSat\uninstall.exe` (or uninstall **ButterSat** from Windows *Installed apps*).
   - Leave **Delete app data** unchecked: verify `xof-daemon.exe` stops, `Custom Scenery/z_buttersat` is unmounted and removed from `scenery_packs.ini`, and `%LOCALAPPDATA%\ButterSat` cache files remain intact.
5. **Re-stage if needed after any code changes**:
   - Note that `cargo test` in the worktree overwrites `target\release` or `target\debug` artifacts only for the profile under test, and the staged NSIS installer at `C:\Users\me\Desktop\Projects\ButterSat-wt-release\target\release\bundle\nsis\ButterSat_0.1.0_x64-setup.exe` remains ready to use.
