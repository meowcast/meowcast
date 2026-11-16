#!/usr/bin/env python3
"""Build the RAM-download helper loader from the pinned rkbin checkout (tools/rkbin).

Output: tools/rkbin/rk322x_loader_v1.10.256_nopad.bin, the package every boot and flash tool
of this board loads into RAM through MaskROM. It is rkbin's RK322x loader (DDR init v1.10 +
usbplug v2.56) with one change to the usbplug: the four-byte Thumb BL at file offset 0x1e16,
which zeroes 512 bytes after every RAM write, becomes two NOPs. Without that, a small RAM
write corrupts the helper's own data.

The result is bit-exact (the header's build time is fixed), and its SHA-256 is pinned here
and in the tools that use it. Nothing is sent to a device.
"""
import hashlib
from pathlib import Path
import struct
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from inspect_loader import rc4  # noqa: E402

RKBIN = HERE / 'tools/rkbin'
BLOBS = RKBIN / 'bin/rk32'
SOURCES = {
    'rk322x_ddr_300MHz_v1.10.bin': 'aff1848efd3993bcec8b832e1ed45cfc4d57a8be46b420c9940698a564f7af73',
    'rk322x_usbplug_v2.56.bin': '273eb4ec41a6ef6bb2b9bbc3ba3fc3bd21a668fdefeff4e0242fe48ebcde424e',
}
NOPAD_SHA = '8d07397be1860bdf311037b6de475bc837b2cd2dc65559e34575db86d6e52dfd'
LOADER_SHA = '4b67c390ac5e55ee4493719e9fb3956c0db8967fc865a414d118d44c52334a39'
PATCH_AT, BEFORE, AFTER = 0x1e16, bytes.fromhex('0ef017ff'), bytes.fromhex('00bf00bf')
# "BOOT", header size, version 2.56, merger version, build time 2026-11-08 11:09:38, chip.
HEADER = bytes.fromhex('424f4f5466005602000000000301ea070b080b092641323233')
ENTRY = struct.Struct('<BI40sIII')


def crc(data):
    value = 0
    for byte in data:
        value ^= byte << 24
        for _ in range(8):
            value = ((value << 1) ^ (0x04c10db7 if value & 0x80000000 else 0)) & 0xffffffff
    return value


def pad(data):
    return data + bytes(-len(data) % 2048)


def whole(data):
    return rc4(pad(data))


def sectors(data):
    data = pad(data)
    return b''.join(rc4(data[i:i + 512]) for i in range(0, len(data), 512))


def read(name, sha=None):
    data = (BLOBS / name).read_bytes()
    if sha and hashlib.sha256(data).hexdigest() != sha:
        sys.exit(f'{name}: not the pinned rkbin file')
    return data


def main():
    ddr = read('rk322x_ddr_300MHz_v1.10.bin', SOURCES['rk322x_ddr_300MHz_v1.10.bin'])
    usbplug = read('rk322x_usbplug_v2.56.bin', SOURCES['rk322x_usbplug_v2.56.bin'])
    miniloader = read('rk322x_miniloader_v2.56.bin')
    if usbplug[PATCH_AT:PATCH_AT + 4] != BEFORE:
        sys.exit('usbplug: unexpected bytes at the patch offset')
    nopad = usbplug[:PATCH_AT] + AFTER + usbplug[PATCH_AT + 4:]
    if hashlib.sha256(nopad).hexdigest() != NOPAD_SHA:
        sys.exit('patched usbplug hash mismatch')
    # kind 1 = CODE471 (DDR init), 2 = CODE472 (USB helper), 4 = flash loader entries (unused
    # for a RAM download, but part of the container).
    groups = [[(1, 'rk322x_ddr_300MHz_v', whole(ddr), 1)],
              [(2, 'rk322x_usbplug_v2.5', whole(nopad), 0)],
              [(4, 'FlashData', sectors(ddr), 0), (4, 'FlashBoot', sectors(miniloader), 0)]]
    count = sum(len(g) for g in groups)
    tables, payloads = b'', b''
    table_at, data_at = 102, 102 + count * ENTRY.size
    descriptors = b''
    for group in groups:
        descriptors += struct.pack('<BIB', len(group), table_at + len(tables), ENTRY.size)
        for kind, name, data, delay in group:
            tables += ENTRY.pack(ENTRY.size, kind, name.encode('utf-16le'), data_at + len(payloads), len(data), delay)
            payloads += data
    head = HEADER + descriptors
    head = bytearray(head + bytes(102 - len(head)))
    head[44] = 1  # as boot_merger writes it for this chip
    body = bytes(head) + tables + payloads
    loader = body + struct.pack('<I', crc(body))
    got = hashlib.sha256(loader).hexdigest()
    if got != LOADER_SHA:
        sys.exit(f'loader hash {got} is not the pinned {LOADER_SHA}')
    (BLOBS / 'rk322x_usbplug_v2.56_nopad.bin').write_bytes(nopad)
    out = RKBIN / 'rk322x_loader_v1.10.256_nopad.bin'
    out.write_bytes(loader)
    print(f'{out}: {len(loader)} bytes, sha256 {got}')


if __name__ == '__main__':
    main()
