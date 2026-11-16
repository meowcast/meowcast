#!/bin/sh
set -eu
cd "$(dirname "$0")"
spi_stage_name=$1
case "$spi_stage_name" in *[!a-zA-Z0-9_]*) exit 2;; esac
. ../toolchain.sh
spi_ndk_tools=$ndk
"$spi_ndk_tools/clang" --target=armv7-none-eabi -mcpu=cortex-a7 -marm -Os \
    -ffreestanding -fno-builtin -fno-stack-protector -fno-unwind-tables \
    -fno-asynchronous-unwind-tables -nostdlib -fuse-ld=lld \
    -Wl,-T,ram_stage.ld -Wl,--entry=stage_entry -Wl,--build-id=none \
    ram_cache_entry.S "$spi_stage_name.c" -o "$spi_stage_name.elf"
"$spi_ndk_tools/llvm-objcopy" -O binary "$spi_stage_name.elf" "$spi_stage_name.bin"
"$spi_ndk_tools/llvm-objdump" -d "$spi_stage_name.elf" > "$spi_stage_name.disassembly.txt"
