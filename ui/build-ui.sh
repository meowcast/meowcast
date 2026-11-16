#!/bin/sh
# Offline cross-build of the UI for the dongle, then package it for the /opt deploy loop.
# Run from ui (needs the Mesa build in the builder volume, i.e. a GPU_TEST=1 build):
#   docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
#     -w /fw/ui meowcast-builder:gpu sh build-ui.sh
set -eu
cd "$(dirname "$0")"
export LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH=1794355200
[ -f /build/mesa-install/usr/lib/libgbm.so.1 ] || { echo "no /build/mesa-install: run the GPU_TEST=1 ramboot build first"; exit 1; }
export CARGO_TARGET_DIR=/build/ui-target
cargo build --release --locked --offline --config .cargo/vendor.toml
python3 package-ui.py "$CARGO_TARGET_DIR/armv7-unknown-linux-gnueabihf/release/miracast-ui" \
    ../os/output/gpu/runtime-manifest.json ../os/deploy/ui
