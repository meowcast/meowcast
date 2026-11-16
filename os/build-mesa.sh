#!/bin/sh
set -eu
cd "$(dirname "$0")"
# Meson records the cross file's path in the build tree: give it a fixed one inside the
# container, so the tree stays valid wherever the repository is mounted.
mkdir -p /work && cp -p mesa-armhf.ini /work/mesa-armhf.ini
export LC_ALL=C TZ=UTC SOURCE_DATE_EPOCH=1794355200
echo 'b9436b3e1a8be76fbd4823dc884f90596ab7c6ac5e1867a305e66ffccfb31a92  sources/mesa-26.2.6.tar.xz' | sha256sum -c -
[ -d /build/mesa-26.2.6 ] || tar -xJf sources/mesa-26.2.6.tar.xz -C /build
# Project patches (patches/mesa-*.patch), applied idempotently: the source tree persists
# in the build volume.
for p in patches/mesa-*.patch; do
    [ -f "$p" ] || continue
    if patch -d /build/mesa-26.2.6 -p1 -R -s -f --dry-run < "$p" >/dev/null 2>&1; then
        echo "mesa patch already applied: $p"
    else
        patch -d /build/mesa-26.2.6 -p1 -N < "$p"
    fi
done
[ -f /build/mesa-build/build.ninja ] || meson setup /build/mesa-build /build/mesa-26.2.6 --cross-file /work/mesa-armhf.ini \
    --prefix=/usr --libdir=lib --buildtype=minsize --strip --wrap-mode=nofallback \
    -Dgallium-drivers=lima -Dvulkan-drivers=[] -Dplatforms=[] \
    -Dglx=disabled -Degl=enabled -Dgbm=enabled -Dgles1=disabled -Dgles2=enabled \
    -Dopengl=false -Dllvm=disabled -Dglvnd=disabled -Dgallium-va=disabled \
    -Dvideo-codecs=[] -Dbuild-tests=false -Dlibunwind=disabled -Dlmsensors=disabled \
    -Dxmlconfig=disabled -Dzstd=disabled
ninja -C /build/mesa-build -j${JOBS:-8}
DESTDIR=/build/mesa-install ninja -C /build/mesa-build install
mkdir -p output/gpu
cp /build/mesa-build/meson-logs/meson-log.txt output/gpu/mesa-configure.log
cp /build/mesa-build/meson-info/intro-buildoptions.json output/gpu/mesa-buildoptions.json
dpkg-query -W > output/gpu/builder-packages.txt
du -sh /build/mesa-install/usr/lib/*
