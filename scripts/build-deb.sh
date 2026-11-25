#!/usr/bin/env bash
# Builds a .deb via `cargo deb`, auto-incrementing the Debian revision
# suffix (the "-N" in the package version) on every run -- cargo-deb
# itself has no persistent counter of its own; Cargo.toml's
# [package.metadata.deb] `revision` is a static value someone has to
# edit by hand. Bare `cargo deb` still works (falls back to whatever
# `revision` is currently set to in Cargo.toml), but repeatedly
# installing that unchanged version+revision requires manually removing
# the previous install first, since dpkg/apt only upgrade in place when
# the version string actually differs.
#
# Counter lives in .deb-revision at the repo root, gitignored --
# local-only build state, not meant to be shared or tracked across
# machines/commits.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COUNTER_FILE="$REPO_ROOT/.deb-revision"

last=$(cat "$COUNTER_FILE" 2>/dev/null || echo 0)
next=$((last + 1))
echo "$next" > "$COUNTER_FILE"

echo "Building .deb with revision $next..."
# Portable build: no -march=native (see build.rs), so the package runs
# on other machines' CPUs, not just this one's.
# Each built on its own, then packaged without cargo deb building: built
# together, Cargo gives hpsdr-remote hpsdr-rs's eframe features (wgpu).
HPSDR_PORTABLE=1 cargo build --release
cargo build --release -p hpsdr-remote
cargo deb --no-build --deb-revision "$next" "$@"
