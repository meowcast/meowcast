#!/usr/bin/env python3
"""Upload a small workspace-built ARM ELF to a fixed RAM-only diagnostic path."""
import argparse
from datetime import datetime,timezone
import fcntl
import glob
import hashlib
import json
import os
from pathlib import Path
import secrets
import select
import struct
import termios
import time

root=Path(__file__).resolve().parent
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('elf')
args=p.parse_args()
src=Path(args.elf).resolve()
if not src.is_relative_to(root/'output'):raise SystemExit('ELF must be a workspace build output')
data=src.read_bytes()
if not 52<=len(data)<=262144 or data[:6]!=b'\x7fELF\x01\x01' or struct.unpack_from('<H',data,18)[0]!=40:
    raise SystemExit('Expected a bounded little-endian 32-bit ARM ELF')
digest=hashlib.sha256(data).hexdigest()
devices=glob.glob('/dev/cu.usbmodem*')
if len(devices)!=1:raise SystemExit('Expected exactly one dongle USB console')
stamp=datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
log=root/'logs'/('upload-'+stamp+'.jsonl')
fd=os.open(devices[0],os.O_RDWR|os.O_NOCTTY|os.O_NONBLOCK)
attrs=termios.tcgetattr(fd)
attrs[0]=0;attrs[1]=0;attrs[2]=termios.CS8|termios.CREAD|termios.CLOCAL;attrs[3]=0
attrs[4]=termios.B115200;attrs[5]=termios.B115200
attrs[6][termios.VMIN]=0;attrs[6][termios.VTIME]=0
termios.tcsetattr(fd,termios.TCSANOW,attrs)
fcntl.ioctl(fd,termios.TIOCMBIS,struct.pack('I',termios.TIOCM_DTR|termios.TIOCM_RTS))
nonce=secrets.token_hex(8)
sequence=0

def send(command):
    wire=command.encode();deadline=time.monotonic()+5
    while wire:
        if time.monotonic()>deadline:raise RuntimeError('Serial write timeout')
        _,ready,_=select.select([],[fd],[],.2)
        if ready:
            try:wire=wire[os.write(fd,wire):]
            except BlockingIOError:pass

def command(payload):
    global sequence
    sequence+=1
    marker=f'UP_{nonce}_{sequence}'
    send(payload+"; r=$?; printf '\\036"+marker+":%s\\037\\n' \"$r\"\n")
    received=bytearray();deadline=time.monotonic()+15
    prefix=b'\x1e'+marker.encode()+b':'
    while time.monotonic()<deadline:
        ready,_,_=select.select([fd],[],[],.2)
        if ready:received.extend(os.read(fd,65536))
        i=received.find(prefix)
        j=received.find(b'\x1f',i+len(prefix)) if i>=0 else -1
        if j>=0:
            code=received[i+len(prefix):j]
            with log.open('a') as f:f.write(json.dumps({'sequence':sequence,'status':code.decode(),'output':received.decode(errors='replace')})+'\n')
            if code!=b'0':raise RuntimeError('Target command failed: '+received.decode(errors='replace'))
            return bytes(received)
    raise RuntimeError('Target acknowledgment timeout')

print(f'Uploading {len(data)} bytes to /tmp/gpu-profile; evidence: {log}',flush=True)
try:
    send('\n');time.sleep(.2)
    command('stty -echo')
    command(': > /tmp/gpu-profile')
    for offset in range(0,len(data),128):
        octal=''.join('\\%03o'%b for b in data[offset:offset+128])
        command("printf '"+octal+"' >> /tmp/gpu-profile")
        if offset%8192==0:print(f'{offset}/{len(data)} bytes acknowledged',flush=True)
    result=command('sha256sum /tmp/gpu-profile')
    if (digest+'  /tmp/gpu-profile').encode() not in result:raise RuntimeError('Uploaded ELF hash mismatch')
    command('chmod 700 /tmp/gpu-profile')
    print('Verified target SHA-256: '+digest,flush=True)
finally:
    try:send('stty echo\n')
    finally:os.close(fd)
