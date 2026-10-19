#!/usr/bin/env bash
#
# Builds hpsdr-rs-<version>-<arch>.AppImage in the repository root.
#
# Usage (from anywhere):  scripts/build-appimage.sh
#
# Needs the normal build prerequisites (Rust, a C compiler, pkg-config,
# libfftw3-dev, libasound2-dev) plus curl, libxkbcommon0 and
# libxkbcommon-x11-0 (bundled, see below). linuxdeploy and its AppImage
# plugin are downloaded into target/appimage-tools on first use. Runs
# without FUSE (APPIMAGE_EXTRACT_AND_RUN), so it works in containers/CI.
#
# Build on the OLDEST distro you want the AppImage to run on: it can't
# run on a system whose glibc is older than the build machine's.
# libfftw3 is bundled; libasound, libGL/Vulkan, X11/Wayland come from
# the host (linuxdeploy's standard excludelist), as they must.

set -euo pipefail

# Pinned so a build is reproducible; bump deliberately.
LINUXDEPLOY_TAG="1-alpha-20251107-1"
APPIMAGE_PLUGIN_TAG="1-alpha-20250213-1"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

arch="$(uname -m)"
case "$arch" in
  x86_64|aarch64) ;;
  *) echo "Unsupported architecture: $arch" >&2; exit 1 ;;
esac

version="$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2)"
tools="$root/target/appimage-tools"
appdir="$root/target/AppDir"
mkdir -p "$tools"

fetch() {
  local url="$1" out="$2"
  if [ ! -x "$out" ]; then
    echo "Downloading $(basename "$out")"
    curl -fL --retry 3 -o "$out.part" "$url"
    mv "$out.part" "$out"
    chmod +x "$out"
  fi
}
fetch "https://github.com/linuxdeploy/linuxdeploy/releases/download/${LINUXDEPLOY_TAG}/linuxdeploy-${arch}.AppImage" \
      "$tools/linuxdeploy-${arch}.AppImage"
fetch "https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/${APPIMAGE_PLUGIN_TAG}/linuxdeploy-plugin-appimage-${arch}.AppImage" \
      "$tools/linuxdeploy-plugin-appimage-${arch}.AppImage"

# Portable build: no -march=native (see build.rs).
HPSDR_PORTABLE=1 cargo build --release

rm -rf "$appdir"
install -Dm755 target/release/hpsdr-rs "$appdir/usr/bin/hpsdr-rs"
install -Dm644 assets/io.github.g0orx.hpsdr-rs.metainfo.xml \
  "$appdir/usr/share/metainfo/io.github.g0orx.hpsdr-rs.metainfo.xml"

# Ozy and RX-888 firmware, where ozy.rs/rx888.rs's bundled_path looks
# (usr/bin/hpsdr-rs -> usr/share/hpsdr-rs/...), same as the .deb.
install -Dm644 -t "$appdir/usr/share/hpsdr-rs/ozy" assets/ozy/*
install -Dm644 -t "$appdir/usr/share/hpsdr-rs/rx888" assets/rx888/*

# Reference copies, same as the .deb ships: the manual, and the udev
# rules a user installs by hand for USB radios (an AppImage can't
# install them itself).
docdir="$appdir/usr/share/doc/hpsdr-rs"
install -Dm644 README.md "$docdir/README.md"
install -Dm644 LICENSE "$docdir/LICENSE"
cp -r docs/manual "$docdir/manual"
install -Dm644 -t "$docdir" assets/*.rules

# winit loads libxkbcommon(-x11) with dlopen, so linuxdeploy can't see
# them as dependencies; bundle them explicitly (issue #14: the AppImage
# panicked with "Library libxkbcommon-x11.so could not be loaded" on a
# system without them).
extra_libs=()
for soname in libxkbcommon.so.0 libxkbcommon-x11.so.0; do
  # First match, but read all input: an early awk exit would SIGPIPE
  # ldconfig and fail the pipeline under pipefail.
  path="$(ldconfig -p | awk -v s="$soname" '$1 == s && !p {p = $NF} END {print p}')"
  if [ -z "$path" ]; then
    echo "$soname not found -- install libxkbcommon0 and libxkbcommon-x11-0" >&2
    exit 1
  fi
  extra_libs+=(--library "$path")
done

export APPIMAGE_EXTRACT_AND_RUN=1
export PATH="$tools:$PATH"
export LINUXDEPLOY_OUTPUT_VERSION="$version"
export OUTPUT="hpsdr-rs-${version}-${arch}.AppImage"

"$tools/linuxdeploy-${arch}.AppImage" \
  --appdir "$appdir" \
  --desktop-file assets/hpsdr-rs.desktop \
  --icon-file assets/icons/hpsdr-rs.png \
  "${extra_libs[@]}" \
  --output appimage

echo "Built $root/$OUTPUT"
