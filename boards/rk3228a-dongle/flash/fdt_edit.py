"""Minimal flattened-device-tree walker for same-size property edits (no dtc needed)."""
import struct

FDT_BEGIN_NODE, FDT_END_NODE, FDT_PROP, FDT_NOP, FDT_END = 1, 2, 3, 4, 9


def walk(blob):
    """Yield (path, name, value_offset, length) for every property."""
    magic, total, off_struct, off_strings = struct.unpack_from('>IIII', blob, 0)
    if magic != 0xd00dfeed or total != len(blob):
        raise ValueError('not an FDT')
    pos, path = off_struct, []
    while True:
        token, = struct.unpack_from('>I', blob, pos)
        pos += 4
        if token == FDT_BEGIN_NODE:
            end = blob.index(b'\0', pos)
            path.append(blob[pos:end].decode())
            pos = (end + 4) & ~3
        elif token == FDT_END_NODE:
            path.pop()
        elif token == FDT_PROP:
            length, nameoff = struct.unpack_from('>II', blob, pos)
            pos += 8
            s = off_strings + nameoff
            name = blob[s:blob.index(b'\0', s)].decode()
            yield '/'.join(path) or '/', name, pos, length
            pos = (pos + length + 3) & ~3
        elif token == FDT_NOP:
            continue
        elif token == FDT_END:
            return
        else:
            raise ValueError('bad FDT token %#x' % token)


def cells(blob, off, length):
    return list(struct.unpack_from('>%dI' % (length // 4), blob, off))


def set_cells(blob, path, name, old, new):
    """Replace a property's u32 cells; must match `old` exactly and keep size."""
    hits = [(o, n) for p, nm, o, n in walk(bytes(blob)) if p == path and nm == name]
    if len(hits) != 1:
        raise ValueError('%s:%s found %d times' % (path, name, len(hits)))
    o, n = hits[0]
    if cells(blob, o, n) != old or len(new) != len(old):
        raise ValueError('%s:%s is %r, expected %r' % (path, name, cells(blob, o, n), old))
    struct.pack_into('>%dI' % len(new), blob, o, *new)
