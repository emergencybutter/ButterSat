# Release Process

This document covers how ButterSat releases are versioned, built, signed, staged for auto-updates, published to `buttersat.butr.cc/updates/` (the update server, reached over SSH as `$BUTTERSAT_RELEASE_HOST`), and rolled back.

---

## 1. Versioning

ButterSat uses Semantic Versioning (`MAJOR.MINOR.PATCH`). When cutting a release, keep the version synchronized in two places:

1. [`Cargo.toml`](../Cargo.toml) (`[workspace.package] version = "X.Y.Z"`)
2. [`crates/xof-ui/tauri.conf.json`](../crates/xof-ui/tauri.conf.json) (`"version": "X.Y.Z"`)

Ensure all tests, formatting, and license checks pass before building a release:

```powershell
cargo fmt --all --check
cargo test --workspace --exclude xof-ui
cargo build -p xof-ui
```

---

## 2. Signing Keys

ButterSat uses two separate keypairs stored outside the repository, in `xporthofs-build\keys\` next to the repository checkout (`BUTTERSAT_UPDATER_KEY` overrides the updater key's path) (never commit private keys to git or print them in logs):

- **Mesh catalog signing key** (`release-signing.key` / `release-signing.pub`, Ed25519): signs `manifest.json.zst` via `xof-publish`. Its public key is embedded in `crates/xof-proto/src/config.rs` (`DEFAULT_MESH_KEYS`).
- **Tauri auto-updater signing key** (`updater.key` / `updater.key.pub`, Minisign): signs Windows NSIS installer bundles (`*.exe.sig`) for `tauri-plugin-updater`. Its public key is embedded in [`crates/xof-ui/tauri.conf.json`](../crates/xof-ui/tauri.conf.json) under `plugins.updater.pubkey`.

---

## 3. Building & Staging the Windows Installer

### Step 1: Stage sidecars and WinFsp MSI

Run [`scripts/stage-release.ps1`](../scripts/stage-release.ps1) from the repository root:

```powershell
pwsh scripts/stage-release.ps1
```

This script:
1. Builds `xof-daemon.exe` and `xof-cli.exe` in `--release` mode.
2. Copies them to `crates/xof-ui/binaries/xof-daemon-x86_64-pc-windows-msvc.exe` and `crates/xof-ui/binaries/xof-cli-x86_64-pc-windows-msvc.exe`.
3. Downloads the unmodified WinFsp MSI (`winfsp-2.1.25156.msi`) to `crates/xof-ui/installer/winfsp.msi` if not already present and verifies its pinned SHA-256 checksum (`073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A`).

### Step 2: Build the signed NSIS installer

Provide the updater private key via `TAURI_SIGNING_PRIVATE_KEY` (and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""` for a passwordless key) and run `cargo tauri build`:

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -Raw "..\xporthofs-build\keys\updater.key").Trim()
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
Push-Location crates/xof-ui
cargo tauri build --bundles nsis --config tauri.release.conf.json
Pop-Location
Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

This produces:
- `target/release/bundle/nsis/ButterSat_<version>_x64-setup.exe`
- `target/release/bundle/nsis/ButterSat_<version>_x64-setup.exe.sig`

### What the installer does

- **Per-user install by default** (`installMode: "currentUser"`): installs into `%LOCALAPPDATA%\ButterSat` (next to the cache and logs; the uninstaller deletes only the files it installed, so caches survive unless "Delete app data" is ticked) without requiring administrator rights when WinFsp is already installed.
- **WinFsp check**: checks `HKLM\SOFTWARE\WOW6432Node\WinFsp` (`InstallDir`). Only if WinFsp is missing does it elevate `msiexec.exe` to silently install the bundled unmodified `winfsp.msi` (satisfying the WinFsp FLOSS exception).
- **Windows Defender exclusion prompt**: during interactive installs, asks the user whether to add `%LOCALAPPDATA%\ButterSat` to Microsoft Defender real-time scanning exclusions.
- **Clean uninstall**: stops `xof-daemon` and unmounts the virtual filesystem, removes the `SCENERY_PACK Custom Scenery/z_buttersat/` line from `scenery_packs.ini` (leaving a timestamped `.bak`), removes the `HKCU\...\Run\ButterSat` login entry, and preserves user caches unless the user checks the "Delete app data" box in the uninstaller.

---

## 4. Publishing an Update (`buttersat.butr.cc/updates`)

The app checks `https://buttersat.butr.cc/updates/latest.json` (0.1.8+; falls back to `https://satsrv.butr.cc/updates/latest.json`, the only feed address of 0.1.0–0.1.7, served from the same folder: keep that DNS name until those versions are gone) 20 s after it starts and every 6 hours (also **Check for updates** in About). With **Install updates automatically** on (default, `startup.auto_update`), it installs by itself as soon as X-Plane is neither running nor connected (re-checked every 5 minutes); otherwise the Overview banner's **Install update** button does the same.

One update replaces the app **and the daemon and CLI** (sidecars in the same NSIS bundle): the app stops the daemon (plain shutdown, the On/Off switch is untouched), the updater runs the installer with `/UPDATE` and exits, `NSIS_HOOK_PREINSTALL` stops any daemon again, the files are replaced, and the installer relaunches the app, which starts the new daemon if ButterSat is on. Verified end-to-end on 2026-11-16 (0.1.2 → 0.1.3 test builds): all three executables replaced, daemon log `0.1.2 … shutdown requested … 0.1.3 starting … mounted` within 8 s, no user action.

Updates only run while the app is open: a daemon started at login without the app does not update until the app is opened.

**Testing an update locally:** build two versions with a config overlay `{"plugins":{"updater":{"endpoints":["http://127.0.0.1:8732/latest.json"],"dangerousInsecureTransportProtocol":true}}}` (`--config overlay.json`), serve the newer one's `latest.json` + installer with `python -m http.server 8732`, install the older one, start it with `BUTTERSAT_UPDATE_DELAY_S=5`. Never publish overlay builds, and rebuild `target/release` without the overlay afterwards.

### Step 1: Generate `latest.json`

Create `latest.json` with the signature from `ButterSat_<version>_x64-setup.exe.sig`:

```json
{
  "version": "0.1.0",
  "notes": "ButterSat 0.1.0 release",
  "pub_date": "2026-04-15T00:00:00Z",
  "platforms": {
    "windows-x86_64": {
      "signature": "<contents of ButterSat_0.1.0_x64-setup.exe.sig>",
      "url": "https://buttersat.butr.cc/updates/ButterSat_0.1.0_x64-setup.exe"
    }
  }
}
```

### Step 2: Upload to the update server

`BUTTERSAT_RELEASE_HOST` holds the server's SSH destination (`user@host`), kept out of the repository; `tools/release/publish_release.py` reads it too.

Upload the versioned installer, its `.sig` file, and `latest.json` to `/var/www/buttersat/updates/` on the update server:

```bash
ssh "$BUTTERSAT_RELEASE_HOST" "mkdir -p /var/www/buttersat/updates"
scp target/release/bundle/nsis/ButterSat_0.1.0_x64-setup.exe \
    target/release/bundle/nsis/ButterSat_0.1.0_x64-setup.exe.sig \
    latest.json \
    "$BUTTERSAT_RELEASE_HOST":/var/www/buttersat/updates/
```

Ensure `/etc/nginx/conf.d/buttersat.conf` (or `/etc/nginx/sites-available/buttersat.conf`) matches [`deploy/nginx/buttersat.conf`](../deploy/nginx/buttersat.conf) and reload nginx:

```bash
ssh "$BUTTERSAT_RELEASE_HOST" "nginx -t && systemctl reload nginx"
```

Verify the feed and installer headers from outside:

```bash
curl -fsSL https://buttersat.butr.cc/updates/latest.json
curl -fsSI https://buttersat.butr.cc/updates/ButterSat_0.1.0_x64-setup.exe
```

---

## 5. Rolling Back a Release

Because versioned installer artifacts in `/var/www/buttersat/updates/` are immutable (`Cache-Control: public, max-age=31536000, immutable`) and `latest.json` has a short 5-minute TTL (`Cache-Control: public, max-age=300`):

1. **Revert `latest.json`**: point `latest.json` back to the previous known-good version's `version`, `url`, and `signature`, and upload it to `/var/www/buttersat/updates/latest.json` on the update server.
2. **Purge or wait 5 minutes**: within 5 minutes (or immediately after purging `/updates/latest.json` in Cloudflare), clients checking for updates will see the previous release version.
3. **Emergency takedown**: if no update should be served at all, replace `latest.json` with a version matching the oldest deployed client (`0.1.0`) so `tauri-plugin-updater` sees `remote_version <= current_version` and reports "Up to date".

---

## All platforms in one feed (Windows, macOS, Linux)

macOS and Linux bundles are built by GitHub Actions; everything is signed on
the release machine, where the updater key stays.

1. Bump the version (`Cargo.toml`, `crates/xof-ui/tauri.conf.json`), commit, push.
2. Build and sign the Windows installer locally (section 3).
3. `gh workflow run release-bundles.yml --ref main` and wait for it to pass
   (macOS arm64 and x86_64: `.dmg` + `.app.tar.gz`; Linux: `.AppImage` + `.deb`).
4. `python tools/release/publish_release.py X.Y.Z` (dry run), then add `--go`:
   downloads the CI artifacts, signs the macOS `.app.tar.gz` and the Linux
   AppImage/`.deb` with `updater.key`, uploads all files, writes `latest.json`
   with `windows-x86_64`, `darwin-aarch64`, `darwin-x86_64`,
   `linux-x86_64-appimage` (also `linux-x86_64`) and `linux-x86_64-deb`, and
   checks every public URL against the local SHA-256.
5. Point the website's download links at the new version and deploy `site/`.

Platform notes: on macOS and Linux the updater replaces the bundle in place and
the app restarts itself into the new version (which starts the new daemon). A
`.deb` update asks for the user's password (`pkexec dpkg -i`). AppImages run
the daemon and CLI from copies in `~/.local/share/ButterSat/bin`, because the
AppImage mount disappears when the window closes.
