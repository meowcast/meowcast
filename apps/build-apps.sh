#!/bin/sh
# Offline build of appd and the example apps, then:
#   ../os/deploy/appd/   appd for the initramfs (build.sh picks it up) + trusted store key
#   ../cloud/store/www/            the store: the example apps, packed and signed, the full Noto Sans
#                             faces (the firmware has Latin subsets) and the signed asset index.
#                             Publish it with ../cloud/store/deploy.sh (Cloudflare, meowcast-store)
# Run from apps (needs the Mesa build in the builder volume, like the UI):
#   docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
#     -w /fw/apps meowcast-builder:gpu sh build-apps.sh
# The store signing key (MEOW_STORE_KEY) is created on first use. Keep it private:
# whoever has it can publish apps every dongle with the matching .pub will run.
set -eu
cd "$(dirname "$0")"
export LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH=1794355200 CARGO_TARGET_DIR=/build/apps-target
host=$(rustc -vV | sed -n 's/^host: //p')
cargo build --release --locked --offline --config .cargo/vendor.toml
cargo build --release --locked --offline --config .cargo/vendor.toml -p appd --target "$host"
arm=$CARGO_TARGET_DIR/armv7-unknown-linux-gnueabihf/release
tool=$CARGO_TARGET_DIR/$host/release/appd
# The store signing key: MEOW_STORE_KEY (config.env; relative to the repository root), by
# default cloud/store/keys/store.key. Its public half goes into the firmware.
key=${MEOW_STORE_KEY:-cloud/store/keys/store.key}
case "$key" in /*) ;; *) key=../$key ;; esac
mkdir -p ../cloud/store/keys "$(dirname "$key")"
[ -f "$key" ] || "$tool" keygen "$key"
[ -f "../cloud/store/keys/$(basename "${key%.key}").pub" ] || cp "${key%.key}.pub" ../cloud/store/keys/

out=../os/deploy/appd
rm -rf "$out"
mkdir -p "$out/bin" "$out/keys"
cp "$arm/appd" "$out/bin/appd"
cp ../cloud/store/keys/*.pub "$out/keys/"
cp ../os/rootfs/appd-session "$out/appd-session"

# The example apps and the system services, which appd
# installs and runs by itself: the network service (crate netsvc/, binary meow-net) and the
# image service (crate imagesvc/, binary meow-image).
# The Store app (crate store/) is installed by appd itself, like the services.
for app in examples/*/ store/ netsvc/ imagesvc/; do
    name=$(basename "$app")
    [ "$name" = netsvc ] && name=meow-net
    [ "$name" = imagesvc ] && name=meow-image
    stage=/build/apps-stage/$name
    rm -rf "$stage"
    mkdir -p "$stage/bin"
    cp "$arm/$name" "$stage/bin/$name"
    [ -d "$app/assets" ] && cp -r "$app/assets" "$stage/"
    "$tool" pack --manifest "$app/app.json" --payload "$stage" --key "$key" --store ../cloud/store/www
done
# Full font faces: the launcher fetches one when text needs a glyph its Latin subset lacks.
mkdir -p ../cloud/store/www/assets/fonts
for f in ../ui/assets/fonts/NotoSans-*.ttf ../ui/assets/fonts/COPYRIGHT.debian; do
    cmp -s "$f" "../cloud/store/www/assets/fonts/$(basename "$f")" || cp "$f" ../cloud/store/www/assets/fonts/
done
"$tool" assets --store ../cloud/store/www --key "$key"
# repo.json: the name and public key a device shows when this store's URL is added as a
# repository (devices with this firmware have it built in and never read the file).
"$tool" repo --store ../cloud/store/www --key "$key" --name "${MEOW_STORE_NAME:-meow store}"
"$tool" verify --store ../cloud/store/www --keys ../cloud/store/keys
(cd "$out" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
ls -l "$out/bin/appd" "$arm"/inspector
