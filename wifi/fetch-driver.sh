#!/bin/sh
# Fetch the RTL8733BU vendor driver at the pinned commit into vendor/rtl8733bu. The kernel
# build copies it into the tree and applies patch-driver.sh; this checkout stays pristine.
set -eu
cd "$(dirname "$0")"
url=https://github.com/Charliechen114514/rtl8733bu-linux-driver
commit=2b72111f68d4cfafa27f189761448c61a8b325c8
if [ ! -d vendor/rtl8733bu/.git ]; then
    mkdir -p vendor
    git clone "$url" vendor/rtl8733bu
fi
[ "$(git -C vendor/rtl8733bu rev-parse HEAD)" = "$commit" ] || {
    git -C vendor/rtl8733bu fetch origin "$commit" 2>/dev/null || git -C vendor/rtl8733bu fetch origin
    git -C vendor/rtl8733bu checkout --detach "$commit"
}
echo "rtl8733bu at $(git -C vendor/rtl8733bu rev-parse --short HEAD)"
