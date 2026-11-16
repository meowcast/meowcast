# GPU/4K performance work — 11 November 2026

Goal: ≥60 fps native-4K triangle rendering, with headroom for a future UI.
All runs are RAM-only (no flash writes), on the actual RK3228A dongle, Mesa 26.2.6 Lima.

## Changes (perf profile)

| Change | Where | Effect |
|---|---|---|
| DDR3 300 → **600 MHz** | [make-ddr-loader.py](../make-ddr-loader.py), [load-helper.py](../load-helper.py) | Same rkbin DDR blob, `ddr3_freq` set with Rockchip `ddrbin_tool`; loader differs from the verified one in 6 bytes (2 field + 4 CRC). Stock NOR loader of this dongle also ran 600 MHz. Stable on every boot. |
| `ACTLR.SMP` set in the handoff | [board/linux_entry.S](../boot/linux_entry.S) | Cortex-A7 treated memory as non-cacheable (UP kernel never sets the bit). L1 read 38 → 637 MB/s, DRAM copy 48 → 552 MB/s, latency 324 → 200 ns. Affects all CPU work. |
| CPLL 1200 MHz; VOP AXI 192 → 400 MHz; GPU on CPLL | [board/rk3228a-miracast-perf.dts](../dts/rk3228a-miracast-perf.dts), build.sh source fixes | CPLL rate table and `aclk_gpu` `CLK_SET_RATE_PARENT` were missing upstream. |
| Lima PP-stream cache 4 MiB | [gpu-test.c](../../../os/gpu-test.c) | 4K: CPU regenerated the PP stream every frame (3 → 27 fps). |
| Pipelined page flips, atomic KMS, VOP scaling, RGB565, blended layers, CRC verify | [gpu-test.c](../../../os/gpu-test.c) | Test tooling; UI-like load. |
| tsadc (CRU tshut 95 °C), writable clk/regmap debugfs, i2c-dev, canary/soak tools | perf profile only | Lab instrumentation. |

## Results (all frame-CRC verified)

| Native 3840×2160 offscreen triangle | fps |
|---|---:|
| Before (GPU 192, DDR 300) | 27.4 |
| GPU 384, DDR 600 (caches still off) | 51.5 |
| GPU 500, DDR 600 (caches still off) | 61.7 — **unstable, see below** |
| GPU 400, DDR 600, caches fixed | **67.2**, CPU 6.5 % |

Onscreen 4K30 (before the cache fix): 18.5–20.6 fps (was 10.6); 1080p rendered and VOP-upscaled to 4K30: 27.6 fps.
These were CPU-stall limited; remeasure with the cache fix.

## Stability limit: GPU clock vs. unknown vdd_logic

A soak (`gpu-soak`: 1080p offscreen, 8 blended layers, 96 MiB canary, boot-time hashes of all binaries):

| GPU | Outcome |
|---|---|
| 300 MHz | 6/6 runs clean, up to 85 °C |
| 400 MHz | 1 clean run, hang in run 2 (~85 °C); earlier boot: a whole 4 KiB page of `/bin/busybox` in RAM overwritten by rendered pixels |
| 500 MHz | hang under the same load |

The corrupted page contained this workload's own pixels (alpha `0x59` = 8 blended layers) in memory never given to the GPU — a wrong GPU address translation, consistent with timing margin. Stock firmware ran 400 MHz at vdd_logic 1.10 V and 500/600 at 1.15 V (PWM regulator on PWM2); this kernel does not control vdd_logic. **Use GPU ≤ 300 MHz until vdd_logic is controlled; 4K60-rate rendering at 300 MHz is not yet demonstrated.**

The SoC idles at ~70 °C and reaches 85 °C within a minute of GPU load (tshut 95 °C; kernel critical trip 90 °C).

## vdd_logic control and thermal throttling (later the same day)

`vdd_logic` is now driven by `pwm-regulator` on PWM2 at a fixed **50 % duty**. The stock map
is linear over 1.0–1.3 V, so 50 % is **1.15 V under either polarity** (stock: 400 MHz needed
1.10 V; 500/600 MHz used 1.15 V). With it, GPU 400 MHz soaked **7/7 runs clean** (canary,
page-cache hashes, frame CRCs); without it the same load corrupted memory and hung.

Stock thermal policy (from the vendor DTB): CPU-only throttling around target 95 °C, floor
600 MHz; the GPU had no thermal limit; hardware reset at 120 °C. The perf profile now has
Lima devfreq (200/300/400 MHz, simple_ondemand, idles at 200) and the GPU as a cooling device:
400 → 300 MHz at 90 °C, 300 → 200 MHz at 95 °C, critical 110 °C, hardware reset 115 °C.

Passive cooling (no fan), 4 minutes of continuous native-4K offscreen rendering, 228/228
frame checks passed, hashes intact, no shutdown:

| Minute | fps | GPU | Temperature |
|---:|---:|---|---|
| 1 | 67.1 | 400 MHz | 71 → 85 °C |
| 2 | 58.8 | 400 → 300 MHz at ~89 °C | 85 → 90 °C |
| 3 | 47.9 | 300 → 200 MHz at ~94 °C | 90 → 94 °C |
| 4 | 38.3 | 200 MHz | 93–95 °C |

Native 4K at ≥60 fps is a ~1.5 minute burst under passive cooling; sustained full load is
~38 fps. Idle is 65–72 °C: the fixed 1.15 V, DDR without power-down, and unused clocks/power
domains (`clk_ignore_unused`, `pd_ignore_unused`) are the next levers.

## CPU caches, CPU DVFS and the UI path (evening)

- `ACTLR.SMP` handoff fix (above) plus CPU DVFS: `vdd_arm` on PWM1 at 50 % duty (1.175 V under
  either polarity; stock needed 1.00 V for 816 MHz), cpufreq-dt 408/600/816 MHz, ondemand, CPU
  as a second cooling device. `cpu0` needed `reg = <0>` (UP kernel maps the boot CPU to ID 0).
  CPU memory at 816 MHz: L1 1075 MB/s, DRAM read 848 / write 1921 / copy 693 MB/s, 169 ns.
- Benchmarks, passive cooling, default governors, all frame-CRC verified:

| Case | fps | CPU |
|---|---:|---:|
| Native 4K offscreen triangle | 66.4 | 6.5 % |
| 1080p offscreen, 8 blended layers | 24.8 | 4.9 % |
| Native 4K onscreen, 4K30 | 28.6 | 7 % |
| 1080p → VOP-upscaled 4K30 | 29.8 | 4.6 % |
| 1080p + 4 blended layers → 4K30 | 29.8 | 6.2 % |

- Idle steady state is high: 79.6 °C after 4 min idle (CPU 408 MHz, GPU 200 MHz).

### Not yet working

- Lower `vdd_logic` per GPU OPP (83 % / 67 % duty): those boots stalled, but they also had the
  decoder nodes disabled (genpd sync_state then powers domains off), so the cause is not
  separated. `vdd_logic` stays fixed at 1.15 V.
- Booting without `clk_ignore_unused`/`pd_ignore_unused` stalls (verified alone at 1.15 V).
  `clk-gate-probe` reproduces the unused-clock pass at runtime one clock at a time: SDIO, I2S,
  SPDIF, TSP, GMAC/MAC-PHY and `cpll_vop` gate safely. The probe itself is flawed for clocks
  whose parent is an ignore-unused PLL (debugfs disable also drops the parent; it powered off
  the DDR PLL), so the culprit among the bus/GRF/NoC/VIO clocks is not identified yet.

## 4K60 output

Still "no signal". The TV's SCDC status (via `scdc-status`) shows clock detected, scrambling on
and the 1/40 clock ratio set, but never all three TMDS data channels locked: which channel fails
moves between runs, and no PHY drive/pre-emphasis/slope setting locked all three. This points to
marginal 6 Gbit/s signal integrity (board, connector or cable), not configuration. 4K30 works.
Earlier register checks: during 3840×2160@60 the source scrambler is on, HDMI PHY PLL locked, HPD/RX-sense high; the TV (P275MV PLUS) EDID advertises SCDC and 600 MHz TMDS. Stronger PHY drive/pre-emphasis (regs 0xef–0xf2) did not help. VOP bandwidth was ruled out (VOP AXI 400 MHz). Next: `hdmi4k60-diag` reads the TV's SCDC lock/scrambling/error status.

## Next steps

1. Lower idle/sustained power: gate unused clocks and power domains, DDR power-down idle, and
   per-OPP voltage once PWM polarity is established (e.g. by a temperature A/B at fixed load).
2. CPU frequency/throttling (cpufreq on vdd_arm), matching stock's CPU policy.
3. Remeasure onscreen 4K30 and 1080p→4K scaling with caches fixed.
4. 4K60: SCDC diagnosis.

## 13 November: DDR clock of flash boots, kernel scheduling for the UI

> **Default since 13 Nov: DDR3 528 MHz** on both boot paths: RAM boots via `load-helper.py
> --ddr 533`, flash boots via the handoff reclock. Earlier sections of this file measured
> DDR600 in RAM boots.

**Flash boots run DDR3 at 300 MHz.**
- **Clocks:** the flashed firmware keeps the stock IDBlock DDR init (V1.07, NOR 0x10000, not ours to
  change). It programs `ddrphy4x` 600 MHz and `ddrphy` 150 MHz, with PCTL timings for 300 MHz:
  CL6, tRFC 105, TREFI 0x96.
- **Comparison:** `load-helper.py --ddr 600` (rkbin) gives `ddrphy4x` 1200 MHz and `ddrphy`
  300 MHz, with CL10, tRFC 210 and TREFI 0x12c.
- **Why it was missed:** `dpll_ddr` shows "600 MHz" in both cases.
- **Effect:** GPU fill halves. A 1080p clear takes 9.7 ms instead of 4.2.
- **Note on 13 Nov tests:** the "DDR600 @ 1.15 V" decision of 13 Nov therefore ran at 300 MHz on
  flash boots. Full register dumps can be taken with `reg 0x11200000 256` / `reg 0x12000000 256`.

**Done (13 Nov, evening): DDR3 528 MHz on flash boots.**
- **What:** `board/ddr_reclock.c` runs in the flash handoff (`linux_entry.S` `DDR_RECLOCK`, built
  by `boards/rk3228a-dongle/flash/build.sh`) from SRAM 0x10081000 with an SRAM stack.
- **Sequence:**
  1. PCTL Config: the 21 timing registers that differ between the two dumps, plus MCFG 2T.
  2. Self-refresh.
  3. DPLL 600 → 1056 MHz.
  4. PHY DLL out of bypass, PHY soft reset.
  5. Wake.
  6. DDR3 mode registers for DLL-on operation: MR2 CWL6, MR1 `0x40` (DLL on, 40 Ω, RTT_NOM
     120 Ω), MR0 CL8 WR8 with DLL reset, then ZQCL.
  7. DQS gate training.
  8. Access.
- **Target:** exactly the register state of rkbin's `ddr3_freq=533` init, which is really 528 MHz.
- **Safety:** it runs only from the recognised stock state. If training fails, it switches back
  to the captured stock values.
- **Status word:** `reg 0x67d03018`. `0x5280000f` = reclocked, training passed on all 4 byte lanes.
- **Build guard:** `boards/rk3228a-dongle/flash/build.sh` rejects the section if it contains NEON/VFP or absolute
  addresses into the DRAM image.
- **Tested:**

| Test | Result |
| --- | --- |
| RAM rehearsal from the stock DDR init (`boards/rk3228a-dongle/flash/rehearse.py`), memtest 32 MiB × 4 threads, 300 s, GPU scrolling, 79→95 °C | 0 errors |
| Flashed (`firmware-meow-ddr528-48fe4ec7af41`), memtest 180 s under the same load | 0 errors |
| Full 1080p clear | 9.7 → 4.7 ms at 400 MHz |
| UI frame | 12.1 → 6.9 ms |

**Options considered:**
- **(a) DDR reclock in the handoff stage:** self-refresh, DPLL, PHY DLL, PCTL timings, then MR
  writes and data training. It runs from SRAM like the existing MCFG/TREFI code in
  `board/linux_entry.S`. It can be prototyped in RAM boots: load at 300, reclock to 533 or 600,
  `memtest`.
- **(b) A new IDBlock DDR init:** writes NOR below 0x50000, which is ruled out by the flash-safety
  rules.
- **Recommendation:** whichever option, target **533 MHz**. DDR600 had DQ1 margin errors on 12 Nov;
  DDR533 was clean.

**Kernel: high-priority GPU job and commit workqueues** (build.sh, perf profile).
- **Change:** Lima's drm_sched submit work (GP job, then PP job) and the nonblocking atomic commit
  tail moved from nice-0 kworkers to `WQ_HIGHPRI` queues.
- **Effect:** with 4 busy loops, UI scrolls went from 12–25 % to 4–16 % late at UI nice 0. With
  the UI at nice −10, it's 1–5 %.
- **`KLAT=1` build experiment** (`PREEMPT`, `HIGH_RES_TIMERS`, `NO_HZ_IDLE`, `HZ=250`): no
  measurable gain, so it stays opt-in.
- **Details:** [../../../ui/PERF-FINDINGS.md §11](../../../ui/PERF-FINDINGS.md#11-13-nov-display-planes-ddr-clock-scheduling).

**labreg** now also maps the NoC QoS generators (0x31030000–0x31070100). They read the same in
flash and RAM boots (GPU priority 0, regulator mode), and raising the GPU's priority made no
difference.
