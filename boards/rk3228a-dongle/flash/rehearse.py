#!/usr/bin/env python3
"""Rehearse the flashed boot from RAM (no flash commands).

  rehearse.py load-helper            MaskROM: download the STOCK-DDR (V1.07, 600 MHz) helper
  rehearse.py boot --image-json J    upload the packed image's zImage to 0x64000000 and its
                                     rehearsal trust blob to 0x68400000, then enter the trust
                                     entry exactly where the miniloader would (0x68400000).

The rehearsal entry differs from the flashed one only in resetting the helper's DWC2
USB core first. After boot: ../lab.py sh 'reg 0x67d03000 6' shows the
breadcrumbs (expect progress word 3).
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import paths  # noqa: E402
TOOL = HERE / 'output' / 'norflash_memory'
RKDEV = paths.RKDEV
LOADER = HERE / 'output' / 'rk322x_loader_stockddr_nopad.bin'
LOADER_SHA = '29216cfab23472ac79f7cd9d480dfd08d9b9ddd4321fa84cd4367f764a6848f4'
CALLBACK, KERNEL, TRUST = 0x63014c38, 0x64000000, 0x68400000

p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
p.add_argument('action', choices=['load-helper', 'boot'])
p.add_argument('--image-json')
args = p.parse_args()

run = HERE / 'runs' / ('rehearse-' + datetime.datetime.now().astimezone().strftime('%Y%m%dT%H%M%S.%f%z'))
run.mkdir(parents=True)
log = (run / 'operations.jsonl').open('x')


def call(cmd, timeout=30, required=True):
    event = {'timestamp': datetime.datetime.now().astimezone().isoformat(), 'args': list(map(str, cmd))}
    try:
        r = subprocess.run(list(map(str, cmd)), cwd=HERE, capture_output=True, text=True, timeout=timeout)
        event.update(returncode=r.returncode, stdout=r.stdout.strip(), stderr=r.stderr.strip())
    except subprocess.TimeoutExpired:
        event.update(returncode=124, error='host timeout')
    log.write(json.dumps(event) + '\n'); log.flush()
    if event['returncode'] and required:
        sys.exit('Failed: ' + json.dumps(event))
    return event


if args.action == 'load-helper':
    if hashlib.sha256(LOADER.read_bytes()).hexdigest() != LOADER_SHA:
        sys.exit('Loader hash mismatch')
    ld = call([RKDEV, 'ld'], timeout=15)
    if ld['stdout'].count('DevNo=') != 1 or 'Maskrom' not in ld['stdout']:
        sys.exit('Expected exactly one MaskROM device: ' + ld['stdout'])
    print(call([RKDEV, 'db', LOADER], timeout=45)['stdout'])
    sys.exit(0)

record = json.loads(Path(args.image_json).read_text())
base = Path(args.image_json).with_suffix('')
zimage = Path(record['zimage']['path']).read_bytes()
trust = base.with_name(base.name + '.rehearsal-trust.bin').read_bytes()
if hashlib.sha256(zimage).hexdigest() != record['zimage']['sha256'] or \
        hashlib.sha256(trust).hexdigest() != record['rehearsal_trust_sha256']:
    sys.exit('zImage / rehearsal trust blob changed since packing')


def read(address, length, name):
    path = run / name
    call([TOOL, 'read', hex(address), str(length), path])
    return path.read_bytes()


vendor = (paths.RKBIN / 'bin/rk32/rk322x_usbplug_v2.56.bin').read_bytes()
offset = 0x6300231c - (0x63001000 - 0x3bc)
if read(0x6300231c, 40, 'dispatcher.bin') != vendor[offset:offset + 40]:
    sys.exit('Resident dispatcher differs')
if read(0x63002a58, 8, 'padding-guard.bin')[2:6] != bytes.fromhex('00bf00bf'):
    sys.exit('No-padding helper not loaded')
if read(CALLBACK, 4, 'callback-before.bin') != bytes(4):
    sys.exit('Callback slot occupied')

for name, address, data in (('zImage', KERNEL, zimage), ('trust', TRUST, trust)):
    padded = data + bytes(-len(data) % 4)
    src = run / (name + '.bin')
    src.write_bytes(padded)
    print(f'Uploading {name}: {len(padded)} bytes at {address:#x}', flush=True)
    call([TOOL, 'write-file', hex(address), src], timeout=600)
    back = run / (name + '.readback.bin')
    call([TOOL, 'read-file', hex(address), str(len(padded)), back], timeout=600)
    if back.read_bytes() != padded:
        sys.exit(name + ' readback mismatch; not launching')
    back.unlink(); src.unlink()
    print(f'  verified sha256 {hashlib.sha256(data).hexdigest()}', flush=True)

cb = run / 'callback-new.bin'
cb.write_bytes(struct.pack('<I', TRUST))
call([TOOL, 'write-ram', hex(CALLBACK), cb])
if read(CALLBACK, 4, 'callback-readback.bin') != struct.pack('<I', TRUST):
    sys.exit('Callback readback mismatch')
print('Entering trust entry at 0x68400000 (USB loss expected)...', flush=True)
r = call([TOOL, 'execute-scratch', '0x62001000'], timeout=10, required=False)
if r['returncode'] == 0:
    sys.exit('Entry returned to the helper: it refused the environment (see breadcrumbs at 0x67d03000)')
print('Handoff sent. Next: ../lab.py sh "reg 0x67d03000 6"', flush=True)
