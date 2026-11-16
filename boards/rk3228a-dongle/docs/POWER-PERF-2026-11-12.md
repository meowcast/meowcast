# Power, thermals and performance — 12 November 2026 (session 2)

These results follow [LAB-RECOVERY.md](LAB-RECOVERY.md) (SMP + hands-off MaskROM recovery). All measurements were taken on the dongle with passive cooling. There is no power meter, and macOS only refreshes `SystemPowerIn` about once a minute (too coarse), so **SoC temperature is the power proxy**. Each A/B was run as alternating phases of 4–5 minutes, with the mean of the last 60 s per phase; 10-minute idle runs use the mean of the last 120 s. HDMI was at 720p on the console.

## New defaults (end of session)

> **Update 13 Nov 2026, evening: the default is DDR3 528 MHz on every boot path.**
> - **Correction:** the stock IDBlock DDR init runs at **300 MHz**, not 600 (the paragraph below
>   is superseded).
> - **Flash boots:** the flash handoff reclocks to 528 MHz (`board/ddr_reclock.c`).
> - **RAM boots:** `load-helper.py --ddr 533` (= 528 MHz) is the default.
> - **Measurements:** [PERF-RESULTS.md](PERF-RESULTS.md#13-november-ddr-clock-of-flash-boots-kernel-scheduling-for-the-ui).
>
> *Earlier the same day:* **DDR3 600 MHz is the default again, for now.** The flashed firmware keeps the
> stock IDBlock, so its DDR init runs DDR3 at 600 MHz; this is staying for now. It requires **vdd_logic 1.15 V**
> (set by `boards/rk3228a-dongle/flash/pack-flash.py` in the flashed DTB). Tests on 13 Nov with the stock DDR init + TREFI 39 + 1.15 V:
> `ddrtest 120 4`, 0 errors (peak 95 C); memtest + `gpu-soak 400`, 0 errors, page cache clean. The 12 Nov
> limit still applies: DDR600 errors appear at vdd_logic <= 1.075 V. Do not lower vdd_logic with DDR600. Moving to
> DDR533 means replacing the IDBlock (button-recovery risk); deferred. See
> [boards/rk3228a-dongle/flash/README.md](../flash/README.md).


| Setting | Before | Now | Where |
|---|---|---|---|
| DDR3 clock | 600 MHz | **533 MHz** (DPLL 1056 → 528 MHz) | `load-helper.py` default `--ddr 533` (pinned SHA `d34ea2f1…`); `--ddr 600` still available |
| DDR refresh interval (TREFI) | 7.8 µs | **3.9 µs** (DDR3 rule above 85 °C case) | `build-host-tools.sh` `-DDDR_TREFI=39` (override `DDR_TREFI=0` to keep the blob value) |
| vdd_logic | 1.15 V | **1.10 V** (67 % duty) | `rk3228a-miracast-perf.dts` `vdd_logic`, GPU OPPs |
| vdd_arm | fixed 1.175 V | DVFS 1.00/1.05/1.10 V for 408/600/816 MHz | `rk3228a-miracast-perf.dts` |

Validation status: the new default boots (DPLL 1056 MHz, TREFI 0x27, vdd_logic 1.0985 V, 4 CPUs). The pieces were each tested
(DDR533 clean under hard 4-thread memtest at 1.05 and 1.10 V; TREFI 39 boots and runs). **The combined
10-minute hot soak (`memtest 64 600 4` + `gpu-soak 400`) and the recovery regression were not run yet on
this exact build. Do them first next session.** Fallback to the previous defaults:
`load-helper.py --ddr 600`, vdd_logic 1.15 V in the DTS, `DDR_TREFI=0 sh build-host-tools.sh`.

## Kept in the default build (validated)

| Change | Where | Evidence |
|---|---|---|
| SMP, 4 cores | see LAB-RECOVERY.md | all 4 cores at 100 % |
| **vdd_arm DVFS.** Continuous PWM regulator, measured polarity V = 1.40 − 0.45 × duty. OPPs 408/600/816 MHz @ 1.00/1.05/1.10 V (stock 0.95/0.975/1.00 V) | `rk3228a-miracast-perf.dts` (`vdd_arm`, `cpu_opp_lab`, `cpu-supply`) | Polarity: at a fixed 408 MHz load, duty 33 % ran 100.3 °C vs 95 °C at 67 %. cpustress runs at these points were clean, except one run with DDR self-refresh enabled (attributed to DRAM, see below). Idle CPU now sits at 1.00 V instead of 1.175 V |
| HDMI fix: `hclk_vio_h2p`/`pclk_vio_h2p` marked critical | `build.sh` (clk-rk3228.c sed, all profiles) | Without it, gating unused clocks killed HDMI (dw_hdmi IRQ "nobody cared", HPD never fires). Upstream bug |
| `make-ddr-loader.py --sr-idle/--pd-idle`, `load-helper.py`/`lab.py --variant/--ddr-variant`, `ramboot.py`/`lab.py --entry`, `linux_entry.S` `DDR_PD_IDLE`/`DDR_SR_IDLE` | tools | Defaults unchanged. The default DDR600 loader still reproduces the verified SHA `1240b5…` |
| `memtest` (C, in the image), `cpustress`, `idletemp`, `regab`, `clkab` | `memtest.c`, `rootfs/` | Test tools |

## Measured but NOT enabled (kept on the side)

| Experiment | Result | How to reproduce |
|---|---|---|
| DDR self-refresh + power-down (`sr_idle=12`, `pd_idle=32`, set from SRAM in the handoff) | −2 °C idle, but **memtest errors** (bit 1/17 flips) | `lab.py boot --entry output/variants/linux_entry_ddridle.bin` |
| DDR power-down only (`pd_idle=32`) | memtest clean, but ≈0 °C saved | `output/variants/linux_entry_ddrpd.bin` |
| DDR blob `sr_idle`/`pd_idle` params | v1.10 blob ignores them (`MCFG`/`MCFG1` unchanged) | `--ddr-variant sr12_pd32` |
| vdd_logic polarity | V = 1.30 − 0.30 × duty (duty 60 % ran ~3 °C cooler than 40 % at idle) | `regab 0x110b0028 0x1e0 0x2d0 300 2` |
| vdd_logic 1.10/1.125 V (from 1.15 V) | ≈ −1.2 °C per 25 mV at idle. Floor test at moderate DRAM load: 1.050 V 243 errors, 1.075 V 1, 1.100 V clean for 5 min. **Not enabled** because of the DDR600 finding below | change `vdd_logic` `voltage-table` (1.125 V = 58 %) |
| Unused clock/power-domain gating (no `clk_ignore_unused pd_ignore_unused`) | Works with the H2P fix; **−2.4 °C idle** (78.4 → 76.0 °C); all recovery paths pass. Not enabled: `pclk_ddrphy` reads hardware-*off* although its enable count is 1 (shared gate bit?) | `--dtb output/variants/noignore.dtb` |
| VOP AXI 200 vs 400 MHz | −0.8 °C idle (enough for 1080p; 4K needs 400) | `clkab aclk_vop_pre 400000000 200000000 300 2` |
| CPU 1008/1200 MHz @ 1.175/1.275 V | Run, but not validated: DRAM errors masked the result | upstream OPP values in the DTS comment |
| Display pipeline cost | With HDMI accidentally off, idle was ~13 °C lower (65 °C): VOP + HDMI PHY/TMDS dominate idle power | — |

## DDR: what the errors are, and the fix candidate (late session)

All memtest failures share one signature: bit 1 or bit 17 (occasionally 22) flipping 0 → 1, i.e. DQ1 of **both** x16 DDR3 chips (Samsung K4B1G1646G, datasheet in the owner's Downloads).

| Test (same workload: `vlogic-floor 1000` = vdd_logic 1.05 V, GPU soak, 2-thread memtest 75 s, ~93 °C) | errors |
|---|---:|
| DDR600, TREFI 78 (7.8 µs, blob default) | 35 |
| DDR600, TREFI 39 (3.9 µs = DDR3 2× refresh for > 85 °C, set from the handoff) | 76 |
| **DDR533**, TREFI 78 | **0** |

- **Not retention.** Doubling the refresh rate does not help, so this is an electrical timing margin on DQ1 in the controller/PHY. It shrinks with lower vdd_logic, higher temperature and DDR self-refresh.
- **DDR533 restores the margin.** Hard 4-thread `memtest 32 120 4`, hot: 0 errors at vdd_logic 1.05 V and at 1.10 V. CPU-side bandwidth is unchanged (copy 679 MB/s; the CPU is the limit); GPU/VOP lose at most ~11 %.
- DDR600 at the default 1.15 V: about 25 min of hot heavy memtest has been clean since the 1008/1200 OPPs were removed. One earlier run (image with a 1.125 V vdd_logic table, 1.15 V forced at runtime) had 4 errors in 120 s and is not explained. Treat DDR600 as low-margin.
- The datasheet's refresh rule still applies: the blob programs TREFI 78 (7.8 µs), while the DRAM runs above 85 °C here. `linux_entry.S -DDDR_TREFI=39` sets 3.9 µs (a runtime write of `TREFI | bit31` in Access state does not take). Self-refresh above 85 °C would also need MR2 SRT/ASR. Both are off by default; enable TREFI 39 together with the next DDR default.
- Stock firmware uses an older DDR blob, "DDR Version V1.07 20180427" (NOR `0x11000`, both IDB copies identical), saved as `output/loader/stock_ddr_v1.07_nor0x11000.bin`. Its parameter block predates `ddrbin_tool`'s format, so its settings are not decoded; it ran DDR3 at 600 MHz.
- Boot variants: `lab.py boot --ddr-variant ddr533` (or `ddr400`), `--entry output/variants/linux_entry_trefi39.bin`. Tools: `ddrtest SECONDS THREADS FREQ` (throttling suspended, 105 °C guard), `vlogic-floor DUTY...`.

## Idle temperature timeline (720p console, idle)

| State | Idle °C |
|---|---:|
| Session start (fixed vdd_arm 1.175 V) | 84 |
| + vdd_arm DVFS (1.00 V at 408 MHz) | ~84 (most of the CPU gain shows under load) |
| (vdd_logic 1.125 V, side) | 78.4 |
| (+ unused clock gating, side) | 76.0 |
| (+ VOP AXI 200 MHz, side) | ~75 |

Sustained 4-core load still reaches 90–95 °C and throttles to 408–600 MHz.

## Plan for the next sessions

1. **(Done at the end of the session: DDR533 + vdd_logic 1.10 V + TREFI 39 are the default.)** Remaining: This is the main power win: vdd_logic 1.15 → 1.05–1.10 V ≈ −3 to −5 °C idle.
   - Soak DDR533 at vdd_logic 1.05/1.10 V for ≥ 10 min hot (`memtest 32 600 4` + `gpu-soak 400 10`).
   - Measure GPU fps cool, DDR533 vs DDR600 (`gpu-bench`; 4K offscreen was bandwidth-bound).
   - Enable `DDR_TREFI=39` with it.
   - Optionally boot the stock V1.07 blob as CODE471 to see if it gives DDR600 more margin (a hang there is inside MaskROM, not covered by the watchdog, and needs a manual power-cycle).
2. (Covered by 1.) Check that the GPU at 400 MHz is still clean (canary) at the chosen vdd_logic; stock used 1.10 V for GPU 400.
3. Enable unused-clock gating by default once `pclk_ddrphy` is understood. Test the LAB=0 image without the NoC patch.
4. Validate CPU 1008/1200 MHz (`cpustress` + `memtest`, pinned, from a cool start) and lower the 408–816 MHz voltages toward stock (0.95–1.00 V) in 25 mV steps.
5. Display: per-mode VOP AXI clock (200 MHz for 1080p), and measure 1080p60 (the UI default) rather than the 720p console.
6. Benchmarks with 4 cores: single- vs multi-core CPU, `gpu-bench`, sustained 10-minute load temperature/fps.
7. Optional: CPU core hotplug or a cpuidle state at idle; DDR power-down revisited after the DDR fix.
