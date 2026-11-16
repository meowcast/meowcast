#!/usr/bin/env python3
"""Build the wallpaper gallery of the meow store from wallpapers.tsv (macOS: uses the built-in sips).

    python3 -I prepare-wallpapers.py            then: sh ../../apps/build-apps.sh (in the builder)
                                                re-signs assets.json, and sh deploy.sh publishes

For every line of wallpapers.tsv the source image is downloaded once (cache: .cache/wallpapers,
checked against its SHA-256 every run) and turned into what the dongle loads:

  www/assets/wallpapers/<id>.jpg           960x540 q90: the launcher's sharp layer is half the
                                           output resolution at 1080p and 4K alike, so a larger
                                           file only costs download and decode time
  www/assets/wallpapers/<id>.jpg.preview   32x18 JPEG (~1 KB), embedded in the signed index by
                                           `appd assets`: shown blurred at once, also offline
  www/assets/wallpapers/thumbs.jpg         every wallpaper's 264x148 thumbnail in ONE picture
                                           (4 per row, in list order): Settings > Wallpaper needs
                                           one download and one decode for the whole grid
  www/assets/wallpapers/catalog.tsv        what Settings lists: the thumbnail sheet's geometry,
                                           then per wallpaper its asset, title, author and mean
                                           colour (the tile's placeholder until the sheet is here)
  www/assets/wallpapers/CREDITS.txt        authors, licences and sources

Files of wallpapers that left the list are removed.
"""
import hashlib
import pathlib
import struct
import subprocess
import sys
import tempfile
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
LIST = HERE / "wallpapers.tsv"
CACHE = HERE / ".cache" / "wallpapers"
OUT = HERE / "www" / "assets" / "wallpapers"
FULL = (960, 540)
PREVIEW = (32, 18)
THUMB = (264, 148)
COLS = 4
# The device accepts at most this many (ui/src/gallery.rs MAX_WALLS).
MAX_WALLS = 24


def sips(src, dst, size, fmt, quality=None):
    cmd = ["sips", "-z", str(size[1]), str(size[0]), "-s", "format", fmt]
    if quality:
        cmd += ["-s", "formatOptions", str(quality)]
    subprocess.run(cmd + [str(src), "--out", str(dst)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def read_bmp(path):
    """(width, height, rows of RGB bytes, top row first) of an uncompressed 24/32-bit BMP."""
    d = pathlib.Path(path).read_bytes()
    assert d[:2] == b"BM", f"{path}: not a BMP"
    off = struct.unpack_from("<I", d, 10)[0]
    w, h, _, bpp, comp = struct.unpack_from("<iiHHI", d, 18)
    assert bpp in (24, 32) and comp in (0, 3), f"{path}: {bpp}-bit BMP, compression {comp}"
    step = bpp // 8
    stride = (w * step + 3) & ~3
    rows = []
    for y in range(abs(h)):
        row = d[off + y * stride : off + y * stride + w * step]
        rgb = bytearray(w * 3)
        rgb[0::3], rgb[1::3], rgb[2::3] = row[2::step], row[1::step], row[0::step]
        rows.append(bytes(rgb))
    if h > 0:
        rows.reverse()  # stored bottom-up
    return w, abs(h), rows


def write_bmp(path, w, h, rows):
    stride = (w * 3 + 3) & ~3
    body = bytearray()
    for row in reversed(rows):
        bgr = bytearray(w * 3)
        bgr[0::3], bgr[1::3], bgr[2::3] = row[2::3], row[1::3], row[0::3]
        body += bgr + bytes(stride - w * 3)
    head = b"BM" + struct.pack("<IHHI", 54 + len(body), 0, 0, 54)
    head += struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0, len(body), 2835, 2835, 0, 0)
    pathlib.Path(path).write_bytes(head + body)


def sheet(thumbs, path):
    """The thumbnails (BMP paths) side by side, COLS per row; unused cells stay black."""
    tw, th = THUMB
    n_rows = -(-len(thumbs) // COLS)
    rows = [bytearray(tw * COLS * 3) for _ in range(n_rows * th)]
    for i, t in enumerate(thumbs):
        w, h, px = read_bmp(t)
        assert (w, h) == THUMB, f"{t}: {w}x{h}"
        x, y = (i % COLS) * tw * 3, (i // COLS) * th
        for r in range(th):
            rows[y + r][x : x + tw * 3] = px[r]
    write_bmp(path, tw * COLS, n_rows * th, [bytes(r) for r in rows])


def mean_colour(bmp):
    _, _, rows = read_bmp(bmp)
    px = b"".join(rows)
    n = len(px) // 3
    return "%02x%02x%02x" % tuple(sum(px[c::3]) // n for c in range(3))


def fetch(url, sha, dst):
    if not (dst.exists() and hashlib.sha256(dst.read_bytes()).hexdigest() == sha):
        print(f"  downloading {url}")
        req = urllib.request.Request(url, headers={"User-Agent": "prepare-wallpapers"})
        with urllib.request.urlopen(req, timeout=120) as r:
            data = r.read()
        got = hashlib.sha256(data).hexdigest()
        if got != sha:
            sys.exit(f"{url}: sha256 {got}, the list says {sha}")
        dst.write_bytes(data)
    return dst


def main():
    walls = []
    for n, line in enumerate(LIST.read_text().splitlines(), 1):
        if not line.strip() or line.startswith("#"):
            continue
        f = line.split("\t")
        if len(f) != 6 or not all(f):
            sys.exit(f"{LIST.name}:{n}: expected id, title, author, licence, url, sha256 (TAB-separated)")
        if not f[0].replace("-", "").isalnum() or f[0] != f[0].lower() or f[0] in ("thumbs", "catalog"):
            sys.exit(f"{LIST.name}:{n}: id {f[0]!r} (lower-case letters, digits and -)")
        walls.append(dict(zip(("id", "title", "author", "licence", "url", "sha256"), f)))
    ids = [w["id"] for w in walls]
    if len(set(ids)) != len(ids) or "default" not in ids or len(ids) > MAX_WALLS:
        sys.exit(f"{LIST.name}: ids must be unique, include 'default', and number at most {MAX_WALLS}")

    CACHE.mkdir(parents=True, exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    catalog = ["# meow wallpapers (written by prepare-wallpapers.py): the thumbnail sheet's asset,",
               "# cell width, cell height and cells per row; then one line per wallpaper, in sheet",
               "# order: asset, title, author, mean colour.",
               f"thumbs\twallpapers/thumbs.jpg\t{THUMB[0]}\t{THUMB[1]}\t{COLS}"]
    credits = []
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        thumbs = []
        for w in walls:
            print(w["id"])
            src = fetch(w["url"], w["sha256"], CACHE / (w["id"] + pathlib.PurePosixPath(w["url"]).suffix))
            full = OUT / f"{w['id']}.jpg"
            sips(src, full, FULL, "jpeg", 90)
            # Preview and thumbnail from the 960x540 copy (sips' area filter averages well at
            # these ratios, and the decode is much faster than the 5120x2880 source's).
            sips(full, tmp / "preview.jpg", PREVIEW, "jpeg", 85)
            (tmp / "preview.jpg").replace(OUT / f"{w['id']}.jpg.preview")
            sips(full, tmp / "preview.bmp", PREVIEW, "bmp")
            thumb = tmp / f"{w['id']}.bmp"
            sips(full, thumb, THUMB, "bmp")
            thumbs.append(thumb)
            catalog.append(f"w\twallpapers/{w['id']}.jpg\t{w['title']}\t{w['author']}\t{mean_colour(tmp / 'preview.bmp')}")
            credits.append(f"{w['id']}.jpg (+ {w['id']}.jpg.preview, and its cell of thumbs.jpg)\n"
                           f"  \"{w['title']}\" by {w['author']}, KDE.\n"
                           f"  Licence: {w['licence']}.\n"
                           f"  Source: {w['url']}\n"
                           f"  (sha256 {w['sha256']})\n")
        sheet(thumbs, tmp / "thumbs.bmp")
        subprocess.run(["sips", "-s", "format", "jpeg", "-s", "formatOptions", "85", str(tmp / "thumbs.bmp"), "--out", str(OUT / "thumbs.jpg")],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    (OUT / "catalog.tsv").write_text("\n".join(catalog) + "\n")
    (OUT / "CREDITS.txt").write_text(
        "Wallpapers of the meow store, from KDE Plasma (https://github.com/KDE/plasma-workspace-wallpapers).\n"
        "Each was downscaled to a 960x540 JPEG (q90), a 32x18 preview and a 264x148 thumbnail by\n"
        "../../../prepare-wallpapers.py; the launcher darkens and blurs them on the device for display.\n"
        "CC-BY-SA-4.0: https://creativecommons.org/licenses/by-sa/4.0/   LGPL-3.0: https://www.gnu.org/licenses/lgpl-3.0.html\n\n"
        + "\n".join(credits))
    keep = {"catalog.tsv", "CREDITS.txt", "thumbs.jpg"} | {f"{i}.jpg" for i in ids} | {f"{i}.jpg.preview" for i in ids}
    for p in OUT.iterdir():
        if p.name not in keep:
            print(f"removing {p.name} (not in the list any more)")
            p.unlink()
    total = sum((OUT / f"{i}.jpg").stat().st_size for i in ids)
    print(f"{len(ids)} wallpapers, {total // 1024} KiB; thumbs.jpg {(OUT / 'thumbs.jpg').stat().st_size // 1024} KiB "
          f"({THUMB[0] * COLS}x{-(-len(ids) // COLS) * THUMB[1]})")


if __name__ == "__main__":
    main()
