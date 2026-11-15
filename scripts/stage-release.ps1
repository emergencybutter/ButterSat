# Stage sidecar binaries and the pinned WinFsp MSI for `cargo tauri build`.
#   pwsh scripts/stage-release.ps1
#   cd crates/xof-ui; cargo tauri build --config tauri.release.conf.json
param(
  [string]$WinFspVersion = "2.1.25156",
  [string]$WinFspRelease = "v2.1",
  [string]$WinFspSha256 = "073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A"
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$ui = Join-Path $root "crates/xof-ui"
$triple = ((rustc -vV) | Select-String "^host:").ToString().Split(" ")[1]
cargo build --release -p xof-daemon -p xof-cli --manifest-path (Join-Path $root "Cargo.toml")
New-Item -ItemType Directory -Force (Join-Path $ui "binaries") | Out-Null
$ext = if ($IsWindows -or $env:OS -eq "Windows_NT") { ".exe" } else { "" }
foreach ($b in "xof-daemon", "xof-cli") {
  Copy-Item (Join-Path $root "target/release/$b$ext") (Join-Path $ui "binaries/$b-$triple$ext") -Force
}
if ($ext -eq ".exe") {
  $msi = Join-Path $ui "installer/winfsp.msi"
  if (-not (Test-Path $msi)) {
    $url = "https://github.com/winfsp/winfsp/releases/download/$WinFspRelease/winfsp-$WinFspVersion.msi"
    Invoke-WebRequest $url -OutFile $msi
  }
  $hash = (Get-FileHash $msi -Algorithm SHA256).Hash
  if ($WinFspSha256 -and $hash -ne $WinFspSha256) { throw "WinFsp MSI hash mismatch: $hash" }
  Write-Host "WinFsp MSI sha256: $hash"
} else {
  # Linux/macOS bundles have no installer resources.
  $cfg = Join-Path $ui "tauri.release.conf.json"
  (Get-Content $cfg -Raw) -replace '(?s),\s*"resources":\s*\{[^}]*\}', '' | Set-Content (Join-Path $ui "tauri.release.unix.conf.json")
  Write-Host "Use --config tauri.release.unix.conf.json on this platform."
}
Write-Host "Staged. Now: cd crates/xof-ui; cargo tauri build --config tauri.release.conf.json"
