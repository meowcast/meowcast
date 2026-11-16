#!/bin/sh
# Rebuilds the synthetic fixtures (needs python3 and libwebp's cwebp, img2webp and dwebp).
# The `*.ref.png` files are what libwebp's own decoder makes of each file. The logo-* files
# are a made-up badge in the sizes and formats a station logo comes in; the JPEG and its
# reference are made with macOS's sips.
set -eu
cd "$(dirname "$0")"
python3 -I - <<'PY'
import zlib, struct, math
def png(path, w, h, px):
    raw = b''.join(b'\0' + bytes(px[y*w*4:(y+1)*w*4]) for y in range(h))
    def ch(t, d): return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d))
    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + ch(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0)) + ch(b'IDAT', zlib.compress(raw, 9)) + ch(b'IEND', b''))
w = h = 64
px = bytearray(w*h*4)
for y in range(h):
    for x in range(w):
        d = math.hypot(x-31.5, y-31.5)
        a = max(0.0, min(1.0, (29-d)/2))                  # a disc with a soft edge
        ring = 1.0 if 17 < d < 22 else 0.0
        bar = 1.0 if abs(y-32) < 5 and abs(x-32) < 15 else 0.0
        if d > 25: a *= 0.5                               # and a half-transparent rim
        px[(y*w+x)*4:(y*w+x)*4+4] = bytes((int(230*(1-ring) + 255*ring), min(255, int(40 + 150*bar + x)), min(255, 60 + 2*y), int(a*255)))
png('source.png', w, h, px)
png('plain.png', w, h, bytes((30, 90, 200, 255)) * (w*h))
PY
cwebp -quiet -q 80 -noalpha source.png -o lossy.webp
cwebp -quiet -q 80 source.png -o lossy-alpha.webp
cwebp -quiet -q 80 -alpha_q 40 source.png -o lossy-alpha-q40.webp
cwebp -quiet -lossless -noalpha source.png -o lossless.webp
cwebp -quiet -lossless -exact source.png -o lossless-alpha.webp
img2webp -lossless -exact -d 100 source.png plain.png -o animation.webp > /dev/null
python3 -I - <<'PY'
import zlib, struct, math
def chunk(t, d): return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d))
def png_rgba(path, w, h, px):
    raw = b''.join(b'\0' + bytes(px[y*w*4:(y+1)*w*4]) for y in range(h))
    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))
def badge(n, corner_alpha):
    px = bytearray(n*n*4); c = (n-1)/2
    for y in range(n):
        for x in range(n):
            dx, dy = (x-c)/c, (y-c)/c
            d = math.hypot(dx, dy)
            r, g, b = 24, 40+int(60*(y/n)), 110+int(90*(x/n))          # blue gradient ground
            if d < 0.78: r, g, b = 245, 196-int(70*d), 40               # amber disc
            if d < 0.62 and abs(dx) < 0.12 or (d < 0.62 and abs(dy+0.1) < 0.1 and dx > -0.45 and dx < 0.45): r, g, b = 250, 250, 250   # a white cross
            if 0.84 < d < 0.9: r, g, b = 255, 255, 255                  # ring
            a = 255
            if corner_alpha and d > 1.33: a = 253 if (x+y) % 2 else 254 # nearly opaque corners
            px[(y*n+x)*4:(y*n+x)*4+4] = bytes((r, g, b, a))
    return px
png_rgba('logo-src.png', 300, 300, badge(300, False))
png_rgba('logo-alpha-src.png', 300, 300, badge(300, True))
small = badge(96, False)
def soften(px, n):                                       # a 3x3 box blur: JPEG decoders differ most at hard colour edges
    out = bytearray(px)
    for y in range(n):
        for x in range(n):
            for c in range(3):
                t = k = 0
                for j in (-1, 0, 1):
                    for i in (-1, 0, 1):
                        if 0 <= x+i < n and 0 <= y+j < n: t += px[((y+j)*n+x+i)*4+c]; k += 1
                out[(y*n+x)*4+c] = t // k
    return out
png_rgba('logo-96-src.png', 96, 96, soften(soften(soften(small, 96), 96), 96))
# logo-96.png: an 8-bit palette PNG (colours quantised to a 6x6x6 cube)
pal = [(r*51, g*51, b*51) for r in range(6) for g in range(6) for b in range(6)]
idx = bytearray()
for y in range(96):
    idx.append(0)
    for x in range(96):
        r, g, b, _ = small[(y*96+x)*4:(y*96+x)*4+4]
        idx.append((round(r/51)*36) + (round(g/51)*6) + round(b/51))
open('logo-96.png', 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 96, 96, 8, 3, 0, 0, 0)) + chunk(b'PLTE', b''.join(bytes(c) for c in pal)) + chunk(b'IDAT', zlib.compress(bytes(idx), 9)) + chunk(b'IEND', b''))
PY
cwebp -quiet -q 80 -noalpha logo-src.png -o logo.webp
cwebp -quiet -q 80 -exact logo-alpha-src.png -o logo-alpha.webp
sips -s format jpeg -s formatOptions 92 logo-96-src.png --out logo-96.jpg > /dev/null
sips -s format png logo-96.jpg --out logo-96.jpg.ref.png > /dev/null
rm logo-src.png logo-alpha-src.png logo-96-src.png
for f in lossy lossy-alpha lossy-alpha-q40 lossless lossless-alpha logo logo-alpha; do dwebp -quiet "$f.webp" -o "$f.ref.png"; done
rm source.png plain.png
