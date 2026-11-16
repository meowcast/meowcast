# meow store (apps and assets)

Everything the dongle downloads comes from one static site on Cloudflare:

**<https://store.example.org>** (a static-assets Worker, `wrangler.jsonc`).

It serves apps, the wallpapers (with the gallery Settings shows) and the full fonts. The firmware keeps only what it needs to work
offline; the rest comes from here over HTTPS. No local server is needed any more: `serve.py` on
the Mac is gone.

```sh
# in the builder: pack + sign the apps, copy the full fonts, sign the asset index
docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
  -w /fw/apps meowcast-builder:gpu sh build-apps.sh
sh cloud/store/deploy.sh      # checks every hash, deploys, compares the live assets.json
```

| Path in `www/` | What | Written by |
| --- | --- | --- |
| `index.json`, `apps/<id>/<code>/{manifest.json,manifest.sig,payload.tar}` | the app store | `../../apps/build-apps.sh` (`appd pack`) |
| `repo.json` | the store as a repository: its name and public key, which a device shows when this address is added in the Store app. Devices with this firmware have the store built in and never read it | `../../apps/build-apps.sh` (`appd repo`) |
| `assets.json`, `assets.sig` | signed index: SHA-256 and size of every file in `assets/`, plus previews | `../../apps/build-apps.sh` (`appd assets`) |
| `assets/wallpapers/<id>.jpg` | the wallpapers, 960×540 (56–240 KB each, 12 today); `default.jpg` is what a new device shows. Authors and licences: `assets/wallpapers/CREDITS.txt` | `prepare-wallpapers.py` |
| `assets/wallpapers/<id>.jpg.preview` | each one's 32×18 preview (~1 KB). Embedded in `assets.json` as base64, not served on its own (`.assetsignore`) | `prepare-wallpapers.py` |
| `assets/wallpapers/catalog.tsv` | what Settings > Wallpaper lists (907 bytes): the thumbnail sheet's name and geometry, then asset, title, author and mean colour per wallpaper | `prepare-wallpapers.py` |
| `assets/wallpapers/thumbs.jpg` | every wallpaper's 264×148 thumbnail in one picture (1056×444, 133 KB): one download and one decode for the whole grid | `prepare-wallpapers.py` |
| `assets/fonts/NotoSans-{Light,Regular,Medium}.ttf` | the full faces; the firmware has Latin subsets | `build-apps.sh` copies them from `../../ui/assets/fonts` (not in git) |
| `_headers` | Cache-Control: short for the indexes, immutable for app versions | by hand |

**Trust:** nothing on this server is trusted by the dongle.

- appd verifies every manifest and the asset index against the store key: the private key is
  `keys/dev-store.key` (never ship it, never put it under `www/`), and the public key is baked
  into the firmware.
- It checks every file's SHA-256 before anything uses it.
- HTTPS (meow-tls: BearSSL with the pinned roots in `../tls/roots.pem`) adds
  confidentiality and protects the unsigned `index.json`. Today's chain is the same as the
  other `*.workers.dev` hosts (WE1 → GTS Root R4 → GlobalSign Root CA).

**Checking a deployed store with the device's own code** (keep-alive client, signatures,
hashes; any Linux host, with a host build of meow-tls in `APPD_TLS`):

```sh
appd store-check --store https://store.example.org --keys cloud/store/keys
# 14 Nov, from the Mac's builder: index + 1 app = 5 requests on 1 connection, 662 ms;
# wallpaper 124 ms, each full font ~0.2-0.5 s
```

## A repository of your own

The Store app takes more repositories by address (Store > Repositories > Add a repository,
or `appctl repo add URL`). One is this same tree without the assets, on any static host:

```sh
appd keygen my.key                                   # once; my.key stays private
appd pack --manifest app.json --payload DIR --key my.key --store www   # per app version
appd repo --store www --key my.key --name "My apps"  # www/repo.json: name and public key
appd verify --store www --keys DIR_WITH_my.pub       # what a device will check
```

Publish `www/` (`index.json`, `repo.json`, `apps/`). The device shows the name and the key's
fingerprint (`appd repo` prints it) before the repository is added, then pins the key: every
app must be signed with it from then on. The rules a repository lives under, and what it
cannot do, are in [../../apps/README.md](../../apps/README.md#the-store-app-and-repositories). The
host needs `https://` with a certificate from Google Trust Services, GlobalSign or Let's
Encrypt; `http://` works for private addresses only.

## What the dongle keeps and what it downloads

| | On the device | From the store |
| --- | --- | --- |
| Wallpaper | which one the user chose (`wallpaper=` in `/data/ui/settings`), and the previews of all of them inside the last signed `assets.json` on `/data` (22 KB) | the chosen `wallpapers/<id>.jpg`, at every boot, into RAM |
| Wallpaper gallery | nothing | `wallpapers/catalog.tsv` and `wallpapers/thumbs.jpg`, when Settings opens, into RAM |
| Fonts | Latin subsets of Noto Sans in the initramfs (3 × 50 KB) | a full face, only when some text needs a glyph the subset lacks |
| Apps | signed manifests and grants of installed apps (`/data/appd`) | payloads, at their first launch after each boot, into RAM |
| Built-in fallback | the procedural wallpaper (code, no data), used before the device has ever been online | — |

**At boot:**

1. The launcher asks appd for the wallpaper, and appd answers at once with the preview from
   the saved index.
2. The launcher upscales and blurs the preview into a soft, out-of-focus version of the
   wallpaper, which is visible from the first frame, offline too.
3. As soon as the network is there (appd checks the default route and the NTP clock every
   second), appd downloads the wallpaper.
4. The launcher cross-fades into it over 0.9 s, like a camera pulling focus.
5. A failed download is retried with backoff (3 s doubling up to 60 s), and right away when the
   network comes back.

Measured on the dongle, 14 Nov (perf build, appd and UI from `/opt`, USB link with the Mac's
Internet Sharing):

- **Refresh:** 5 requests on 1 TLS connection in 450–686 ms.
- **Wallpaper:** 83 KB downloaded in 344 ms, then decoded and shaded in 266 ms (decode 93 ms;
  the 1920×1080 file took 0.9 s, decode 0.4 s).
- **Preview:** processed in ~110 ms, while the boot work runs.
- **Offline:** started with no default route, the home screen showed the blurred preview, and
  after the route came back the wallpaper focus-pulled in. Screenshots in
  `../../ui/shots/assets-offline2-{0,1}.png`.
- **Full font:** `fonts/NotoSans-Regular.ttf` (512 KB) arrived in 405 ms (`appd ctl asset`).

Nothing is written to flash for assets. Earlier firmware cached them in `/data/assets`, which
appd now deletes at start.

## Development store

A `wrangler dev` of this site works as a local store, for example to try assets before
publishing them:

```sh
cd cloud/store && npx --prefix ../relay wrangler dev --ip 0.0.0.0 --port 8787
# dongle: echo '{"store":"http://192.168.2.1:8787/"}' > /data/appd/config.json; appd-session restart
```

`config.json` wins over the firmware's `/etc/appd/store.conf`, which `os/build.sh`
writes from `STORE_URL`.

## Wallpapers

A curated selection from [KDE Plasma's wallpapers](https://github.com/KDE/plasma-workspace-wallpapers),
listed in [wallpapers.tsv](wallpapers.tsv): id, title, author, licence, source URL (pinned to a
commit) and SHA-256. Only 16:9 sources, the dark variant where there is one. The user chooses
among them in Settings > Wallpaper (`../../ui/README.md`, "Wallpaper gallery").

```sh
python3 -I cloud/store/prepare-wallpapers.py   # macOS (sips); then build-apps.sh + deploy.sh as above
```

- **To add or remove one:** edit `wallpapers.tsv` and run the script. It downloads each source
  once (`.cache/wallpapers`, checked against the SHA-256 on every run), writes the files of the
  table above and `CREDITS.txt`, and removes the files of wallpapers that left the list. Keep
  the `default` line: it is what a new device shows, and the fallback when a chosen wallpaper
  is gone. The device takes up to 24.
- **Per wallpaper:** 960×540 JPEG q90, a 32×18 preview at q85, and a 264×148 cell of the
  thumbnail sheet (q85), all made with macOS `sips`. The sheet is composed as a BMP in plain
  Python, so no image library is needed.
- **Licences:** CC-BY-SA-4.0, except Kokkini (LGPL-3.0). `CREDITS.txt` is published with them.
- **On the dongle:** the launcher darkens and blurs the picture (`ui/src/wall.rs`), so light
  wallpapers work under the white text too. The thumbnails show them as they are.
- **Why 960×540:** the launcher's sharp layer is half the output resolution (it is upscaled by
  the GPU/VOP at 1080p and 4K alike). The earlier 1920×1080 file was 2.75× larger and took
  4× the decode work, just to be averaged down.
