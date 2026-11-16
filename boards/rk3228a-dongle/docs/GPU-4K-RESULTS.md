# GPU acceleration and 4K RAM tests — 11 November 2026

**Hardware GLES2 rendering and visible 4K30 output work.** The owner confirmed
a rotating RGB triangle at 720p60 and native 3840×2160 at 30 Hz. At 4K60, DRM
accepts the mode and rendering continues, but the owner observed black/no signal.
The test restored 720p, and the owner confirmed the console visible again.

All work ran from RAM with USB diagnostics retained. No flash/efuse programming
or Wi-Fi testing was performed. This remains a one-CPU development kernel.

## Hardware acceleration evidence

- Linux **7.3.4**, Mesa **26.2.6**, Mali-400 MP2; EGL **1.5**, GLES **2.0**.
- Runtime: `GL_VENDOR=Mesa`, `GL_RENDERER=Mali400`,
  `GL_VERSION=OpenGL ES 2.0 Mesa 26.2.6`.
- Mesa includes only `gallium-drivers=lima`; LLVM, llvmpipe and softpipe are not
  built. The test also rejects a non-Mali400 renderer.
- Vertex/fragment shaders compile, and actual pixel readback matches the
  triangle and background: approximately `(73,74,108,255)` and `(10,15,26,255)`.
- First offscreen run: GP interrupts **0 → 101**, PP interrupts **0 → 142**,
  GPU-MMU interrupts remained zero.
- Onscreen tests use EGL/GBM, shared Rockchip/Lima buffers, DRM modesetting and
  page-flip events. The owner verified the actual 720p and 4K30 pictures.

This proves the tested small rendering path, not full GLES conformance, video
decoding or production stability.

## Measured results

| Test | HDMI output | Render cap | Observed rendering | Outcome |
|---|---|---:|---:|---|
| Offscreen 720p, 4 s | Unchanged | 30 fps | 100 / 4.029 s = **24.82 fps** | Pixel verification passed |
| Visible 720p triangle, 25 s | **1280×720 at 60 Hz** | 30 fps | 623 / 25.040 s = **24.88 fps** | Visible, page flips passed |
| Offscreen 4K, 5 s | Unchanged | 10 fps | 15 / 5.172 s = **2.90 fps** | Native 4K pixels verified |
| Visible 4K triangle, 25 s | **3840×2160 at 30 Hz** | 10 fps | 51 / 25.432 s = **2.01 fps** | Visible, page flips passed |
| First 4K60 attempt, 25 s | Driver accepts 4K60 | 10 fps | 43 / 25.416 s = **1.69 fps** | Visual observation uncertain; repeated |
| Repeated 4K60 attempt, 45 s | Driver accepts 4K60 | 5 fps | 78 / 45.141 s = **1.73 fps** | **Black/no signal**, owner confirmed |

HDMI refresh and rendered fps are different: 4K30 can display the same frame
repeatedly while the GPU produces about two new frames per second. These are
measurements from an unoptimized test, not maximum hardware throughput. Offscreen
uses `glFinish()` each frame; onscreen includes buffer sharing, page flips and
frame pacing. No bottleneck profiling or clock tuning was done. The driver
reported a 192 MHz GPU clock; the CPU clock was 480 MHz, with one CPU active.

Texture, renderbuffer and viewport limits are **4096×4096**. A 3840×2160 target
fits, but a responsive native-4K UI has not been established. Rendering a smaller
canvas and scaling it with the display engine is a useful next performance test;
that path was not tested here.

## Memory and recovery

An eight-second sample during the first 4K60 run showed `MemAvailable: 142420 kB`
(**139.1 MiB**), process `VmRSS`/`VmHWM: 45412 kB` (**44.3 MiB**) and
`CmaFree: 15972 kB`. Process RSS excludes some GPU/display allocations, and this
was one sample, not measured peak usage. Lima's `GPUActive` counters remained
zero, so they are not useful activity evidence here.

Afterward, available memory returned to about **210.2 MiB** and the CRTC returned
to 720p60. No GPU fault/reset/timeout, display IOMMU BUS_ERROR or OOM was observed
in the collected diagnostics. The earlier UP topology, dummy-regulator and
optional HDMI-PHY IRQ warnings remain. A mode-validation warning occurred after
4K60 restoration, but the visible console recovered.

**4K60 is not a working display mode on this tested setup.** `KMS_MODESET_PASS`
only means the ioctl was accepted; rendering `status=PASS` covers GPU/software
execution. Neither establishes a usable monitor picture. The cause of the 4K60
link failure was not isolated.

## Reproduction and artifacts

See [README.md](../../../os/README.md) for building and RAM booting. From the USB shell:

```sh
gpu-test --list
gpu-test --offscreen --mode 1280x720@60 --seconds 5 --fps 30
gpu-test --mode 1280x720@60 --seconds 25 --fps 30
gpu-test --offscreen --mode 3840x2160@30 --seconds 5 --fps 10
gpu-test --mode 3840x2160@30 --seconds 25 --fps 10
```

The test restores the previous CRTC on normal completion and handled
SIGINT/SIGTERM, and limits each invocation to 60 seconds. The kernel still starts
at 720p. Avoid 4K60 for normal use until its output failure is fixed.

The GPU-test zImage is **8,289,872 bytes** (7.91 MiB), including the compressed
graphics userspace; the exported uncompressed initramfs is **17,222,144 bytes**.
zImage SHA-256: `1bba1df968abc31617a10f667dad96fb1747b13b9360fb639486b0fd851c96f5`.
The DTB and corrected handoff are unchanged. NEON is enabled in this kernel.
This is not a complete flash image. The original minimal working baseline is
preserved in `output/working-720p/`.

The active [manifest](../../../os/output/manifest.json) pins the image, test ELF, build options
and [runtime library manifest](../../../os/output/gpu-runtime.json). Mesa's source SHA-256 was
checked against its [release notes](https://docs.mesa3d.org/relnotes/26.2.6.html).

## Evidence

- [Lima-only build options](../../../os/output/mesa-buildoptions.json).
- [First render, pixel checks and hardware interrupt counters](../../../os/logs/usb-console-20261111T121917.150562Z.log).
- [Completed visible 720p test](../../../os/logs/usb-console-20261111T122101.778469Z.log).
- [4K offscreen test](../../../os/logs/usb-console-20261111T122145.025156Z.log).
- [Visible 4K30 test](../../../os/logs/usb-console-20261111T122218.920260Z.log).
- [First 4K60 run and memory sample](../../../os/logs/usb-console-20261111T122356.996070Z.log).
- [Repeated 4K60 failure and restored CRTC](../../../os/logs/usb-console-20261111T122543.799400Z.log).
- [Post-test memory, clocks and fault check](../../../os/logs/usb-console-20261111T122522.114000Z.log).

The first visible-test capture exited early when terminal wrapping made an
echoed completion marker resemble an executed result. The remaining output was
captured at the next session's start, and the test was repeated. Capture now
uses framed per-session nonces. Three regression tests cover wrapped echo,
stale-session output and reversed markers. The table uses the completed repeat.
QEMU boot/shell testing and five pre-USB artifact-guard tests also passed; QEMU
does not validate this hardware GPU or HDMI link.
