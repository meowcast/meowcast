# The OS image: kernel, initramfs and system services

`build.sh` builds what runs on the device: a pinned Linux **7.3.4** kernel with the whole
system embedded as an initramfs (static BusyBox **1.37.0**, Mesa/Lima, the launcher, the app
manager, the network daemon and the small C daemons in this directory). The result is one
`zImage` and one device tree in `output/`; a board's tools boot or flash them.

Everything that depends on the hardware comes from `../boards/<BOARD>/`
([../boards/README.md](../boards/README.md)); results and measurements of the first board are
in [its docs](../boards/rk3228a-dongle/docs/).

## Build

Use `./meow build` from the repository root ([../docs/BUILDING.md](../docs/BUILDING.md)). It
runs these scripts in the builder image (`Dockerfile`, `Dockerfile.gpu`), with the repository
at `/fw` and a Docker volume at `/build`:

| Step | Script | Writes |
| --- | --- | --- |
| Mesa 26.2.6, Lima only | `build-mesa.sh` | `/build/mesa-install` |
| Launcher | `../ui/build-ui.sh` | `deploy/ui` |
| App manager, services, apps | `../apps/build-apps.sh` | `deploy/appd`, `../cloud/store/www` |
| Wi-Fi userspace, TLS client | `../wifi/build-userspace.sh` | `deploy/wifi` |
| Setup page | `../cloud/pilot/package-pilot.sh` | `deploy/pilot` |
| Kernel + initramfs | `build.sh` | `output/zImage`, `output/<board>.dtb`, `output/linux.config` |

`build.sh` reads these from the environment (`./meow` passes them on from `config.env`):

| Variable | Meaning |
| --- | --- |
| `BOARD` | Directory under `../boards` (default `rk3228a-dongle`) |
| `PROFILE` | One of the board's profiles; its full product profile by default |
| `LAB` | 1 adds lab machinery (writable clock/register debugfs, sysrq, the board's lab tools). Default 1 for the full profile: **build with `LAB=0` for a device you hand to someone** |
| `GPU_TEST` | 1 adds Mesa, the GPU test and the runtime libraries the launcher and apps need (`./meow` sets it) |
| `UI`, `APPD`, `WIFI`, `MEDIA`, `THERMALD`, `BUTTOND` | 0 leaves a part out; each defaults to 1 when its inputs exist |
| `MEOW_STORE_URL`, `MEOW_RELAY_HOST`, `MEOW_PILOT_URL` | Your cloud side, baked into `/etc/appd/store.conf` and `/etc/meow/cloud.conf` ([../docs/CLOUD.md](../docs/CLOUD.md)) |
| `KLAT` | 1: low-latency scheduler options (an experiment without measurable effect) |

The Debian base image is pinned by digest and every source archive by SHA-256
(`fetch-sources.sh`). Builds use `--network=none`. The kernel, device tree and initramfs came
out bit-identical across rebuilds on the same builder image; apt packages inside the image are
not a hermetic snapshot (`output/builder-packages.txt` records them).

`smoke-test.py` boots the image in QEMU (`virt` machine): kernel, init and shell only, since
QEMU does not emulate a board's peripherals.

## Image size: XZ (14 Nov)

The zImage must end below the data partition (0xc00000 in the flash layout: 11 729 920 bytes
for the kernel slot), so size is the scarce resource and boot time is not.

- **Initramfs:** `build.sh` compresses `rootfs.cpio` itself and the kernel embeds the
  `.cpio.xz` as it is: `xz --armthumb --lzma2=preset=9e,dict=<archive size>,lc=1,lp=1,pb=1`,
  CRC32 check, one thread (reproducible). The userspace is Thumb-2 code, which the BCJ filter
  helps; the kernel's own XZ option uses a 1 MiB dictionary and no filter.
- **Kernel:** `CONFIG_KERNEL_XZ`. Only the XZ unpacker is built in (`RD_XZ`).

| The same 21.7 MB rootfs | Bytes |
| --- | --- |
| zstd -19 (before) | 5 938 902 |
| xz as the kernel would do it (1 MiB dictionary) | 5 590 728 |
| xz -9e | 5 230 500 |
| xz, ARM-Thumb filter, whole-archive dictionary (now) | 5 048 328 |

| zImage (perf, lab, Wi-Fi, media, UI, apps) | Bytes | Below the limit |
| --- | --- | --- |
| gzip kernel + zstd initramfs | 11 310 904 | 0.42 MB |
| XZ kernel + XZ initramfs | 8 753 248 | 2.98 MB |

Booted from RAM on the dongle on 14 Nov: `/init` starts 4.6 s into the kernel's clock, and the
shell answers 15.3 s after the hand-off. The difference from the earlier build was not measured.

## USB networking and deploy loop

The OTG port runs a configfs composite gadget (`rootfs/usb-gadget`, called from `init`):
CDC ACM first (the `/dev/cu.usbmodem*` console, `/dev/ttyGS0` on the device) and CDC ECM
Ethernet (`usb0`). The ECM MACs are fixed (device `02:52:4b:32:32:01`, host
`02:52:4b:32:32:02`), so macOS keeps one stable interface named "RK3228A RAM dongle". `usb-gadget
ncm` switches to CDC NCM instead (also built in).

One-time Mac setup: System Settings → General → Sharing → Internet Sharing, share from Wi-Fi to
"RK3228A RAM dongle", then turn it on. macOS then serves DHCP/DNS/NAT on `bridge100`
(`192.168.2.1`). The dongle's `net-up` runs `udhcpc` in the background and keeps retrying, so the
order doesn't matter. The lease script (`/usr/share/udhcpc/default.script`) writes
`/etc/resolv.conf`, the default route and `/run/net.env`, then starts the client-only `ntpd`.
The clock (no RTC: every boot starts in 1970) is set before `ntpd` gets to it: meow-netd
asks time servers itself the moment a default route exists
([../apps/netd/src/timesync.rs](../apps/netd/src/timesync.rs): several servers at once by
fixed address, the gateway and the usual names, repeated every 0.3 s, the first sane answer
wins), and `ntpd` disciplines the clock from there. BusyBox `ntpd` alone wants two answers
and waits 16 s for a lost one. Measured 15 Nov on a RAM boot over USB: set 0.02 s after the
route came up (6.65 s after power-on); the launcher shows the new time at once (`UI_CLOCK
stepped` in `/run/ui.log`) instead of at the next minute of the old clock.
`net-status` prints the state as `NET_*` lines.

Deploy without rebuilding the initramfs:

```sh
python3 serve.py                 # Mac: serves ./deploy on 192.168.2.1:8000 (bridge100 only)
fetch my-binary                  # dongle: -> /opt/my-binary, prints FETCH_OK size sha256
```

`/opt` is a 96 MiB tmpfs that only uses RAM for what is stored. Measured: 16 MiB in about 2.4 s,
SHA-256 included. Nothing listens on the dongle; `ntpd` runs without `-l`.

## Apps and /data at boot (13 Nov)

The perf kernel mounts the SPI-NOR data partition (0xc00000–0xfe0000, JFFS2) at `/data` and cgroup v2
at `/sys/fs/cgroup`. It then starts the app manager with `appd-session`; logs go to `/run/appd.log`.

- **What `build.sh` adds:** appd, `appctl` and the trusted store key, whenever
  [../apps/build-apps.sh](../apps/build-apps.sh) has written `deploy/appd` (`APPD=0` leaves them out).
- **If `/data` doesn't mount:** it is a 4 MiB tmpfs and `/run/data-volatile` says so.
- **SPI0 pins:** the flash sits on the "spi-1" pin route (see the perf DTS). Never use the default
  spi0 pinctrl: its cs1 pin is the vdd_logic PWM.

Architecture and permissions are in [../apps/README.md](../apps/README.md).

## TV UI at boot

**Kernel settings for the UI (13 Nov, perf profile):**
- **Workqueues:** `build.sh` gives Lima's GPU job hand-off and the nonblocking DRM commit tail
  high-priority workqueues. That keeps frames on time while other threads keep the CPUs busy.
- **`KLAT=1`** adds `PREEMPT`, `HIGH_RES_TIMERS`, `NO_HZ_IDLE` and `HZ=250` as an experiment. It
  had no measurable effect, so it is off by default.
- **Measurements:** [PERF-RESULTS.md](../boards/rk3228a-dongle/docs/PERF-RESULTS.md#13-november-ddr-clock-of-flash-boots-kernel-scheduling-for-the-ui).

With `GPU_TEST=1` the image includes the TV UI (`UI=1` is the default; `UI=0` leaves it out).
Build the UI first: [../ui/build-ui.sh](../ui/build-ui.sh) writes `deploy/ui`, and `build.sh` takes the
binary and fonts from there and checks them against `deploy/ui/SHA256SUMS`.

```sh
(cd ../ui && docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
  -w /fw/ui meowcast-builder:gpu sh build-ui.sh)
docker run --rm --network=none -e PROFILE=perf -e GPU_TEST=1 -v "$PWD/..:/fw" -w /fw/os \
  -v meowcast-build:/build meowcast-builder:gpu sh build.sh
```

### Compressed swap (zram)

Everything on the dongle lives in RAM (the rootfs and `/apps` are tmpfs), so without swap nothing can
be reclaimed. `build.sh` enables `CONFIG_ZRAM` with the lz4 backend and BusyBox `mkswap`/`swapon`;
`init` sets up `/dev/zram0` as swap: 64 MB logical, at most 32 MB of RAM (`mem_limit`),
`vm.page-cluster=0`. Nothing is compressed until memory is tight. Apps never swap (appd writes
`memory.swap.max=0`), so their limits still mean RAM; what gets compressed is cold rootfs pages and
idle daemons.

Pressure test (15 Nov, RAM boot): 125 MB of incompressible data pinned in a ramfs (5 MB would be left
without swap), then YouTube launched, ran and was closed: 22.5 MB swapped into 10.8 MB (2.1:1), no
OOM kill, launch and return animations 0 late frames. Swap stays unused in normal operation.

**At boot**, `init` runs `ui-session start` right after the USB console and the watchdog. The UI plays the
boot intro, then hands over to the home screen (see [../ui/README.md](../ui/README.md#boot-intro-and-hand-over)).

**`ui-session start|stop|restart|status`** ([rootfs/ui-session](rootfs/ui-session)):

- It supervises `/usr/bin/miracast-ui` and logs to `/run/ui.log`.
- After a crash it restarts the UI within 2 s, with only the entrance cascade (no intro).
- `/opt/ui/run.sh` stops the session before running a development build.

**Kernel command line (perf DTS):**

| Argument | Purpose |
| --- | --- |
| `video=HDMI-A-1:1920x1080@60` | Starts the console in the UI's mode, so the UI takes over without an HDMI re-sync. `ui-session` derives the UI mode from it: use `3840x2160@30` for `4k30` |
| `vt.global_cursor_default=0` | No blinking cursor |
| `ui.tvsync=MS` (optional) | This TV's picture delay after the HDMI signal starts; the intro waits for it. Default 3700 (test TV); 0 = start at once |
| `ui.synctest` (optional) | Show a large uptime counter instead of the UI. Read the first number visible on the TV and subtract the console's HDMI start (`dmesg \| grep "switching to colour"`, ≈0.62 s) to get `ui.tvsync` |

To boot with `ui.synctest` without rebuilding, use a DTB variant:

```sh
cp output/rk3228a-miracast-ram.dtb output/variants/synctest.dtb
docker run --rm -v "$PWD/..:/fw" -w /fw/os meowcast-builder:gpu sh -c \
  'f=/work/output/variants/synctest.dtb; fdtput -t s $f /chosen bootargs "$(fdtget $f /chosen bootargs) ui.synctest"'
python3 lab.py cycle --dtb output/variants/synctest.dtb
```

**Mesa:** [patches/mesa-rand-no-boot-stall.patch](patches/mesa-rand-no-boot-stall.patch) stops Mesa from
stalling about 8 s in `gbm_create_device` before the kernel RNG is ready.

## Thermal protection (15 Nov)

The dongle has no heat sink and its display pipeline alone keeps it near 100 °C. Three layers,
from mild to drastic:

| Layer | Where | At | What |
| --- | --- | --- | --- |
| Kernel passive trips (step_wise) | [board/rk3228a-miracast-perf.dts](../boards/rk3228a-dongle/dts/rk3228a-miracast-perf.dts) | 90 / 95 °C | CPU ≤ 600 / 408 MHz (cpufreq cooling, vdd_arm follows), GPU ≤ 300 / 200 MHz (Lima devfreq cooling) |
| Throttle levels | `meow-thermald` ([thermald.c](thermald.c)) + [rootfs/thermal-actions](rootfs/thermal-actions) | 100 / 104 °C | Level 2 "hot": apps' cgroup limited to two cores' worth of CPU (`cpu.max`), the launcher stops boosting clocks for animations. Level 3 "severe": one core's worth, a notice "The device is hot: slowing down". 3 °C hysteresis |
| Emergency cooldown | the same daemon | 108 °C twice in a row (1 s apart), or 111 °C once | Notice, everything stopped, video output off for a minute; see below |
| Kernel critical trip | DTS | 115 °C | Last resort when the daemon is dead: `hw_protection` reboot, as before (was 110 °C) |
| TSADC hardware reset | DTS | 120 °C | CRU reset without software (the stock firmware's value; was 115 °C) |

Why these numbers: the SoC's sensor table and rating end at 125 °C and the stock firmware reset at
120 °C. The old 110 °C critical trip left no room for anything between "throttled as far as the
kernel can" (the device idles at 99–103 °C with HDMI on) and a reboot. The cooldown starts at
108 °C, below the old trip, so the device never runs hotter than it used to; the kernel trips above
it only moved up to stay out of its way (the daemon itself gives up at 113 °C).

### The emergency cooldown

```
normal --(>= 108 C twice, or >= 111 C)--> notice --5 s--> cooldown round 1..5 (60 s each)
   ^                                                        |  end of a round:
   |                                                        |   <= 97 C            -> exit
   +---- exit: output on, UI + appd start, notice ----------+   > start + 2 C      -> fail
                                                            |   round 5 and > 97 C -> fail
                                                            +-- >= 113 C any time  -> fail
fail: log, `thermal-actions fail`, then fail_cmd (`sync; reboot -f`), which is what the kernel's
critical trip would have done.
```

1. **Notice.** `/run/thermal/cooldown` is created (from now on `ui-session start` and `appd-session
   start` refuse to start anything). `thermal-actions prepare` closes the app in front through
   appd (`appctl stop`), so the launcher has the screen back; the daemon then publishes the notice
   in `/run/thermal/state` and the launcher shows "Too hot: pausing for a minute to cool down" (its
   normal notice pill; it also wakes a dimmed or sleeping screen). Without a running launcher the
   text goes to the kernel console on HDMI.
2. **Enter** (`thermal-actions enter`): every app and background service is asked to stop
   (`appctl stop`), appd is stopped, what is left in the apps' cgroups is killed, the launcher is
   stopped (SIGKILL if it does not leave). CPU capped at its lowest OPP (408 MHz, 1.00 V) and the
   GPU at 200 MHz through `scaling_max_freq` / devfreq `max_freq`. Then `echo 4 >
   /sys/class/graphics/fb0/blank`: with no DRM client left the console owns the CRTC, and blanking
   it disables the CRTC. That stops the VOP (runtime-suspended, `aclk_vop`/`dclk_vop`/`hclk_vop`
   off) and powers the HDMI PHY and its PLL down (`hdmiphy` off); Lima and the video decoder are
   runtime-suspended because nothing uses them; HDMI audio (I2S0) stops with the apps.
3. **Rounds.** 60 s, sampling every 5 s. The TV shows "no signal".
4. **Exit** (`thermal-actions exit`): caps removed, console unblanked (HDMI back), launcher started
   with the entrance cascade (no boot intro), appd started. 7 s later (TV re-sync) the launcher
   shows "Cooled down: back to normal". The app that was running is not restarted.

**Left running on purpose:** the daemon, the watchdog daemon, the TSADC, meow-netd with Wi-Fi, the
cloud link and the debug bridge (the Wi-Fi chip is in the cable, not next to the SoC, and it is the
only way in when nobody is near the device), the USB gadget. **Never touched:** the DDR clock and
timings, any regulator voltage, `/data` mounts. CPU cores stay online by default (`offline_cores=1`
lets `thermal-actions` take CPUs 2-3 offline at level 3 and 1-3 in a cooldown; hotplug through the
BootROM mailbox is not validated on this board yet, so it is off).

**Robustness.** `meow-thermald` is a static 0.4 MB binary under its own respawn loop
([rootfs/thermald-session](rootfs/thermald-session)), started by `init` before the UI and appd and
exempt from the OOM killer. It sleeps between samples (3 s; 1 s from 95 °C up). Every action is a
`thermal-actions` run with a 45 s limit. If the daemon dies during a cooldown its successor finds
`/run/thermal/cooldown` and continues; `thermald-session stop` during a cooldown restores the
output first. A garbage sensor reading is ignored (and logged).

**Logs.** `/run/thermal/log` (RAM, 2 × 64 KiB): one `TEMP now/min/max level` line a minute, every
level change, every action with its duration and the power state before and after (`thermal-actions
status`: clocks, runtime-PM states, CRTC). Emergencies also go to `/data/thermal.log` (flash, 2 ×
16 KiB, three or four lines per event), so they survive the reboot. `/run/thermal/state` is the
live state (`state`, `level`, `temp`, `round`, `notice_seq`, `notice`).

### Measured (15 Nov, flashed firmware, 1080p60, USB cable to the Mac)

| | °C |
| --- | --- |
| Output off (launcher stopped, CRTC inactive), idle, after 5 min | 82, flat |
| Home screen idle, HDMI on, from 82: after 15 s / 45 s / 90 s / 150 s / 240 s | 87.5 / 90 / 93 / 95 / 97 (the owner saw 99-103 after longer) |
| Output switched off at 96: after 8 s / 35 s / 65 s / 130 s | 94 / 91 / 89 / 87.5 |
| Output switched off at 94 / on at 87.5: after 7 s / 5 s | 90 / 91 |

Two time constants: about 6 °C follows the display pipeline within ~10 s (the die), the rest moves
with ~4 min (the case). From a soaked 108 °C a one-minute round should therefore end near 97 °C
(6 °C at once, then about a quarter of the remaining 20 °C to the 82 °C floor): `resume` is 97 °C,
so expect one or two rounds. Single samples can read several degrees high (93 between 86 °C
readings): every trigger needs a second reading.

Sequence on the dongle with `emul_temp` (launcher only, and with YouTube in front): app closed
1.4 s, notice shown by the launcher, `enter` 2.9 s (CRTC inactive, `hdmiphy`/`dclk_vop`/`aclk_vop`
off, VOP and GPU runtime-suspended), 60 s, `exit` 1.4 s, launcher + appd + background services
back, "Cooled down" notice; with the emulated temperature held at 100 °C: three rounds, then the
(test) fail command, and the session scripts refused to start anything. meow-netd and the debug
bridge stayed up throughout.

**Idle-power findings.** (1) rkvdec never gated its clocks (`aclk_rkvdec` 192 MHz, `hclk_rkvdec`,
`sclk_vdec_core`/`cabac` 300 MHz enabled with the decoder suspended): fixed in `build.sh`, effect
not measured. (2) The display pipeline is the idle heat; since 15 Nov evening the launcher's sleep
switches the output off, see [the next section](#where-the-display-heat-comes-from-15-nov). (3) The
launcher renders nothing when idle but wakes every 700 ms to read state files (CPU 98.5 % idle;
fine). (4) Load average sits at 1.1 with idle CPUs: a kernel thread in uninterruptible sleep, not
load. (5) The unused clocks are gated at boot now (`clk-gate-unused`), and the VOP AXI clock was
measured again at 1080p60: no gain (next section).

### Where the display heat comes from (15 Nov)

Measured on the flashed firmware (`80feb9ac25a2`), 1080p60, idle home screen, over the debug bridge.
Method: one thing switched back and forth (A, B, A, B) in phases of 90–150 s, mean of the last 30 s
of each phase, CPU load logged per phase (0.3–0.6 % busy throughout); for the quick steps one
sample every 3 s. The sensor steps in 0.8–1 °C, so anything below 1 °C is "nothing measurable".
The room was cooler than in the morning: the home screen idled at 86–88 °C, not 97 °C.

| Switched | Effect on the idle home screen |
| --- | --- |
| **Sleep: video output off** (CRTC inactive: VOP stopped, its power domain idle, `aclk_vop`/`hclk_vop` gated, HDMI PHY and PLL off) | **−8 °C within 30 s, −11 °C after 150 s and still falling** (86.0–89.7 → 77.9 / 75.4) |
| HDMI TMDS line drivers off (PHY reg 0xe1) | nothing measurable (two runs) |
| + TMDS serialiser PLL ("post-PLL") and bandgap off | ≤ 0.5 °C |
| + pixel-clock PLL ("pre-PLL") off: `dclk` stops, the VOP's timing and the HDMI controller stand still | about −1 °C |
| 720p60 instead of 1080p60 (half the pixel clock, 2.25× fewer pixels fetched) | −1.5 °C |
| VOP AXI clock 200 instead of 400 MHz (`aclk_vop_pre`) | ≤ 0.4 °C (two runs; 12 Nov on a 720p console: −0.8) |
| `hclk_vop` gated (CRU gate, driver not told) | nothing |
| `aclk_vop` gated (the VOP can fetch nothing; picture gone) | −3 °C within 36 s |
| **vdd_logic 1.10 V instead of 1.15 V** | **−1.7 °C** (87.5 → 85.8, both cycles) |
| **Clocks of unused blocks gated** (`clk-gate-unused`) | **−2.4 °C** (86.8 → 84.4, both cycles) |

What this says:

- The cost of a running output is real, 10–12 °C at idle, and it arrives within seconds, so the
  source sits on the die next to the sensor. It is **not** the part one would suspect first: the
  whole HDMI transmitter (drivers, both PLLs) and everything clocked by the pixel clock together
  are worth about 1 °C, and lowering the pixel rate (720p) 1.5 °C.
- About 3 °C belong to the VOP's AXI side being active at all; it does not depend on that
  clock's rate (200 vs 400 MHz) or much on the amount of data fetched (720p). The remaining
  5 °C or so only go away with the whole VOP domain off, and could not be separated: requesting
  the VOP's bus idle (GRF `0x40c` bit 5) while it scans out **hangs the board hard** (USB dead,
  the watchdog daemon evidently still fed; power cycle needed). Do not repeat that test.
- So there is no cheap trick that keeps the picture and removes the heat. What helps while the
  picture is on is the general power level of the logic rail: the lower voltage and the gated
  clocks (−1.7 and −2.4 °C alone; switched together on one boot: 86.7 / 85.8 → 84.1 / 82.7 °C,
  about −3 °C). What helps most is not showing a picture when nobody looks: sleep.

Applied (15 Nov evening, flashed as `firmware-4745a1b477b9`, see [../boards/rk3228a-dongle/flash/README.md](../boards/rk3228a-dongle/flash/README.md)):

1. **Sleep switches the output off** ([../ui/README.md](../ui/README.md), "Dimming and sleep"), by
   the Power button or 10 minutes after the idle dim.
2. **vdd_logic 1.10 V on flash boots too** (`pack-flash.py` no longer raises the DTB's value to
   1.15 V; that dated from the belief that flash boots run DDR3 at 600 MHz).
3. **`clk-gate-unused`** ([rootfs/clk-gate-unused](../boards/rk3228a-dongle/rootfs/clk-gate-unused)) at boot: Ethernet,
   crypto, SD/eMMC/NAND, TSP, RGA, IEP, HDCP, VPU bus clocks, S/PDIF, I2S1/2, unused USB hosts,
   UARTs, I2C, SIM, eFuse. A named list through the clk debugfs, because booting without
   `clk_ignore_unused` also takes the DDR PHY's APB clock away.
4. **`ui-session stop` waits for the supervisor loop.** A quick stop/start (or `restart`) could
   leave two loops behind: one launcher on screen and a second one starting, failing ("present:
   Permission denied") and restarting every 3 s. That cost about a third of a core and 2 °C, for
   as long as the device stayed up.

Not applied: the VOP AXI clock (no gain at 1080p), anything in the HDMI PHY (no gain), a lower
resolution (1.5 °C for a visibly worse picture).

Seen on the way: with sweet.tv playing in front the SoC sat at 98–99 °C (idle home screen at that
time: 87 °C). Playback, not the launcher, is where the remaining heat problem is.

### Configuration and testing

`key=value` lines in `/etc/meow/thermal.conf` (not shipped: built-in defaults), then
`/data/thermal.conf`, then `THERMAL_<KEY>` in the daemon's environment; `kill -HUP` re-reads.
Temperatures in millidegrees: `warm` 90000, `hot` 100000, `severe` 104000, `level_hyst` 3000,
`emergency` 108000, `emergency_now` 111000, `resume` 97000, `panic` 113000, `rise_abort` 2000.
Times: `poll_ms` 3000, `poll_hot_ms` 1000 (within `fast_below` 13000 of `emergency`), `confirm` 2,
`notice_s` 5, `cooldown_s` 60, `max_rounds` 5, `settle_s` 7, `action_timeout_s` 45. Also
`fail_cmd`, `offline_cores`, `sensor`, `actions`, `run_dir`, `persist_log`. Thresholds that
contradict each other are refused as a whole (defaults apply; logged).

The kernel has no `CONFIG_THERMAL_EMULATION` (it would also drive the kernel's own critical trip).
The daemon has its own: it uses the higher of the sensor and `/run/thermal/emul_temp`, so a
forgotten test file can never hide real heat.

```sh
echo 109000 > /run/thermal/emul_temp     # emergency: notice, then the output goes off
rm /run/thermal/emul_temp                # "cooled down": back at the end of the round (if really <= 97 C)
echo 101000 > /run/thermal/emul_temp     # level 2; 105000: level 3
tail -f /run/thermal/log; thermald-session status; thermal-actions status
# the failure path without a reboot: THERMAL_FAIL_CMD='echo would reboot' meow-thermald -f
python3 test-thermald.py                 # host: the state machine against a fake sensor (13 cases)
```

## The button: pair a phone, factory reset (15 Nov)

The dongle has one button (GPIO3_D1, active low). Held at power-on it is the stock loader's way
into MaskROM, which is unchanged. Once Linux runs, `meow-buttond` ([buttond.c](buttond.c), started
last by init through `rootfs/buttond-session`) reads it. It does not depend on the launcher, appd
or the network daemon, so the reset works when any of them hangs and when the TV shows nothing.

| What the finger does | What happens |
|---|---|
| Press, let go within 1 s | The TV shows the pairing code for a phone (or the Wi-Fi setup codes when it has no network). Again: hides it. An app in front closes first, a sleeping screen wakes. |
| Hold | Nothing for 3 s. Then a warning with a countdown: "keep holding to erase everything". Letting go cancels. |
| Still holding at 10 s | Armed: "let go of the button now to erase everything". |
| Let go between 10 s and 15 s | Factory reset. |
| Still holding at 15 s | Cancelled. The button is ignored until it has been let go. |

Why this shape:

- **The reset happens on letting go inside a window, never while the button is down.** A button
  that is stuck or squeezed (the dongle wedged behind a TV) can never erase anything, at boot or
  later. A button that is already down when the daemon starts is ignored until it is let go.
- **A knock or an impatient long press cancels.** Presses between 1 s and 3 s do nothing at all.
- **No second key is needed.** The reset is for the moment the paired phone is gone, so it cannot
  ask for a remote. Without a picture: hold, count to twelve, let go.
- **Pairing by button is harmless.** A phone still needs the code that only the TV shows; the
  phones that are already paired stay paired.
- **The daemon owns the timing.** The launcher only draws: each step is one byte on its key FIFO
  (`1`–`6`, not key names, so a phone remote cannot send them). If the launcher is gone, the
  reset happens just the same. If the daemon's next step never comes, the launcher's screen
  closes by itself.
- **Doubt cancels.** If the daemon was not scheduled for a while, a let-go it sees too late is
  not a reset (`meow-buttond selftest`, run by `build.sh`, sweeps sampling intervals for this).

### What a factory reset does

`rootfs/factory-reset`, run by the daemon or by Settings > About > Factory reset (a dialog that
starts on Cancel):

1. Stops appd (and every app), meow-netd and the launcher.
2. Unmounts `/data` and erases the whole partition block by block (`meow-buttond erase /dev/mtd0`,
   62 blocks), so the Wi-Fi password, pairing tokens and developer keys do not stay readable in
   the flash as deleted JFFS2 nodes. If `/data` cannot be unmounted, its files are deleted.
3. Clears the RAM leftovers (`/apps`, app storage, daemon state), mounts the empty `/data` and
   starts the three again. The TV comes back as on the first boot, with the phone setup.

It takes about 10 s and does not reboot: on this board a software reset is a lab matter
([LAB-RECOVERY.md](../boards/rk3228a-dongle/docs/LAB-RECOVERY.md)). The firmware cannot be damaged by it: the data partition is
the only MTD device Linux has, so nothing below `0xc00000` is reachable.

Testing: `meow-buttond probe` prints the line's level; `BUTTOND_ARGS=-n buttond-session restart`
logs a reset instead of running it; `printf 2 > /run/ui-keys` (then `3`, `4`…) shows the
launcher's screens without the button; `FACTORY_RESET_KEEP=1 factory-reset test` runs everything
but the erase. Logs: `/run/button.log`, `/run/factory-reset.log`. Timing can be changed with
`BUTTON_WARN_MS` and friends (see the head of `buttond.c`); the launcher's countdowns assume the
defaults.

Checked on the dongle (15 Nov, new binaries in RAM over the flashed firmware, then flashed as
`firmware-780915f48fde`): the line reads "up" with its pull-up; the launcher's screens for every
step (bytes written by hand); a real reset from Settings: 62 blocks erased, `/data` mounted
empty, no paired phone, services back, then `/data` restored from a copy.

Not checked yet: the button pressed by a finger (down level, bounce, the whole hold), and
pairing by button with the Wi-Fi cable.
