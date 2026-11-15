# Release Process

This document covers how ButterSat releases are versioned, built, signed, staged for auto-updates, published to `satsrv.butr.cc` (`update-server`), and rolled back.

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

ButterSat uses two separate keypairs stored outside the repository in `C:\Users\me\Desktop\Projects\xporthofs-build\keys\` (never commit private keys to git or print them in logs):

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
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -Raw "C:\Users\me\Desktop\Projects\xporthofs-build\keys\updater.key").Trim()
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

- **Per-user install by default** (`installMode: "currentUser"`): installs into `%LOCALAPPDATA%\Programs\ButterSat` without requiring administrator rights when WinFsp is already installed.
- **WinFsp check**: checks `HKLM\SOFTWARE\WOW6432Node\WinFsp` (`InstallDir`). Only if WinFsp is missing does it elevate `msiexec.exe` to silently install the bundled unmodified `winfsp.msi` (satisfying the WinFsp FLOSS exception).
- **Windows Defender exclusion prompt**: during interactive installs, asks the user whether to add `%LOCALAPPDATA%\ButterSat` to Microsoft Defender real-time scanning exclusions.
- **Clean uninstall**: stops `xof-daemon` and unmounts the virtual filesystem, removes the `SCENERY_PACK Custom Scenery/z_buttersat/` line from `scenery_packs.ini` (leaving a timestamped `.bak`), removes the `HKCU\...\Run\ButterSat` login entry, and preserves user caches unless the user checks the "Delete app data" box in the uninstaller.

---

## 4. Publishing an Update (`satsrv.butr.cc`)

The Tauri updater polls `https://satsrv.butr.cc/updates/latest.json` at startup (and when clicking **Check for updates** in the About tab). When an update is available, the UI displays an **Install update** banner on the Overview tab and refuses to install while X-Plane is connected (`sim_connected == true`).

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
      "url": "https://satsrv.butr.cc/updates/ButterSat_0.1.0_x64-setup.exe"
    }
  }
}
```

### Step 2: Upload to `update-server`

Upload the versioned installer, its `.sig` file, and `latest.json` to `/var/www/buttersat/updates/` on `selfhost`:

```bash
ssh user@update-server "mkdir -p /var/www/buttersat/updates"
scp target/release/bundle/nsis/ButterSat_0.1.0_x64-setup.exe \
    target/release/bundle/nsis/ButterSat_0.1.0_x64-setup.exe.sig \
    latest.json \
    user@update-server:/var/www/buttersat/updates/
```

Ensure `/etc/nginx/conf.d/buttersat.conf` (or `/etc/nginx/sites-available/buttersat.conf`) matches [`deploy/nginx/buttersat.conf`](../deploy/nginx/buttersat.conf) and reload nginx:

```bash
ssh user@update-server "nginx -t && systemctl reload nginx"
```

Verify the feed and installer headers from outside:

```bash
curl -fsSL https://satsrv.butr.cc/updates/latest.json
curl -fsSI https://satsrv.butr.cc/updates/ButterSat_0.1.0_x64-setup.exe
```

---

## 5. Rolling Back a Release

Because versioned installer artifacts in `/var/www/buttersat/updates/` are immutable (`Cache-Control: public, max-age=31536000, immutable`) and `latest.json` has a short 5-minute TTL (`Cache-Control: public, max-age=300`):

1. **Revert `latest.json`**: point `latest.json` back to the previous known-good version's `version`, `url`, and `signature`, and upload it to `/var/www/buttersat/updates/latest.json` on `selfhost`.
2. **Purge or wait 5 minutes**: within 5 minutes (or immediately after purging `/updates/latest.json` in Cloudflare), clients checking for updates will see the previous release version.
3. **Emergency takedown**: if no update should be served at all, replace `latest.json` with a version matching the oldest deployed client (`0.1.0`) so `tauri-plugin-updater` sees `remote_version <= current_version` and reports "Up to date".
