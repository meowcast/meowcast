#!/usr/bin/env python3
"""One fixed RK322x RAM-download operation; no flash-upgrade/write commands."""
import datetime
import hashlib
import json
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parent
loader = root / 'tools/rkbin/rk322x_loader_v1.10.256.bin'
expected = '4cd1f5195684cfa6d599ed52b8b9efdd84810515658f5b90bb970579160aed33'
if sys.argv[1:]==['--no-ram-padding']:
    loader=root/'tools/rkbin/rk322x_loader_v1.10.256_nopad.bin'
    expected='4b67c390ac5e55ee4493719e9fb3956c0db8967fc865a414d118d44c52334a39'
elif sys.argv[1:]:
    raise SystemExit('Unknown loader variant')
if hashlib.sha256(loader.read_bytes()).hexdigest() != expected:
    raise SystemExit('Loader hash mismatch')
tool = root / 'tools/rkdeveloptool/rkdeveloptool'
check = subprocess.run([str(tool), 'ld'], cwd=root, capture_output=True, text=True, timeout=20)
if check.returncode or check.stdout.count('DevNo=') != 1 or 'Vid=0x2207,Pid=0x320b' not in check.stdout or 'Maskrom' not in check.stdout:
    raise SystemExit('Expected exactly one RK322x MaskROM device: ' + check.stdout + check.stderr)
event = {'timestamp': datetime.datetime.now().astimezone().isoformat(),
         'command': ['db', str(loader)], 'loader_sha256': expected, 'device_before': check.stdout}
try:
    result = subprocess.run([str(tool), 'db', str(loader)], cwd=root,
                            capture_output=True, text=True, timeout=45)
    event.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
except subprocess.TimeoutExpired as error:
    event.update(timeout=True, stdout=(error.stdout or b'').decode(errors='replace'),
                 stderr=(error.stderr or b'').decode(errors='replace'))
with (root / 'rockusb-queries.jsonl').open('a') as log:
    log.write(json.dumps(event) + '\n')
print(json.dumps(event, indent=2))
raise SystemExit(event.get('returncode', 124))
