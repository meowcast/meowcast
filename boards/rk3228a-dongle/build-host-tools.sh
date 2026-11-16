#!/bin/sh
set -eu
cd "$(dirname "$0")"
audit=rockusb
. ./toolchain.sh
out=../../os/output
mkdir -p "$out"
# Derive an isolated transport from the working audited implementation.
# Keep the original acquisition tool and evidence unchanged.
python3 - "$audit/rockusb_memory.cpp" $out/ramboot_memory.cpp <<'PY'
from pathlib import Path
import sys
s=Path(sys.argv[1]).read_text()
old='!in_range(address,length,0x62000000,0x64000000)'
assert s.count(old)==1
s=s.replace(old,'''!(in_range(address,length,0x62000000,0x62004000) ||
            in_range(address,length,0x63014c38,0x63014c3c) ||
            in_range(address,length,0x64000000,0x66000000) ||
            in_range(address,length,0x68000000,0x68010000))''')
assert s.count('opcode==0x19?60000:10000')==1
s=s.replace('opcode==0x19?60000:10000','opcode==0x19?5000:10000')
Path(sys.argv[2]).write_text(s)
PY
clang++ -std=c++11 -O2 $usb_cflags \
    $out/ramboot_memory.cpp $usb_libs -o $out/ramboot_memory
"$ndk/clang" --target=armv7-none-eabi -mcpu=cortex-a7 -marm -Os \
    -ffreestanding -fno-builtin -fno-stack-protector -nostdlib -fuse-ld=lld \
    -Wl,-T,"$audit/ram_stage.ld" -Wl,--build-id=none \
    "$audit/ram_cache_entry.S" boot/cpu_probe.c -o $out/cpu_probe.elf
"$ndk/llvm-objcopy" -O binary $out/cpu_probe.elf $out/cpu_probe.bin
# LAB=1 (default): handoff with the MaskROM-recovery flag, watchdog pre-arm and
# GRF/CRU snapshot (LAB_RECOVERY). LAB=0: production-like handoff.
lab_def=; [ "${LAB:-1}" = 1 ] && lab_def=-DLAB_RECOVERY
# DDR3 refresh 3.9 us (TREFI 39): Samsung K4B1G1646G requires 2x refresh above
# 85 C case temperature, and this passively cooled SoC runs DRAM-adjacent at
# 85-100 C. The rkbin blob programs 7.8 us. Production-relevant (not LAB).
lab_def="$lab_def -DDDR_TREFI=${DDR_TREFI:-39}"
"$ndk/clang" --target=armv7-none-eabi -mcpu=cortex-a7 -marm -nostdlib -fuse-ld=lld $lab_def \
    -Wl,-T,"$audit/ram_stage.ld" -Wl,--build-id=none boot/linux_entry.S -o $out/linux_entry.elf
echo "${LAB:-1}" > $out/linux_entry.lab
mkdir -p $out/variants
# Variant handoffs for A/B tests (boot with ramboot.py --entry). DDR idle power:
"$ndk/clang" --target=armv7-none-eabi -mcpu=cortex-a7 -marm -nostdlib -fuse-ld=lld $lab_def -DDDR_PD_IDLE=32 -DDDR_SR_IDLE=12 \
    -Wl,-T,"$audit/ram_stage.ld" -Wl,--build-id=none boot/linux_entry.S -o $out/variants/linux_entry_ddridle.elf
mkdir -p $out/variants
"$ndk/llvm-objcopy" -O binary $out/variants/linux_entry_ddridle.elf $out/variants/linux_entry_ddridle.bin
"$ndk/llvm-objcopy" -O binary $out/linux_entry.elf $out/linux_entry.bin
"$ndk/llvm-objdump" -d $out/linux_entry.elf > $out/linux_entry.disassembly.txt
"$ndk/llvm-objdump" -d $out/cpu_probe.elf > $out/cpu_probe.disassembly.txt
