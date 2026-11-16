#!/usr/bin/env python3
"""Latin subsets of the Noto Sans faces for the firmware (stdlib only, no fonttools).

    python3 subset.py            NotoSans-*.ttf -> latin/NotoSans-*.ttf (+ latin/SHA256SUMS)

The firmware carries only these (~10 % of the full faces): every character the launcher draws
itself and the Latin alphabets (Latin-1, Latin Extended-A, Romanian comma letters), common
punctuation, currency and arrows. The full faces are network assets (fonts/NotoSans-*.ttf on the
store, see ../../../cloud/store/README.md); the launcher fetches one when some text needs a glyph the
subset lacks (an SSID, a phone or app name in another script) and swaps it in.

Glyph ids are kept ("retain gids"): a glyph has the same id in the subset and the full face,
so cached glyphs and the layout stay valid when the full face replaces the subset. Removed
glyphs become empty and are unmapped in cmap, so the launcher sees them as missing (id 0).

Also dropped, because ab_glyph never reads them: TrueType hinting (fpgm, prep, cvt, per-glyph
instructions), OpenType layout (GDEF, GPOS, GSUB: ab_glyph kerns from the legacy `kern` table
only, which Noto Sans does not have), glyph names (post version 3), DSIG and gasp.
"""
import hashlib
import struct
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FACES = ['NotoSans-Light.ttf', 'NotoSans-Regular.ttf', 'NotoSans-Medium.ttf']
KEEP_TABLES = {b'head', b'hhea', b'maxp', b'OS/2', b'hmtx', b'cmap', b'loca', b'glyf', b'name', b'post'}

RANGES = [
    (0x0020, 0x007E),  # Basic Latin
    (0x00A0, 0x017F),  # Latin-1 Supplement, Latin Extended-A (Polish, Czech, Turkish, ...)
    (0x0218, 0x021B),  # Romanian S/T with comma below
    (0x02C6, 0x02C7), (0x02D8, 0x02DD),  # spacing accents
    (0x2010, 0x2027), (0x2030, 0x203A), (0x2044, 0x2044),  # dashes, quotes, bullets, ellipsis, per mille
    (0x20AC, 0x20AC), (0x2122, 0x2122), (0x2212, 0x2212),  # euro, trade mark, minus
    (0x2190, 0x2195),  # arrows
]


def tables(data):
    n = struct.unpack('>H', data[4:6])[0]
    out = {}
    for i in range(n):
        tag, _, off, ln = struct.unpack('>4sIII', data[12 + 16 * i:28 + 16 * i])
        out[tag] = data[off:off + ln]
    return out


def cmap_map(cmap):
    """codepoint -> glyph id from the best Unicode subtable (format 12, else format 4)."""
    n = struct.unpack('>H', cmap[2:4])[0]
    subs = {}
    for i in range(n):
        pid, eid, off = struct.unpack('>HHI', cmap[4 + 8 * i:12 + 8 * i])
        subs[(pid, eid)] = off
    m = {}
    off = subs.get((3, 10)) or subs.get((0, 4))
    if off is not None and struct.unpack('>H', cmap[off:off + 2])[0] == 12:
        ngroups = struct.unpack('>I', cmap[off + 12:off + 16])[0]
        for g in range(ngroups):
            start, end, gid = struct.unpack('>III', cmap[off + 16 + 12 * g:off + 28 + 12 * g])
            for c in range(start, end + 1):
                m[c] = gid + c - start
        return m
    off = subs.get((3, 1)) or subs.get((0, 3))
    assert off is not None and struct.unpack('>H', cmap[off:off + 2])[0] == 4, 'no usable cmap'
    segx2 = struct.unpack('>H', cmap[off + 6:off + 8])[0]
    seg = segx2 // 2
    ends = struct.unpack(f'>{seg}H', cmap[off + 14:off + 14 + segx2])
    starts = struct.unpack(f'>{seg}H', cmap[off + 16 + segx2:off + 16 + 2 * segx2])
    deltas = struct.unpack(f'>{seg}h', cmap[off + 16 + 2 * segx2:off + 16 + 3 * segx2])
    ro_at = off + 16 + 3 * segx2
    ros = struct.unpack(f'>{seg}H', cmap[ro_at:ro_at + segx2])
    for i in range(seg):
        for c in range(starts[i], ends[i] + 1):
            if c == 0xFFFF:
                continue
            if ros[i] == 0:
                gid = (c + deltas[i]) & 0xFFFF
            else:
                p = ro_at + 2 * i + ros[i] + 2 * (c - starts[i])
                gid = struct.unpack('>H', cmap[p:p + 2])[0]
                gid = (gid + deltas[i]) & 0xFFFF if gid else 0
            if gid:
                m[c] = gid
    return m


def cmap_format4(m):
    """A cmap with one format 4 subtable (BMP) for {codepoint: gid}, for (0,3) and (3,1)."""
    segs = []  # [start, end, delta]
    for c in sorted(m):
        d = (m[c] - c) & 0xFFFF
        if segs and segs[-1][1] == c - 1 and segs[-1][2] == d:
            segs[-1][1] = c
        else:
            segs.append([c, c, d])
    segs.append([0xFFFF, 0xFFFF, 1])
    n = len(segs)
    es = 1 << (n.bit_length() - 1)
    body = struct.pack('>HHHH', 2 * n, 2 * es, es.bit_length() - 1, 2 * n - 2 * es)
    body += struct.pack(f'>{n}H', *[s[1] for s in segs]) + b'\0\0'
    body += struct.pack(f'>{n}H', *[s[0] for s in segs])
    body += struct.pack(f'>{n}H', *[s[2] for s in segs])
    body += struct.pack(f'>{n}H', *[0] * n)
    sub = struct.pack('>HHH', 4, 6 + len(body), 0) + body
    return struct.pack('>HH', 0, 2) + struct.pack('>HHI', 0, 3, 20) + struct.pack('>HHI', 3, 1, 20) + sub


def components(g):
    """Glyph ids used by a composite glyph."""
    out, p = [], 10
    while True:
        flags, gid = struct.unpack('>HH', g[p:p + 4])
        out.append(gid)
        p += 4 + (4 if flags & 1 else 2)
        p += 2 if flags & 8 else 4 if flags & 0x40 else 8 if flags & 0x80 else 0
        if not flags & 0x20:
            return out


def strip_instructions(g):
    """The glyph without its TrueType hinting instructions."""
    if len(g) == 0:
        return g
    nc = struct.unpack('>h', g[:2])[0]
    if nc >= 0:
        p = 10 + 2 * nc
        ilen = struct.unpack('>H', g[p:p + 2])[0]
        return g[:p] + b'\0\0' + g[p + 2 + ilen:]
    p = 10
    while True:
        flags = struct.unpack('>H', g[p:p + 2])[0]
        start = p
        p += 4 + (4 if flags & 1 else 2)
        p += 2 if flags & 8 else 4 if flags & 0x40 else 8 if flags & 0x80 else 0
        if not flags & 0x20:
            if flags & 0x100:  # WE_HAVE_INSTRUCTIONS: clear it, drop the instructions
                g = g[:start] + struct.pack('>H', flags & ~0x100) + g[start + 2:p]
            return g[:p]


def checksum(b):
    b = b + b'\0' * (-len(b) % 4)
    return sum(struct.unpack(f'>{len(b) // 4}I', b)) & 0xFFFFFFFF


def subset(src, chars):
    t = tables(src)
    head = bytearray(t[b'head'])
    num = struct.unpack('>H', t[b'maxp'][4:6])[0]
    long_loca = struct.unpack('>h', head[50:52])[0] == 1
    loca = struct.unpack(f'>{num + 1}{"I" if long_loca else "H"}', t[b'loca'][:(num + 1) * (4 if long_loca else 2)])
    if not long_loca:
        loca = [x * 2 for x in loca]
    glyf = t[b'glyf']
    glyph = lambda i: glyf[loca[i]:loca[i + 1]]
    full = cmap_map(t[b'cmap'])
    m = {c: full[c] for c in chars if c in full}
    keep, todo = {0}, [0] + list(m.values())
    while todo:
        gid = todo.pop()
        keep.add(gid)
        g = glyph(gid)
        if len(g) >= 10 and struct.unpack('>h', g[:2])[0] < 0:
            todo += [c for c in components(g) if c not in keep]
    new_glyf, offs = bytearray(), [0]
    for i in range(num):
        if i in keep:
            new_glyf += strip_instructions(glyph(i))
            new_glyf += b'\0' * (-len(new_glyf) % 4)
        offs.append(len(new_glyf))
    head[8:12] = b'\0\0\0\0'  # checkSumAdjustment, set below
    head[50:52] = struct.pack('>h', 1)  # long loca
    post = bytearray(t[b'post'][:32])
    post[0:4] = struct.pack('>I', 0x00030000)
    out = {tag: data for tag, data in t.items() if tag in KEEP_TABLES}
    out.update({b'head': bytes(head), b'glyf': bytes(new_glyf), b'loca': struct.pack(f'>{num + 1}I', *offs),
                b'cmap': cmap_format4(m), b'post': bytes(post)})
    tags = sorted(out)
    n = len(tags)
    es = 1 << (n.bit_length() - 1)
    font = bytearray(struct.pack('>IHHHH', 0x00010000, n, 16 * es, es.bit_length() - 1, 16 * n - 16 * es))
    off = 12 + 16 * n
    records, body = bytearray(), bytearray()
    for tag in tags:
        d = out[tag]
        records += struct.pack('>4sIII', tag, checksum(d), off + len(body), len(d))
        body += d + b'\0' * (-len(d) % 4)
    font += records + body
    adj = (0xB1B0AFBA - checksum(bytes(font))) & 0xFFFFFFFF
    i = tags.index(b'head')
    hp = struct.unpack('>I', font[12 + 16 * i + 8:12 + 16 * i + 12])[0]
    font[hp + 8:hp + 12] = struct.pack('>I', adj)
    return bytes(font), m, len(keep), num


def main():
    chars = sorted({c for a, b in RANGES for c in range(a, b + 1)})
    dest = HERE / 'latin'
    dest.mkdir(exist_ok=True)
    sums = []
    for face in FACES:
        src = (HERE / face).read_bytes()
        font, m, kept, num = subset(src, chars)
        (dest / face).write_bytes(font)
        sums.append(f'{hashlib.sha256(font).hexdigest()}  {face}\n')
        print(f'{face}: {len(src)} -> {len(font)} bytes, {len(m)} characters, {kept} of {num} glyphs')
    (dest / 'SHA256SUMS').write_text(''.join(sums))


if __name__ == '__main__':
    sys.exit(main())
