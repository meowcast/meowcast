# UI performance findings (2026-11-11 session)

This covers what was measured and built while turning the RK3228A dongle into a 10-foot launcher:
- the Slint spike;
- the custom GLES2 renderer;
- a Lima driver patch;
- the memory-bandwidth and thermal analysis;
- the phone remote.

How-to material lives elsewhere:

| Topic | Where |
|---|---|
| Renderer architecture and usage | [README.md](README.md) |
| Slint spike details | not published (an abandoned experiment) |
| Phone remote | [../cloud/pilot/README.md](../cloud/pilot/README.md) |
| Earlier GPU/DDR/clock work | [../boards/rk3228a-dongle/docs/PERF-RESULTS.md](../boards/rk3228a-dongle/docs/PERF-RESULTS.md) |

**Test setup:**
- Test TV P275MV PLUS; `perf` kernel; DDR3 at 600 MHz; GPU devfreq 200/300/400 MHz; CPU 408/600/816 MHz.
- The application binaries were deployed to `/opt` over USB networking, with no reboots.
- **The SoC sat at 85–97 °C all session**, so the GPU spent most runs throttled to 200–300 MHz. Treat every
  number as *hot, sustained*.
- `gpu_hz` is sampled once per line, so per-line clock attribution is approximate.

## 1. Summary

| | Slint 1.18 + FemtoVG | Custom renderer |
|---|---|---|
| 1080p60 navigation | 6.7 fps | **60 fps** (16.7–17.1 ms frames, 0–2 % late) |
| 4k30 navigation | 1.8 fps (full scene) | 27–30 fps for row moves (RGB565); about 20 fps for vertical scrolls |
| Idle | 0 fps, GPU at 200 MHz | 0 fps, 0 % CPU, GPU at 200 MHz |
| First frame, 1080p / 4K | 0.77 s / 0.87 s | 1.45 s / 2.9 s |
| RSS | 20–21 MiB | 15–16 MiB |
| Binary (stripped) | 7.3 MB | 1.0 MB |

**Three findings mattered most:**
1. **Mali-400 is per-pixel-instruction bound.** A generic vector renderer's fragment shaders are 5–10× over
   budget.
2. **Lima reloads the framebuffer for every partial frame,** adding 57–85 % GPU time. The project's Mesa
   patch removes that when the app does a tile-aligned scissored clear.
3. **At 4K the floor is DRAM bandwidth:** writing one 4K XRGB frame takes about 21 ms. RGB565 and partial
   redraw are mandatory there.

## 2. Mali-400 / Lima cost model

**Fragment throughput.** The PP issues about **1 instruction per clock per core**. With 2 cores at 400 MHz
that is 0.8 G instruction-pixels per second. Per screen pixel per frame, that gives:

| Mode | Instruction·pixels per screen pixel per frame |
|---|---:|
| 1080p60 | about 6.4 |
| 1080p60, throttled to 200 MHz | about 3.2 |
| 4k30 | about 3.2 |

**Validation:** a 55-instruction full-screen gradient at 1080p was predicted at
2.07 Mpx × 55 / 0.8 G ≈ 140 ms, and measured at 5.5 fps.

**Shader sizes.** FemtoVG's fragment shader variants compiled to 55 instructions (gradient), 36 (image/text),
27 (solid colour with edge AA) and 1 (stencil). They do scissor matrices, `sdroundrect`/`length()` and a dither
hash on every pixel. The custom renderer's single shader, `texture2D(tex, v_uv) * v_col` with the coordinate
used straight from the varying, costs about 2.

**How to measure:** `LIMA_DEBUG=shaderdb MESA_SHADER_CACHE_DISABLE=true` prints `SHADER-DB: … N inst`. The
disk cache otherwise hides recompiles.

**Blending** is done in the tile buffer and doesn't cost fragment instructions. Overdraw and instruction
count are what matter.

**Mesa/EGL facts verified on the dongle (Mesa 26.2.6):**
- Both `EGL_EXT_buffer_age` and `EGL_KHR_partial_update` are exposed on the GBM platform.
- Lima's PP stream covers only the bounding box of a job's draws (the scissor, in 16×16 tiles). Partial
  redraw therefore saves real GPU work.
- Lima **always reloads** the old contents of every processed tile unless the job began with a clear. Even
  with `eglSetDamageRegionKHR` it reloads.
- Lima's `glClear` (before the patch) always damaged the **whole** surface, so clearing defeated partial
  redraw.
- The `LIMA_PLB_PP_STREAM_CACHE_SIZE=4194304` setting from the handoff is still required. Varying damage
  rectangles create new PP streams; 32 px-aligned rectangles improve cache reuse.

## 3. Slint spike: rejected

**What worked:**
- Slint 1.18.1 `linuxkms` + FemtoVG ran on Lima with no libinput, libseat or udev (feature
  `backend-linuxkms` without `-libinput`).
- With fontique's `fontconfig-dlopen` feature it ran without fontconfig, using `SLINT_DEFAULT_FONT`.

**Full-redraw fps per scene part** (`UI_SCENE` bitmask):

| Scene | 1080p60 | 4k30 |
|---|---:|---:|
| Empty (clear + 6 px dot) | 50 | 15 |
| Full-screen 2-stop gradient | **5.5** | – |
| Header text | 35 | – |
| Card shapes | 30 | 10 |
| Everything | 5.0 | 1.8 |

The UI used 8 % CPU with the CPU at 408 MHz, so it was GPU-bound. Slint also redraws the whole window every
frame and doesn't pipeline page flips; even the empty scene missed vblanks.

**Verdict:** no-go. Patching FemtoVG's shaders wouldn't fix the full-window redraws or the 4K budget.

## 4. Custom renderer

**Principle: do expensive visual work once; per frame, only copy texels, and only where something changed.**
Architecture is in [README.md](README.md). In short:
- **Artwork, drawn once by tiny-skia:** rounded 9-slices, blurred shadow, ring, icons, gradient badges, and a
  procedural pre-blurred wallpaper (960×540, upscaled by the GPU).
- **Atlas:** one texture, 1024² at 1080p (4 MiB) and 2048² at 4K (16 MiB). It was 42 % used after prefetching
  everything at 4K: 144 entries, 112 glyphs and 21 badges.
- **Text:** ab_glyph glyphs rasterised lazily into the atlas.
- **Per frame:** about 2 draw calls (wallpaper, atlas).
- **Damage:** item-list diff + `EGL_BUFFER_AGE` + a 32 px-aligned scissor.
- **Presentation:** pipelined legacy page flips, ported from `gpu-test.c`.

### GPU cost per full static frame (`UI_GPU_BENCH=1`, ms, `glFinish`-timed)

| Variant | 1080p, pre-patch @300 | 1080p, patched @300–400 | 4K XRGB, patched @200–400 | 4K RGB565, patched @300–400 |
|---|---:|---:|---:|---:|
| Clear only (= framebuffer writeback) | 5.8 | 5.0 | 20.7 | 15.2 |
| Clear + wallpaper | 9.5 | 7.6 | 32.3 | 22.9 |
| Clear + UI | 8.5 | 6.9 | 32.8 | 22.8 |
| Clear + wallpaper + UI | 14.1 | 11.3 | 49.4 | 36.6 |
| Reload + wallpaper + UI | 22.2 | 19.1 | 92.9 | 73.2 |
| Card-row band (about 42 %), reload | – | 11.9 | 59.9 | 38.8 |
| Card-row band, scissored clear (patched) | – | **8.3** | **38.5** | **25.7** |

### What each optimisation bought

| Step | Effect |
|---|---|
| Custom renderer instead of Slint, 1080p | 6.7 → 30–49 fps on the first run |
| `glClear` before full frames (no reload) | Full-redraw frames about 22 → 14 ms |
| Skipping the shadow 9-slice centre (hidden under the opaque card) | Saves one card-area fill per card |
| Lima scissored-clear patch (§5) | Partial frames −30 to −35 %; 1080p navigation reached 16.7 ms with 0–2 % late |
| RGB565 + dithering at 4K (default `UI_RGB565=auto`) | Clear 20.7 → 15.2 ms; band 38.5 → 25.7 ms; no visible banding in 4K screenshots |
| Idle-time prefetch of badges and glyphs | Removed a **1.4 s freeze** when navigation first reached the third row at 4K (inline rasterisation in a frame) |

### Navigation results (continuous auto-navigation, `bench.sh`)

| Mode | Frame interval while animating | Notes |
|---|---|---|
| 1080p60, partial | **16.7–17.1 ms**, 0–2 % late | GPU about 9 ms/frame at 30–45 % repaint; CPU build about 3 ms; GPU 200–300 MHz, 90–92 °C |
| 1080p60, forced full redraw | 17–23 ms, 3–40 % late | |
| 4k30 XRGB, partial | 33–87 ms | GPU about 55 ms/frame at 200 MHz, 94–97 °C |
| 4k30 RGB565, partial | 33.4 ms for row moves; about 50 ms for vertical scrolls | Vertical scroll repaints 70–85 % of the screen; 300 MHz, 91 °C |

The 4K cold start (2.4–3.3 s) is dominated by CPU art and glyph rasterisation at 2× scale, done with the CPU
partly throttled.

## 5. Lima patch: scissored clear skips the tile reload

The patch is [../os/patches/mesa-lima-scissored-clear.patch](../os/patches/mesa-lima-scissored-clear.patch),
applied idempotently by `build-mesa.sh`.

**What it does:**
- Lima advertises `pipe_caps.clear_scissored`, so Mesa's state tracker passes scissored `glClear` to the
  driver instead of drawing a quad.
- **Tile-aligned (16 px) scissored clear:** the job processes only those tiles, starting from the clear value,
  with no reload. `job->clear_rect` records the rectangle.
- **Safety guard:** a later draw in that job that reaches outside `clear_rect` submits the job first. The next
  job reloads normally, so the patch stays correct for any GL client.
- **Unaligned scissored clear:** falls back to a `util_blitter` quad clear, matching the previous behaviour.
- Partial clears never merge with other clears in one job.

**Measured:** −31 % at 1080p and −36 % at 4K on a card-row repaint (11.9 → 8.3 ms, 59.9 → 38.5 ms).

**Deployment:** until the initramfs is rebuilt, the patched `libgallium-26.2.6.so` (13 MB) ships in
`/opt/ui/lib` through `LD_LIBRARY_PATH`. `package-ui.py` ships any Mesa library whose hash differs from
`runtime-manifest.json`.

## 6. Memory bandwidth: is the RAM slow?

**No. It's ordinary low-end DDR3, used at a normal efficiency.**

**The memory:** 256 MiB DDR3 at 600 MHz (1200 MT/s), the same speed as the stock firmware. The bus width
isn't recorded anywhere, but the measurements imply **32-bit**: 4.8 GB/s peak.

**What the GPU achieved during framebuffer writeback** (clear-only runs; scanout keeps running alongside):

| Case | GPU writes | Scanout | Total |
|---|---:|---:|---:|
| 1080p | 8.3 MB in 5.0 ms = 1.66 GB/s | 0.5 GB/s | about 2.2 GB/s |
| 4K XRGB | 33 MB in 20.7 ms = 1.6 GB/s | 1.0 GB/s | **about 2.6 GB/s**, above a 16-bit bus's 2.4 GB/s peak, hence 32-bit; about 55 % of peak, typical |
| 4K RGB565 | 1.09 GB/s | 0.5 GB/s | below the bandwidth limit; the GPU's own tile rate (about 0.55 Gpixel/s) takes over |

**Memory-bound, not clock-bound:** going from GPU 300 to 400 MHz sped up clear-only frames by only 15 %
(1080p) and 8 % (4K).

**CPU memory figures** (`membw`, Cortex-A7 at 816 MHz):

| Metric | Value |
|---|---|
| Read | 848 MB/s |
| Write | 1921 MB/s |
| Copy | 693 MB/s |
| Latency | 169 ns |

These are normal for one in-order A7 core, whose reads are limited by latency.

**Where bandwidth limits us:**
- **1080p60: no.** Partial redraw keeps the GPU at about 9 ms per frame.
- **4k30: yes.** Full-frame writeback (about 21 ms) plus about 1 GB/s of continuous scanout leave little of
  the 33 ms budget.

**Not misconfigured, but possible levers:**
1. **DDR clock above 600 MHz.** Needs the DRAM part numbers and speed grade from the PCB first; carries a real
   instability risk.
2. **DDR controller idle/self-refresh timers and bus priorities** set by the rkbin DDR blob. Probably a few
   percent.
3. **Display IOMMU with 4 KB pages at 4K.** Modest extra traffic.
4. **No framebuffer compression** on Mali-400 or this display controller. A hardware limit.

**Cheap confirmations:**
- Count the DRAM chips and read their part numbers. Two ×16 chips means a 32-bit bus.
- Run `membw` with the display at 4K30 and again at 720p; the difference should match the scanout load.

## 7. Thermal

- **Idle:** 85–88 °C during this session, above the handoff's earlier "about 80 °C". Repeated benchmarking
  pushed the chip to 90–97 °C, where the GPU throttles to 300 MHz and then 200 MHz, and the CPU throttles too.
- **Effect:** everything that isn't memory-bound lost 25–50 %.
- **Root cause** (known open issue): unused clocks and power domains stay on (`clk_ignore_unused
  pd_ignore_unused`), and `vdd_logic` is fixed at 1.15 V.
- **Design consequence:** render-on-change is essential. When nothing changes, the UI submits nothing and the
  GPU stays at 200 MHz.

## 8. Other findings

- **HDMI hot-plug leaves a stale mode list.** After a hot-plug, the connector's cached modes held only ≤720p
  CEA modes, although the EDID was fine. `echo detect > /sys/class/drm/card0-HDMI-A-1/status` restored 4K and
  1080p. The UI forces a probe (`get_connector(.., true)`) before choosing a mode.
- **Never fall through to the TV's preferred mode** (4K60 gives no signal on this board). Mode fallback is
  `UI_OUTPUT` → 1080p60 → 720p60 → refuse to start.
- **Legacy KMS** (`set_crtc` + `page_flip`) with a linear XRGB8888 or RGB565 GBM surface works. Restoring the
  CRTC on exit brings the console back.
- **Screenshots for review:** `glReadPixels` → PPM → BusyBox `nc` → `shot.py` on the Mac → PNG. Shots are taken
  on a normal *partial* frame, so they also show stale or reload bugs. None were seen.
- **BusyBox `nc` half-closes** on stdin EOF but keeps reading. A server receiving from it must allow
  half-open sockets.

## 9. Tools added

| Tool | Purpose |
|---|---|
| `bench.sh [MODE] [SECS]` | Idle / navigation / full-redraw / GPU-timed navigation runs |
| `UI_GPU_BENCH=1` | Static-frame GPU cost breakdown (§4 table) |
| `UI_PROFILE=1` | Per-frame CPU build time and GPU time (`glFinish`, serialises) |
| `UI_FULL_REDRAW=1`, `UI_NO_SCISSOR_CLEAR=1`, `UI_RGB565=0/1` | A/B switches |
| `UI_STATS` lines | fps, animation frame interval, late %, repaint %, CPU %, RSS, GPU Hz, temperature |
| `shot.py` + `p` key / `UI_SHOT_AT` | Screenshots |
| `cpuprobe.sh` | Tells CPU-bound from GPU-bound |
| `../cloud/pilot/` | Phone remote: React app served by the Mac; keys relayed over USB via `remote-link.sh` (outbound `nc` from the dongle) |

## 10. Open items

**4k30:**
- Put the wallpaper on its own VOP plane, so the GPU never draws or rewrites it.
- Make vertical scrolls at 4K cheaper (shorter, or crossfade instead of a full-screen move).

**Thermal and memory:**
- Find the clock or power domain that `clk_ignore_unused` keeps on, to lower idle temperature. This is the
  biggest lever on sustained GPU clock.
- Check the DRAM part numbers before considering a higher DDR clock.

**Packaging:**
- Bake the UI and the patched Mesa into the initramfs; `/opt` deployment is currently development-only.
- Autostart `remote-link.sh` with the UI.

**Input:** evdev (USB keyboard) or the IR receiver (`pwm@110b0030`).

**Phone remote:** reaching the Mac from the iPhone failed with "192.168.1.4 is currently unreachable". The
Mac's firewall is off, and no packets from the phone reached the Mac. The likely cause is iOS's Local Network
permission for non-Safari browsers, or Wi-Fi client isolation. Unresolved at the time of writing.

## 11. 13 Nov: display planes, DDR clock, scheduling

The goal was the depth blur during vertical scrolls, plus a smoother UI overall. The work was
measured on the TV with `scrollbench.sh` and `UI_GPU_BENCH`. Results tables: [README.md](README.md#display-planes-ui_planes-default-on-up-to-1080p).

**1. Display planes (driver level): the biggest UI win.**
- **Layout:** the RK3228 VOP has two scaling windows: win0, the primary plane, and win1, exposed as
  a "cursor" plane but a full window with per-pixel premultiplied alpha. The wallpaper now goes on
  win0 at 960×540 and the VOP scales it; the UI goes on win1, through atomic commits.
- **GPU cost:** it no longer draws the wallpaper at full size. A dissolve frame costs a quarter-size
  blend, about 5.7 ms at DDR600 and 300 MHz.
- **Repaints:** a wallpaper change never forces a full-screen UI repaint.
- **Effect:** vertical scrolls with the blur *during* the scroll went from 18–42 % to 0.6–3 % late
  frames. GPU time per scroll frame went from 16–24 ms to 9–12 ms; less GPU time is also less heat.

**2. The flashed firmware runs DDR3 at 300 MHz, not 600.**
- **Clocks, flash boot:** the stock IDBlock DDR init (V1.07) is kept for flash boots. It leaves
  `ddrphy4x` at 600 MHz and `ddrphy` at 150 MHz, with PCTL timings CL6, tRFC 105 and TREFI 0x96.
- **Clocks, RAM boot:** the rkbin `--ddr 600` blob gives `ddrphy4x` 1200 MHz and `ddrphy` 300 MHz,
  with CL10, tRFC 210 and TREFI 0x12c.
- **Why it was missed:** `dpll_ddr` reads "600 MHz" in both cases, which hid it.
- **Effect:** every GPU number halves. A full-screen clear takes 9.7 ms instead of 4.2, a full
  GPU-composited frame 15.5 ms instead of 10.5.
- **The 11 Nov numbers** (clear 5.0 ms) were RAM boots at DDR600.
- **Fixed the same evening:** the flash handoff reclocks DDR to 528 MHz (boards/rk3228a-dongle/boot/ddr_reclock.c;
  see boards/rk3228a-dongle/docs/PERF-RESULTS.md). On the flashed firmware, a clear takes 4.7 ms and a UI frame 6.9 ms.

**3. Ruled out with measurements:**
- **NoC QoS** (`qos_gpu` 0x31050000: priority 0, regulator mode; VOP priority 3): identical in flash
  and RAM boots. Raising the GPU's priority to 2 or 3 changed nothing beyond heat noise.
- **Fixed per-job cost:** 0.4 ms. Not a factor; the GPU is DRAM-bandwidth-bound.
- **`PREEMPT` + `HIGH_RES_TIMERS` + `NO_HZ_IDLE` + `HZ=250`** (`KLAT=1` build experiment): no
  measurable gain idle or under load, alone or with item 4. The frame loop is paced by flip events,
  not timers. It stays opt-in.

**4. Scheduling under CPU load (kernel and UI policy).**
- **The problem:** with 4 busy loops at nice 0 (like an app install or decode), plane-mode scrolls
  were 12–25 % late.
- **The cause:** the GPU job hand-offs (Lima's drm_sched `submit_wq`: run the GP job, then the PP job)
  and the nonblocking atomic commit tail (wait for the GPU fence, program the VOP) ran in nice-0
  kworkers. They queued behind the busy threads.
- **Fix, kernel** (os/build.sh): a `WQ_HIGHPRI` ordered submit workqueue per Lima pipe, and
  `system_highpri_wq` for nonblocking commits.
- **Fix, UI:** `UI_NICE=-10`.
- **Results, % late frames under load** (two runs each):

| Kernel | Vertical, nice 0 | Horizontal, nice 0 | Vertical, nice −10 | Horizontal, nice −10 |
| --- | --- | --- | --- | --- |
| baseline | 11.8 / 16.8 | 15.3 / 24.9 | 6.7 / 6.9 | 5.5 / 6.2 |
| `KLAT` | 16.5 / 16.3 | 23.0 / 20.5 | 6.0 / 4.9 | 3.1 / 5.0 |
| high-priority workqueues (**shipped**) | 4.2 / 6.7 | 10.7 / 16.2 | 4.7 / 4.1 | 1.1 / 2.7 |
| both | 5.8 / 13.9 | 9.6 / 14.8 | 2.8 / 6.3 | 0.5 / 2.1 |

  The load runs sit at 93–95 °C, where the GPU throttles to 300 and then 200 MHz: the remaining
  late frames are mostly heat.

**5. Metric fix:** an idle wait (poll with a timeout) now ends the animated interval. Previously,
an animation that started right after another ended counted the idle gap as one 430 ms "frame".
