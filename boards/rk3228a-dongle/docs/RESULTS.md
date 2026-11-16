# First Linux RAM boot — 11 November 2026

This records the initial minimal baseline. The subsequent
[GPU and 4K tests](GPU-4K-RESULTS.md) supersede the GPU-rendering and output-mode
limits below: hardware GLES2 and visible 4K30 are now demonstrated; 4K60 failed
visually. The current active image includes Mesa, while this original minimal
image is preserved under `output/working-720p/`.

**Working baseline:** Linux 7.3.4 and BusyBox 1.37.0 boot from RAM on the actual
RK3228A dongle. The owner confirmed visible Linux text on the attached 4K display
after the corrected bootloader handoff. Linux sends **1280×720 at 60 Hz**.
USB CDC ACM remains available as `/dev/cu.usbmodem101` on this Mac and provides an
interactive diagnostic shell. No flash program/erase command was issued.

The owner explicitly deferred Wi-Fi and prioritized HDMI. The active `graphics`
profile leaves internal USB host controllers and SDIO disabled; no external
adapter is required or used. The original SPI-NOR firmware remains installed.

## Hardware results

| Component | Result | Evidence / limit |
|---|---|---|
| RAM loader and Linux handoff | Working | Uploaded zImage and DTB read back byte for byte; actual Linux shell executes commands |
| Linux kernel | Working | Runtime `uname`: Linux 7.3.4, ARMv7, build #1 dated 11 November 2026 |
| Minimal userspace | Working | Static BusyBox 1.37.0, embedded initramfs, proc/sysfs/devtmpfs, RAM-only runtime |
| RAM | Working at declared 256 MiB | `/proc/iomem`: `60000000-6fffffff`; `MemTotal: 249692 kB` (243.84 MiB); no exhaustive DRAM stress test |
| CPU | One Cortex-A7 active | MIDR `0x410fc075`; four-core/SMP startup not tested |
| Timer | Working | ARM timer at 24 MHz; Linux scheduler and shell operate |
| USB device connection | Working | `0525:a4a7` CDC ACM, Linux 7.3.4 gadget, interactive shell |
| HDMI hotplug and EDID | Working | Connected sink; 384-byte EDID; 4K, 1080p and 720p modes advertised |
| HDMI image | Working at 720p60 | Owner confirmed visible Linux text after final handoff; long-term stability, other modes, audio and CEC untested |
| Display IOMMU | Basic framebuffer path working | Final baseline keeps the upstream VOP IOMMU enabled; prior BUS_ERROR disappears after handoff fixes |
| GPU identification / Lima probe | Working | Mali-400 GP, PP0 and PP1, version 1.1; 64 KiB L2; render node created |
| GPU rendering acceleration | Not yet tested | Mesa/EGL userspace and a rendering workload are not included |
| Wi-Fi | Deferred | Stock DTB says `rtl8188eu`; physical chip/bus and Linux driver not verified |
| Video decode, HDMI audio, CEC, UART, temperatures | Not yet tested | Outside this first boot test |
| Persistent firmware / restoration | Not tested | No flash driver or write path used |

## Black-screen investigation and working fix

Early boots reached Linux but produced a black picture. The first all-peripheral
image also failed to provide a responsive USB console; removing several features
at once recovered USB, so that initial failure's precise cause was not isolated.
The USB kernel console remains disabled in the baseline. PID 1 launches a shell
on the explicit gadget TTY; `cttyhack` was removed because it chose the active
display console instead.

For HDMI, forcing a fabricated mode before EDID was available produced invalid
mode selection. The baseline uses `video=HDMI-A-1:1280x720@60` without forced
connector-on, with the display attached before the kernel starts. It selects the
CEA timing: 74.25 MHz, horizontal 1280/1390/1430/1650 and vertical
720/725/730/750.

Even with this valid timing, HDMI had sync but black pixels. Writing 3,686,400
bytes of noise to `/dev/fb0` succeeded without visible pixels. The display IOMMU
reported `BUS_ERROR occurred at 0x00000000`. Bypassing the IOMMU removed that
error but did not fix the black picture.

The recovery probe then established two omitted bootloader initializations:

| Register | Recovery value | Required handoff action |
|---|---|---|
| SGRF_SOC_CON0, `0x10150000` | `0x4000` | Clear bit 14, the DDR secure-region restriction |
| CRU_MISC_CON, `0x110e0134` | `0xa000` | Clear bit 13, selecting HDMI PHY clock output |

The first action is present in [upstream U-Boot RK322x initialization](https://github.com/u-boot/u-boot/blob/master/arch/arm/mach-rockchip/rk322x/rk322x.c).
Both actions are present in the cached Rockchip next-dev U-Boot source at commit
`1c535d65b8509f388d09e49fb6961f49fda35a1d`, already acquired during the earlier
audit. The [Rockchip write-mask helpers](https://github.com/u-boot/u-boot/blob/master/arch/arm/include/asm/arch-rockchip/hardware.h)
confirm writes `0x40000000` and `0x20000000`, respectively.

The final handoff performs and reads back both writes before taking USB away
from the helper. With both fixes and the original display IOMMU restored, the
owner confirmed Linux text. This combined test establishes the working sequence;
it does not independently measure the effect of each change in isolation.

## Initial baseline artifacts

The preserved first-build hashes are in
[output/working-720p/manifest.json](../../../os/output/working-720p/manifest.json).
The working profile is `graphics`. The active image has since grown to include
the GPU runtime described in the follow-up report.

- zImage, with embedded initramfs: **3,975,032 bytes**; SHA-256
  `4ee15b2ca719556b8d577a4134c17a7badc5e10da2961c8fd6c58ebd60f35c93`.
- Device tree: **23,531 bytes**; SHA-256
  `304370f312c984bcc02e5d85a887f0253151b29d306bac33d2ef2344db09f26b`.
- ARM handoff: **428 bytes**, built from [board/linux_entry.S](../boot/linux_entry.S).
- Static BusyBox: **809,808 bytes**, already included inside the kernel initramfs.
- Exported uncompressed initramfs: **820,736 bytes**, also already included.

Do not add the exported BusyBox/initramfs sizes to the zImage size when budgeting
this RAM payload. It is not yet a complete flash image: bootloader, graphics
userspace, application, updates and persistent data have not been budgeted.

## Evidence and validation

- Preboot register probe: [hardware-20261111T120304.282329Z/cpu-probe.json](../../../os/logs/hardware-20261111T120304.282329Z/cpu-probe.json).
- Successful final upload and handoff: [hardware-20261111T120406.888424Z/usb.jsonl](../../../os/logs/hardware-20261111T120406.888424Z/usb.jsonl).
- Final kernel, memory, GPU and USB-shell output: [usb-console-20261111T120402.000722Z.log](../../../os/logs/usb-console-20261111T120402.000722Z.log).
- Earlier display state, errors and full dmesg: [usb-console-20261111T114747.245064Z.log](../../../os/logs/usb-console-20261111T114747.245064Z.log).
- Direct-DMA isolation test: [usb-console-20261111T115819.229259Z.log](../../../os/logs/usb-console-20261111T115819.229259Z.log).
- QEMU kernel/init/shell smoke test passed. Five host artifact-guard tests passed.
  Shell scripts also passed syntax checks. Hardware boot success is established
  by the actual USB shell and owner's display observation, not the emulator.

The kernel currently warns about the stock four-core device-tree topology in a
UP build and cannot associate cpu0 for cacheinfo. This did not prevent the tested
boot, but must be resolved before SMP or CPU power/performance work. HDMI/USB
supply warnings use dummy regulators; real board rails remain to be characterized.
The optional HDMI-PHY IRQ warning also remains. No thermal monitoring or sustained
load validation was performed.

The next useful tests are a small DRM color/text application, sustained HDMI
operation and hotplug recovery, then Mesa/Lima rendering. Wi-Fi remains deferred.
