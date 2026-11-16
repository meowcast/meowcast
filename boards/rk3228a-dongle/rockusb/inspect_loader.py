#!/usr/bin/env python3
"""Inspect Rockchip loader containers and compare their RAM payloads to local vendor files."""
import hashlib
import json
from pathlib import Path
import struct
import sys


def rc4(data):
    state = list(range(256))
    key = bytes([124, 78, 3, 4, 85, 5, 9, 7, 45, 44, 123, 56, 23, 13, 23, 17])
    j = 0
    for i in range(256):
        j = (j + state[i] + key[i % 16]) % 256
        state[i], state[j] = state[j], state[i]
    result = bytearray()
    i = j = 0
    for value in data:
        i = (i + 1) % 256
        j = (j + state[i]) % 256
        state[i], state[j] = state[j], state[i]
        result.append(value ^ state[(state[i] + state[j]) % 256])
    return bytes(result)


def inspect(path):
    data = path.read_bytes()
    assert data[:4] in (b"BOOT", b"LDR ")
    output = {"file": str(path), "size": len(data), "sha256": hashlib.sha256(data).hexdigest(),
              "chip_type": hex(struct.unpack_from('<I', data, 21)[0]), "entries": []}
    sources = {1: path.parent / 'bin/rk32/rk322x_ddr_300MHz_v1.10.bin',
               2: path.parent / 'bin/rk32/rk322x_usbplug_v2.56.bin'}
    for offset in (25, 31, 37):
        count, table, stride = struct.unpack_from('<BIB', data, offset)
        for index in range(count):
            size, kind, name, begin, length, delay = struct.unpack_from('<BI40sIII', data, table + index * stride)
            assert size == stride == 57 and begin + length <= len(data) - 4
            entry = {"kind": kind, "name": name.decode('utf-16le').rstrip('\0'),
                     "offset": begin, "size": length, "delay_ms": delay}
            payload = data[begin:begin + length]
            if kind in sources:
                source = sources[kind].read_bytes()
                full = rc4(payload)
                short_length = length % 65536
                truncated = rc4(payload[:short_length]) + payload[short_length:]
                entry['full_rc4_matches_vendor_payload'] = full[:len(source)] == source
                entry['uint16_rc4_matches_vendor_payload'] = truncated[:len(source)] == source
                entry['full_rc4_zero_padding'] = not any(full[len(source):])
            output['entries'].append(entry)
    return output


if __name__ == '__main__':
    for name in sys.argv[1:]:
        print(json.dumps(inspect(Path(name)), indent=2))
