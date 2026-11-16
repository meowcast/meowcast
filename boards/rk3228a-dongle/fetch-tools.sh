#!/bin/sh
# Fetch and build what this board's host tools need, into rockusb/tools (ignored by git):
#   rkbin          Rockchip's loader binaries, at the pinned commit (their licence: rkbin/LICENSE)
#   rkdeveloptool  Rockchip's USB tool, at the pinned commit, with rockusb/rkdeveloptool-local.patch
# and derive the RAM-download helper loader (rockusb/make-helper-loader.py, bit-exact).
# Needs git, a C++ compiler and libusb-1.0 (macOS: brew install libusb pkg-config).
set -eu
cd "$(dirname "$0")"
. ./toolchain.sh
mkdir -p rockusb/tools
cd rockusb/tools
get() { # get DIR URL COMMIT
    [ -d "$1/.git" ] || git clone "$2" "$1"
    [ "$(git -C "$1" rev-parse HEAD)" = "$3" ] || { git -C "$1" fetch origin; git -C "$1" checkout --detach "$3"; }
}
get rkbin https://github.com/rockchip-linux/rkbin 3e288fe814e059dd06833495f845cab04ac20a5c
get rkdeveloptool https://github.com/rockchip-linux/rkdeveloptool 304f073752fd25c854e1bcf05d8e7f925b1f4e14
if [ ! -x rkdeveloptool/rkdeveloptool ]; then
    (cd rkdeveloptool
     git apply --check ../../rkdeveloptool-local.patch 2>/dev/null && git apply ../../rkdeveloptool-local.patch
     [ -f config.h ] || printf '#define PACKAGE_VERSION "1.32"\n#define PACKAGE_BUGREPORT ""\n' > config.h
     ${CXX:-c++} -std=c++11 -O2 -fno-strict-aliasing -D_FILE_OFFSET_BITS=64 -D_LARGE_FILE $usb_cflags \
        main.cpp crc.cpp RKBoot.cpp RKComm.cpp RKDevice.cpp RKImage.cpp RKLog.cpp RKScan.cpp \
        $usb_libs -o rkdeveloptool)
fi
cd ..
python3 -I make-helper-loader.py
${CXX:-c++} -std=c++11 -O2 $usb_cflags rockusb_memory.cpp $usb_libs -o rockusb_memory
echo 'board tools ready'
