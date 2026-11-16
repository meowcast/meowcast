#!/bin/sh
set -eu
cd "$(dirname "$0")"
mkdir -p output/gpu
arm-linux-gnueabihf-gcc -Os -Wall -Wextra -Werror \
    -I/build/mesa-install/usr/include -I/usr/include/libdrm gpu-test.c \
    -L/build/mesa-install/usr/lib -Wl,-rpath-link,/build/mesa-install/usr/lib \
    -lEGL -lGLESv2 -lgbm -ldrm -lm -o output/gpu/gpu-test
arm-linux-gnueabihf-strip output/gpu/gpu-test
arm-linux-gnueabihf-gcc -O2 -mfpu=neon-vfpv4 -mfloat-abi=hard -Wall -Wextra -Werror membw.c -o output/gpu/membw
arm-linux-gnueabihf-strip output/gpu/membw
arm-linux-gnueabihf-gcc -O2 -Wall -Wextra -Werror canary.c -o output/gpu/canary
arm-linux-gnueabihf-strip output/gpu/canary
arm-linux-gnueabihf-gcc -O2 -mcpu=cortex-a7 -pthread -Wall -Wextra -Werror memtest.c -o output/gpu/memtest
arm-linux-gnueabihf-strip output/gpu/memtest
python3 package-gpu.py /build/rootfs
