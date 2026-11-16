#!/usr/bin/env python3
"""Build the full 16 MiB NOR target image for the first flash (phase 1).

Layout (stock RKFP table, vendor storage, both IDBlocks and misc are untouched):
  0x050000 trust slot:  flash_entry.bin + DTB (vdd_logic 1.10 V), then 0xff to 0x0d0000
  0x0d0000 kernel slot: 2 KiB "KERNEL" header (load 0x64000000, size), zImage,
                        then 0xff to 0xc00000
  0xc00000 data:        0xff (= empty JFFS2 /data; flash_nor.py keeps the live one by
                        default, it writes only up to 0xc00000)
Also writes the rehearsal trust blob (flash_entry_rehearsal.bin + same DTB).
Nothing is sent to the device.
"""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import sys
import zlib

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))
import paths  # noqa: E402

RAMBOOT = paths.OUT
FLOOR, TRUST_END, CEIL = 0x50000, 0xd0000, 0xfe0000
KERNEL_PART, HEADER = 0xd0000, 0x800
DATA_START = 0xc00000  # DT data@c00000 (boards/rk3228a-dongle/dts/rk3228a-miracast-perf.dts)
TRUST_LOAD, TRUST_READ = 0x68400000, 0x28c * 512
KERNEL_LOAD = 0x64000000

p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
p.add_argument('--zimage', default=str(RAMBOOT / 'zImage'))
p.add_argument('--dtb', default=str(RAMBOOT / 'rk3228a-miracast-ram.dtb'))
p.add_argument('--name', default='firmware')
args = p.parse_args()

_, reference = paths.stock_dump()
zimage = Path(args.zimage).read_bytes()
magic, start, end = struct.unpack_from('<III', zimage, 0x24)
if magic != 0x016f2818 or start != 0 or end != len(zimage):
    sys.exit('Not a self-contained zImage')

# vdd_logic stays at the DTB's 1.10 V (67 % duty; V = 1.30 - 0.30*duty), as on RAM boots. Until
# 15 Nov this raised it to 1.15 V, from the days when the flash boot was believed to run DDR3
# at 600 MHz (it is reclocked to 528 MHz, which was memtest-clean down to 1.05 V). 1.10 V runs
# the idle home screen 1.7 C cooler (os/README.md, "Where the display heat comes from").
dtb = bytearray(Path(args.dtb).read_bytes())
from fdt_edit import walk, cells  # noqa: E402
for path, name, o, n in walk(bytes(dtb)):
    if path == '/regulator-vdd-logic' and name == 'voltage-table' and cells(dtb, o, n) != [1100000, 67]:
        sys.exit('DTB: vdd_logic is not 1.10 V at 67 % duty')
if len(dtb) > 0x10000:
    sys.exit('DTB too large')
# The kernel's data partition (DT) and the flash tools must agree on where /data starts:
# flash_nor.py keeps [DATA_START, 0xfe0000) and the kernel slot must end below it.
parts = [cells(dtb, o, n) for path, name, o, n in walk(bytes(dtb)) if '/partitions/' in path and name == 'reg']
if parts != [[DATA_START, CEIL - DATA_START]]:
    sys.exit(f'DTB flash partitions {parts} != data@{DATA_START:#x}+{CEIL - DATA_START:#x}; '
             'moving /data needs a deliberate migration (it would be erased)')


def trust_blob(name):
    code = (HERE / 'output' / (name + '.bin')).read_bytes()
    sym = (HERE / 'output' / (name + '.fdt_blob')).read_text().split()
    if int(sym[0], 16) != TRUST_LOAD + len(code):
        sys.exit(name + ': fdt_blob is not at the end of the code')
    blob = code + bytes(dtb)
    if len(blob) > TRUST_READ:
        sys.exit(name + ': trust blob larger than what the miniloader reads')
    return blob, code


trust, entry_code = trust_blob('flash_entry')
rehearsal, _ = trust_blob('flash_entry_rehearsal')

header = bytearray(HEADER)
header[0:6] = b'KERNEL'
struct.pack_into('<III', header, 0x10, KERNEL_LOAD, len(zimage), zlib.crc32(zimage))
kernel = bytes(header) + zimage
if KERNEL_PART + len(kernel) > DATA_START:
    sys.exit('zImage does not fit below the data partition (0xc00000)')

image = bytearray(reference)
image[FLOOR:TRUST_END] = trust + b'\xff' * (TRUST_END - FLOOR - len(trust))
image[KERNEL_PART:CEIL] = kernel + b'\xff' * (CEIL - KERNEL_PART - len(kernel))
assert len(image) == 0x1000000
assert image[:FLOOR] == reference[:FLOOR] and image[CEIL:] == reference[CEIL:]

out = HERE / 'images'
out.mkdir(exist_ok=True)
sha = hashlib.sha256(image).hexdigest()
path = out / f'{args.name}-{sha[:12]}.bin'
path.write_bytes(image)
(out / f'{args.name}-{sha[:12]}.dtb').write_bytes(dtb)
(out / f'{args.name}-{sha[:12]}.rehearsal-trust.bin').write_bytes(rehearsal)


def h(b):
    return hashlib.sha256(b).hexdigest()


record = {'image': path.name, 'sha256': sha,
          'zimage': {'path': str(Path(args.zimage).resolve()), 'bytes': len(zimage), 'sha256': h(zimage)},
          'dtb_source_sha256': h(Path(args.dtb).read_bytes()), 'dtb_flash_sha256': h(dtb),
          'flash_entry_sha256': h(entry_code), 'trust_blob_bytes': len(trust),
          'rehearsal_trust_sha256': h(rehearsal),
          'kernel_slot_end': hex(KERNEL_PART + len(kernel)),
          'changed_sectors': sum(image[i:i + 4096] != reference[i:i + 4096] for i in range(0, len(image), 4096))}
(out / f'{args.name}-{sha[:12]}.json').write_text(json.dumps(record, indent=2) + '\n')
print(json.dumps(record, indent=2))
