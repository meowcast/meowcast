#!/bin/sh
# Vendor the crates for the offline (--network=none) apps build. Needs network, so run it in
# the builder image with networking:
#   cd apps
#   docker run --rm -v "$PWD/..:/fw" -w /fw/apps meowcast-builder:gpu sh fetch-deps.sh
# UPDATE_LOCK=1 after changing a Cargo.toml dependency; otherwise Cargo.lock is enforced.
set -eu
cd "$(dirname "$0")"
export CARGO_HOME=${CARGO_HOME:-/opt/cargo}
lock=; [ -f Cargo.lock ] && [ -z "${UPDATE_LOCK:-}" ] && lock=--locked
cargo vendor $lock --versioned-dirs vendor > .cargo/vendor.toml.partial
mv .cargo/vendor.toml.partial .cargo/vendor.toml
echo "vendor: $(ls vendor | wc -l) crates, $(du -sh vendor | cut -f1)"
