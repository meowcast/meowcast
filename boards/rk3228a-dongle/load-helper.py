#!/usr/bin/env python3
"""MaskROM RAM download of the USB helper with a selectable DDR3 frequency.

Same single `rkdeveloptool db` operation as boards/rk3228a-dongle/rockusb/rockusb_ram_loader.py
(no flash commands). --ddr 300 uses the original verified nopad loader;
--ddr 533 (default since 12 Nov) / 600 use output/loader/rk322x_loader_ddrNNN_nopad.bin,
which differ only in the DDR-init frequency field (see make-ddr-loader.py).
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import subprocess

import sys
root = Path(__file__).resolve().parent
sys.path.insert(0, str(root))
import paths  # noqa: E402
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--ddr', type=int, choices=[300, 533, 600], default=533)
p.add_argument('--variant', help='loader variant tag from make-ddr-loader.py, e.g. sr12_pd32 (DDR600) or ddr400')
args = p.parse_args()
# Pinned loader records (make-ddr-loader.py output; each differs from the
# verified rkbin loader only in the DDR-init parameter block + CRC).
PINNED = {600: '1240b53c8bf8c332c5d946e95fec075526e7f8a924b3eb34c7be8a71ccb17638',
          # Default since 12 Nov: DDR600 had a DQ1 timing margin problem (bit 1/17
          # errors under heavy load); DDR533 was error-free down to vdd_logic 1.05 V.
          533: 'd34ea2f116515d20eacd562ec5686ce1553f7ce217854402ee5e8acf16183b81'}
if args.ddr == 300:
    loader = paths.RKBIN/'rk322x_loader_v1.10.256_nopad.bin'
    expected = '4b67c390ac5e55ee4493719e9fb3956c0db8967fc865a414d118d44c52334a39'
elif args.variant:
    tag = args.variant if args.variant.startswith('ddr') else 'ddr600_'+args.variant
    record = json.loads((paths.OUT/'loader'/(tag+'.json')).read_text())
    loader = paths.OUT/'loader'/record['loader']
    expected = record['sha256']
else:
    record = json.loads((paths.OUT/'loader'/('ddr%d.json' % args.ddr)).read_text())
    loader = paths.OUT/'loader'/record['loader']
    expected = record['sha256']
    if expected != PINNED[args.ddr]:
        raise SystemExit('DDR%d loader record changed; re-review make-ddr-loader.py output' % args.ddr)
if hashlib.sha256(loader.read_bytes()).hexdigest() != expected:
    raise SystemExit('Loader hash mismatch')
tool = paths.RKDEV
check = subprocess.run([str(tool), 'ld'], capture_output=True, text=True, timeout=20)
if check.stdout.count('DevNo=') != 1 or 'Vid=0x2207,Pid=0x320b' not in check.stdout or 'Maskrom' not in check.stdout:
    raise SystemExit('Expected exactly one RK322x MaskROM device: '+check.stdout+check.stderr)
event = {'timestamp': datetime.datetime.now().astimezone().isoformat(), 'ddr_mhz': args.ddr, 'variant': args.variant,
         'command': ['db', str(loader)], 'loader_sha256': expected, 'device_before': check.stdout}
try:
    result = subprocess.run([str(tool), 'db', str(loader)], capture_output=True, text=True, timeout=45)
    event.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
except subprocess.TimeoutExpired as error:
    event.update(timeout=True, stdout=(error.stdout or b'').decode(errors='replace'),
                 stderr=(error.stderr or b'').decode(errors='replace'))
paths.LOGS.mkdir(exist_ok=True)
with (paths.LOGS/'rockusb-loader.jsonl').open('a') as log:
    log.write(json.dumps(event)+'\n')
print(json.dumps(event, indent=2))
raise SystemExit(event.get('returncode', 124))
