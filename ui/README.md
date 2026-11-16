# miracast-ui: custom GLES2 launcher for the RK3228A dongle

A 10-foot home screen in the style of [Plasma Bigscreen](https://plasma-bigscreen.org/), with its own Rust
renderer for Mali-400 (Lima, OpenGL ES 2.0). Screenshots: [../docs/SHOWCASE.md](../docs/SHOWCASE.md).

Measurements and analysis are in [PERF-FINDINGS.md](PERF-FINDINGS.md).

The principle: **do expensive visual work once, then let the GPU only copy texels, and only where something
changed.**

## How it renders

| Piece | What it does | File |
| --- | --- | --- |
| Artwork | Drawn once at the active scale with tiny-skia: rounded-card 9-slice, pre-blurred card shadow, focus ring, line icons, gradient app badges, and a procedural fallback wallpaper (960×540, upscaled by the GPU). The real wallpaper is an asset, see [Wallpaper](#wallpaper) | [src/art.rs](src/art.rs), [src/wall.rs](src/wall.rs) |
| Atlas | One RGBA texture (1024² at 1080p, 2048² at 4K). Entries are uploaded once with no CPU copy, with 1-texel transparent borders. Skyline (bottom-left) packing: 52 % of the rows hold everything at 1080p (shelves had run out of rows at ~45 % texel use) | [src/atlas.rs](src/atlas.rs) |
| Text | Noto Sans Light/Regular/Medium. ab_glyph rasterises glyphs lazily into the atlas; pens snap to whole pixels. The firmware has Latin subsets; see [Fonts](#fonts) | [src/text.rs](src/text.rs) |
| Shader | One program: `texture2D(tex, uv) * colour`, a varying-direct texture coordinate, mediump. About 2 draw calls per frame (wallpaper, atlas) | [src/gpu.rs](src/gpu.rs) |
| Damage | Each frame is a list of items (stable id + quads), diffed against the previous list. The damage, plus that of the last `EGL_BUFFER_AGE` frames, becomes a 32 px-aligned scissor; only items intersecting it are drawn | [src/main.rs](src/main.rs) |
| Clear | Full frames `glClear`. Partial frames use a *scissored* clear, which with the project's Lima patch skips the tile reload (below) | [src/gpu.rs](src/gpu.rs) |
| KMS | Named mode (`UI_OUTPUT`) with a forced connector probe, legacy modeset, pipelined page flips (render N+1 while N is queued, as in `gpu-test`), and the console CRTC restored on exit | [src/kms.rs](src/kms.rs) |
| Home | Layout measured from `Bigscreen_plasma.png` in 1920×1080 logical units. Focus scales the card 1.05 and fades in the ring (150 ms); rows scroll horizontally (250 ms), sections vertically (300 ms); content fades under the top bar | [src/home.rs](src/home.rs) |
| Idle | Nothing renders without a change; the clock wakes once a minute. After the first frame, idle time pre-renders the remaining badges and glyphs, one item per tick, so navigation never rasterises | |
| Input | Dev FIFO `/run/ui-keys` (WASD, vi keys, arrows; `e` Enter, `b` Back, `m` Home; `p` = screenshot, `q` = quit) | [src/input.rs](src/input.rs) |
| Top bar | Up from the first row focuses the icons: a Breeze-blue disc and a tooltip. Five icons: Settings, the speaker (shows the volume as it is; OK opens the volume pop-up), Wi-Fi (Settings › Wi-Fi), the phone remote's state (Settings › Wi-Fi, where pairing is) and Power (sleep: the video output goes off until a key). See [Top bar, volume and Recent](#top-bar-volume-pop-up-and-the-recent-row) | [src/home.rs](src/home.rs), [src/volpop.rs](src/volpop.rs) |
| Settings | Categories left, frosted panel right, picker and dialogs; see [Settings](#settings) | [src/setview.rs](src/setview.rs), [src/settings.rs](src/settings.rs) |
| Apps | Client of appd (`/run/appd/ui.sock`). The Applications row lists the store's and installed apps, the Recent row the ones opened last; Enter installs (consent dialog) and launches them. The screen is handed over by dropping DRM master and taken back with a full redraw. While an app is in front, keys go to it, except Home. Without appd the built-in placeholder cards stay. See [../apps/README.md](../apps/README.md) | [src/apps.rs](src/apps.rs) |

### Lima patch: [../os/patches/mesa-lima-scissored-clear.patch](../os/patches/mesa-lima-scissored-clear.patch)

**The problem:** Lima reloads the old framebuffer contents into its tile buffer for every partially
redrawn frame. Measured, that is 57–85 % extra GPU time.

**What the patch adds:**

- Lima now advertises `clear_scissored`.
- A tile-aligned scissored clear makes the job process only those tiles, starting from the clear value,
  with no reload.
- If a later draw in that job reaches outside the cleared tiles, the job is submitted first, so the next
  job reloads as usual. This keeps the patch correct for any GL client.
- An unaligned scissored clear falls back to a blitter quad.

**How it ships:**

- `build-mesa.sh` applies the patch idempotently.
- Until the next initramfs build, `package-ui.py` ships the rebuilt `libgallium` in `/opt/ui/lib`
  (`LD_LIBRARY_PATH`), so no reboot is needed.

**Effect:** a card-row repaint went from 11.9 to 8.3 ms at 1080p, and from 59.9 to 38.5 ms at 4K.

## Results

The runs below were measured on 2026-11-11 on the test TV, with the perf kernel and DDR600.

**Thermal state during the runs:** the SoC sat at 85–97 °C the whole session, so the GPU was mostly
throttled to 200–300 MHz. These are hot, sustained numbers.

| | 1080p60 (XRGB8888) | 4k30 (RGB565, dithered) |
| --- | --- | --- |
| Navigation, frame interval while animating | **16.7–17.1 ms, 0–2 % late** (60 fps) | 33–37 ms on row moves (27–30 fps); about 50 ms on vertical scrolls |
| GPU per animated frame | about 9 ms at 30–45 % repaint (200–300 MHz) | about 26 ms for a row band (300 MHz) |
| Idle | 0 fps, 0 % CPU, GPU at 200 MHz | same |
| Cold start to first frame | 1.45 s | 2.9 s |
| RSS | 15 MiB | 15.5 MiB |
| GPU textures | 4 MiB atlas + 2 MiB wallpaper | 16 MiB atlas (42 % used after prefetch) + 2 MiB |
| Binary | 1.0 MB (needs only libc/libm/libgcc_s/libEGL/libgbm) | |

**GPU cost per full static frame** (`UI_GPU_BENCH=1`, ms):

| Variant | 1080p @400 | 4K 8888 | 4K 565 @300–400 |
| --- | ---: | ---: | ---: |
| Clear only (framebuffer writeback) | 5.0 | 20.7 | 15.2 |
| + wallpaper | 7.6 | 32.3 | 22.9 |
| Clear + wallpaper + UI | 11.3 | 49.4 | 36.6 |
| Reload instead of clear | 19.1 | 92.9 | 73.2 |
| Card-row band, reload | 11.9 | 59.9 | 38.8 |
| Card-row band, scissored clear (patched) | 8.3 | 38.5 | 25.7 |

At 4K the floor is memory bandwidth: just writing a 4K XRGB frame takes 21 ms. Hence:

- RGB565 is the 4K default.
- Partial redraw is essential at 4K.
- A full-screen change (the vertical scroll) can't hold 30 fps when hot.

**Still to do for 4k30:**

- Put the wallpaper on its own VOP plane, so the GPU never draws it.
- Shorten or simplify the vertical scroll at 4K.

## Settings

Opened from the top bar (Settings, Wi-Fi, Device icons) or the home screen's Settings row (one card per
category). The home rows fade and slide away, and the wallpaper recedes to its blurred layer.
Up and Down move. Right or Enter goes into a category and activates a row. Left or Back goes back out, and Home closes Settings.

| Category | Settings (all take effect at once, saved to `/data/ui/settings`) |
| --- | --- |
| Wi-Fi | From meow-netd's `/run/netd/state`: status, signal, IP, the setup network, *Set up with phone*, *Pair a phone*, *Unpair all phones*, *Forget this network*. Without an adapter: why (e.g. the port is in device mode). The "Set up with your phone" prompt (QR codes) is `src/onboard.rs`; see [../wifi/README.md](../wifi/README.md) |
| Network | Live USB Ethernet status: link, IP, gateway, DNS (from `/run/net.env`), MAC, host name |
| Display | **Resolution** (1080p60 / 4K30 / 720p60, see below) · **Background depth effect** (on/off) · **Dim the screen when idle** (never, 1/5/15/30 min) |
| Wallpaper | A grid of the store's wallpapers; OK uses the focused one (see "Wallpaper gallery" below) |
| Date & time | **Time zone** (19 POSIX TZ strings, no zoneinfo needed; `tzset` live) · **24-hour clock** · time source (NTP synced or not) |
| Apps | Installed apps from appd (`APPINFO`); OK opens an app's page: **Run in background** (switch, with the service's state), its permissions (what it may do, standard or needing consent, the app's reason, hosts, allowed or not) and **Uninstall** (confirmation starts on Cancel). Below the list: free app storage and **Background apps**, every app that may run in the background with its switch |
| Developer | **Network debugging** (Dropbear SSH, keys enrolled with the TV's code via `tools/meowdbg.py pair`) · **Performance overlay** (once a second: fps, % late, GPU MHz, CPU %, °C) · **Restart the launcher** |
| About | Processor, GPU clock, memory, `/data` space, temperature, kernel, uptime (resampled every 2 s while shown) · **Factory reset** (a dialog that starts on Cancel, then "Erasing…" while `/bin/factory-reset` erases `/data` and restarts the launcher) |

**The button on the dongle** ([src/reset.rs](src/reset.rs), `Input::Button` in [src/input.rs](src/input.rs)). `meow-buttond` reports its steps as the bytes `1`–`6` on the key FIFO; the launcher draws them and decides nothing: a short press shows or hides the pairing screen (an app in front goes Home first, a sleeping screen wakes), a hold shows the factory-reset warning with its countdown, then "let go now", then "Erasing…". The remote's keys do nothing while those screens are up. The pairing screen asked for this way stays until one more phone is paired, also while another phone is connected. Rules and reasons: [../os/README.md](../os/README.md#the-button-pair-a-phone-factory-reset).

**Resolution change.** Settings are saved with `confirm=<previous output>`, the screen fades to black, the
console is blanked (no text flash) and the UI exits with code 3. `ui-session` (and `run.sh`) start it again
at once in the saved output. The new process reopens Display with a **Keep this resolution?** dialog. Its
15 s countdown starts once the TV can show a picture (`UI_TV_SYNC_MS` after a modeset). On timeout or Revert it
switches back the same way. If kms.rs had to fall back because the TV lacks the mode, it says so and keeps the
fallback. If the UI crashes while a mode is unconfirmed, `ui-session` reverts the file itself. At boot,
`ui-session` waits up to 2.5 s for `/data`, so the saved output applies from the first frame.

**Dimming and sleep.** Dimming is one black layer over everything (`dim` tween; idle 82 %). The first
key only wakes the screen. When the fade ends, the frame is static (0 fps).

Sleep (the top bar's Power button, or by itself 10 minutes after the idle dim, `SLEEP_AFTER_DIM`) fades to
black and then **switches the video output off** (`Kms::set_output`, the connector's DPMS property): the
CRTC goes inactive, the VOP stops and runtime-suspends, the HDMI PHY and its PLL power down, and the TV
reports "no signal". An output that is on is most of this board's idle heat: the home screen idles 10–12 °C
above the sleeping board (measured 15 Nov, [../os/README.md](../os/README.md#where-the-display-heat-comes-from)).
Mode, planes and framebuffers stay as they are. Nothing is drawn while the output is off (the loop keeps
`need_build` for the wake-up frame and sleeps in `poll`).

Waking: any key, a notice (meow-thermald's, a paired phone), or an app coming to the front (a cast that
starts by itself). The output goes back on first; `Kms::release` and `restore` also make sure of it, since an
app only flips planes and would draw into a dead output. After a key or a notice the picture stays black
for `WAKE_SYNC` (3.8 s, what the test TV needs to lock on again) and then fades in; a notice is shown from
that moment. Log lines: `UI_SLEEP output off`, `UI_SLEEP wake: output on`, `UI_SLEEP idle`.

**Cost (13–14 Nov, 1080p60, planes, `settingsbench.sh 1080p60 3 220`).** The tour covers the top bar,
opening and closing Settings, every category, row moves, the scrolling time-zone picker and switches, with a
key every 220 ms (continuous animation):

| Run | Animated frames | Late (> 1.5 vsync) | Mean interval | Worst |
| --- | ---: | ---: | ---: | ---: |
| Final build | 1842 | 3 (0.2 %) | 16.40 ms | 40.5 ms |
| Before the atlas packer | 1849 | 1 (0.1 %) | 16.28 ms | 32.1 ms |

- **Memory:** high-water RSS 29.2 MiB, the same as before Settings (28.9).
- **Glyphs:** 392 at start (was 136). Home ready went from about 1.0 to 1.2 s; the boot intro hides it.
- **4K30, one round:** 12 % late, about 27 fps on the full-screen open and category changes. This is the
  known 4K bandwidth floor.

**What keeps it cheap:**
- All art and every static string is rasterised on the start-up workers (`setview::render_art`,
  `setview::text_runs`), in the home screen's font sizes.
- Focus moves repaint only the rows they touch, and the countdown repaints only its own line.
- The panel is one translucent quad with no shadow: under a translucent panel the shadow showed as an inner frame.
- System facts are a few small file reads every 2 s, and only while Settings is open.
- Settings are saved by a nice-10 thread, because a JFFS2 write can wait for a NOR erase.

**Bugs found along the way:**
- The Close command was applied after the whole key burst, so keys right after Back went to the closing
  view. Close is now applied at once.
- Pre-existing: the home screen waited for the built-in wallpaper texture, which is dropped unseen when the
  asset wallpaper is processed first. 4K never drew a frame. Either wallpaper now counts.

**Heat (15 Nov).** The launcher reads `/run/thermal/state` (meow-thermald,
[../os/README.md](../os/README.md#thermal-protection)) with the network state, every 700 ms:
- A new `notice_seq` shows its `notice` as the normal notice pill ("Too hot: pausing for a minute to cool
  down", "Cooled down: back to normal") and wakes a dimmed or sleeping screen. While an app has the screen
  the notice waits; the daemon closes the app first. Log: `UI_HEAT notice …`.
- `level` 2 or more (100 °C): no transition boost (the CPU/GPU minimum clocks are left alone).
- In a cooldown the daemon stops the launcher (`ui-session stop`) and starts it again afterwards with
  `UI_SESSION_INTRO=entrance`.

## Top bar, volume pop-up and the Recent row

**Top bar (14 Nov).** Search, All apps and Edit home are gone. What is left says something:

| Icon | Shows | OK |
| --- | --- | --- |
| Speaker | The system volume: crossed out when muted (or 0), one wave up to 33, two up to 66, three above. Tooltip "Volume 70" / "Muted". Follows appd's `VOLUME` live | Opens the volume pop-up for the D-pad |
| Phone | The phone remote ("pilot"), from meow-netd's `/run/netd/state`: crossed out and dimmed when no phone is paired (`pilots=0`), plain when paired but not connected, with a green dot when a paired phone is connected now (`pilot_connected=1`, or `online` > 0 from a daemon that does not publish the flag) | Settings › Wi-Fi (pair, unpair) |

**Volume pop-up** ([src/volpop.rs](src/volpop.rs)). One pop-up for the whole system: under
the top bar at the right margin, speaker + level bar + number. It appears whenever anything
sets the volume (the phone remote, `appctl`, a cast sender, Settings, the top bar), holds 2 s
and fades (120 ms in with an 8 px rise, 280 ms out). The pixels come from the app SDK
(`meow_app::osd`, [../apps/sdk/src/osd.rs](../apps/sdk/src/osd.rs)), which apps use to show
the same pop-up while they have the screen; the speaker in the top bar is drawn from the same
shapes.
- Opened from the speaker it gets the focus ring and the keys: Left/Down −5, Right/Up +5,
  OK mutes, Back closes; 3.5 s after the last key it lets go by itself.
- appd owns the volume: a key only sends `VOLUME +5`, and the pop-up shows what appd answers.
- Cost: the renderer (font, panel, four speaker masks) is prepared on a start-up thread; a
  change redraws the bar and the number into a 496×152 sprite (0.3 ms on the build host) and
  uploads it; the pop-up is one extra textured quad (its own texture, one extra draw call)
  that repaints only its own rectangle. While it holds, nothing renders.

**Recent row.** The apps opened last, most recent first, from appd (`RECENTS`, kept in
`/data/appd/recents`). Without any the row is not there. A Recent card shares its badge with
the app's card in Applications (one atlas entry).
- When the row appears or goes, the focus stays on the card it was on: the rows shift under
  it and nothing moves on screen. Only an untouched home screen (boot) moves the focus to a
  Recent row that arrives, so the home screen opens on it.
- Inside the row the focus follows its app: back from an app opened from the third card, that
  app is first and focused, and the return transition lands on it.

**Opened from outside.** When appd announces an app coming to the front that the user did
not start here (a cast opened it through its background service), the launcher wakes a
dimmed or sleeping screen, closes Settings and the pop-up at once and plays the launch
transition from the app's card.

## Fonts

The firmware carries Latin subsets of the three Noto Sans faces (50 KB each instead of 500 KB;
the initramfs shrinks by ~600 KB after compression). They are made by
[assets/fonts/subset.py](assets/fonts/subset.py), a stdlib-only subsetter, and checked against
the full faces with ab_glyph: the 369 kept characters have the same glyph ids, metrics and
coverage.

- **Coverage:** Basic Latin, Latin-1, Latin Extended-A, Romanian, dashes, quotes, bullets,
  currency and arrows, which includes every character the launcher draws itself.
- **Glyph ids are kept** ("retain gids"), so the full face can replace a subset while the UI
  runs. Cached glyphs and layouts stay valid.
- **Missing glyphs:** text with a character outside the subset (an SSID, a phone or app name in
  Cyrillic, Greek, Vietnamese, ...) marks its face. The launcher then asks appd for the full face
  (store asset `fonts/NotoSans-<face>.ttf`), swaps it in (`Text::replace_font`) and redraws.
  Until then the character shows the .notdef box. Offline, it waits for the network like the
  wallpaper does.
- **Dropped, because ab_glyph never reads them:** hinting (fpgm/prep/cvt and per-glyph
  instructions), GPOS/GSUB/GDEF (ab_glyph kerns from the legacy `kern` table only, which Noto
  Sans does not have, so the subsets render exactly like the full faces did), glyph names.
- **Regenerate** with `python3 assets/fonts/subset.py` (writes `assets/fonts/latin/` +
  `SHA256SUMS`). `package-ui.py` ships the subsets; `../apps/build-apps.sh` puts the full faces
  in the store.

## Text that fits, lists that clip

Text is drawn at a position, with no box, so every string that is not a fixed short label has to be
given its room. Three shared pieces do that:

| Piece | Use |
| --- | --- |
| `Text::fit` / `Text::wrap` ([src/text.rs](src/text.rs)) | One line cut with an ellipsis, or up to N lines broken at spaces (the last one cut; a word wider than a line is broken inside) |
| `text::Fits` | The cache in front of them, one per view: `fits.line(text, face, px, s, width)` / `fits.lines(.., max)`. A frame pays one hash of the string, with no allocation and no measuring. The width must not animate. Emptied when a font is replaced and when it holds 512 entries |
| `home::clip(verts, start, rect)` | Cuts an item's quads (and their texture coordinates) to a window before `Frame::end`: for anything that scrolls inside a box |

**Where:**
- **Home cards:** the title on one line, the description on up to two, both as wide as the card.
- **Settings rows:** the label and the line under it end 32 px before the row's value or switch; a
  value has at most half the row (an information row's value: the row minus a 300 px label column).
  Headings, captions, dialogs (the body wraps to up to three lines and the panel grows), the install
  consent (lead and reasons wrap to two lines), notices, the launch splash and the phone set-up
  screen go through `Fits` too.
- **Pickers:** the box is as wide as its widest label and hint side by side need (480 px to the
  panel's width; only beyond that do they get an ellipsis), measured once when it opens
  (`PickLayout`). It opens below the panel's heading, the panel behind it dims, its options are
  clipped to the list's window, and a list longer than 8 options shows a scroll thumb.
- **Scrolling rows and the wallpaper grid** are clipped to their window. A page longer than the panel shows a thumb.

Unit tests: `text::tests` (with the firmware's fonts) and `home::tests::clip_…`.

## Wallpaper

The home wallpaper is an asset from the meow store: the one chosen in Settings > Wallpaper
(`wallpaper=` in the settings file; `wallpapers/default.jpg`, KDE's "Sub-Arctic", until the user
chooses; `UI_WALLPAPER` overrides it for one run). All are 960×540. It mimics the reference screenshot:
the image is clearly visible, dimmed, slightly soft, and darker behind the cards.

**How it arrives (14 Nov: downloaded at every boot, never stored on flash):**
- On connecting to appd the launcher sends `ASSET <name>`.
- **Preview first.** When the file is not in RAM yet (every boot, and while offline), appd
  answers at once with the preview from the signed index: a 32×18 JPEG of the same picture,
  ~1 KB. `wall::preview` upscales it to 320×180 and blurs it, using the same shading, into both
  layers. That gives an out-of-focus version of the real wallpaper from the first frame, with
  its colours and light where they will be, offline too.
- **Then the file.** appd downloads it from the store as soon as the network is up, checks it
  against the signed index and sends an open file. [src/wall.rs](src/wall.rs) decodes it
  (zune-jpeg) on a nice-10 worker thread into the two textures. The blur passes are row-sliding
  windows on 4 threads. The server's file is 960×540, the sharp layer's size, so there is no
  downscale: the 1920×1080 file of 13 Nov took 0.9 s, 0.4 s of it decoding 4× the pixels.
- **Focus pull.** The result is uploaded in 72-row strips and cross-faded over 900 ms (eased)
  from the preview layer at the current depth.
- **Fallback.** A device that has never reached the store has no preview: the built-in
  procedural wallpaper is shown, and the downloaded one cross-fades over it in 600 ms.
  `UI_WALLPAPER=procedural` keeps the procedural one.

**The two textures:**

| Layer | Texture | Shown | Look |
| --- | --- | --- | --- |
| sharp | 960×540 (2× GPU upscale), radius-1 blur | top row focused | ~60 % brightness at the top easing to ~36 % behind the cards, dithered |
| soft | 480×270, 3× radius-3 blur (~16 px at 1080p) | lower rows | 46 % → 28 %, "recedes" like Bigscreen when scrolled |

**Cost decisions (measured 13 Nov, vertical scrolls, 1080p60):**
- **Full-resolution sharp layer:** an 8 MB texture made scroll frames ~28 ms (texel bandwidth).
  Half resolution looks the same once upscaled.
- **Cross-fading the depth during the scroll:** the wallpaper changes over the whole screen, so every
  frame is a full repaint with two texture fetches, ~20 ms GPU even at 400 MHz, and 25–35 % late
  frames. Neither a one-pass `mix()` shader (now used for any two-layer wallpaper, `FS_MIX` in
  gpu.rs) nor the boost fixed that alone.
- **First fix (GPU-composited fallback, `UI_PLANES=0`):** the depth dissolve (300 ms) starts when
  the scroll lands, so the moving frames draw one layer: 4–12 % late frames.
- **Fix now in use: display planes** (next section). The dissolve runs *with* the scroll at 0.2–1.4 %
  late frames.
- **Input boost:** the transition boost (CPU/GPU minimum clock to maximum) also runs while the
  home screen animates, so scrolls do not start at the 200 MHz idle GPU clock. It is restored when idle.
- **Memory:** GPU textures add about 2.5 MiB (the procedural 2 MiB texture is freed after the fade).

### Wallpaper gallery (Settings > Wallpaper)

The page shows every wallpaper of the store as a 4-column grid of thumbnails
([src/gallery.rs](src/gallery.rs), `setview::build_walls`). The D-pad moves in two dimensions,
OK uses the focused wallpaper, Left from the first column (or Back) returns to the categories.
A check badge marks the one in use, and the caption names the focused one and its author.

**Where the grid comes from.** Two signed assets, asked from appd when Settings opens (any
category), so the page is usually complete before the user reaches it:

| Asset | Size | Use |
| --- | ---: | --- |
| `wallpapers/catalog.tsv` | 0.9 KB | The list: asset, title, author and mean colour per wallpaper, plus the thumbnail sheet's name and geometry. Parsed on the render thread |
| `wallpapers/thumbs.jpg` | 133 KB | All thumbnails (264×148 each) in one JPEG: one download, one decode on a nice-10 worker (273 ms), uploaded in 64-row strips |

- **Before the sheet is there**, each tile is a rounded card in its wallpaper's mean colour, and
  the thumbnails fade in over those (260 ms). Opening the page from the home screen's Wallpaper
  card before the catalog has arrived puts the focus into the grid as soon as it does.
- **One extra texture, one extra draw call.** The sheet does not fit the UI atlas (1.9 MB of
  texels), so it is a texture of its own: each cell has its rounded corners in the alpha and a
  transparent 1-texel frame, and the tiles are plain quads. Frame items now carry an optional
  texture, and main.rs draws consecutive items of one texture as one batch: atlas, thumbnails,
  atlas while the page is shown, a single atlas batch otherwise. At 1080p the tiles map texels
  1:1.
- **No catalog** (offline, store unreachable): the page says so. The wallpaper in use stays.

**Choosing one** saves the setting (nice-10 thread) and asks appd for the asset:

1. The preview from the signed index arrives at once (62 ms to process) and cross-fades in over
   the current wallpaper in 600 ms.
2. The file is downloaded (56–240 KB, kept in appd's RAM cache until reboot), decoded and shaded
   in ~0.3 s, and pulls into focus over 900 ms. It waits for the first fade to end, so the two
   never cut into each other. A wallpaper already in the cache skips the preview.
3. Results for a wallpaper the user has moved on from are dropped (`wall_gen`).

At the next boot the launcher asks for the chosen asset, and shows its blurred preview offline.
If the asset has left the store, the default one is used.

**Measured on the dongle (14 Nov, `/opt` build, store on Cloudflare):**

| Run | Animated frames | Late (> 1.5 vsync) | Mean interval | Worst |
| --- | ---: | ---: | ---: | ---: |
| `settingsbench.sh 1080p60 3 220`, now with the grid tour and two wallpaper changes per round | 2380 | 7 (0.3 %) | 16.35 ms | 42.6 ms |

- **Memory:** high-water RSS 22.9 MiB in that run; the sheet texture adds 1.9 MiB of GPU memory
  once Settings has been opened.
- **4K30 (no display planes):** a wallpaper cross-fade repaints the whole screen under the
  Settings view at 100–133 ms per frame, like the other full-screen transitions there. A
  wallpaper change by the user therefore uses 240 ms fades without planes (`WALL_QUICK_MS`):
  two or three slow frames instead of 1.5 s of them.
- **Screenshots:** `shots/wall-1-open.png`, `wall-2-applied.png`, `wall-3-home.png`, and
  `wall-noplanes.png` (GPU-composited path, `UI_PLANES=0`). A 4K screenshot needs more RAM than
  the dongle has free (the launcher was OOM-killed): check 4K drawing with `UI_PLANES=0` at 1080p.

**Unit tests** (catalog parsing, cell placement, and the store's real catalog and sheet) run on
the builder's own architecture, with empty stand-ins for the GPU libraries:

```sh
docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build -w /fw/ui \
  meowcast-builder:gpu sh -c 'mkdir -p /tmp/stub && echo "" | cc -shared -x c - -o /tmp/stub/libEGL.so \
  && cp /tmp/stub/libEGL.so /tmp/stub/libgbm.so && CARGO_TARGET_DIR=/build/ui-target-host \
  RUSTFLAGS="-L native=/tmp/stub -C link-arg=-Wl,--unresolved-symbols=ignore-all" LD_LIBRARY_PATH=/tmp/stub \
  cargo test --locked --offline --config .cargo/vendor.toml --target $(rustc -vV | sed -n "s/^host: //p")'
```

## Display planes (UI_PLANES, default on up to 1080p)

The VOP composes the screen from two hardware windows, so the GPU never draws the wallpaper at
full size:

| VOP window | DRM plane | Content | Size |
| --- | --- | --- | --- |
| win0 | primary | wallpaper surface: the layers above, blended at half resolution | 960×540, scaled ×2 by the VOP |
| win1 | "cursor" (a full window with scaler and per-pixel alpha) | the UI, cleared to transparent, premultiplied ARGB8888 | 1920×1080 |

**How it works:**
- **Presenting:** each frame is one non-blocking atomic commit (kms.rs `present_planes`). It carries
  the new UI frame, the new wallpaper frame, or both.
- **A wallpaper change** (the depth dissolve, the intro fade, the asset cross-fade) re-renders only
  the quarter-size wallpaper surface. It is a second EGL window surface on its own GBM surface, with
  the same config. It never forces a full-screen UI repaint, so the dissolve runs during the
  vertical scroll (`UI_SCROLL_BLUR`, default on with planes; 400 ms).
- **Blending:** the VOP blends win1 premultiplied (rockchip_drm_vop.c, `has_alpha && win_index > 0`),
  which is exactly how the UI renders.
- **App handover:** before an app gets the display, the overlay is switched off and the last UI frame
  (the opaque launch tint) moves onto the primary plane. The app's `SetCrtc` then owns a plain
  primary plane. On return, the next commit restores both planes. If the app left another mode,
  the UI uses `SetCrtc` once.
- **Screenshots** in plane mode compose UI over the wallpaper on the CPU, the way the VOP does.
- **Not at 4K:** an ARGB8888 4K overlay would double the RGB565 path's framebuffer bandwidth, so
  4k30 keeps the GPU-composited path.

**Results:** 13 Nov, 1080p60, `scrollbench.sh` (vertical: Down/Up every 800 ms; horizontal: row
tour every 170 ms). Each cell is the % of late frames, two alternating runs.

| Boot | Variant | Vertical | Horizontal | GPU ms / vertical frame |
| --- | --- | --- | --- | --- |
| flash (DDR 300 MHz) | GPU wallpaper, dissolve after scroll (old default) | 9.9 / 18.2 | 5.2 / 8.3 | 18.5 |
| flash (DDR 300 MHz) | GPU wallpaper, dissolve during scroll | 22.7 / 42.1 | – | 24.1 |
| flash (DDR 300 MHz) | **planes**, dissolve during scroll | **0.6 / 3.3** | **0.7 / 1.5** | **12.4** |
| RAM (DDR 600 MHz), final kernel | GPU wallpaper, dissolve after scroll | 7.2 / 9.2 | 5.5 / 4.6 | 16.2 |
| RAM (DDR 600 MHz), final kernel | GPU wallpaper, dissolve during scroll | 18.4 / 21.7 | – | 20.4 |
| RAM (DDR 600 MHz), final kernel | **planes**, dissolve during scroll | **1.4 / 1.2** | **0.2 / 0.5** | **9.4** |

`UI_GPU_BENCH` static frame costs, at 400 MHz, ms. The "UI" cases are what plane mode draws.

| Frame | DDR 300 | DDR 600 |
| --- | --- | --- |
| clear (= framebuffer writeback) | 9.7 | 4.2 |
| clear + UI | 12.1 | 6.3 |
| clear + wallpaper + UI | 15.5 | 10.5 |
| clear + dissolve mix + UI | 24.8 | 15.7 |
| wallpaper surface, dissolve mix (960×540) | 8.0 (300 MHz) | 5.7 (300 MHz) |

The GPU is bandwidth-bound: a job's fixed cost is 0.4 ms, and the frame cost follows the DDR
clock. See [PERF-FINDINGS.md](PERF-FINDINGS.md#11-13-nov-display-planes-ddr-clock-scheduling).

**Scheduling under CPU load:** the UI runs at `UI_NICE=-10` (default). The kernel gives Lima's job
hand-offs and the atomic commit tail high-priority workqueues (os/build.sh). With 4 busy
threads, vertical scrolls went from 12–17 % to 3–4 % late frames.

**Benchmarks:**
- `/opt/ui/scrollbench.sh [SECS] [REPS]` runs every variant alternately, so heat affects each alike.
  `ONLY=load` runs only the CPU-load variants; `PROFILE=0` skips the profiled and static runs.
- Each run ends with a `UI_BENCH_SUMMARY` line.
- `UI_STALL` lines break down any animated frame interval over 60 ms.

## App launch transition

The launch and return animation is in [src/launch.rs](src/launch.rs) (the timeline) and `Home::build_launch` (the drawing).

**Opening (420 ms, ease-out quart):**
- The focused card grows into a full-screen panel in the app's tint. At t = 0 it covers the card
  exactly, so the start is seamless.
- The app icon glides to the centre, from a 2× copy rendered in idle time while the card is focused
  (`prewarm_launch`). The name rises in.
- A soft glow comes in at the end of the zoom.
- `LAUNCH` goes to appd at once, so the download and sandbox start overlap the zoom.

**Waiting:** after 300 ms the screen shows "Downloading 45%" with a progress bar, or "Starting…"
with a 10-dot spinner. Each spinner frame repaints only the spinner's own box.

**Hand-over:** appd's `RELEASE_DISPLAY` is deferred until the zoom ends.
- Icon, glow and name fade out (150 ms) to the plain tint.
- Once that frame is on screen, the launcher drops DRM master.
- Apps get the tint as `MEOW_SPLASH` and can fade in from it (the inspector does, in 260 ms). So
  there is no black frame and no hard cut.

**Return (360 ms, ease-in-out cubic):**
- The launcher takes master back with a page flip over the app's framebuffer; `SetCrtc` is only the
  fallback, because it costs a frame.
- The plain panel shrinks back onto the card, re-measured in case the row scrolled.
- The icon and name fade in as it shrinks.
- A failed download or start, or a cancelled install, reverses from wherever the zoom is, and appd's
  notice is shown.

**Why it stays at 60 fps on the Mali-400:**

| Measure | Effect |
| --- | --- |
| Opaque panel as an occluder | Home items fully behind it are not drawn; the wallpaper is drawn only as up to 4 strips around it, so overdraw stays about 1 pass even when the panel covers most of the screen |
| Glow only in the last 20 % of the zoom, and not while closing | A 1000 px blended quad would add half a screen pass to every zoom frame. Closing without it also avoids 8-bit banding at low alpha |
| Splash art pre-rendered in idle time | The 2× icon took 128 ms on the CPU; nothing is rasterised on Enter |
| Transition boost | While the panel moves or fades, the CPU and GPU *minimum* clocks are raised to their maximum, then restored. Maxima and thermal caps are unchanged. Without it, a cold launch at 408 MHz / 200 MHz dropped 2 frames |

**Measured (13 Nov, 1080p60, 4 cold cycles with 10 s idle before each):**
- 206 of 208 transition frames on time.
- The two outliers were 25.4 and 26.8 ms, still under a missed vsync (33 ms).
- Opening is 26 frames, closing 22.
- RSS rose 0.4 MiB.

**Dev knobs:** `UI_LAUNCH_TRACE=1` logs every transition frame. `UI_LAUNCH phase=…` lines give
per-phase frame counts.

## Memory

Measured on the dongle at 1080p60 with planes (256 MB board, `MemTotal` 243.8 MB). With the UI stopped
170 MB is available; the home screen leaves 118 MB. What the UI holds:

| | MB | |
|---|---|---|
| UI scanout buffers, 1920x1080 ARGB x3 | 25.1 | GBM allocates the 2nd and 3rd on first use and never frees them |
| Wallpaper plane buffers, 960x540 x3 | 6.3 | |
| Lima (textures, per-context buffers) | 11-14 | Unevictable minus framebuffers; Lima's BO cache is only 1.2 MB of it (`LIMA_DEBUG=nobocache`) |
| Process heap and stacks | 9.4 | 7.4 after `malloc_trim` |

The kernel console's framebuffer is separate: 8.1 MB at 32 bpp, never freed. The perf device trees now boot
with `video=HDMI-A-1:1920x1080-16@60` (16 bpp console, 4.05 MB).

**Trim (`UI_TRIM`, default on).** While an app has the screen the UI frees all its scanout buffers:

- `Kms::release(Some(tint))` shows the launch tint from a 320x180 dumb buffer scaled up by the VOP (primary
  plane) instead of from the last full-size UI frame. Needed because an app may draw only on the overlay
  plane and leave the primary plane as it found it (YouTube does until a video plays): our frame would
  stay on screen, pinned. Without a launch layer (`release(None)`) the old frame stays until the app
  shows a framebuffer of its own.
- 300 ms later (then every 250 ms) the main loop checks `Kms::foreign_scanout()` and, when none of our
  framebuffers is on the CRTC, makes the EGL context current without a surface (`Gpu::detach`), removes
  the framebuffers (`Kms::free_buffers`: a DRM framebuffer keeps its pixels alive after the GBM surface
  is gone) and drops both GBM surfaces. Textures and the atlas stay.
- On `RECLAIM_DISPLAY` the surfaces are recreated (44 ms), the first frame goes out, and then the other
  two UI buffers are allocated before the return animation's clock starts (2 empty swaps with the front
  buffers held, 72-82 ms; an 8 MB buffer costs ~27-40 ms to allocate, which was 2 late frames in the
  middle of the animation). The next three frames repaint fully (the new buffers hold nothing).

| YouTube in the foreground (idle screen) | before | trim |
|---|---|---|
| MemAvailable | 82.8 MB | 98-104 MB |
| UI framebuffers held | 6 (31.3 MB) | 0 (+ 0.2 MB tint buffer) |
| Return animation (Closing), 3 runs | 0 late | 0 late, starts ~125 ms later; first frame after ~45 ms |

Flashed 15 Nov (`firmware-fd0a3ac9a533`, with the 16 bpp console and zram): 115 MB available with
YouTube in the foreground.

Log lines: `UI_TRIM freed framebuffers=N`, `UI_TRIM surfaces back ms=`, `UI_TRIM swap chain primed ms=`.

Not done, with reasons: two buffers per plane instead of three (loses the pipelining that keeps scroll
frames on time); trimming idle buffers at the home screen (the first input after idle would pay the
allocation); dropping textures while an app runs (at most ~6 MB, and the return would wait for a
wallpaper decode).

## Boot intro and hand-over

The UI starts from `init` at boot (see [../os/README.md](../os/README.md#tv-ui-at-boot)) and opens
with a short intro, [src/intro.rs](src/intro.rs). The wordmark is **meow**, the firmware's codename, held
in `intro::WORDMARK`.

**The mark is the UI's own focus ring in its purest form:**

1. A point of light traces a cyan (`#3daee9`) ring from 12 o'clock, drawing it behind itself with a comet
   tail, over a violet/cyan bloom.
2. The ring closes with a soft ripple, and a centre dot pops in.
3. "meow" fades up letter by letter while its tracking tightens.

**Hand-over (1 s):**

- The wallpaper fades in.
- The top bar drops in, and the section headers and cards rise in a staggered cascade
  (`Home::start_entrance`).
- The ring morphs from the circle into the focus ring of the first card and lands as that card finishes
  rising. From then on the home screen draws the identical ring.

**One process, so no seam.** The intro, the loading and the home screen share one GL context:

- At start, worker threads (4 cores) build the wallpaper, the card art and all badges, and rasterise
  every glyph the home screen uses ([src/text.rs](src/text.rs) `rasterise_runs`, [src/home.rs](src/home.rs)
  `render_art`/`render_badges`/`text_runs`).
- The render thread only uploads the results, within a 5 ms budget per frame; the wallpaper goes up in
  60-row strips.

**The intro waits for the TV.** A TV needs a few seconds after the HDMI signal starts before it shows a
picture: 3.7 s on the test TV, measured with `ui.synctest`. The intro therefore starts at
*HDMI link-up + `UI_TV_SYNC_MS`*:

- When the UI keeps the console's mode, link-up is when the kernel console took HDMI (read from
  `/dev/kmsg`).
- Otherwise it is the UI's own modeset.

A UI started later (`ui-session restart`, `/opt/ui/run.sh`) therefore plays at once. The home screen keeps
loading during the wait.

**Cost:** every element is a sprite in a small intro atlas, freed when the intro ends (4 MiB at 1080p,
16 MiB at 4K):

- The glows are additive (premultiplied colour with alpha 0).
- The ring sweep is a fan of wedges cut from one ring texture.
- The morph is a 9-slice from one of nine ring sprites with nearby radii.

| Measured on the dongle | 1080p60 | 4k30 |
| --- | --- | --- |
| Process start → first frame | 0.33 s | 0.47 s |
| Home ready (built in the background) | 0.9 s | 2.4 s |
| Intro frame pacing | 60 fps, 0 % late | 30 fps, 0 % late |
| Hand-over (full-screen wallpaper fade) | 60 fps, 0 % late (95 % repaint) | ~25 fps (bandwidth-bound, as expected at 4K) |
| RSS after hand-over | 16 MiB | 18 MiB |

**Boot timeline (perf image, 1080p60, test TV):**

| Event | Time from kernel start |
| --- | --- |
| HDMI on (console) | 0.62 s |
| UI starts | 1.0 s |
| Home ready | 1.9 s |
| Intro begins as the TV shows a picture | ≈ 4.3 s |
| Home screen fully up | ≈ 7.2 s |

**Two pitfalls fixed on the way (both made the intro invisible):**

- **A full modeset re-syncs the TV for seconds.** The UI sets the
  [`DRM_CLIENT_CAP_ASPECT_RATIO`](src/kms.rs) cap and reuses the console's current mode, so taking over
  (and exiting) is a plain plane update.
  - Without the cap, userspace modes lose their 16:9 picture-aspect flag and compare unequal to the
    console's CEA mode.
  - The EDID can also list the same 1080p60 timing twice, with different sync polarity.
  - The kernel must boot in the UI's mode (`video=`).
- **Mesa stalled for 8 s at boot.** Its disk-cache seeding read `/dev/urandom` before the kernel RNG was
  ready, which makes the kernel generate jitter entropy, about 8 s on this SoC.
  [../os/patches/mesa-rand-no-boot-stall.patch](../os/patches/mesa-rand-no-boot-stall.patch)
  uses `GRND_INSECURE` instead.

## Build and deploy

The toolchain is Rust 1.99.1 in [../os/Dockerfile.gpu](../os/Dockerfile.gpu). Crates are vendored
(61 crates; licences are in [Cargo.toml](Cargo.toml)), and the fonts come from Debian's
`fonts-noto-core`/`fonts-noto-extra` (OFL).

```sh
cd ui
docker run --rm -v "$PWD/..:/fw" -w /fw/ui meowcast-builder:gpu sh fetch-deps.sh   # network; UPDATE_LOCK=1 after dep changes
docker run --rm --network=none -v "$PWD/..:/fw" -v meowcast-build:/build \
  -w /fw/ui meowcast-builder:gpu sh build-ui.sh                                   # -> ../os/deploy/ui
# After changing patches/mesa-*.patch: run ../os/build-mesa.sh in the builder, then build-ui.sh.
```

On the dongle, with `python3 serve.py` running on the Mac in `os`:

```sh
fetch ui/install.sh && sh /opt/install.sh   # -> /opt/ui, SHA-256 verified (+ lib/ if Mesa is newer than the initramfs)
/opt/ui/run.sh 1080p60 &                    # or 4k30 / 720p60; stops the boot session first (ui-session start restores it)
printf dddsp > /run/ui-keys                 # right x3, down, screenshot; q quits
/opt/ui/bench.sh 1080p60 12                 # idle / navigation / full redraw / GPU-timed navigation
/opt/ui/settingsbench.sh 1080p60 3 220      # top bar + Settings tour, prints UI_BENCH_SUMMARY
```

**Screenshots for review:** run `python3 shot.py NAME` on the Mac, then `nc 192.168.2.1 8001 < /opt/shot.ppm`
on the dongle. This writes `shots/NAME.png`. The receiver binds to the USB bridge only. Screenshots capture a
normal *partial* frame, so they also show any stale tiles.

**Environment variables:**

| Variable | Effect |
| --- | --- |
| `UI_OUTPUT` | Named output mode: `1080p60`, `4k30` or `720p60` |
| `UI_DEMO=1`, `UI_DEMO_MS` | Automatic D-pad navigation, and its step interval |
| `UI_SECONDS` | Exit after N seconds |
| `UI_INTRO=1\|entrance\|0` | Boot intro with hand-over (default), only the entrance cascade, or neither (`bench.sh` uses 0) |
| `UI_TV_SYNC_MS` | How long the TV needs to show a picture after the HDMI signal starts (default 3700); the intro waits for it |
| `UI_SYNC_TEST=1` | Show a large kernel-uptime counter instead of the UI, to measure `UI_TV_SYNC_MS` for a TV |
| `UI_STATS=0` | No per-second `UI_STATS` lines (the boot session log) |
| `UI_SHOT`, `UI_SHOT_AT` | Screenshot path, and the time(s) to take it: `UI_SHOT_AT=0.6,0.9,1.4` writes `shot-0.ppm`… (`python3 shot.py NAME 3` receives them) |
| `UI_RGB565=auto\|1\|0` | 16-bit scanout with dithering; `auto` turns it on above 1080p |
| `UI_FULL_REDRAW=1` | Disable partial redraw (A/B) |
| `UI_NO_SCISSOR_CLEAR=1` | Partial frames reload instead of clearing (A/B) |
| `UI_PROFILE=1` | Time the GPU per frame with `glFinish` |
| `UI_GPU_BENCH=1` | Static-frame GPU cost breakdown |
| `TZ` | POSIX time-zone string for the clock, e.g. `CET-1CEST,M3.5.0,M10.5.0/3`. The clock is UTC without it. The Settings time zone overrides it |
| `UI_SETTINGS` | Settings file (default `/data/ui/settings`, key=value). `settingsbench.sh` points it at `/tmp` so benchmarks never write flash |
