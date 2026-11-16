# Building

Everything is built inside a Docker image, so the host needs little:

| Host tool | Used for |
| --- | --- |
| Docker | The builder image: cross-compilers, Rust, Meson (`os/Dockerfile`, `os/Dockerfile.gpu`) |
| git, curl, Python 3 | Fetching sources, small host scripts |
| Node.js 20+ | The cloud side and the phone remote (`cloud/`) |
| A board's own needs | For `rk3228a-dongle`: LLVM with lld and libusb, to build the USB boot and flash tools ([its page](../boards/rk3228a-dongle/README.md#host-tools)) |

Disk: about 12 GB (sources 1 GB, the build volume 8 GB, crates and images the rest).
A first build takes 30 to 60 minutes; later ones a few minutes.

## The four commands

```sh
./meow setup     # config.env from config.env.example; builds the builder images
./meow fetch     # downloads everything the offline build needs (see below)
./meow build     # builds all parts and the firmware image
./meow test      # host-side tests
```

`./meow fetch` is the only step that uses the network. It gets:

- the kernel, BusyBox, Mesa, wpa_supplicant, libnl, BearSSL and Dropbear archives, each
  checked against a pinned SHA-256 (`os/fetch-sources.sh`);
- the Rust crates of `ui/` and `apps/`, pinned by their `Cargo.lock` (`fetch-deps.sh`, into
  `vendor/`), and the Noto Sans fonts from Debian's package;
- the Wi-Fi driver at a pinned commit (`wifi/fetch-driver.sh`);
- the board's tools (`boards/<board>/fetch-tools.sh`).

`./meow build` then runs with `--network=none`. Its steps, each of which can be run alone
(`./meow build ui`, `./meow build os`, ...), are listed in [os/README.md](../os/README.md#build).
The results:

| File | What |
| --- | --- |
| `os/output/zImage` | Kernel with the whole system inside |
| `os/output/<board>.dtb` | Device tree (the name is `BOARD_DTB_OUT` in the board's `board.sh`) |
| `os/output/linux.config`, `SHA256SUMS`, `features.json` | What was built |
| `cloud/store/www/` | Your store: the apps, packed and signed, and the asset index |

What happens to the image next is the board's business: the `rk3228a-dongle` tools load it into
RAM over USB or pack it into a flash image (`./meow board help`).

## Choosing what to build

Set these in `config.env` or in the environment for one build (`LAB=0 ./meow build`):

- `BOARD`: a directory under `boards/`.
- `PROFILE`: a board profile. The default is the board's full product profile.
- `LAB=0`: leave out the lab machinery. **Do this for any device that leaves your desk**: LAB
  images make clock and register debugfs writable and reboot into the boot ROM's USB mode.
- `MEOW_STORE_URL`, `MEOW_RELAY_HOST`, `MEOW_PILOT_URL`: your cloud side
  ([CLOUD.md](CLOUD.md)). They are baked into the image, so rebuild after changing them.

The full list is in [CONFIGURATION.md](CONFIGURATION.md) and [os/README.md](../os/README.md#build).

## Working on one part

The device has a deploy loop that avoids rebuilding the image: binaries are fetched into a
RAM disk (`/opt`) over the USB network and run from there. See
[os/README.md](../os/README.md#usb-networking-and-deploy-loop), and the "Build and deploy"
sections of [ui/README.md](../ui/README.md) and [apps/README.md](../apps/README.md).

`./meow shell` opens a shell in the builder with the repository at `/fw` and the build volume
at `/build`.

## Reproducibility

Sources and the base image are pinned by hash, timestamps are fixed (`SOURCE_DATE_EPOCH`), and
rebuilding the same tree on the same builder image has given a bit-identical kernel, device
tree and initramfs. The Debian packages inside the image are whatever apt served when the
image was built, so two builder images made at different times can differ.
