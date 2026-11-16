# Third-party material

meowcast itself is GPL-3.0-or-later ([LICENSE](LICENSE)); `apps/sdk`, `apps/media` and
`apps/examples` are MIT OR Apache-2.0. This file lists what comes from
elsewhere: what is stored in the repository, and what the build downloads.

## In the repository

| Path | What | Licence |
| --- | --- | --- |
| `ui/assets/fonts/` | Noto Sans (full faces and Latin subsets), from Debian's `fonts-noto-core` | SIL OFL 1.1 (`COPYRIGHT.debian`) |
| `cloud/store/www/assets/wallpapers/` | Wallpapers from [KDE Plasma](https://github.com/KDE/plasma-workspace-wallpapers), downscaled | CC-BY-SA-4.0, one LGPL-3.0; authors in `CREDITS.txt` |
| `cloud/tls/roots.pem` | Five public root certificates (Google Trust Services, GlobalSign, ISRG) | Public certificates |
| `boards/rk3228a-dongle/dts/` | Device trees based on the kernel's `rk322x.dtsi` | GPL-2.0+ OR MIT |
| `boards/rk3228a-dongle/kernel/*.c`, `boot/ddr_reclock.c` | Kernel-side code | GPL-2.0 |
| `boards/rk3228a-dongle/rockusb/rkdeveloptool-local.patch` | Patch to Rockchip's rkdeveloptool | GPL-2.0 (as the tool) |
| `cloud/pilot/src/components/ui/` | Components following the shadcn/ui pattern | MIT |

## Downloaded by the build (`./meow fetch`)

| What | Version | Licence |
| --- | --- | --- |
| Linux | 7.3.4 | GPL-2.0 |
| BusyBox | 1.37.0 | GPL-2.0 |
| Mesa | 26.2.6 | MIT and others |
| wpa_supplicant | 2.12 | BSD-3-Clause |
| libnl | 3.12.0 | LGPL-2.1 |
| BearSSL | git 7bea48e | MIT |
| Dropbear | 2026.94 | MIT |
| RTL8733BU driver | [Charliechen114514/rtl8733bu-linux-driver](https://github.com/Charliechen114514/rtl8733bu-linux-driver) 2b72111 | GPL-2.0 |
| Rust crates | pinned in `ui/Cargo.lock`, `apps/Cargo.lock` | MIT/Apache-2.0 mostly; each crate's `Cargo.toml` here comments its dependencies' licences |
| npm packages | pinned in `cloud/*/package-lock.json` | MIT, Apache-2.0, ISC ([cloud/pilot/README.md](cloud/pilot/README.md#stack-and-licences)) |
| Rockchip rkbin (`rk3228a-dongle` only) | commit 3e288fe | Rockchip's binary licence (`LICENSE` in rkbin). Fetched to your machine, loaded into the device's RAM, never redistributed here |
| Rockchip rkdeveloptool (`rk3228a-dongle` only) | commit 304f073 | GPL-2.0 |

A firmware image you build contains the GPL and LGPL programs above. If you give that image,
or a device flashed with it, to someone else, you take on those licences' obligations (offer
the corresponding source: this repository at your commit plus the pinned archives).

## Apps for other companies' devices and services

The casting receivers and service clients are not in this repository. They live in
[meowcast/apps-extra](https://github.com/meowcast/apps-extra), which has its own notices.
