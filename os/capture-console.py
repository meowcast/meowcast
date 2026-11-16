#!/usr/bin/env python3
"""Wait for the experimental CDC ACM console and retain its byte stream."""
import argparse
from datetime import datetime, timezone
import fcntl
import glob
import os
from pathlib import Path
import select
import struct
import termios
import time
import secrets
from console_markers import frame_command,completed

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--seconds',type=int,default=90)
p.add_argument('--command',default='bringup-report',help='Diagnostic shell commands to run after connection')
args = p.parse_args()
root = Path(__file__).resolve().parent
stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
log = root/'logs'/('usb-console-'+stamp+'.log')
deadline = time.monotonic()+args.seconds
fd = None
received = bytearray()
opened = None
sent = False
prompt_requested = False
nonce = secrets.token_hex(8)
framed,begin,end = frame_command(args.command,nonce)
print('Waiting for /dev/cu.usbmodem*; log: '+str(log),flush=True)
with log.open('xb') as output:
    try:
        while time.monotonic() < deadline:
            if fd is None:
                devices = glob.glob('/dev/cu.usbmodem*')
                if len(devices) > 1:
                    raise SystemExit('Multiple USB serial devices; select the dongle explicitly')
                if not devices:
                    time.sleep(0.25)
                    continue
                fd = os.open(devices[0],os.O_RDWR|os.O_NOCTTY|os.O_NONBLOCK)
                attr = termios.tcgetattr(fd)
                attr[0] = 0; attr[1] = 0
                attr[2] = termios.CS8|termios.CREAD|termios.CLOCAL
                attr[3] = 0
                attr[4] = termios.B115200; attr[5] = termios.B115200
                attr[6][termios.VMIN] = 0; attr[6][termios.VTIME] = 0
                termios.tcsetattr(fd,termios.TCSANOW,attr)
                fcntl.ioctl(fd,termios.TIOCMBIS,struct.pack('I',termios.TIOCM_DTR|termios.TIOCM_RTS))
                opened = time.monotonic()
                print('Opened '+devices[0],flush=True)
            ready, _, _ = select.select([fd],[],[],0.25)
            if ready:
                data = os.read(fd,65536)
                if data:
                    received.extend(data)
                    output.write(data); output.flush()
                    print(data.decode(errors='replace'),end='',flush=True)
            if not prompt_requested and time.monotonic()-opened > 1:
                os.write(fd,b'\n')
                prompt_requested = True
            if not sent and (received.endswith(b'# ') or time.monotonic()-opened > 20):
                # Framed nonce markers cannot be mistaken for echoed command
                # text when the interactive shell wraps a long line.
                os.write(fd,framed.encode())
                sent = True
            if completed(received,begin,end):
                print('\nConfirmed interactive USB shell on the actual dongle.',flush=True)
                break
    finally:
        if fd is not None:
            os.close(fd)
if not completed(received,begin,end):
    raise SystemExit('No confirmed interactive shell; inspect the captured log for partial progress.')
