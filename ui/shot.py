#!/usr/bin/env python3
"""Receive one screenshot from the dongle and save it as PNG (for reviewing the UI).

Mac:    python3 shot.py NAME            # waits on the USB bridge address, port 8001
Dongle: nc 192.168.2.1 8001 < /opt/shot.ppm
Writes shots/NAME.png. Binds only to the Internet Sharing bridge (bridge100).
"""
import re, socket, struct, subprocess, sys, zlib
from pathlib import Path

name = sys.argv[1] if len(sys.argv) > 1 else 'shot'
count = int(sys.argv[2]) if len(sys.argv) > 2 else 1  # shot.py NAME N: receive N images NAME-0..
ip = re.search(r'inet (\d+\.\d+\.\d+\.\d+)', subprocess.run(['ifconfig', 'bridge100'], capture_output=True, text=True).stdout)[1]
def save(data, name):
    m = re.match(rb'P6\s+(\d+)\s+(\d+)\s+255\s', data)
    w, h = int(m[1]), int(m[2]); px = data[m.end():m.end() + w * h * 3]
    assert len(px) == w * h * 3, f'short image: {len(px)} of {w*h*3}'
    raw = b''.join(b'\0' + bytes(px[y * w * 3:(y + 1) * w * 3]) for y in range(h))
    chunk = lambda t, d: struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d))
    png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw, 6)) + chunk(b'IEND', b'')
    out = Path(__file__).resolve().parent / 'shots' / f'{name}.png'; out.parent.mkdir(exist_ok=True); out.write_bytes(png)
    print(f'saved {out} {w}x{h}')

srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind((ip, 8001)); srv.listen(1); srv.settimeout(120)
print(f'waiting on {ip}:8001', flush=True)
for i in range(count):
  conn, _ = srv.accept(); data = bytearray()
  while chunk := conn.recv(1 << 20):
    data += chunk
  conn.close()
  save(data, name if count == 1 else f'{name}-{i}')
srv.close()
