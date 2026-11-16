# SMP and hands-off lab recovery (12 November 2026)

Two results from 12 November, both on the actual RK3228A dongle:

1. **All four Cortex-A7 cores run.** Until now only CPU0 ran (UP kernel).
2. **The dongle recovers by itself.** A reboot, kernel panic, soft lockup, thermal-critical event or hard hang now ends in MaskROM, and the host reloads the RAM image. Nobody has to press the button or re-plug USB.

Everything that exists only for (2) is behind the `LAB` build flag. The inventory below lists every code path and says whether production needs it.

## Building

| Build | What you get |
|---|---|
| `PROFILE=perf` (implies `LAB=1`) + `sh build-host-tools.sh` (`LAB=1` default) | Lab image: SMP + watchdog + all lab recovery/debug machinery |
| `PROFILE=perf LAB=0` + `LAB=0 sh build-host-tools.sh` | Production-like image: SMP + watchdog only, no lab code. Archived build: `output/nonlab-2026-11-12/` (built, not yet booted) |

`output/features.json` records `"lab"`, and `build-host-tools.sh` writes `output/linux_entry.lab`. `ramboot.py verify` refuses a kernel/handoff mismatch, because a LAB kernel with a non-LAB handoff would silently lose auto-recovery. `LAB=1` with any other profile is an error.

## Results

### SMP

- 4 CPUs online (`smp: Brought up 1 node, 4 CPUs`, MPIDR `0xf00`–`0xf03`). `cpuburn-sh 4` shows all four at 100 % user time.
- Method: `enable-method = "rockchip,rk3036-smp"`, the upstream Linux method for RK3036 without a secure monitor. Linux runs in secure SVC, so the BootROM's secondary-core mailbox is usable. After a core reset the secondary waits in WFE until SRAM+4 = `0xDEADBEAF`, then jumps to SRAM+8. OP-TEE's RK322x PSCI uses the same mailbox.
- The `CONFIG_SMP=y` kernel sets `ACTLR.SMP` on every core itself (`proc-v7.S`). The timer node is `arm,cpu-registers-not-fw-configured` (secure physical timer), so the secondaries need no CNTFRQ/CNTVOFF setup.
- `cpu0` is back at its real `reg = <0xf00>`; the UP kernel needed `0`. The CPU OPP table is `opp-shared`, so one cpufreq policy covers all 4 cores.
- DT `status = "disabled"` on a CPU node does **not** keep the core offline (DT spec: "disabled" CPUs may be started by the enable method).

### Recovery paths (regression run on the final LAB=1 build)

| Failure | Mechanism | Back in MaskROM | Reload |
|---|---|---:|---|
| `lab-reboot` | sysrq `b` → restart handlers → first global reset | 2 s | automatic |
| Kernel panic / oops | `panic=3 oops=panic` → same restart path | 4–5 s | automatic; pstore keeps the crash log (`/sys/fs/pstore/dmesg-ramoops-0`) |
| Hard hang / stuck kernel | dw_wdt reset (daemon stopped in the test) | ~21 s | automatic |
| Hang during handoff, before Linux | watchdog armed in `linux_entry.S` | ~22 s | automatic |

Host loop: `python3 lab.py boot | sh 'CMD' | reboot | cycle | state`. Full Linux → MaskROM → Linux cycle ≈ 25 s.

### How MaskROM is reached from software

- The stock miniloader (SPI NOR dump, string "reboot to maskrom") writes `BOOT_BROM_DOWNLOAD = 0xEF08A53C` to `GRF_OS_REG0 = 0x110005c8`, then resets. Upstream U-Boot uses the same register for RK322X (`CONFIG_ROCKCHIP_BOOT_MODE_REG`). The BootROM honours it after both global soft resets and after a watchdog reset. A power-on reset clears it, so a normal power cycle still boots the stock firmware.
- **First global reset** (CRU `0x1f0 = 0xfdb9`, Linux's normal Rockchip restart) keeps the flag and resets enough state: MaskROM works.
- **Second global reset** (CRU `0x1f4 = 0xeca8`, used by the stock loader and the vendor kernel) and the **watchdog reset** keep GRF and CRU state. MaskROM then enumerates but times out on the first `0x471` (DDR-init) data stage (`err -7` at offset 0).
- **Root cause, bisected by restoring GRF/CRU registers to a handoff snapshot:** `GRF_SOC_CON3` (`0x40c`). It holds genpd's NoC idle requests for the VPU, RKVDEC and GPU domains. Linux sets them when those devices runtime-suspend. Restoring only `0x40c` fixes the second global reset; `0x760`, `0x680` and the iomux/pull block do not. RK3228's power domains are idle-only (no power switch), so skipping the idle requests costs no measurable power; the drivers still gate their clocks.
- Other pitfalls found on the way:
  - A clean `reboot -f` suspends the OTG PHY through GRF (`0x760 = 0x1d1`), and MaskROM then does not even enumerate. `lab-reboot` therefore uses the emergency path (no device shutdown), the same path a panic takes.
  - With SMP, the SRAM mailbox survives warm resets: see `rk3228-smp-reset.c` below.
  - Reading QoS registers of an idled domain, or some CRU offsets outside `0x000–0x13c`, stalls the bus. Never sweep regmap debugfs. (A full regmap sweep caused the first hang of the day.)
  - A `no-map` reserved region smaller than 1 MiB hung the ARM32 kernel early (section mappings).

## Code-path inventory

### Production-relevant (in every `PROFILE=perf` build, `LAB=0` too)

| Where | What | Notes |
|---|---|---|
| `build.sh` (perf block) | `CONFIG_SMP=y`, `NR_CPUS=4`, `HOTPLUG_CPU`, `FORCE_NR_CPUS` off | |
| `board/rk3228a-miracast-perf.dts` | SMP: `int_sram` + `rockchip,rk3066-smp-sram` node, `enable-method = "rockchip,rk3036-smp"` on cpu0–3, `cpu0 reg = <0xf00>`, `opp-shared` | |
| `board/rk3228-smp-reset.c` → `drivers/soc/rockchip/` | Restart handler (priority 255) that clears the SRAM mailbox | **Needed in production.** Otherwise, after any warm reset, CPUs 1–3 leave the BootROM into stale DRAM code while the next boot stage runs. Observed together with other state; not yet tested alone |
| `build.sh` (perf block) | `CONFIG_WATCHDOG`, `DW_WATCHDOG`, `WATCHDOG_HANDLE_BOOT_ENABLED`, `SOFTLOCKUP_DETECTOR` | Useful in production; there a watchdog reset boots the normal firmware |
| `board/rk3228a-miracast-perf.dts` | `&wdt { status = "okay"; }` | |
| `rootfs/init` | Starts BusyBox `watchdog -T 15 -t 2` if `/dev/watchdog` exists | Production decision: keep or replace with a supervisor |
| `busybox.config` | `REBOOT`, `HALT`, `WATCHDOG`, `NPROC`, `TASKSET`, `NICE`, `CHRT`, `MPSTAT`, `SEQ`, `USLEEP`, `TIME` | General tools; trim for production if wanted |
| `ramboot.py` | `SMP` allowed in the perf profile; `--dir`, `--dtb` | Host tooling |

### LAB-only (`LAB=1`)

| Where | What |
|---|---|
| `board/linux_entry.S`, `#ifdef LAB_RECOVERY` (`build-host-tools.sh` `-DLAB_RECOVERY`) | (a) MaskROM flag `0xEF08A53C` → `0x110005c8`; (b) dw_wdt pre-arm (TOP 15 ≈ 22 s, reset mode); (c) GRF and CRU `0x000–0x13c` snapshot → `0x67d00000` / `0x67d01000` |
| `board/rk3228a-miracast-perf-lab.dts` (selected when `LAB=1`) | Recovery bootargs `panic=3 oops=panic softlockup_panic=1 hw_protection=reboot`; 1 MiB `lab-snapshot@67d00000` reserved, `no-map` |
| `board/labreg.c` → `drivers/soc/rockchip/` | `/sys/kernel/debug/labreg` allow-listed peek/poke (SGRF, GRF, WDT, PWM, timer, CRU, TSADC, DDR PCTL/PHY, snapshot; no flash/MMC/SPI/efuse). Restart handler (priority 250) that disables the GIC distributor, hard-resets OTG controller/PHY, puts PLLs in slow mode and issues the chosen global reset. `echo "reset fst|snd|snd-restore MASK"` selects the reset (default first) |
| `build.sh` lab block: `pm-domains.c` patch marked `LAB_NO_NOC_IDLE` | `rockchip_pd_power()` never NoC-idles domains without a power switch. **Required for watchdog/hang recovery** (root cause above). Power-neutral, so production may keep it, but it is not needed there |
| `build.sh` lab block | Writable clk/regmap debugfs (`CLOCK_ALLOW_WRITE_DEBUGFS`, `REGMAP_ALLOW_WRITE_DEBUGFS`), `CONFIG_MAGIC_SYSRQ` |
| `rootfs/` (copied only when `LAB=1`) | `reg`, `lab-status` (CPUs, reset status, flag, watchdog, temperatures, pstore), `lab-reboot [fst\|snd\|snd-restore MASK] [clean]`, `lab-diff` (GRF/CRU vs handoff snapshot) |
| `lab.py` (host) | Hands-off boot/command/reboot loop; falls back to an already-resident helper when MaskROM `db` refuses |

Removed after the bisection: the DT `syscon-reboot` node issuing the second global reset (and the `"syscon"` compatible on the CRU). Linux's default first global reset is the one that works.

### Diagnostic register facts

- `CRU_GLB_RST_ST` (`0x110e0150`) is sticky across soft resets. The values seen (`0x2` → `0x3` after a first global reset, `0xa`/`0xb` after a watchdog reset) suggest bit 0 = first global reset, bit 1 = second, bit 3 = watchdog. Inferred, not documented.
- The global soft resets do not reset the CRU gates/soft resets or GRF. The stock loader puts APLL/GPLL into slow mode (`CRU_MODE_CON`) before its own reset.
