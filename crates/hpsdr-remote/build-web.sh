#!/bin/sh
# Builds hpsdr-remote for a web browser into crates/hpsdr-remote/web/:
# serve that directory (index.html plus pkg/) from any web server, or
# build hpsdr-rs afterwards and it has the client built in (Settings ->
# Network -> Web client; see build.rs).
#
# Sets up what it needs the first time: the wasm32 target (via rustup) and
# wasm-bindgen-cli at exactly the version of the wasm-bindgen crate in
# Cargo.lock (they must match). The release workflows run this before
# building hpsdr-rs.
set -e
cd "$(dirname "$0")/../.."

if command -v rustup >/dev/null 2>&1; then
    rustup target add wasm32-unknown-unknown
fi
version=$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p')
if ! wasm-bindgen --version 2>/dev/null | grep -qx "wasm-bindgen $version"; then
    echo "Installing wasm-bindgen-cli $version (to match Cargo.lock)"
    cargo install wasm-bindgen-cli --version "$version" --locked
fi

cargo build -p hpsdr-remote --release --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript \
    --out-dir crates/hpsdr-remote/web/pkg \
    target/wasm32-unknown-unknown/release/hpsdr-remote.wasm
echo "Built crates/hpsdr-remote/web/ -- e.g. python3 -m http.server -d crates/hpsdr-remote/web 8080"
