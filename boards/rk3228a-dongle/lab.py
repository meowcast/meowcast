#!/usr/bin/env python3
"""Hands-off hardware loop for the RK3228A RAM-boot lab.

The perf image sets the MaskROM flag at handoff and arms the watchdog, so a
reboot, panic, lockup or hard hang returns the dongle to MaskROM by itself.
This script then reloads it without anyone touching the board.

  lab.py boot [--dir D] [--dtb F]   wait for MaskROM, load DDR helper, boot, wait for shell
  lab.py sh 'CMD' [--timeout S]     run CMD in the USB console shell, print its output
  lab.py reboot                     reboot into MaskROM and wait for it
  lab.py cycle [--dir D] [--dtb F]  reboot (if Linux is up) + boot
  lab.py state                      print linux / maskrom / absent

Exit codes: 0 ok, 2 no shell / no device, 3 command timed out.
"""
import argparse
from datetime import datetime, timezone
import fcntl
import glob
import os
from pathlib import Path
import secrets
import select
import struct
import subprocess
import sys
import termios
import time

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))
import paths  # noqa: E402
sys.path.insert(0, str(paths.OS))
from console_markers import frame_command  # noqa: E402
RKDEV = paths.RKDEV
LOG = paths.LOGS/('lab-'+datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'.log')


def log(msg):
    line = f'[{datetime.now().strftime("%H:%M:%S")}] {msg}'
    print(line, flush=True)
    with LOG.open('a') as f:
        f.write(line+'\n')


def maskrom_present():
    try:
        out = subprocess.run([str(RKDEV), 'ld'], capture_output=True, text=True, timeout=15).stdout
    except subprocess.TimeoutExpired:
        return False
    return 'Maskrom' in out and 'Pid=0x320b' in out


def console_device():
    devs = glob.glob('/dev/cu.usbmodem*')
    return devs[0] if len(devs) == 1 else None


def state():
    if console_device():
        return 'linux'
    if maskrom_present():
        return 'maskrom'
    return 'absent'


def wait_for(pred, timeout, what):
    end = time.monotonic()+timeout
    while time.monotonic() < end:
        if pred():
            return True
        time.sleep(1)
    log(f'timeout waiting for {what} ({timeout}s)')
    return False


def open_console(dev):
    fd = os.open(dev, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    attr = termios.tcgetattr(fd)
    attr[0] = 0; attr[1] = 0
    attr[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    attr[3] = 0
    attr[4] = termios.B115200; attr[5] = termios.B115200
    attr[6][termios.VMIN] = 0; attr[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, attr)
    fcntl.ioctl(fd, termios.TIOCMBIS, struct.pack('I', termios.TIOCM_DTR | termios.TIOCM_RTS))
    return fd


def run_shell(cmd, timeout=60, quiet=False):
    """Run cmd in the console shell. Returns (rc, output); rc 0/2/3 as documented."""
    dev = console_device()
    if not dev:
        return 2, ''
    nonce = secrets.token_hex(8)
    framed, begin, end = frame_command(cmd, nonce)
    try:
        fd = open_console(dev)
    except OSError as e:
        log(f'console open failed: {e}')
        return 2, ''
    buf = bytearray()
    try:
        os.write(fd, b'\n')
        # Drain the prompt briefly, then send the framed command.
        t0 = time.monotonic()
        while time.monotonic()-t0 < 1.0:
            r, _, _ = select.select([fd], [], [], 0.1)
            if r:
                try:
                    os.read(fd, 65536)
                except OSError:
                    pass
        os.write(fd, framed.encode())
        deadline = time.monotonic()+timeout
        while time.monotonic() < deadline:
            r, _, _ = select.select([fd], [], [], 0.25)
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return 2, buf.decode(errors='replace')
                buf.extend(data)
                s = buf.find(begin)
                if s >= 0 and buf.find(end, s+len(begin)) >= 0:
                    out = buf[s+len(begin):buf.find(end, s+len(begin))]
                    text = out.decode(errors='replace').replace('\r\n', '\n').strip('\n')
                    with LOG.open('a') as f:
                        f.write(f'$ {cmd}\n{text}\n')
                    if not quiet:
                        print(text, flush=True)
                    return 0, text
            elif not os.path.exists(dev):
                return 2, buf.decode(errors='replace')
        with LOG.open('a') as f:
            f.write(f'$ {cmd}\n[timeout] {buf.decode(errors="replace")}\n')
        return 3, buf.decode(errors='replace')
    finally:
        os.close(fd)


def boot(args):
    if state() == 'linux':
        log('Linux already up; use cycle to reboot first')
        return 2
    if not wait_for(maskrom_present, args.maskrom_timeout, 'MaskROM'):
        return 2
    time.sleep(1)
    log('MaskROM present: loading DDR helper (%s)' % (args.ddr_variant or 'default DDR533'))
    helper = [sys.executable, str(ROOT/'load-helper.py')] + (['--variant', args.ddr_variant] if args.ddr_variant else [])
    r = subprocess.run(helper, capture_output=True, text=True)
    if r.returncode:
        # The helper may already be resident (a previous load); ramboot.py's
        # guard verifies the resident dispatcher before uploading anything.
        log('load-helper failed (helper already resident?); trying ramboot directly')
    time.sleep(1)
    cmd = [sys.executable, str(ROOT/'ramboot.py'), 'boot', '--dir', args.dir]
    if args.dtb:
        cmd += ['--dtb', args.dtb]
    if args.entry:
        cmd += ['--entry', args.entry]
    log('ramboot: '+' '.join(cmd[2:]))
    r = subprocess.run(cmd, capture_output=True, text=True)
    tail = (r.stdout+r.stderr).strip().splitlines()[-3:]
    log('ramboot: '+' | '.join(tail))
    if 'Handoff sent' not in r.stdout:
        return 2
    t0 = time.monotonic()
    if not wait_for(console_device, args.shell_timeout, 'USB console'):
        log('no console; MaskROM present again: %s' % maskrom_present())
        return 2
    time.sleep(2)
    rc, out = run_shell('lab-status 2>/dev/null || cat /proc/cmdline', timeout=30)
    log(f'shell up {time.monotonic()-t0:.1f}s after handoff (rc={rc})')
    return rc


def reboot(args):
    st = state()
    if st == 'linux':
        rc, out = run_shell('lab-reboot', timeout=15, quiet=True)
        log(f'lab-reboot sent (rc={rc})')
    elif st == 'maskrom':
        log('already in MaskROM')
        return 0
    ok = wait_for(lambda: state() == 'maskrom', args.maskrom_timeout, 'MaskROM after reboot')
    if ok:
        log('back in MaskROM')
    return 0 if ok else 2


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('action', choices=['boot', 'sh', 'reboot', 'cycle', 'state'])
    p.add_argument('command', nargs='?')
    p.add_argument('--dir', default=str(paths.OUT))
    p.add_argument('--dtb')
    p.add_argument('--entry', help='handoff stage variant (ramboot.py --entry)')
    p.add_argument('--ddr-variant', help='DDR loader variant, e.g. sr12_pd32 (make-ddr-loader.py)')
    p.add_argument('--timeout', type=int, default=60)
    p.add_argument('--maskrom-timeout', type=int, default=60)
    p.add_argument('--shell-timeout', type=int, default=60)
    args = p.parse_args()
    if args.action == 'state':
        print(state())
        return 0
    if args.action == 'sh':
        rc, out = run_shell(args.command or 'true', timeout=args.timeout)
        if rc:
            log(f'sh rc={rc}; state={state()}')
        return rc
    if args.action == 'boot':
        return boot(args)
    if args.action == 'reboot':
        return reboot(args)
    if args.action == 'cycle':
        rc = reboot(args)
        return rc or boot(args)


if __name__ == '__main__':
    sys.exit(main())
