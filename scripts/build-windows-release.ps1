# Builds a Windows .msi installer via `cargo wix` -- the Windows analog
# of scripts/build-deb.sh. Run this from a normal PowerShell prompt on
# the Windows machine already set up for the MSVC+vcpkg build (see
# README.md's "Windows via MSVC" section) -- this script assumes that
# environment already works, it doesn't set it up.
#
# Unlike build-deb.sh, there's no local revision counter here: WiX's
# <MajorUpgrade> element (wix/main.wxs) already handles reinstalling a
# genuinely newer version cleanly, so the only thing that needs bumping
# between releases is Cargo.toml's own `version` field, the same as any
# other platform's release.
#
# One-time setup this script does NOT do for you (matching build-deb.sh
# not running `cargo install cargo-deb` either):
#   cargo install cargo-wix
#   Install the WiX Toolset v3 (https://wixtoolset.org/) and make sure
#   candle.exe/light.exe are on PATH, or set the WIX env var to its
#   install directory (the WiX installer normally does this for you).

$ErrorActionPreference = "Stop"

if (-not (Get-Command cargo-wix -ErrorAction SilentlyContinue)) {
    Write-Error "cargo-wix not found. Install it once with: cargo install cargo-wix"
    exit 1
}

Write-Host "Building .msi with cargo wix..."
cargo wix @args
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$msi = Get-ChildItem -Path "target\wix" -Filter "*.msi" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($msi) {
    Write-Host "Built: $($msi.FullName)"
}
