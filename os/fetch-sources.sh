#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p sources
fetch() {
    source_name=$1 source_hash=$2 source_url=$3
    if [ ! -f "sources/$source_name" ]; then
        curl -fL --retry 3 "$source_url" -o "sources/$source_name.partial"
        echo "$source_hash  sources/$source_name.partial" | shasum -a 256 -c -
        mv "sources/$source_name.partial" "sources/$source_name"
    fi
    echo "$source_hash  sources/$source_name" | shasum -a 256 -c -
}
fetch linux-7.3.4.tar.xz b11b6df8295e8e991ff040b71aaa4994a8b55caa23b6b092907f195b5840fd1f \
    https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.4.tar.xz
fetch busybox-1.37.0.tar.bz2 3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4 \
    https://busybox.net/downloads/busybox-1.37.0.tar.bz2
if [ "${GPU_TEST:-0}" = 1 ]; then
    fetch mesa-26.2.6.tar.xz b9436b3e1a8be76fbd4823dc884f90596ab7c6ac5e1867a305e66ffccfb31a92 \
        https://archive.mesa3d.org/mesa-26.2.6.tar.xz
fi
if [ "${WIFI:-0}" = 1 ]; then
    # Wi-Fi userspace (../wifi/build-userspace.sh). Hashes pinned at first download
    # (14 Nov 2026); the release signatures (.asc next to each URL) were not checked.
    fetch wpa_supplicant-2.12.tar.gz 08e23937e16d0155e55cab2b51f51fbe10d80a1aa91c4e15442645059b737ef6 \
        https://w1.fi/releases/wpa_supplicant-2.12.tar.gz
    fetch libnl-3.12.0.tar.gz fc51ca7196f1a3f5fdf6ffd3864b50f4f9c02333be28be4eeca057e103c0dd18 \
        https://github.com/thom311/libnl/releases/download/libnl3_12_0/libnl-3.12.0.tar.gz
    # BearSSL git snapshot (7bea48e, 6 Apr 2026: TLS client of the cloud link, cloud/tls).
    # gitweb generates the tarball on request; the hash is of the copy fetched on 14 Nov 2026.
    fetch bearssl-7bea48e.tar.gz ed397832b1613582737d9c1ce01c1af74ad6a9e9c52a341e24433987cda0f372 \
        "https://bearssl.org/gitweb/?p=BearSSL;a=snapshot;h=7bea48e5e850ab4cafbe68d3765cdaba13a86d6f;sf=tgz"
    fetch dropbear-2026.94.tar.bz2 e098034a843699200c8c977a991fff73159735bf795d5f72ef672c41a6b1ae81 \
        https://matt.ucc.asn.au/dropbear/releases/dropbear-2026.94.tar.bz2
fi
