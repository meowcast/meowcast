#!/usr/bin/env python3
"""Serve files to the dongle over macOS Internet Sharing (USB Ethernet).

Binds only to the Internet Sharing bridge address (bridge100 by default), so
nothing is exposed on Wi-Fi. On the dongle: `fetch NAME` downloads to /opt.
"""
import argparse
import functools
import http.server
import re
import subprocess
from pathlib import Path

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--dir', default=str(Path(__file__).resolve().parent/'deploy'))
p.add_argument('--iface', default='bridge100')
p.add_argument('--port', type=int, default=8000)
args = p.parse_args()
out = subprocess.run(['ifconfig', args.iface], capture_output=True, text=True).stdout
m = re.search(r'inet (\d+\.\d+\.\d+\.\d+)', out)
if not m:
    raise SystemExit(f'No IPv4 on {args.iface}: enable Internet Sharing to the dongle first')
root = Path(args.dir); root.mkdir(parents=True, exist_ok=True)
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(root))
server = http.server.ThreadingHTTPServer((m[1], args.port), handler)
print(f'Serving {root} on http://{m[1]}:{args.port}/', flush=True)
server.serve_forever()
