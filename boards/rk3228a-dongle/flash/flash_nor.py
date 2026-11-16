#!/usr/bin/env python3
"""Make SPI-NOR [start,end) equal to a full 16 MiB image, via the MaskROM RAM helper.

The target must be in MaskROM with the no-padding helper loaded
(boards/rk3228a-dongle/rockusb/rockusb_ram_loader.py --no-ram-padding).

Safety:
- The image's protected regions (0x000000-0x04ffff: RKFP table, vendor, both IDBlocks;
  0xfe0000-end: misc, backup table) must equal the verified stock dump. The stage
  cannot write them either (compiled-in window).
- Default is a dry run (reads and compares only). --write is required to program.
- Default range ends at 0xc00000: the Linux data partition (JFFS2 /data: app manifests,
  permission grants, app storage; 0xc00000-0xfe0000) is kept. --end 0xfe0000 wipes it too
  (the image has it erased = an empty /data).
- Every chunk is CRC-checked in DRAM before the stage touches flash, and each chunk's
  final flash CRC (an independent read of the whole chunk) must match the image.
  --verify-sectors also reads back every sector right after writing it (the 13 Nov
  behaviour; slower, same guarantee, earlier failure address).
- Speed: 64 KiB block erases where most of a block changes, burst FIFO transfers, SPI clock
  divider 2 (--spi-div 4 = the first flashes' clock). 13 Nov: kernel-slot write 79 s (was 457 s).
Afterwards verify the whole chip with the audit reader (see README.md).
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys
import time
import zlib

t_start = time.monotonic()

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import paths  # noqa: E402
TOOL = HERE / 'output' / 'norflash_memory'
STAGE = HERE / 'output' / 'stage_spi_write.bin'
FLOOR, CEIL, SIZE = 0x50000, 0xfe0000, 0x1000000
DATA_START = 0xc00000  # Linux /data partition (DT data@c00000) up to CEIL
CHUNK = 0x40000
IMAGE_BASE = 0x64000000
ENTRY, OUT, CALLBACK = 0x62002000, 0x62000000, 0x63014c38
CB_MAGIC, MODE_WRITE, MODE_DRY = 0x4243574e, 0x54495257, 0x59524420
OKAY = 0x59414b4f
ERRORS = {1: 'unexpected SPI/pin/clock config', 2: 'SPI busy', 3: 'JEDEC ID mismatch',
          6: 'control block invalid', 7: 'outside write window', 8: 'DRAM CRC mismatch',
          9: 'SR1 not 0 (busy/WEL/protection)', 10: 'WEL state wrong', 11: 'flash busy timeout',
          12: 'readback mismatch', 13: 'SPI transfer error', 14: 'final flash CRC mismatch'}

parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
parser.add_argument('--image', required=True, help='full 16 MiB image the flash should match')
parser.add_argument('--image-sha256', required=True)
parser.add_argument('--start', type=lambda s: int(s, 0), default=FLOOR)
parser.add_argument('--end', type=lambda s: int(s, 0), default=DATA_START,
                    help='default 0xc00000 keeps /data; 0xfe0000 also erases it')
parser.add_argument('--write', action='store_true', help='program flash (default: dry run)')
parser.add_argument('--spi-div', type=int, default=2, help='SPI clock divider, even 2..64 (default 2; 4 = the 13 Nov first flashes)')
parser.add_argument('--verify-sectors', action='store_true', help='also read back each sector after writing')
args = parser.parse_args()

image = Path(args.image).read_bytes()
if hashlib.sha256(image).hexdigest() != args.image_sha256 or len(image) != SIZE:
    sys.exit('Image hash/size mismatch')
_, reference = paths.stock_dump()
if image[:FLOOR] != reference[:FLOOR] or image[CEIL:] != reference[CEIL:]:
    sys.exit('Image differs from the stock dump in a protected region; refusing')
if args.start % 4096 or args.end % 4096 or not FLOOR <= args.start < args.end <= CEIL:
    sys.exit('Range must be 4 KiB aligned within [0x50000, 0xfe0000)')
if args.spi_div < 2 or args.spi_div > 64 or args.spi_div % 2:
    sys.exit('--spi-div must be even, 2..64')
stage = STAGE.read_bytes()
mode = MODE_WRITE if args.write else MODE_DRY
flags = 1 if args.verify_sectors else 0

run = HERE / 'runs' / datetime.datetime.now().astimezone().strftime('%Y%m%dT%H%M%S.%f%z')
run.mkdir(parents=True)
log = (run / 'operations.jsonl').open('x')
print('Evidence directory:', run, flush=True)
(run / 'plan.json').write_text(json.dumps({
    'image': str(Path(args.image).resolve()), 'image_sha256': args.image_sha256,
    'start': hex(args.start), 'end': hex(args.end), 'write': args.write,
    'spi_div': args.spi_div, 'verify_sectors': args.verify_sectors,
    'stage_sha256': hashlib.sha256(stage).hexdigest(),
    'tool_sha256': hashlib.sha256(TOOL.read_bytes()).hexdigest()}, indent=2) + '\n')


def step(*command, required=True, timeout=30):
    event = {'timestamp': datetime.datetime.now().astimezone().isoformat(), 'args': list(map(str, command))}
    try:
        p = subprocess.run([str(TOOL), *map(str, command)], cwd=HERE, capture_output=True, text=True, timeout=timeout)
        event.update(returncode=p.returncode, stdout=p.stdout.strip(), stderr=p.stderr.strip())
    except subprocess.TimeoutExpired:
        event.update(returncode=124, error='host timeout')
    log.write(json.dumps(event) + '\n'); log.flush()
    if event['returncode']:
        print(json.dumps(event), flush=True)
        if required:
            raise RuntimeError('USB step failed')
    return event['returncode'] == 0


def read(address, length, name):
    path = run / name
    step('read', hex(address), str(length), path)
    return path.read_bytes()


def write_bytes(address, data, name):
    path = run / name
    path.write_bytes(data)
    step('write-ram', hex(address), path)


# Same guards as audit run_ram_callback.py: exact resident dispatcher, no-padding patch,
# empty callback slot.
vendor = (paths.RKBIN / 'bin/rk32/rk322x_usbplug_v2.56.bin').read_bytes()
offset = 0x6300231c - (0x63001000 - 0x3bc)
if read(0x6300231c, 40, 'dispatcher.bin') != vendor[offset:offset + 40]:
    sys.exit('Resident dispatcher differs; nothing written')
if read(0x63002a58, 8, 'padding-guard.bin')[2:6] != bytes.fromhex('00bf00bf'):
    sys.exit('No-padding helper not loaded; nothing written')
if read(CALLBACK, 4, 'callback-before.bin') != bytes(4):
    sys.exit('Callback slot occupied; nothing written')
scratch_before = read(OUT, 256, 'scratch-before.bin')
code_before = read(ENTRY, len(stage), 'code-before.bin')

# Upload the target range of the image to DRAM (byte N at IMAGE_BASE+N).
segment = run / 'segment.bin'
segment.write_bytes(image[args.start:args.end])
print(f'Uploading {args.end - args.start:#x} bytes to DRAM...', flush=True)
step('write-file', hex(IMAGE_BASE + args.start), segment, timeout=600)
segment.unlink()

installed = False
results = []
failed = None
try:
    write_bytes(ENTRY, stage, 'stage.bin')
    if read(ENTRY, len(stage), 'code-readback.bin') != stage:
        raise RuntimeError('Stage readback mismatch')
    installed = True
    write_bytes(CALLBACK, struct.pack('<I', ENTRY), 'callback-new.bin')
    if read(CALLBACK, 4, 'callback-readback.bin') != struct.pack('<I', ENTRY):
        raise RuntimeError('Callback readback mismatch')
    for start in range(args.start, args.end, CHUNK):
        length = min(CHUNK, args.end - start)
        crc = zlib.crc32(image[start:start + length])
        words = [CB_MAGIC, start, length, crc, mode, args.spi_div, flags]
        x = 0
        for w in words:
            x ^= w
        block = b'WAIT' + bytes(124) + struct.pack('<8I', *words, ~x & 0xffffffff) + bytes(96)
        write_bytes(OUT, block, f'cb-{start:06x}.bin')
        t_chunk = time.monotonic()
        if not step('execute-scratch', '0x62001000', required=False, timeout=90):
            failed = f'execute failed at chunk {start:#x}; helper state unknown'
            break
        w = struct.unpack('<32I', read(OUT, 128, f'result-{start:06x}.bin'))
        r = {'start': hex(start), 'length': hex(length), 'status': hex(w[1]),
             'error': ERRORS.get(w[1]) if w[1] != OKAY else None, 'progress': hex(w[2]),
             'equal': w[12], 'differ': w[15], 'erased': w[13], 'pages': w[14],
             'ram_crc_ok': w[16] == crc, 'flash_crc_ok': w[17] == crc, 'sr1': hex(w[18]),
             'id': hex(w[20] & 0xffffff), 'sctlr': hex(w[19]),
             'fail': hex(w[21]) if w[21] != 0xffffffff else None,
             'pins_restored': w[26] == w[4], 'blocks': w[29], 'spi_div': w[30], 'fifo': w[25],
             'secs': round(time.monotonic() - t_chunk, 2)}
        results.append(r)
        print(json.dumps(r), flush=True)
        if w[0] != 0x57524f4e or w[1] != OKAY or not r['ram_crc_ok'] or not r['pins_restored'] \
                or w[22] != mode or w[30] != args.spi_div or (args.write and not r['flash_crc_ok']):
            failed = f'chunk {start:#x} failed: {r["error"] or "bad result record"}'
            break
finally:
    cleanup = True
    if installed:
        cleanup = step('write-ram', hex(CALLBACK), run / 'callback-before.bin', required=False)
    if cleanup:
        cleanup = step('write-ram', hex(ENTRY), run / 'code-before.bin', required=False) and cleanup
        cleanup = step('write-ram', hex(OUT), run / 'scratch-before.bin', required=False) and cleanup
        if cleanup:
            cleanup = (read(CALLBACK, 4, 'callback-restored.bin') == bytes(4)
                       and read(ENTRY, len(stage), 'code-restored.bin') == code_before
                       and read(OUT, 256, 'scratch-restored.bin') == scratch_before)
    summary = {'write': args.write, 'start': hex(args.start), 'end': hex(args.end),
               'chunks_done': len(results), 'failed': failed,
               'sectors_equal': sum(r['equal'] for r in results),
               'sectors_differ': sum(r['differ'] for r in results),
               'sectors_erased': sum(r['erased'] for r in results),
               'pages_programmed': sum(r['pages'] for r in results),
               'blocks_erased': sum(r['blocks'] for r in results),
               'spi_div': args.spi_div, 'verify_sectors': args.verify_sectors,
               'stage_secs': round(sum(r['secs'] for r in results), 1),
               'total_secs': round(time.monotonic() - t_start, 1),
               'helper_restored': cleanup}
    (run / 'summary.json').write_text(json.dumps({'summary': summary, 'chunks': results}, indent=2) + '\n')
    print(json.dumps(summary, indent=2), flush=True)
if failed:
    sys.exit(failed)
