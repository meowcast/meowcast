#!/bin/sh
# Build the NOR writer stage and its USB transport. Nothing here talks to the device.
set -eu
cd "$(dirname "$0")"
mkdir -p output
audit=../rockusb
. ../toolchain.sh

# Transport: derived from the reviewed audit tool. RAM writes only to the stage
# scratch (0x62000000-0x62004000), the helper callback slot and the image buffer
# 0x64000000-0x65000000 (image byte N at 0x64000000+N). Adds write-file/read-file,
# which loop 16 KiB transfers in one USB session.
python3 - "$audit/rockusb_memory.cpp" output/norflash_memory.cpp <<'PY'
from pathlib import Path
import sys
s=Path(sys.argv[1]).read_text()
old='!in_range(address,length,0x62000000,0x64000000)'
assert s.count(old)==1
s=s.replace(old,'''!(in_range(address,length,0x62000000,0x62004000) ||
            in_range(address,length,0x63014c38,0x63014c3c) ||
            in_range(address,length,0x64000000,0x65000000) ||
            in_range(address,length,0x68400000,0x68460000))''')
assert s.count('opcode==0x19?60000:10000')==1
anchor='    std::vector<unsigned char> data;\n    unsigned char opcode=0;\n'
assert s.count(anchor)==1
s=s.replace(anchor,anchor+r'''    if(!std::strcmp(argv[1],"write-file") && argc==4) {
        FILE *f=std::fopen(argv[3],"rb"); if(!f)return 2;
        std::fseek(f,0,SEEK_END); long length=std::ftell(f); std::rewind(f);
        if(length<=0 || (length&3) || !(in_range(address,length,0x64000000,0x65000000) ||
            in_range(address,length,0x68400000,0x68460000))) { std::fclose(f); return 2; }
        std::vector<unsigned char> all(length);
        bool ok=std::fread(all.data(),1,length,f)==static_cast<size_t>(length); std::fclose(f); if(!ok)return 2;
        Device device; if(!device.open())return 1;
        for(long off=0;off<length;off+=16384) {
            long n=length-off<16384?length-off:16384;
            std::vector<unsigned char> chunk(all.begin()+off,all.begin()+off+n);
            if(!device.transfer(0x18,address+off,chunk)) { std::fprintf(stderr,"write-file failed at +0x%lx\n",off); return 1; }
        }
        std::printf("write-file address=%08x length=%ld success\n",address,length);
        return 0;
    }
    if(!std::strcmp(argv[1],"read-file") && argc==5) {
        uint32_t length=std::strtoul(argv[3],&end,0);
        if(*end || !length || (length&3) || !readable(address,length))return 2;
        FILE *f=std::fopen(argv[4],"wbx"); if(!f)return 1;
        Device device; if(!device.open()) { std::fclose(f); return 1; }
        for(uint32_t off=0;off<length;off+=16384) {
            uint32_t n=length-off<16384?length-off:16384;
            std::vector<unsigned char> chunk(n);
            if(!device.transfer(0x17,address+off,chunk) || std::fwrite(chunk.data(),1,n,f)!=n) { std::fclose(f); return 1; }
        }
        std::fclose(f);
        std::printf("read-file address=%08x length=%u success\n",address,length);
        return 0;
    }
''')
Path(sys.argv[2]).write_text(s)
PY
clang++ -std=c++11 -O2 $usb_cflags \
    output/norflash_memory.cpp $usb_libs -o output/norflash_memory

"$ndk/clang" --target=armv7-none-eabi -mcpu=cortex-a7 -marm -Os \
    -ffreestanding -fno-builtin -fno-stack-protector -fno-unwind-tables \
    -fno-asynchronous-unwind-tables -nostdlib -fuse-ld=lld \
    -Wl,-T,"$audit/ram_stage.ld" -Wl,--entry=stage_entry -Wl,--build-id=none \
    "$audit/ram_cache_entry.S" stage_spi_write.c -o output/stage_spi_write.elf
"$ndk/llvm-objcopy" -O binary output/stage_spi_write.elf output/stage_spi_write.bin
"$ndk/llvm-objdump" -d output/stage_spi_write.elf > output/stage_spi_write.disassembly.txt
shasum -a 256 output/stage_spi_write.bin output/norflash_memory

# Trust-slot handoff = boards/rk3228a-dongle/boot/linux_entry.S built with FLASH_ENTRY, same
# LAB_RECOVERY + TREFI 39 flags as the tested RAM-boot handoff.
#  flash_entry.bin:           flashed; skips DWC2 (USB unused by the miniloader).
#  flash_entry_rehearsal.bin: RAM rehearsal from the USB helper; resets DWC2 first.
entry=../boot/linux_entry.S
for v in flash_entry:-DFLASH_NO_DWC2 flash_entry_rehearsal:; do
    name=${v%%:*}; extra=${v#*:}
    # DDR_RECLOCK (13 Nov): DDR3 300 -> 528 MHz before Linux (boards/rk3228a-dongle/boot/ddr_reclock.c).
    "$ndk/clang" --target=armv7-none-eabi -mcpu=cortex-a7 -marm -nostdlib -fuse-ld=lld \
        -O2 -ffreestanding -fno-builtin -fno-stack-protector -fno-jump-tables -mfloat-abi=soft -mfpu=none \
        -fno-unwind-tables -fno-asynchronous-unwind-tables \
        -DLAB_RECOVERY -DDDR_TREFI=39 -DFLASH_ENTRY -DDDR_RECLOCK $extra \
        -Wl,-T,flash_entry.ld -Wl,--build-id=none "$entry" ../boot/ddr_reclock.c -o output/$name.elf
    "$ndk/llvm-objcopy" -O binary output/$name.elf output/$name.bin
    "$ndk/llvm-objdump" -d output/$name.elf > output/$name.disassembly.txt
    "$ndk/llvm-nm" output/$name.elf | grep ' fdt_blob$' > output/$name.fdt_blob
    # ddr_reclock.c runs from SRAM with the FPU off and DRAM in self-refresh: no NEON/VFP
    # and no absolute addresses into this image (0x684xxxxx) in the copied section.
    lo=$("$ndk/llvm-nm" output/$name.elf | awk '$3=="__ddr_reclock_start"{print $1}')
    hi=$("$ndk/llvm-nm" output/$name.elf | awk '$3=="__ddr_reclock_end"{print $1}')
    python3 -c 'import sys,re; lo,hi=int(sys.argv[1],16),int(sys.argv[2],16)
for l in open(sys.argv[3]):
    m=re.match(r"([0-9a-f]{8}):",l)
    if m and lo<=int(m[1],16)<hi: sys.stdout.write(l)' "$lo" "$hi" output/$name.disassembly.txt > output/$name.reclock.txt
    [ -s output/$name.reclock.txt ] || { echo "$name: no ddr_reclock section"; exit 1; }
    if grep -E '\bv(ld|st|mov|dup|add|mul|cvt)|\.word[[:space:]]+0x684' output/$name.reclock.txt; then
        echo "$name: ddr_reclock section is not SRAM-safe"; exit 1
    fi
done
shasum -a 256 output/flash_entry.bin output/flash_entry_rehearsal.bin
