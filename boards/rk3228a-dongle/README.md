# rk3228a-dongle

A generic HDMI "Miracast" stick built around the Rockchip RK3228A. This is the board
meowcast was developed on.

| | |
| --- | --- |
| SoC | Rockchip RK3228A, 4 × Cortex-A7, Mali-400 MP2 |
| RAM | 256 MiB DDR3 (2 × Samsung K4B1G1646G) |
| Storage | 16 MiB SPI NOR, Winbond W25Q128FV (JEDEC `ef 40 18`) |
| Wi-Fi | Realtek RTL8733BU (2.4 GHz 802.11n), on the SoC's USB OTG port, inside the antenna cable assembly |
| Ports | HDMI plug, one micro-USB (power and data), one button |
| Stock firmware | Android 5.1-derived, Linux 3.10, firmware version 8.03.13 on the development unit |
| USB in boot ROM mode | `2207:320b` ("MaskROM") |

**Is yours the same?** Many sticks look alike and contain different chips (RK3036, Actions,
AM8252...). Open the case and read the markings: the SoC must say RK3228A, the flash
W25Q128. The tools check the USB ID and the flash's JEDEC ID and stop if either differs, but
they cannot tell a board with a different RAM or regulator layout. A unit whose stock firmware
differs from the development unit's gets a warning: do the RAM steps first, they write nothing.

What works: display up to 4K30 (4K60 gives no picture), GPU at 60 fps in 1080p, hardware
H.264 decoding to 1080p60, HDMI audio, Wi-Fi, all four cores, frequency and voltage scaling,
thermal protection, boot from flash. The device has no heat sink and runs hot (about 100 °C
at idle with the display on): see the thermal section of [os/README.md](../../os/README.md).
Measurements are in [docs/](docs/).

## Safety model

- The stock boot chain stays. Flash below `0x050000` (partition table, vendor storage, both
  copies of the stock loader) is **never written**: the window `[0x050000, 0xfe0000)` is
  compiled into the writer and checked again by the host tool.
- That stock loader contains the "button held at power-on → boot ROM USB mode" path, so the
  button keeps working as the way back whatever is in the rest of the flash.
- You read your own stock firmware before writing anything, and the flash tool refuses
  images that differ from your dump outside the window.
- RAM booting needs no flash write at all. Unplugging the device undoes it.

It is still possible to end up with a device that needs the button to recover, and nobody has
tried these tools on a second unit yet. Do not flash a device you cannot afford to lose.

## Host tools

macOS or Linux with: Python 3, git, a C++ compiler, libusb-1.0 and pkg-config, and LLVM with
lld for the bare-metal parts.

```sh
brew install libusb pkg-config llvm lld          # macOS
sudo apt install libusb-1.0-0-dev pkg-config clang lld llvm   # Debian/Ubuntu
./meow board tools
```

`./meow board tools` clones Rockchip's `rkbin` and `rkdeveloptool` at pinned commits into
`rockusb/tools/`, applies a small patch to the tool, and derives the USB helper that all
later steps load into RAM (`rockusb/make-helper-loader.py`: rkbin's loader with four bytes of
its USB code changed, rebuilt bit-exactly and checked against a pinned hash). No vendor binary
is stored in this repository. If LLVM is somewhere unusual, set `MEOW_LLVM` in `config.env`.

## 1. Enter the boot ROM's USB mode

Unplug HDMI and USB. Hold the button, plug the USB **data** cable into the computer, let go
after two seconds. `./meow board state` should say `maskrom` (`absent` means the computer does not see it). On macOS, allow the accessory
when asked.

## 2. Back up the stock firmware

```sh
./meow board dump
```

It loads the helper into RAM, reads the whole flash twice through a small read-only program,
compares the two reads, and saves `stock/nor-stock.bin` with its SHA-256 next to it. Do this
while the device still has its stock firmware: the command saves whatever is in the flash. Copy both
somewhere safe: this file is the only way back to the stock firmware, it is specific to your
unit, and it must not be committed or shared (it is the vendor's software).

## 3. Try the firmware from RAM

```sh
./meow build          # if you have not yet
./meow board boot
```

The firmware is uploaded into RAM and started; the flash is not touched. Plug HDMI in once
it runs. The device then shows up as a USB serial console and a USB network card:
`./meow board sh 'uname -a'` runs a command on it. With a LAB build (the default),
`./meow board cycle` reboots into the boot ROM and loads the image again without touching the
button ([docs/LAB-RECOVERY.md](docs/LAB-RECOVERY.md)).

How it gets there: the boot ROM runs Rockchip's DDR init and USB helper from RAM; our tools
verify the helper byte for byte, upload the kernel and device tree with read-back, and install
a small hand-off (`boot/linux_entry.S`) that sets two SoC registers the stock loader would
have set, turns the caches off and enters Linux. Load addresses: hand-off `0x62002000`,
kernel `0x64000000`, device tree `0x68000000`.

## 4. Flash

Only after step 3 works on your unit, including picture on the TV.

```sh
LAB=0 ./meow build                     # a build without lab machinery, if this is for real use
./meow board pack                      # prints the image name and its SHA-256
./meow board flash flash/images/firmware-XXXXXXXXXXXX.bin SHA256            # dry run
./meow board flash flash/images/firmware-XXXXXXXXXXXX.bin SHA256 --write    # about 80 s
```

The image is your stock dump with two regions replaced: a small entry stub and device tree
where the stock loader expects its next stage (`0x050000`), and the kernel (`0x0d0000`).
`/data` (`0xc00000` to `0xfe0000`) is kept by default, so settings and installed apps survive
an update. Details and timings: [flash/README.md](flash/README.md).

Unplug and plug the device in normally: it boots meowcast.

## 5. Back to stock

Enter the boot ROM's USB mode with the button (step 1), then:

```sh
./meow board rollback            # dry run
./meow board rollback --write    # writes your stock dump back, including the data area
```

## Files

| Path | What |
| --- | --- |
| `board.sh`, `dts/`, `kernel/` | What the OS build takes from this board ([../README.md](../README.md)) |
| `boot/` | Hand-off into Linux, DDR reclocking for flash boots, a CPU state probe |
| `rockusb/` | Boot ROM USB transport, the helper derivation, read-only flash reader stages |
| `flash/` | The flash writer stage, image packer, rehearsal tools |
| `ramboot.py`, `lab.py`, `load-helper.py` | RAM boot with verification, the lab loop, loading the helper |
| `make-ddr-loader.py` | Helper variants with another DDR clock (the default runs DDR3 at 528 MHz) |
| `rootfs/` | Diagnostic scripts for this SoC that go into the image |
| `docs/` | Measurements: bring-up, GPU and 4K, performance, power and heat, lab recovery |
