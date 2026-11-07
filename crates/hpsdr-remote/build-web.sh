#!/bin/sh
# Builds hpsdr-remote for a web browser into crates/hpsdr-remote/web/:
# serve that directory (index.html plus pkg/) from any web server.
# Needs: rustup target add wasm32-unknown-unknown, and wasm-bindgen-cli at
# the same version as the wasm-bindgen crate in Cargo.lock.
set -e
cd "$(dirname "$0")/../.."
cargo build -p hpsdr-remote --release --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript \
    --out-dir crates/hpsdr-remote/web/pkg \
    target/wasm32-unknown-unknown/release/hpsdr-remote.wasm
echo "Built crates/hpsdr-remote/web/ -- e.g. python3 -m http.server -d crates/hpsdr-remote/web 8080"
