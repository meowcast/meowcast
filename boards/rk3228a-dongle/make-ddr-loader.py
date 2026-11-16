#!/usr/bin/env python3
"""Derive a RAM-download loader whose DDR init runs DDR3 at a chosen frequency.

Only the CODE471 (DDR init) payload of the already verified `nopad` loader is
replaced. The replacement is the same rkbin rk322x_ddr_300MHz_v1.10.bin, with
its documented parameter block changed by Rockchip's own ddrbin_tool.py
(RK322x ddrbin >= V1.08 supports a DDR frequency change). The script asserts
that only the DDR3 frequency field differs, rebuilds the RC4 payload and the
vendor CRC, then decodes the result again. Nothing is written to the device.

The stock firmware of this dongle (NOR loader, DDR V1.07) runs DDR3 at 600 MHz,
so 600 is the default and the frequency proven on this board.
"""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parent
sys.path.insert(0, str(root))
import paths  # noqa: E402
rkbin = paths.RKBIN
sys.path.insert(0, str(paths.ROCKUSB))
from inspect_loader import rc4  # noqa: E402

BASE = rkbin/'rk322x_loader_v1.10.256_nopad.bin'
BASE_SHA = '4b67c390ac5e55ee4493719e9fb3956c0db8967fc865a414d118d44c52334a39'
DDR = rkbin/'bin/rk32/rk322x_ddr_300MHz_v1.10.bin'
DDR_SHA = 'aff1848efd3993bcec8b832e1ed45cfc4d57a8be46b420c9940698a564f7af73'
RKBIN_COMMIT = '3e288fe814e059dd06833495f845cab04ac20a5c'


def crc(data):
    value = 0
    for byte in data:
        value ^= byte << 24
        for _ in range(8):
            value = ((value << 1) ^ (0x04c10db7 if value & 0x80000000 else 0)) & 0xffffffff
    return value


def param_block(blob):
    i = blob.find(struct.pack('<I', 0x12345678))
    if i < 0 or blob.find(struct.pack('<I', 0x12345678), i+1) >= 0:
        raise SystemExit('Expected exactly one ddrbin parameter block')
    return i


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--mhz', type=int, default=600)
    # DDR controller idle power-down / self-refresh (stock DTB dram_timing:
    # sr_idle=12, pd_idle=32). 0 = disabled (rkbin default).
    p.add_argument('--sr-idle', type=int, default=0)
    p.add_argument('--pd-idle', type=int, default=0)
    args = p.parse_args()
    if not (0 <= args.sr_idle <= 255 and 0 <= args.pd_idle <= 255):
        raise SystemExit('Refusing sr_idle/pd_idle outside 0..255')
    if not 300 <= args.mhz <= 800:
        raise SystemExit('Refusing DDR3 frequency outside 300..800 MHz')
    base, ddr = BASE.read_bytes(), DDR.read_bytes()
    if hashlib.sha256(base).hexdigest() != BASE_SHA or hashlib.sha256(ddr).hexdigest() != DDR_SHA:
        raise SystemExit('Base loader or DDR blob hash mismatch')
    tool = subprocess.check_output(['git', '-C', str(rkbin), 'show', RKBIN_COMMIT+':tools/ddrbin_tool.py'])
    out = paths.OUT/'loader'
    out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp/'ddrbin_tool.py').write_bytes(tool)
        blob = tmp/'ddr.bin'
        blob.write_bytes(ddr)
        (tmp/'param.txt').write_text('start tag=0x12345678\nddr3_freq=%d\nsr_idle=%d\npd_idle=%d\nend\n'
                                     % (args.mhz, args.sr_idle, args.pd_idle))
        log = subprocess.run([sys.executable, '-I', str(tmp/'ddrbin_tool.py'), 'rk322x',
                              str(tmp/'param.txt'), str(blob)], cwd=tmp, capture_output=True, text=True)
        if log.returncode:
            raise SystemExit('ddrbin_tool failed:\n'+log.stdout+log.stderr)
        patched = blob.read_bytes()
        check = subprocess.run([sys.executable, '-I', str(tmp/'ddrbin_tool.py'), 'rk322x', '-g',
                                str(tmp/'gen.txt'), str(blob)], cwd=tmp, capture_output=True, text=True)
        gen = (tmp/'gen.txt').read_text()
    if len(patched) != len(ddr):
        raise SystemExit('ddrbin_tool changed the blob size')
    diff = [i for i in range(len(ddr)) if ddr[i] != patched[i]]
    table_start = param_block(ddr)+12  # magic + 8 reserved bytes, then four u16 frequencies
    table = range(table_start, table_start+8, 2)
    if [struct.unpack_from('<H', ddr, i)[0] for i in table] != [300]*4:
        raise SystemExit('Unexpected original frequency table')
    # Allowed changes: the DDR3 frequency (fourth u16 of the table) and the
    # sr/pd idle word at parameter block + 0x1c (pd_idle low 16, sr_idle high 16).
    freq_field = table_start + 6
    idle_word = param_block(ddr) + 0x1c
    allowed = {freq_field, freq_field+1} | set(range(idle_word, idle_word+4))
    if set(diff) - allowed or struct.unpack_from('<H', patched, freq_field)[0] != args.mhz \
            or struct.unpack_from('<I', ddr, idle_word)[0] != 0 \
            or struct.unpack_from('<I', patched, idle_word)[0] != (args.sr_idle << 16 | args.pd_idle):
        raise SystemExit('Unexpected ddrbin change at offsets %r' % diff)
    expect = {'ddr2_freq': 300, 'lp2_freq': 300, 'ddr3_freq': args.mhz, 'lp3_freq': 300,
              'sr_idle': args.sr_idle, 'pd_idle': args.pd_idle}
    for key, value in expect.items():
        if '%s=%d' % (key, value) not in gen.splitlines():
            raise SystemExit('ddrbin_tool readback mismatch for '+key)

    loader = bytearray(base)
    count, table, stride = struct.unpack_from('<BIB', loader, 25)  # CODE471 entries
    _, kind, _, begin, length, _ = struct.unpack_from('<BI40sIII', loader, table)
    if count != 1 or stride != 57 or kind != 1 or rc4(bytes(loader[begin:begin+length]))[:len(ddr)] != ddr:
        raise SystemExit('Base loader CODE471 layout differs from the verified one')
    loader[begin:begin+length] = rc4(patched+bytes(length-len(patched)))
    loader[-4:] = struct.pack('<I', crc(loader[:-4]))
    decoded = rc4(bytes(loader[begin:begin+length]))
    if decoded[:len(patched)] != patched or any(decoded[len(patched):]):
        raise SystemExit('Re-decoded payload mismatch')
    changed = [i for i in range(len(base)) if base[i] != loader[i]]
    if changed and (min(changed) < begin or max(changed) >= begin+length) and \
            any(not (begin <= i < begin+length or i >= len(base)-4) for i in changed):
        raise SystemExit('Changes outside CODE471 payload and CRC')
    tag = 'ddr%d' % args.mhz + ('_sr%d_pd%d' % (args.sr_idle, args.pd_idle) if args.sr_idle or args.pd_idle else '')
    name = 'rk322x_loader_%s_nopad.bin' % tag
    (out/name).write_bytes(loader)
    (out/(tag+'.bin')).write_bytes(patched)
    record = {'loader': name, 'bytes': len(loader), 'sha256': hashlib.sha256(loader).hexdigest(),
              'base_loader_sha256': BASE_SHA, 'ddr_blob_source': 'rkbin '+RKBIN_COMMIT+' bin/rk32/'+DDR.name,
              'ddr_blob_source_sha256': DDR_SHA, 'ddr3_freq_mhz': args.mhz, 'sr_idle': args.sr_idle, 'pd_idle': args.pd_idle,
              'ddr_blob_sha256': hashlib.sha256(patched).hexdigest(),
              'changed_blob_offsets': diff, 'crc': hex(crc(loader[:-4])),
              'unchanged': 'CODE472 usbplug nopad helper, FlashData/FlashBoot entries, header',
              'ddrbin_tool_stdout': log.stdout.strip().splitlines()}
    (out/(tag+'.json')).write_text(json.dumps(record, indent=2)+'\n')
    print(json.dumps(record, indent=2))


if __name__ == '__main__':
    main()
