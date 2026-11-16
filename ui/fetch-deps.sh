#!/bin/sh
# Fetch everything the offline (--network=none) UI build needs. Needs network, so run it
# in the builder image with networking (like fetch-sources.sh for the tarballs):
#   cd ui
#   docker run --rm -v "$PWD/..:/fw" -w /fw/ui meowcast-builder:gpu sh fetch-deps.sh
# 1. crates -> ./vendor (+ .cargo/vendor.toml), pinned by Cargo.lock.
# 2. Noto Sans (OFL-1.1) from Debian bookworm's fonts-noto-core (apt-verified) -> assets/fonts.
set -eu
cd "$(dirname "$0")"
export CARGO_HOME=${CARGO_HOME:-/opt/cargo}
# UPDATE_LOCK=1 after changing Cargo.toml dependencies; otherwise the lock file is enforced.
lock=; [ -f Cargo.lock ] && [ -z "${UPDATE_LOCK:-}" ] && lock=--locked
cargo vendor $lock --versioned-dirs vendor > .cargo/vendor.toml.partial
mv .cargo/vendor.toml.partial .cargo/vendor.toml
echo "vendor: $(ls vendor | wc -l) crates, $(du -sh vendor | cut -f1)"

mkdir -p assets/fonts
if [ ! -f assets/fonts/SHA256SUMS ]; then
    tmp=$(mktemp -d); chmod 755 "$tmp"
    (cd "$tmp" && apt-get update -qq && apt-get download -qq fonts-noto-core fonts-noto-extra)
    for deb in "$tmp"/*.deb; do dpkg-deb -x "$deb" "$tmp/x"; done
    for w in Regular Light Medium; do
        cp "$tmp/x/usr/share/fonts/truetype/noto/NotoSans-$w.ttf" assets/fonts/
    done
    cp "$tmp/x/usr/share/doc/fonts-noto-core/copyright" assets/fonts/COPYRIGHT.debian
    (cd "$tmp" && ls *.deb && sha256sum *.deb) > assets/fonts/SOURCE
    (cd assets/fonts && sha256sum NotoSans-*.ttf > SHA256SUMS)
    rm -rf "$tmp"
fi
(cd assets/fonts && sha256sum -c SHA256SUMS)
