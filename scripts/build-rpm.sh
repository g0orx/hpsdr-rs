#!/usr/bin/env bash
#
# Builds target/generate-rpm/hpsdr-rs-<version>-<release>.<arch>.rpm
# for Fedora, RHEL, openSUSE and other RPM distributions, using
# cargo-generate-rpm (`cargo install cargo-generate-rpm`, one-time) and
# the [package.metadata.generate-rpm] section of Cargo.toml. No
# rpmbuild needed, so this runs on any Linux distribution.
#
# Extra arguments are passed to cargo generate-rpm.
#
# The RPM requires the glibc version of the machine it was built on, so
# build on the oldest system you want it to install on.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Portable build: no -march=native (see build.rs).
HPSDR_PORTABLE=1 cargo build --release
cargo generate-rpm "$@"
ls target/generate-rpm/*.rpm
