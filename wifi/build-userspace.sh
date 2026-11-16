#!/bin/sh
# build-userspace.sh: offline cross-build of the Wi-Fi userspace into ../os/deploy/wifi/
#   bin/wpa_supplicant  station on wlan0 + the setup access point on wlan1 (CONFIG_AP),
#                       nl80211 via a static libnl, internal crypto (WPA2-PSK; no WPA3/SAE:
#                       that needs an ECC library)
#   bin/wpa_cli         for diagnostics
#   bin/dropbear        the network debug bridge (Settings > Developer), public keys only
#   bin/meow-tls        verified TLS to the cloud relay (BearSSL), stdin/stdout tunnel
# Run from wifi after `WIFI=1 sh ../os/fetch-sources.sh`:
#   docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
#     -w /fw/wifi meowcast-builder:gpu sh build-userspace.sh
set -eu
cd "$(dirname "$0")"
here=$(pwd)
src=../os/sources
out=../os/deploy/wifi
work=/build/wifi-userspace
export LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH=1794355200
host=arm-linux-gnueabihf
export CC=$host-gcc AR=$host-ar RANLIB=$host-ranlib STRIP=$host-strip
cflags='-Os -pipe -march=armv7-a -mfpu=neon-vfpv4 -mfloat-abi=hard -ffunction-sections -fdata-sections'
ldflags='-Wl,--gc-sections -Wl,-O1'
jobs=$(nproc)
rm -rf "$work"
mkdir -p "$work" "$work/sysroot"

# libnl 3 (core + genl), static, for wpa_supplicant's nl80211 driver.
tar -xzf "$src/libnl-3.12.0.tar.gz" -C "$work"
(cd "$work/libnl-3.12.0" && CFLAGS="$cflags" ./configure --host=$host --prefix=/usr \
    --disable-shared --enable-static --disable-cli --disable-debug --disable-pthreads >/dev/null &&
    make -j"$jobs" lib/libnl-3.la lib/libnl-genl-3.la >/dev/null &&
    make DESTDIR="$work/sysroot" install-libLTLIBRARIES install-nobase_libnlincludeHEADERS >/dev/null 2>&1 ||
    make DESTDIR="$work/sysroot" install >/dev/null)
nl="$work/sysroot/usr"
[ -f "$nl/lib/libnl-genl-3.a" ]

# wpa_supplicant
tar -xzf "$src/wpa_supplicant-2.12.tar.gz" -C "$work"
w="$work/wpa_supplicant-2.12/wpa_supplicant"
cp "$here/wpa_supplicant.config" "$w/.config"
make -C "$w" -j"$jobs" V=0 CC="$CC" \
    EXTRA_CFLAGS="$cflags -I$nl/include/libnl3" \
    LDFLAGS="$ldflags -L$nl/lib" \
    PKG_CONFIG=false LIBNL_INC="$nl/include/libnl3" \
    wpa_supplicant wpa_cli >/dev/null

# dropbear: server only, ed25519 host and user keys, no passwords, no forwarding.
tar -xjf "$src/dropbear-2026.94.tar.bz2" -C "$work"
d="$work/dropbear-2026.94"
cp "$here/dropbear-localoptions.h" "$d/localoptions.h"
(cd "$d" && CFLAGS="$cflags" LDFLAGS="$ldflags" ./configure --host=$host --prefix=/usr \
    --disable-zlib --disable-syslog --disable-lastlog --disable-utmp --disable-utmpx \
    --disable-wtmp --disable-wtmpx --disable-pututline --disable-pututxline --disable-pam \
    --disable-harden >/dev/null &&
    make -j"$jobs" PROGRAMS='dropbear dropbearkey' >/dev/null)

# meow-tls: the cloud link's TLS client (BearSSL; ../cloud/tls). Trust anchors from roots.pem,
# converted to C by a host build of BearSSL's brssl.
tar -xzf "$src/bearssl-7bea48e.tar.gz" -C "$work"
b="$work/BearSSL-7bea48e"
cp -r "$b" "$work/bearssl-host"
make -C "$work/bearssl-host" -j"$jobs" tools >/dev/null
"$work/bearssl-host/build/brssl" ta ../cloud/tls/roots.pem > "$work/ta.h" 2>/dev/null
grep -q 'TAs_NUM   5' "$work/ta.h" || { echo "meow-tls: expected 5 trust anchors"; exit 1; }
make -C "$b" -j"$jobs" lib CC="$CC" AR="$AR" CFLAGS="$cflags -W -Wall" >/dev/null
$CC $cflags $ldflags -I"$b/inc" -I"$work" -o "$work/meow-tls" ../cloud/tls/meow-tls.c "$b/build/libbearssl.a"

rm -rf "$out"
mkdir -p "$out/bin"
cp "$w/wpa_supplicant" "$w/wpa_cli" "$d/dropbear" "$d/dropbearkey" "$work/meow-tls" "$out/bin/"
$STRIP --strip-all "$out"/bin/*
(cd "$out" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
ls -l "$out/bin"
