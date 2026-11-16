#!/usr/bin/env python3
"""RAM-download loader whose DDR init is the stock NOR DDR blob (V1.07, DDR3 600 MHz).

Only CODE471 of the verified `nopad` loader is replaced, with the linearised stock
blob (re-derived from your stock dump: IDBlock
pages 0x11000-0x14800 of the verified dump). Same RC4/CRC handling and checks as
boards/rk3228a-dongle/make-ddr-loader.py. Used to rehearse the flashed boot from RAM with the
exact DDR setup the stock miniloader will leave. Nothing is sent to the device.
"""
import hashlib
from pathlib import Path
import struct
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import paths  # noqa: E402
sys.path.insert(0, str(paths.ROCKUSB))
from inspect_loader import rc4  # noqa: E402

BASE = paths.RKBIN / 'rk322x_loader_v1.10.256_nopad.bin'
BASE_SHA = '4b67c390ac5e55ee4493719e9fb3956c0db8967fc865a414d118d44c52334a39'
RKBIN_DDR_SHA = 'aff1848efd3993bcec8b832e1ed45cfc4d57a8be46b420c9940698a564f7af73'


def crc(data):
    value = 0
    for byte in data:
        value ^= byte << 24
        for _ in range(8):
            value = ((value << 1) ^ (0x04c10db7 if value & 0x80000000 else 0)) & 0xffffffff
    return value


_, dump = paths.stock_dump()
# Re-derive the blob from the dump rather than trusting the saved copy.
stock = b''.join(dump[0x11000 + i * 0x1000:0x11000 + i * 0x1000 + 0x800] for i in range(4)).rstrip(b'\0')
if stock[:4] != b'RK32' or b'DDR Version V1.07 20180427' not in stock:
    sys.exit('Unexpected stock DDR blob')

base = BASE.read_bytes()
if hashlib.sha256(base).hexdigest() != BASE_SHA:
    sys.exit('Base loader hash mismatch')
loader = bytearray(base)
count, table, stride = struct.unpack_from('<BIB', loader, 25)
_, kind, _, begin, length, _ = struct.unpack_from('<BI40sIII', loader, table)
old = rc4(bytes(loader[begin:begin + length]))
if count != 1 or stride != 57 or kind != 1 or hashlib.sha256(old[:9124]).hexdigest() != RKBIN_DDR_SHA:
    sys.exit('Base loader CODE471 layout differs from the verified one')
if len(stock) > length:
    sys.exit('Stock blob larger than the CODE471 slot')
loader[begin:begin + length] = rc4(stock + bytes(length - len(stock)))
loader[-4:] = struct.pack('<I', crc(loader[:-4]))
decoded = rc4(bytes(loader[begin:begin + length]))
if decoded[:len(stock)] != stock or any(decoded[len(stock):]):
    sys.exit('Re-decoded payload mismatch')
changed = [i for i in range(len(base)) if base[i] != loader[i]]
if any(not (begin <= i < begin + length or i >= len(base) - 4) for i in changed):
    sys.exit('Changes outside CODE471 payload and CRC')
out = HERE / 'output' / 'rk322x_loader_stockddr_nopad.bin'
out.write_bytes(loader)
print(out, hashlib.sha256(loader).hexdigest(), 'stock blob', len(stock), 'bytes, slot', length)
