#!/usr/bin/env python3
"""Boot the real ARM kernel/initramfs on QEMU virt, not an RK3228 emulation."""
from pathlib import Path
import os
import selectors
import subprocess
import time

root = Path(__file__).resolve().parent
command = ['qemu-system-arm', '-M', 'virt', '-cpu', 'cortex-a7', '-m', '256M',
           '-kernel', str(root/'output/zImage'), '-append',
           'console=ttyAMA0 loglevel=6 rdinit=/init panic=0',
           '-display', 'none', '-monitor', 'none', '-serial', 'stdio', '-nic', 'none', '-no-reboot']
p = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
selector = selectors.DefaultSelector()
selector.register(p.stdout, selectors.EVENT_READ)
data = bytearray()
sent = False
passed = False
deadline = time.monotonic()+45
try:
    while time.monotonic() < deadline and p.poll() is None:
        for key, _ in selector.select(0.2):
            chunk = os.read(key.fileobj.fileno(), 65536)
            if not chunk:
                continue
            data.extend(chunk)
        if (b'RAMBOOT_READY' in data or data.endswith(b'# ')) and not sent:
            time.sleep(1)
            p.stdin.write(b'bringup-report; echo QEMU_SHELL_OK; uname -r; free; cat /proc/mounts\n')
            p.stdin.flush()
            sent = True
        if b'RAMBOOT_READY' in data and b'\r\nQEMU_SHELL_OK\r\n' in data and b'rootfs / rootfs rw' in data:
            passed = True
            break
finally:
    p.terminate()
    try:
        tail, _ = p.communicate(timeout=3)
    except subprocess.TimeoutExpired:
        p.kill()
        tail, _ = p.communicate()
    data.extend(tail)
    (root/'logs/qemu-smoke.log').write_bytes(data)
if not passed:
    print(data.decode(errors='replace')[-10000:])
    raise SystemExit('QEMU smoke test FAILED; see logs/qemu-smoke.log')
print('PASS: ARM Linux reached init, produced a hardware report, and ran an interactive BusyBox shell.')
print('QEMU virt validates the image/userspace only; check RK3228 peripherals and handoff separately on hardware.')
