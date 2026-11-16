# Boards

A board is a directory here. It holds everything that is true for one device and not for
another: device trees, kernel configuration, the code that gets Linux started, the flash
layout and the tools that put the firmware on the device. `BOARD=<name>` (in `config.env`)
selects it; `./meow boards` lists them.

| Board | Description |
| --- | --- |
| [`rk3228a-dongle`](rk3228a-dongle/README.md) | RK3228A HDMI dongle, 256 MiB DDR3, 16 MiB SPI NOR, Mali-400, RTL8733BU Wi-Fi on USB |

## What a board directory contains

| Path | Required | What |
| --- | --- | --- |
| `board.sh` | yes | Shell variables and one function that `os/build.sh` sources (below) |
| `dts/` | yes | Device tree sources. All of them are copied into the kernel tree; `board_dts` picks one per profile |
| `kernel/kernel.config` | yes | The kernel configuration (a complete, minimal one: `olddefconfig` fills in the rest) |
| `kernel/kernel-media.config` | for `MEDIA=1` | Extra options for the video decoder and HDMI audio |
| `kernel/patch.sh` | no | Hook for kernel source changes and extra config lines this SoC needs |
| `rootfs/` | no | Diagnostic scripts installed into `/bin` (`BOARD_TOOLS`, `BOARD_LAB_TOOLS`), and `usb-role` if the board switches its USB port between gadget and host |
| `board` | no | Command-line tool behind `./meow board ...`: boot, dump, flash |
| `fetch-tools.sh`, `build-host-tools.sh`, `make-manifest.py` | no | Run by `./meow fetch` and `./meow build` when present |
| `README.md` | yes | How to recognise the device, boot it, flash it and get back to stock |

### `board.sh`

| Name | Example | Meaning |
| --- | --- | --- |
| `BOARD_DESCRIPTION` | `'RK3228A Miracast dongle ...'` | One line for `./meow boards` |
| `BOARD_ARCH`, `BOARD_CROSS_COMPILE` | `arm`, `arm-linux-gnueabihf-` | Kernel `ARCH` and toolchain prefix |
| `BOARD_CFLAGS` | `'-mcpu=cortex-a7 -mthumb'` | For the static C daemons |
| `BOARD_DTS_DIR` | `rockchip` | Vendor directory under `arch/$ARCH/boot/dts` |
| `BOARD_DTB_OUT` | `rk3228a-miracast-ram.dtb` | File name of the built device tree in `os/output` |
| `BOARD_PROFILES`, `BOARD_DEFAULT_PROFILE`, `BOARD_FULL_PROFILE` | `'perf graphics usb'`, `perf`, `perf` | Profiles are steps of a bring-up (console only, display, everything). Only the full profile gets SMP, the sandbox options, `/data`, Wi-Fi and media |
| `board_dts PROFILE LAB` | prints `rk3228a-miracast-perf` | DTS base name for a profile; non-zero for an unknown one |
| `BOARD_MEDIA_DTSI` | `rk3228a-miracast-media.dtsi` | Included after the profile's DTS when `MEDIA=1` |
| `BOARD_TOOLS`, `BOARD_LAB_TOOLS` | script names | From `rootfs/`, the second list only with `LAB=1` |

### `kernel/patch.sh`

Called with `LINUX` (source tree), `KCONFIG` (the `.config` being assembled), `BOARD_DIR`,
`PROFILE`, `FULL` (1 for the full profile) and `LAB`. The kernel tree persists between builds,
so every change must be idempotent and must be undone when its condition is off. Prefer a
device tree property over a patch, and say in a comment what was measured without it.

## What the rest of the tree expects from a board

The launcher and apps use standard Linux interfaces, so most of a port is the kernel:

| Feature | Needs |
| --- | --- |
| Launcher | A DRM/KMS driver and a Mesa GLES2 driver. Mesa is built with Lima only (`os/build-mesa.sh`, `os/mesa-armhf.ini`): a Mali Midgard/Bifrost board needs Panfrost added there |
| Two-plane compositing (wallpaper under the UI) | An overlay plane; without one the launcher falls back to a single plane (`UI_PLANES`, [ui/README.md](../ui/README.md)) |
| Video apps | A V4L2 **stateless** H.264 decoder (the `media` crate drives the request API as rkvdec implements it; other drivers may need work) and an ALSA device for HDMI audio |
| `/data` | A device tree partition named `data` (JFFS2 on NOR here). Without it `/data` is a RAM disk and nothing survives a reboot |
| Wi-Fi | A cfg80211 driver. `wifi/kernel-wifi.sh` adds the RTL8733BU vendor driver; another chip needs its own script or an in-tree driver in `kernel.config` |
| USB console and network | A USB device controller (the image is a CDC ACM + ECM gadget) |
| Thermal protection | A thermal zone, cpufreq, and devfreq for the GPU ([os/README.md](../os/README.md#thermal-protection-15-nov)) |
| Button | A GPIO line the button daemon can read (`os/buttond.c`) |
| Memory | The defaults assume 256 MiB (`APPS_MEMORY_MAX` in `apps/appd/src/daemon.rs`, zram in `os/rootfs/init`) |

Booting and flashing are entirely the board's: U-Boot, a vendor loader, or, as on the first
board, a few hundred lines of assembly entered from the stock loader.

## What is still tied to the first board

Honest list, so a port knows where to look:

- **32-bit ARM userspace.** `armv7-unknown-linux-gnueabihf` and `arm-linux-gnueabihf` are
  written into `ui/` and `apps/` (`.cargo/config.toml`, `build-*.sh`), `wifi/build-userspace.sh`,
  `os/package-gpu.*` and `os/Dockerfile.gpu`.
- **Kernel version.** `os/build.sh` pins Linux 7.3.4 and applies two generic patches by text
  match (Lima and atomic-commit work queues).
- **zImage.** The build produces `arch/arm/boot/zImage`; an arm64 board needs `Image`.
- **Device paths.** The launcher raises the GPU and CPU frequency floors for animations
  through sysfs paths of this SoC (`/sys/class/devfreq/20000000.gpu/...` in `ui/src/launch.rs`
  and `ui/src/main.rs`); elsewhere the writes fail quietly and need new paths. The button
  daemon defaults to `/dev/gpiochip3` line 25 (`BUTTON_CHIP`, `BUTTON_LINE` in `os/buttond.c`).
- **Names.** Binaries and device trees still say `miracast` in places (`miracast-ui`,
  `rk3228a-miracast-*.dts`).

## Adding a board

1. Copy the shape of `rk3228a-dongle/`: `board.sh`, `dts/`, `kernel/kernel.config`.
2. Start with a console-only profile (`usb` there) and get a shell before anything else.
3. Add display, then the GPU, then the full profile. Keep each step as a profile until the
   next one works.
4. Write the board's `README.md`: how to tell this device from similar ones (photos of the
   PCB markings help), how to boot without writing flash, how to back up the stock firmware,
   how to flash, how to get back.
5. Never commit vendor firmware, dumps or loader binaries: fetch them, or have the user read
   them from their own device.
