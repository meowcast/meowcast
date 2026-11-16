//! miracast-ui: 10-foot launcher for the RK3228A dongle with a custom renderer for
//! Mali-400 (Lima, OpenGL ES 2.0). Expensive visuals are pre-rendered once (art.rs);
//! per frame the GPU only copies texels with a trivial shader, and only inside the area
//! that changed (damage from diffing consecutive draw lists + EGL buffer age).
//!
//! Environment:
//!   UI_OUTPUT=1080p60|4k30|720p60   named output mode (default 1080p60)
//!   UI_INTRO=1|entrance|0           boot intro + hand-over (default), entrance cascade only, or none
//!   UI_DEMO=1  UI_DEMO_MS=300       automatic D-pad navigation
//!   UI_DEMO_KEYS=du                 its key sequence (l r u d; default: a tour of the rows)
//!   UI_TRIM=1|0                     free the scanout buffers while an app has the screen
//!                                   (README "Memory"; default on)
//!   UI_PLANES=auto|1|0              wallpaper on the display's primary plane (VOP win0, scaled
//!                                   from half resolution) and the UI on the overlay plane with
//!                                   per-pixel alpha; the GPU then never draws the wallpaper at
//!                                   full size (default: on up to 1080p)
//!   UI_NICE=-10                     scheduling priority of the UI (render thread and the start-up
//!                                   workers; the wallpaper worker drops itself to nice 10)
//!   UI_SCROLL_BLUR=auto|1|0         wallpaper depth dissolve during the vertical scroll
//!                                   (default: with planes) or after it
//!   UI_SECONDS=N                    quit after N seconds (0 = run until q / SIGTERM)
//!   UI_TV_SYNC_MS=3700              time the TV needs to show a picture after the HDMI signal
//!                                   starts; the intro waits for it (0 = start at once)
//!   UI_SYNC_TEST=1                  show a large kernel-uptime counter (measures how long the TV
//!                                   takes to show a picture after the HDMI signal starts)
//!   UI_STATS=0                      no per-second UI_STATS lines (boot session log)
//!   UI_KEYS=/run/ui-keys            key FIFO (see input.rs)
//!   (apps: appd's /run/appd/ui.sock when present: app cards, consent, display handover)
//!   UI_FONTS=/opt/ui/fonts          Noto Sans directory
//!   UI_WALLPAPER=wallpapers/x.jpg   start with this wallpaper asset instead of the one chosen
//!                                   in Settings (wall.rs: downloaded by appd, kept in RAM only;
//!                                   its preview from the signed index is shown blurred until
//!                                   then); "procedural" keeps the built-in one
//!   UI_RGB565=auto|1|0              16-bit scanout + dithering (default: on above 1080p)
//!   UI_FULL_REDRAW=1                disable partial redraw (for comparison)
//!   UI_NO_SCISSOR_CLEAR=1           partial frames reload instead of clearing (A/B)
//!   UI_SHOT=/opt/shot.ppm UI_SHOT_AT=3   full-frame screenshot after N s (also key 'p')
//!   UI_GPU_BENCH=1                  time static full frames (glFinish) per draw variant, exit
//!   UI_PROFILE=1                    time the GPU per frame with glFinish (serialises!)
//!   TZ=CET-1CEST,M3.5.0,M10.5.0/3   POSIX TZ string for the clock (no zoneinfo needed); the
//!                                   Settings time zone overrides it
//!   UI_SETTINGS=/data/ui/settings   persisted user settings (settings.rs)
//! Stats go to stderr as UI_* lines. Exit code 3 = restart me (new resolution, or Restart the
//! launcher in Settings): ui-session and run.sh start the UI again at once.

mod apps;
mod art;
mod atlas;
mod gallery;
mod gpu;
mod home;
mod input;
mod intro;
mod kms;
mod launch;
mod onboard;
mod reset;
mod settings;
mod setview;
mod text;
mod util;
mod volpop;
mod wall;

/// meow-thermald's /run/thermal/state (os/thermald.c): (notice_seq, level, notice).
/// None while the file is missing or being replaced.
fn thermal_state() -> Option<(u32, u32, String)> {
    let s = std::fs::read_to_string("/run/thermal/state").ok()?;
    if !s.contains("\nend=1") {
        return None;
    }
    let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).unwrap_or("");
    Some((get("notice_seq").parse().ok()?, get("level").parse().unwrap_or(0), get("notice").to_string()))
}

use home::{Frame, Home, Key};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use util::Rect;

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn env_str(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_else(|_| "?".into())
}
fn proc_status(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
/// utime + stime of this process in clock ticks.
fn cpu_ticks() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = s.rsplit_once(')').map(|r| r.1).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

/// CLOCK_MONOTONIC in seconds (the clock of printk timestamps and of `Instant`).
fn mono_s() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// When the kernel console started driving HDMI (seconds since boot), from the kernel log:
/// the TV starts syncing then.
fn console_link_up() -> Option<f64> {
    let fd = unsafe { libc::open(c"/dev/kmsg".as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut buf = vec![0u8; 8192];
    let mut found = None;
    for _ in 0..4096 {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPIPE) {
                continue; // overwritten records
            }
            break; // EAGAIN: end of the log
        }
        let rec = String::from_utf8_lossy(&buf[..n as usize]);
        if let Some((hdr, msg)) = rec.split_once(';') {
            if msg.contains("switching to colour frame buffer device") {
                found = hdr.split(',').nth(2).and_then(|u| u.parse::<f64>().ok()).map(|us| us / 1e6);
            }
        }
    }
    unsafe { libc::close(fd) };
    found
}

/// CLOCK_REALTIME minus CLOCK_MONOTONIC in seconds: constant until someone sets the clock.
fn wall_minus_mono() -> f64 {
    let mut w: libc::timespec = unsafe { std::mem::zeroed() };
    let mut m: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut w);
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut m);
    }
    (w.tv_sec - m.tv_sec) as f64 + (w.tv_nsec - m.tv_nsec) as f64 / 1e9
}

/// ("8:35 AM" or "20:35", "Thursday, November 5, 2026", seconds until the next minute)
fn clock(h24: bool) -> (String, String, u64) {
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        let h = match tm.tm_hour % 12 { 0 => 12, h => h };
        (
            if h24 { format!("{:02}:{:02}", tm.tm_hour, tm.tm_min) } else { format!("{h}:{:02} {}", tm.tm_min, if tm.tm_hour < 12 { "AM" } else { "PM" }) },
            format!("{}, {} {}, {}", DAYS[tm.tm_wday as usize % 7], MONTHS[tm.tm_mon as usize % 12], tm.tm_mday, tm.tm_year + 1900),
            (60 - tm.tm_sec.clamp(0, 59)) as u64,
        )
    }
}

/// Damage between two consecutive draw lists: bounds of every item that was added,
/// removed or changed (old and new position).
/// A launch transition starting on app `id`'s card (None if the card is not on the home).
fn new_launch(hm: &Home, id: &str, now: Instant, phase: launch::Phase, app: Option<&apps::AppInfo>) -> Option<launch::Launch> {
    let v = hm.card_visual(id, now)?;
    let mut l = launch::Launch {
        id: id.to_string(),
        name: v.title,
        tint: v.tint,
        glyph: v.glyph,
        grad: v.grad,
        shape: v.shape,
        card: v.rect,
        card_badge_w: v.badge_w,
        card_corner: v.corner,
        phase,
        t0: now,
        e0: 1.0,
        release_pending: false,
        status: String::new(),
        progress: None,
        faded_frame_built: false,
    };
    if let Some(a) = app {
        launch_status(&mut l, a);
    }
    Some(l)
}

fn launch_status(l: &mut launch::Launch, a: &apps::AppInfo) {
    if a.state == "fetching" {
        l.status = format!("Downloading  {}", a.detail);
        l.progress = Some(a.detail.trim_end_matches('%').parse::<f32>().unwrap_or(0.0) / 100.0);
    } else {
        l.status = "Starting\u{2026}".into();
        l.progress = None;
    }
}

/// The wallpaper as up to four quads around the opaque `occ` (whole screen if empty).
fn wall_pieces(out: &mut Vec<gpu::Vertex>, w: f32, h: f32, occ: Rect, alpha: f32) {
    let c = util::rgba(0xffffff, alpha);
    let mut piece = |x0: f32, y0: f32, x1: f32, y1: f32| {
        if x1 > x0 && y1 > y0 {
            home::quad(out, x0, y0, x1, y1, [x0 / w, y0 / h, x1 / w, y1 / h], c);
        }
    };
    if occ.is_empty() {
        piece(0.0, 0.0, w, h);
        return;
    }
    let (ox0, oy0, ox1, oy1) = (occ.x0.max(0) as f32, occ.y0.max(0) as f32, (occ.x1 as f32).min(w), (occ.y1 as f32).min(h));
    piece(0.0, 0.0, w, oy0);
    piece(0.0, oy1, w, h);
    piece(0.0, oy0, ox0, oy1);
    piece(ox1, oy0, w, oy1);
}

/// Plane mode: draw the wallpaper layers (bottom-up (texture, opacity)) into the bound
/// wallpaper surface. Two layers whose first is opaque take one pass: mix(first, second).
fn draw_wall_layers(gpu: &mut gpu::Gpu, layers: &[(glow::Texture, f32)], wall_alpha: f32, w: u32, h: u32, quads: &mut [Vec<gpu::Vertex>]) {
    let saved = gpu.clear_rgba;
    gpu.clear_rgba = [0.0, 0.0, 0.0, 1.0];
    for q in quads.iter_mut() {
        q.clear();
    }
    let mut ls = if wall_alpha > 0.0 { layers.to_vec() } else { Vec::new() };
    gpu.mix = None;
    if ls.len() >= 2 && ls[0].1 >= 1.0 {
        gpu.mix = Some((ls[1].0, ls[1].1));
        ls.remove(1);
    }
    let mut wl: Vec<(glow::Texture, bool)> = Vec::with_capacity(3);
    for (i, (t, a)) in ls.iter().enumerate().take(quads.len()) {
        let opaque = i == 0 && *a >= 1.0;
        wall_pieces(&mut quads[i], w as f32, h as f32, Rect::EMPTY, if opaque { wall_alpha } else { a * wall_alpha });
        wl.push((*t, opaque));
    }
    let batches: Vec<(glow::Texture, bool, &[gpu::Vertex])> = wl.iter().enumerate().map(|(i, (t, o))| (*t, *o, quads[i].as_slice())).collect();
    let target = Rect::new(0, 0, gpu.width as i32, gpu.height as i32);
    // No layers (no wallpaper yet): a black frame; the primary plane always needs one.
    gpu.draw_ex(&batches, target, true);
    gpu.mix = None;
    gpu.clear_rgba = saved;
}

/// Screenshot in plane mode: the UI overlay (premultiplied RGBA) over the wallpaper plane
/// (scaled up by nearest neighbour, like a 2x VOP scale without its filtering).
fn write_composed_ppm(path: &str, ui: &[u8], (w, h): (u32, u32), wall: &[u8], (ww, wh): (u32, u32)) -> std::io::Result<()> {
    let (w, h, ww, wh) = (w as usize, h as usize, ww as usize, wh as usize);
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    out.reserve(w * h * 3);
    for y in 0..h {
        let wy = (y * wh / h).min(wh - 1);
        for x in 0..w {
            let wx = (x * ww / w).min(ww - 1);
            let u = &ui[(y * w + x) * 4..][..4];
            let b = &wall[(wy * ww + wx) * 4..][..4];
            let inv = 255 - u[3] as u32;
            for c in 0..3 {
                out.push((u[c] as u32 + (b[c] as u32 * inv + 127) / 255).min(255) as u8);
            }
        }
    }
    std::fs::write(path, out)
}

fn diff(prev: &mut HashMap<u32, (u64, Rect)>, frame: &Frame) -> Rect {
    let mut dmg = Rect::EMPTY;
    let mut next = HashMap::with_capacity(frame.items.len());
    for it in &frame.items {
        match prev.remove(&it.id) {
            Some((h, _)) if h == it.hash => {}
            Some((_, b)) => dmg = dmg.union(&b).union(&it.bounds),
            None => dmg = dmg.union(&it.bounds),
        }
        next.insert(it.id, (it.hash, it.bounds));
    }
    for (_, (_, b)) in prev.drain() {
        dmg = dmg.union(&b);
    }
    *prev = next;
    dmg
}

const DEMO: &[Key] = &[
    Key::Right, Key::Right, Key::Right, Key::Right, Key::Down, Key::Right, Key::Right, Key::Right,
    Key::Right, Key::Right, Key::Right, Key::Down, Key::Right, Key::Right, Key::Up, Key::Left,
    Key::Left, Key::Left, Key::Left, Key::Left, Key::Left, Key::Left, Key::Left, Key::Up, Key::Left,
    Key::Left, Key::Left,
];

/// Results of the start-up worker threads (CPU-only work; the GL uploads stay on the
/// render thread).
enum Msg {
    Wall(atlas::Image),
    Art(home::ArtImages),
    SetArt(setview::ArtImages),
    Badge(usize, usize, atlas::Image),
    Glyphs(Vec<(text::Key, text::Raster)>),
    Fonts(Vec<ab_glyph::FontVec>),
    Error(String),
    Done,
}
const WORKERS: usize = 3;
/// Cross-fade from the built-in wallpaper to the asset one, when it arrives on screen.
const WALL_FADE_MS: u64 = 600;
/// Preview (blurred placeholder) -> the downloaded wallpaper: a slower "focus pull".
const WALL_FOCUS_MS: u64 = 900;
/// Both fades when the user changes the wallpaper and the GPU composites it (no display
/// planes: 4K). A cross-fade there repaints the whole screen under the Settings view at
/// ~10 frames/s (measured 100-133 ms per frame), so it is kept to a couple of frames.
const WALL_QUICK_MS: u64 = 240;
/// Exit code asking the session to start the UI again (settings.rs output, Restart).
const EXIT_RESTART: i32 = 3;
/// Screen dimming: how dark the idle dim gets (1 = black, the Sleep button).
const DIM_IDLE: f32 = 0.82;
/// Sleep by itself this long after the idle dim (only with the idle dim switched on).
const SLEEP_AFTER_DIM: Duration = Duration::from_secs(10 * 60);
/// Waking from sleep: the TV needs this long to lock on to the signal again, so the picture
/// stays black until then and fades in afterwards (measured 3.7 s on the test TV).
const WAKE_SYNC: Duration = Duration::from_millis(3800);
/// Item ids of the performance overlay and the dimming layer (above everything else).
const ID_HUD: u32 = 980_000;
const ID_DIM: u32 = 990_000;

/// The performance overlay's line: frame rate and pacing of the last second, GPU clock,
/// CPU use and temperature.
fn hud_line(flips: u64, dt: f64, n: u64, late: u64, cpu_pct: f64) -> String {
    let mhz = read_trim("/sys/class/devfreq/20000000.gpu/cur_freq").parse::<u64>().unwrap_or(0) / 1_000_000;
    let temp = read_trim("/sys/class/thermal/thermal_zone0/temp").parse::<f64>().unwrap_or(0.0) / 1000.0;
    let fps = flips as f64 / dt.max(0.001);
    // Its own once-a-second repaint is the only frame when nothing moves.
    let motion = if flips <= 1 { "idle".to_string() } else { format!("{fps:.0} fps  ·  {}% late", if n > 0 { late * 100 / n } else { 0 }) };
    format!("{motion}  ·  GPU {mhz} MHz  ·  CPU {cpu_pct:.0}%  ·  {temp:.0} °C")
}

/// Blank the text console the kernel shows for a moment when the UI exits, so a restart
/// goes black -> new UI instead of flashing console text.
fn blank_console() {
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty1") {
        let _ = std::io::Write::write_all(&mut tty, b"\x1b[H\x1b[2J");
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum IntroMode {
    Full,
    Entrance,
    Off,
}

fn main() {
    let t0 = Instant::now();
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        // Mesa's default PP-stream cache is far too small (CPU regenerates it every frame).
        if std::env::var_os("LIMA_PLB_PP_STREAM_CACHE_SIZE").is_none() {
            std::env::set_var("LIMA_PLB_PP_STREAM_CACHE_SIZE", "4194304");
        }
    }
    // Frames must not wait behind background CPU work (app installs, decoding): with 4
    // busy threads at the default priority, scrolls measured 14-19 % late frames.
    let nice: i32 = std::env::var("UI_NICE").ok().and_then(|v| v.parse().ok()).unwrap_or(-10);
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } != 0 {
        eprintln!("UI_WARN nice {nice}: {}", std::io::Error::last_os_error());
    }
    let sync_test = env_u64("UI_SYNC_TEST", 0) != 0;
    let intro_mode = match if sync_test { "0".to_string() } else { env_str("UI_INTRO", "1") }.as_str() {
        "0" => IntroMode::Off,
        "entrance" => IntroMode::Entrance,
        _ => IntroMode::Full,
    };
    let fonts_dir = env_str("UI_FONTS", "/opt/ui/fonts");
    eprintln!("UI_START uptime={}", read_trim("/proc/uptime"));
    // Start-up work runs on worker threads (4 cores) while this thread brings up KMS/EGL
    // and plays the intro: wallpaper first (it does not depend on the output scale).
    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Wall(art::wallpaper(960, 540)));
            let _ = tx.send(Msg::Done);
        });
    }
    let (mut kms, summary) = match kms::Kms::open(&env_str("UI_OUTPUT", "1080p60")) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    eprintln!("{summary} ms={}", t0.elapsed().as_millis());
    let (w, h) = (kms.width, kms.height);
    let scale = h as f32 / 1080.0;
    // User settings (settings.rs). ui-session waits for /data at boot; if it is still not
    // mounted, try again for a while (time zone, clock format, ... apply when they arrive).
    let (mut cfg, cfg_found) = settings::Settings::load(settings::Settings::defaults(&env_str("UI_OUTPUT", "1080p60")));
    let mut cfg_retry: Option<Instant> = (!cfg_found && !settings::data_mounted()).then(|| Instant::now() + Duration::from_secs(1));
    let cfg_retry_end = Instant::now() + Duration::from_secs(20);
    cfg.apply_tz();
    eprintln!("UI_SETTINGS {} found={cfg_found} output={} tz={} clock24={} depth={} dim_min={} hud={} wallpaper={} confirm={:?}", settings::path(), cfg.output, cfg.tz, cfg.clock24, cfg.depth, cfg.dim_min, cfg.hud, cfg.wallpaper, cfg.confirm);
    let saver = settings::Saver::new();
    let (mut clock_s, mut date_s, next_min) = clock(cfg.clock24);
    let intro_rx = (intro_mode == IntroMode::Full).then(|| {
        let (itx, irx) = mpsc::channel();
        let dir = fonts_dir.clone();
        std::thread::spawn(move || {
            let _ = itx.send(intro::render(scale, &dir));
        });
        irx
    });
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Art(home::render_art(scale)));
            let _ = tx.send(Msg::SetArt(setview::render_art(scale)));
            home::render_badges(scale, |si, ci, img| {
                let _ = tx.send(Msg::Badge(si, ci, img));
            });
            let _ = tx.send(Msg::Done);
        });
    }
    {
        let tx = tx.clone();
        let dir = fonts_dir.clone();
        let mut runs = home::text_runs(scale, &date_s);
        runs.extend(setview::text_runs(scale));
        std::thread::spawn(move || {
            match text::load_fonts(&dir) {
                Ok(fonts) => {
                    let mut glyphs = text::rasterise_runs(&fonts, &runs);
                    while !glyphs.is_empty() {
                        let rest = glyphs.split_off(glyphs.len().min(24));
                        let _ = tx.send(Msg::Glyphs(std::mem::replace(&mut glyphs, rest)));
                    }
                    let _ = tx.send(Msg::Fonts(fonts));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(e));
                }
            }
            let _ = tx.send(Msg::Done);
        });
    }
    drop(tx);

    let usage = gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR;
    // RGB565 halves framebuffer writeback and scanout bandwidth: the measured floor at 4K.
    // Default: on above 1080p (dithered; invisible at 4K pixel size), off otherwise.
    kms.rgb565 = match std::env::var("UI_RGB565").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => h > 1080,
    };
    // Plane mode (UI_PLANES): the VOP composes the wallpaper (primary plane, half size,
    // scaled up by the VOP) under the UI (overlay plane, ARGB, per-pixel alpha). The GPU then
    // draws only the UI, and a wallpaper change (the depth dissolve) re-renders only the
    // quarter-size wallpaper surface. Not at 4K: an ARGB8888 4K overlay would double the
    // framebuffer bandwidth of the RGB565 path.
    let want_planes = match std::env::var("UI_PLANES").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => h <= 1080 && !kms.rgb565,
    };
    if want_planes && !kms.rgb565 {
        match kms.enable_planes() {
            Ok(()) => eprintln!("UI_PLANES on {}", kms.planes.as_ref().map_or("", |p| p.info.as_str())),
            Err(e) => eprintln!("UI_PLANES off: {e}"),
        }
    }
    let planes = kms.planes.is_some();
    let format = if kms.rgb565 { gbm::Format::Rgb565 } else if planes { gbm::Format::Argb8888 } else { gbm::Format::Xrgb8888 };
    // `surface` and `wall_plane` are None while an app has the screen (see "trim" below).
    let mut surface = Some(
        kms.gbm
            .create_surface::<drm::control::framebuffer::Handle>(w, h, format, usage)
            .unwrap_or_else(|e| fail(&format!("GBM surface: {e}"))),
    );
    let mut gpu = gpu::Gpu::new(&kms.gbm, surface.as_ref().unwrap(), w, h, kms.rgb565, planes).unwrap_or_else(|e| fail(&e));
    gpu.scissor_clear = env_u64("UI_NO_SCISSOR_CLEAR", 0) == 0;
    eprintln!("{} ms={}", gpu.info, t0.elapsed().as_millis());
    // The wallpaper plane's surface: half the output size (the wallpaper textures are 960x540
    // and 480x270 anyway), ARGB like the UI (same EGL config; win0 ignores the alpha).
    let wall_size = (w / 2, h / 2);
    let mut wall_plane = planes.then(|| {
        let s = kms
            .gbm
            .create_surface::<drm::control::framebuffer::Handle>(wall_size.0, wall_size.1, gbm::Format::Argb8888, usage)
            .unwrap_or_else(|e| fail(&format!("GBM wallpaper surface: {e}")));
        let e = gpu.window_surface(&s).unwrap_or_else(|e| fail(&e));
        (s, e)
    });
    if planes {
        gpu.clear_rgba = [0.0; 4];
    }
    let scroll_blur = match std::env::var("UI_SCROLL_BLUR").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => planes,
    };

    let mut atlas = atlas::Atlas::new(&gpu, if scale > 1.0 { 2048 } else { 1024 });
    // The TV shows nothing for a few seconds after the HDMI signal starts (measured 3.7 s on
    // the test TV). Keeping the console's mode, the link came up when the kernel console
    // started; otherwise with our own modeset (about now). The intro starts when the TV
    // can show it; the home screen keeps loading meanwhile.
    let tv_sync = env_u64("UI_TV_SYNC_MS", 3700) as f64 / 1000.0;
    let link_up = if kms.kept_mode { console_link_up() } else { Some(mono_s()) };
    let intro_wait = link_up.map_or(0.0, |l| (l + tv_sync - mono_s()).max(0.0));
    eprintln!("UI_TV_SYNC link_up_s={} sync_ms={} intro_wait_ms={:.0}", link_up.map_or("?".into(), |l| format!("{l:.2}")), (tv_sync * 1000.0) as u64, intro_wait * 1000.0);
    let mut intro = intro_rx.and_then(|irx| match irx.recv() {
        Ok(Ok(img)) => Some(intro::Intro::new(&gpu, &img, scale, Instant::now() + Duration::from_secs_f64(intro_wait))),
        Ok(Err(e)) => {
            eprintln!("UI_WARN intro: {e}");
            None
        }
        Err(_) => None,
    });
    if intro.is_some() {
        eprintln!("UI_INTRO_ART ms={}", t0.elapsed().as_millis());
    }
    let mut home: Option<Home> = None;
    let mut text: Option<text::Text> = None;
    let mut wall_tex: Option<glow::Texture> = None;
    let mut workers_done = 0usize;
    let mut q_art: Option<home::ArtImages> = None;
    let mut q_setart: Option<setview::ArtImages> = None;
    let mut q_wall: Option<atlas::Image> = None;
    let mut wall_rows = 0u32; // wallpaper rows uploaded so far (strips keep frames short)
    let mut q_badges: VecDeque<(usize, usize, atlas::Image)> = VecDeque::new();
    let mut q_glyphs: VecDeque<(text::Key, text::Raster)> = VecDeque::new();
    let mut ready_at: Option<Instant> = None;

    let mut keys = input::Keys::open(&env_str("UI_KEYS", "/run/ui-keys"));
    // Apps (apps appd): list, install consent, screen handover to the running app.
    let mut appd = apps::Appd::new();
    let mut appd_events: Vec<apps::Event> = Vec::new();
    let mut app_list: Vec<apps::AppInfo> = Vec::new();
    let mut apps_dirty = false;
    let mut consent: Option<home::Consent> = None;
    let mut notice: Option<(String, Instant)> = None;
    // The system volume (appd owns it): (level, muted); None until appd says.
    let mut volume: Option<(u32, bool)> = None;
    // The volume pop-up (volpop.rs): shown whenever something sets the volume.
    let mut vol_pop = volpop::VolPopup::new(&fonts_dir, h);
    // The apps opened last (appd keeps them), and appd's details of the installed apps.
    let mut recents: Vec<String> = Vec::new();
    let mut app_details: HashMap<String, apps::AppDetail> = HashMap::new();
    // Settings view (setview.rs) and what it shows.
    let mut sv = setview::SettingsView::new(scale);
    let mut sysinfo = settings::SysInfo::default();
    let mut info_at = Instant::now();
    // meow-netd's state (/run/netd/state) and the "Set up with your phone" prompt.
    let mut net = onboard::NetState::default();
    let mut net_at = Instant::now();
    // meow-thermald's state: a notice it wants on screen (shown once per notice_seq; what was
    // there before this start is not news) and the throttle level (2 = hot: no clock boost).
    let (mut heat_seq, mut heat_level) = thermal_state().map_or((0, 0), |(seq, level, _)| (seq, level));
    let mut heat_notice: Option<String> = None;
    let mut ob = onboard::Onboard::new();
    // The factory-reset screens (reset.rs): the hardware button's warning, and "Erasing…".
    let mut rst = reset::Reset::new();
    // Pairing is accepted only while the prompt shows the code (meow-netd pair-open/close).
    let mut pair_window = false;
    let mut pair_renew_at = Instant::now();
    let mut confirm_check = cfg.confirm.is_some();
    let mut pending_uninstall: Option<String> = None;
    // Screen dimming (idle timer, Sleep button) and the restart fade.
    let mut dim = util::Tween::fixed(0.0);
    let mut last_input = Instant::now();
    // Sleep (the top bar's button, or some time after the idle dim): the fade to black, then
    // the video output goes off (kms.set_output). `wake_fade_at`: output is back on, the
    // picture fades in once the TV shows it again.
    let mut sleeping = false;
    let mut dimmed_at: Option<Instant> = None;
    let mut wake_fade_at: Option<Instant> = None;
    let mut restart_at: Option<Instant> = None;
    let mut exit_code = 0;
    // Performance overlay: counters of the current second.
    let (mut hud_at, mut hud_flips, mut hud_n, mut hud_late, mut hud_ticks, mut hud_t) = (Instant::now(), 0u64, 0u64, 0u64, cpu_ticks(), Instant::now());
    let mut hud_text = String::new();
    // Some(app id) while an app has the screen: the launcher draws nothing and forwards keys.
    let mut suspended: Option<String> = None;
    let mut force_full = false;
    // Trim: once the app that took the screen shows its own framebuffer, free our scanout
    // buffers (31 MiB at 1080p with planes) and recreate them when the screen comes back.
    let trim = env_u64("UI_TRIM", 1) != 0;
    let mut trim_at: Option<Instant> = None;
    let mut trim_tries = 0u32;
    // After a trim: allocate the rest of the swap chain right after the first frame (`prime`),
    // then repaint fully while those buffers still hold nothing (`prime_full` frames).
    let mut prime = false;
    let mut prime_full = 0u8;
    // Card <-> full-screen transition around app launches (launch.rs).
    let mut launch: Option<launch::Launch> = None;
    let mut boost = launch::Boost::new();
    let trace_launch = env_u64("UI_LAUNCH_TRACE", 0) != 0;
    // Frame pacing of each transition phase: (phase, frames, late, max interval s, start).
    let mut lstat: Option<(launch::Phase, u32, u32, f64, Instant)> = None;
    let full = Rect::new(0, 0, w as i32, h as i32);
    let full_redraw = env_u64("UI_FULL_REDRAW", 0) != 0;
    let profile = env_u64("UI_PROFILE", 0) != 0;
    let period = 1.0 / kms.refresh_hz as f64;
    let shot_path = env_str("UI_SHOT", "/opt/shot.ppm");
    // UI_SHOT_AT=3 or a list (0.6,0.9,1.4): one screenshot per time; with a list, files are
    // numbered (shot-0.ppm, ...) for reviewing animations frame by frame.
    let mut shot_times: VecDeque<Instant> = std::env::var("UI_SHOT_AT")
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .map(|s| t0 + Duration::from_secs_f64(s))
        .collect();
    let numbered = shot_times.len() > 1;
    let mut shot_n = 0usize;
    let mut shot_now = false;
    let demo_ms = env_u64("UI_DEMO", 0).min(1) * env_u64("UI_DEMO_MS", 300);
    let demo_keys: Vec<Key> = match std::env::var("UI_DEMO_KEYS") {
        Ok(k) if !k.is_empty() => k
            .chars()
            .filter_map(|c| match c {
                'l' => Some(Key::Left),
                'r' => Some(Key::Right),
                'u' => Some(Key::Up),
                'd' => Some(Key::Down),
                _ => None,
            })
            .collect(),
        _ => DEMO.to_vec(),
    };
    let demo_keys = if demo_keys.is_empty() { DEMO.to_vec() } else { demo_keys };
    let quit_at = match env_u64("UI_SECONDS", 0) { 0 => None, s => Some(t0 + Duration::from_secs(s)) };
    let mut wall_quads: Vec<Vec<gpu::Vertex>> = vec![Vec::new(), Vec::new(), Vec::new()];
    // Asset wallpaper (wall.rs): asked from appd, decoded and shaded on a worker thread (which
    // wakes the poll loop through a pipe), uploaded in strips, then cross-faded in.
    // Which one: the user's choice (Settings > Wallpaper, gallery.rs), unless UI_WALLPAPER
    // names one for this run. `wall_gen` counts the changes: results of worker threads for an
    // earlier choice are dropped.
    let wall_env = std::env::var("UI_WALLPAPER").ok().filter(|v| !v.is_empty());
    let mut wall_name = wall_env.clone().unwrap_or_else(|| cfg.wallpaper.clone());
    let mut wall_gen = 0u32;
    let mut wall_switch: Option<String> = None;
    let mut wall_quick = false; // the current wallpaper was chosen in this run, without planes
    // (generation, (set, is the real wallpaper rather than its preview))
    let (wset_tx, wset_rx) = mpsc::channel::<(u32, Result<(wall::WallSet, bool), String>)>();
    // The store's wallpapers (Settings > Wallpaper): the catalog, and the thumbnail sheet
    // decoded on a worker thread and uploaded in strips like the wallpaper.
    let mut gal = gallery::Gallery::new();
    let (sheet_tx, sheet_rx) = mpsc::channel::<(String, Result<atlas::Image, String>)>();
    let mut sheet_q: Option<(atlas::Image, glow::Texture, u32)> = None;
    let (wake_rd, wake_wr) = {
        let mut p = [0; 2];
        unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        (p[0], p[1])
    };
    let mut wset_q: Option<(wall::WallSet, glow::Texture, glow::Texture, u32, bool)> = None;
    let mut wset: Option<(glow::Texture, glow::Texture)> = None; // (sharp, soft)
    let mut wset_real = false; // wset is `wall_name` as downloaded (not its preview)
    let mut wset_preview = false; // wset is the preview of `wall_name`
    // Full font faces (text.rs: the firmware has Latin subsets): asked from appd when some
    // text needs them, loaded on a worker thread, swapped in.
    let (font_tx, font_rx) = mpsc::channel::<(text::Face, Result<ab_glyph::FontVec, String>)>();
    let mut fonts_asked = 0u8;
    let mut wset_fade: Option<(Instant, u64)> = None; // (start, duration ms)
    // UI_WALL_DEPTH=0: keep the sharp layer in every row (no blur cross-fade).
    let wall_depth = env_u64("UI_WALL_DEPTH", 1) != 0;
    let mut prev_wall_key: Option<Vec<(u32, u16)>> = None;

    let mut prev_home: HashMap<u32, (u64, Rect)> = HashMap::new();
    let mut prev_intro: HashMap<u32, (u64, Rect)> = HashMap::new();
    let mut history: Vec<Rect> = Vec::new(); // damage of previous frames, newest first
    let mut batch: Vec<gpu::Vertex> = Vec::new();
    let mut runs: Vec<(glow::Texture, usize, usize)> = Vec::new(); // (texture, range of `batch`)
    let mut ibatch: Vec<gpu::Vertex> = Vec::new();
    let mut need_build = true;
    let mut first_frame = true;
    let mut minute_at = Instant::now() + Duration::from_secs(next_min);
    let mut wall_base = wall_minus_mono();
    let mut demo_i = 0usize;
    let mut demo_at = Instant::now();
    let stats_on = env_u64("UI_STATS", 1) != 0;
    let mut stats_at = Instant::now() + Duration::from_secs(if stats_on { 1 } else { 1 << 30 });
    let (mut frames, mut st_frames, mut st_flips, mut st_px, mut st_ticks) = (0u64, 0u64, 0u64, 0i64, cpu_ticks());
    let mut st_t = Instant::now();
    // Frame pacing during continuous animation: interval between consecutive presents.
    let mut last_present: Option<Instant> = None;
    let (mut iv_n, mut iv_sum, mut iv_late, mut iv_max) = (0u64, 0.0f64, 0u64, 0.0f64);
    let (mut pr_cpu, mut pr_gpu) = (0.0f64, 0.0f64);
    // Whole-run totals for benchmarks (UI_EXIT): animated intervals, late ones, sum, max;
    // profiled GPU time and frames.
    let (mut tot_n, mut tot_late, mut tot_sum, mut tot_max, mut tot_gpu, mut tot_pf) = (0u64, 0u64, 0.0f64, 0.0f64, 0.0f64, 0u64);
    let mut events = Vec::new();
    let mut prefetching = true;

    while !STOP.load(Ordering::Relaxed) {
        let now = Instant::now();
        if quit_at.is_some_and(|q| now >= q) {
            break;
        }
        let loading = ready_at.is_none();
        let intro_live = intro.as_ref().is_some_and(|it| it.started(now));
        let overlay_anim = home.as_ref().is_some_and(|h| h.overlay_animating(now, consent.as_ref(), notice.as_ref()));
        let launch_anim = launch.as_ref().is_some_and(|l| l.animating(now));
        let view_anim = sv.animating(now) || ob.animating(now) || rst.animating(now) || dim.active(now) || restart_at.is_some() || (sv.visible(now) && gal.fading(now)) || vol_pop.animating(now);
        let busy = wset_q.is_some()
            || sheet_q.is_some()
            || suspended.is_none() && !kms.is_off() && (need_build || intro_live || overlay_anim || launch_anim || view_anim || wset_fade.is_some() || home.as_ref().is_some_and(|h| h.animating(now)));
        let mut wake = vec![stats_at, minute_at, net_at];
        wake.extend(sv.wake_at(now));
        wake.extend(vol_pop.wake_at(now));
        wake.extend(cfg_retry);
        if cfg.hud {
            wake.push(hud_at);
        }
        if sv.is_open() {
            wake.push(info_at);
        }
        if cfg.dim_min > 0 && dim.target() == 0.0 {
            wake.push(last_input + Duration::from_secs(cfg.dim_min as u64 * 60));
        }
        if !sleeping {
            wake.extend(dimmed_at.map(|t| t + SLEEP_AFTER_DIM));
        }
        wake.extend(wake_fade_at);
        wake.extend(appd.retry_at());
        wake.extend(launch.as_ref().and_then(|l| l.wake_at()));
        if let Some(it) = intro.as_ref().filter(|_| !intro_live) {
            wake.push(it.start_at());
        }
        if demo_ms > 0 && ready_at.is_some() {
            wake.push(demo_at);
        }
        if let Some(q) = quit_at {
            wake.push(q);
        }
        wake.extend(shot_times.front().copied());
        wake.extend(trim_at);
        // Idle-time prefetch (badges, glyphs) one item per loop, once the home screen is up.
        let prefetch = !busy && !kms.is_off() && prefetching && !first_frame && home.is_some() && intro.is_none();
        let timeout = if busy || prefetch {
            0
        } else if loading {
            4 // worker results arrive on a channel, not an fd
        } else {
            wake.iter().min().map_or(-1, |t| t.saturating_duration_since(now).as_millis().min(60_000) as i32 + 1)
        };
        let mut fds = [
            libc::pollfd { fd: kms.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: keys.as_ref().map_or(-1, |k| k.fd), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: appd.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_rd, events: libc::POLLIN, revents: 0 },
        ];
        let t_poll = Instant::now();
        unsafe { libc::poll(fds.as_mut_ptr(), 4, timeout) };
        let poll_ms = t_poll.elapsed().as_secs_f64() * 1000.0;
        if timeout != 0 {
            // We were idle (waiting for input or a timer): the next frame starts a new
            // animation; its interval must not include the idle wait.
            last_present = None;
        }
        if fds[3].revents != 0 {
            let mut b = [0u8; 16];
            while unsafe { libc::read(wake_rd, b.as_mut_ptr() as *mut _, b.len()) } > 0 {}
        }
        if fds[0].revents & libc::POLLIN != 0 {
            if let Err(e) = kms.dispatch() {
                eprintln!("UI_WARN drm events: {e}");
            }
        }

        // Start-up results: queue them (cheap), then upload within a per-frame budget so
        // the intro keeps its frame rate.
        if loading {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    // The built-in wallpaper is only the fallback: not needed once the
                    // preview (or the wallpaper itself) is here.
                    Msg::Wall(img) => {
                        if wset.is_none() && wset_q.is_none() {
                            q_wall = Some(img)
                        }
                    }
                    Msg::Art(a) => q_art = Some(a),
                    Msg::SetArt(a) => q_setart = Some(a),
                    Msg::Badge(si, ci, img) => q_badges.push_back((si, ci, img)),
                    Msg::Glyphs(g) => q_glyphs.extend(g),
                    Msg::Fonts(f) => text = Some(text::Text::new(f)),
                    Msg::Error(e) => fail(&e),
                    Msg::Done => workers_done += 1,
                }
            }
            let budget = Duration::from_millis(if intro.is_some() { 5 } else { 50 });
            let t_up = Instant::now();
            loop {
                if let Some(img) = q_wall.as_ref() {
                    let t = *wall_tex.get_or_insert_with(|| gpu.texture(img.w, img.h, None));
                    let n = 60.min(img.h - wall_rows);
                    let row = (img.w * 4) as usize;
                    let at = wall_rows as usize * row;
                    gpu.upload(t, 0, wall_rows, img.w, n, &img.data[at..at + n as usize * row]);
                    wall_rows += n;
                    if wall_rows == img.h {
                        q_wall = None;
                    }
                } else if let Some(a) = q_art.take() {
                    let mut hm = Home::new(&gpu, &mut atlas, scale, a);
                    hm.blur_with_scroll = scroll_blur;
                    home = Some(hm);
                } else if let Some(a) = q_setart.take() {
                    sv.set_art(&gpu, &mut atlas, a);
                } else if let (Some(hm), Some((si, ci, img))) = (home.as_mut(), q_badges.front()) {
                    hm.set_badge(&gpu, &mut atlas, *si, *ci, img);
                    q_badges.pop_front();
                } else if let (Some(tx), false) = (text.as_mut(), q_glyphs.is_empty()) {
                    let n = q_glyphs.len().min(16);
                    tx.preload(&gpu, &mut atlas, q_glyphs.drain(..n).collect());
                } else {
                    break;
                }
                if t_up.elapsed() >= budget {
                    break;
                }
            }
            // Either wallpaper will do: when the asset one is processed before the home screen
            // is ready, the built-in one is dropped unseen (nothing to cross-fade from), so
            // waiting for it would never end (seen at 4K, where start-up takes longer).
            let wall_ready = wall_tex.is_some() || wset.is_some();
            if workers_done == WORKERS && home.is_some() && text.is_some() && wall_ready && q_wall.is_none() && q_setart.is_none() && q_badges.is_empty() && q_glyphs.is_empty() {
                ready_at = Some(Instant::now());
                eprintln!(
                    "UI_HOME_READY ms={} atlas_used_pct={:.1} entries={} glyphs={}",
                    t0.elapsed().as_millis(),
                    atlas.used_px as f64 * 100.0 / (atlas.size as f64 * atlas.size as f64),
                    atlas.entries,
                    text.as_ref().map_or(0, |t| t.rasterised)
                );
                if intro_mode == IntroMode::Entrance {
                    if let Some(hm) = home.as_mut() {
                        hm.start_entrance(Instant::now());
                    }
                }
                need_build = true;
                demo_at = Instant::now() + Duration::from_millis(demo_ms.max(1) * 3 + 1500);
            }
        }
        let now = Instant::now();
        // Hand-over from the intro once both the intro has played and the home is ready.
        if let (Some(it), Some(hm), Some(_)) = (intro.as_mut(), home.as_mut(), ready_at) {
            if !it.handing_over() && it.played(now) {
                it.begin_handover(now);
                hm.start_entrance(now);
                hm.ring_hidden = true;
                eprintln!("UI_HANDOVER ms={}", t0.elapsed().as_millis());
            }
        }
        if intro.as_ref().is_some_and(|it| it.finished(now)) {
            intro.take().unwrap().free(&gpu);
            if let Some(hm) = home.as_mut() {
                hm.ring_hidden = false;
            }
            need_build = true;
            eprintln!("UI_INTRO_DONE ms={}", t0.elapsed().as_millis());
        }

        if let Some(k) = keys.as_mut() {
            k.read(&mut events);
        }
        // The launch splash has faded to its plain tint and that frame is on screen: hand
        // the screen to the app (kms.release waits for the pending flip).
        if let Some(l) = launch.as_mut().filter(|l| l.phase == launch::Phase::Fading && l.faded_frame_built) {
            match kms.release(Some(l.tint)) {
                Ok(()) => eprintln!("UI_APPS screen -> {} (after transition)", l.id),
                Err(e) => eprintln!("UI_WARN release display: {e}"),
            }
            suspended = Some(l.id.clone());
            last_present = None;
            l.set_phase(launch::Phase::Released, now);
            appd.send("DISPLAY_RELEASED");
        }
        if let Some(l) = launch.as_mut() {
            if let launch::Tick::Finished = l.tick(now) {
                launch = None;
                need_build = true;
            }
        }
        appd.poll(now, &mut appd_events);
        for ev in appd_events.drain(..) {
            match ev {
                apps::Event::Connected => {
                    app_list.clear();
                    app_details.clear();
                    if wall_name != "procedural" {
                        appd.send(&format!("ASSET\t{wall_name}"));
                    }
                    fonts_asked = 0; // ask again below if still needed
                    gal.asked = false;
                }
                apps::Event::AssetUpdated(name) if name == wall_name => appd.send(&format!("ASSET\t{wall_name}")),
                // A new catalog or sheet: ask again (now if Settings is open, else when it opens).
                apps::Event::AssetUpdated(name) if name == gallery::CATALOG || gal.catalog.as_ref().is_some_and(|c| c.thumbs == name) => gal.asked = false,
                apps::Event::AssetUpdated(_) => {}
                apps::Event::AssetPreview(name, jpeg) if name == wall_name && !wset_real && wset_q.as_ref().is_none_or(|q| !q.4) => {
                    let (tx, wake, wgen) = (wset_tx.clone(), wake_wr, wall_gen);
                    std::thread::spawn(move || {
                        let _ = tx.send((wgen, wall::preview(&jpeg).map(|s| (s, false))));
                        unsafe { libc::write(wake, b"w".as_ptr() as *const _, 1) };
                    });
                }
                apps::Event::AssetPreview(..) => {}
                apps::Event::Asset(name, r) if text::ALL_FACES.iter().any(|f| text::full_asset(*f) == name) => {
                    let face = *text::ALL_FACES.iter().find(|f| text::full_asset(**f) == name).unwrap();
                    match r {
                        Ok(fd) => {
                            let (tx, wake) = (font_tx.clone(), wake_wr);
                            std::thread::spawn(move || {
                                unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10) };
                                let mut data = Vec::new();
                                let r = std::io::Read::read_to_end(&mut std::fs::File::from(fd), &mut data)
                                    .map_err(|e| e.to_string())
                                    .and_then(|_| ab_glyph::FontVec::try_from_vec(data).map_err(|e| e.to_string()));
                                let _ = tx.send((face, r));
                                unsafe { libc::write(wake, b"f".as_ptr() as *const _, 1) };
                            });
                        }
                        Err(e) => eprintln!("UI_FONT {name}: {e}"),
                    }
                }
                apps::Event::Asset(name, r) if name == gallery::CATALOG => {
                    // ~1 KB from appd's RAM cache: read here.
                    let parsed = r.and_then(|fd| {
                        let mut text = String::new();
                        std::io::Read::read_to_string(&mut std::fs::File::from(fd), &mut text).map_err(|e| e.to_string())?;
                        gallery::Catalog::parse(&text)
                    });
                    match parsed {
                        Ok(c) => {
                            eprintln!("UI_GALLERY catalog: {} wallpapers, sheet {}", c.walls.len(), c.thumbs);
                            appd.send(&format!("ASSET\t{}", c.thumbs));
                            gal.error = None;
                            if gal.catalog.as_ref() != Some(&c) {
                                gal.catalog = Some(c);
                                need_build = true;
                            }
                        }
                        Err(e) => {
                            eprintln!("UI_GALLERY {e}");
                            gal.error = Some(e);
                            need_build = true;
                        }
                    }
                }
                apps::Event::Asset(name, r) if gal.catalog.as_ref().is_some_and(|c| c.thumbs == name) => match r {
                    Ok(fd) => {
                        let (tx, wake, cat) = (sheet_tx.clone(), wake_wr, gal.catalog.clone().unwrap());
                        std::thread::spawn(move || {
                            unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10) };
                            let t = Instant::now();
                            let mut data = Vec::new();
                            let r = std::io::Read::read_to_end(&mut std::fs::File::from(fd), &mut data).map_err(|e| e.to_string()).and_then(|_| cat.sheet(&data));
                            eprintln!("UI_GALLERY sheet: {} bytes decoded in {} ms", data.len(), t.elapsed().as_millis());
                            let _ = tx.send((cat.thumbs, r));
                            unsafe { libc::write(wake, b"g".as_ptr() as *const _, 1) };
                        });
                    }
                    Err(e) => eprintln!("UI_GALLERY {name}: {e}"),
                },
                apps::Event::Asset(name, r) => match r {
                    Ok(fd) if name == wall_name => {
                        let (tx, wake, wgen) = (wset_tx.clone(), wake_wr, wall_gen);
                        let t_w = Instant::now();
                        std::thread::spawn(move || {
                            // Background work (also during the boot intro): nice 10, inherited
                            // by wall::process's helper threads.
                            unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10) };
                            let mut data = Vec::new();
                            let r = std::io::Read::read_to_end(&mut std::fs::File::from(fd), &mut data)
                                .map_err(|e| format!("wallpaper read: {e}"))
                                .and_then(|_| wall::process(&data));
                            eprintln!("UI_WALL processed {} bytes in {} ms", data.len(), t_w.elapsed().as_millis());
                            let _ = tx.send((wgen, r.map(|s| (s, true))));
                            unsafe { libc::write(wake, b"w".as_ptr() as *const _, 1) };
                        });
                    }
                    Ok(_) => {}
                    // The chosen wallpaper left the store: back to the default one.
                    Err(e) if name == wall_name && name != gallery::DEFAULT => {
                        eprintln!("UI_WALL asset {name}: {e} (using the default wallpaper)");
                        wall_switch = Some(gallery::DEFAULT.into());
                    }
                    Err(e) => eprintln!("UI_WALL asset {name}: {e} (keeping the current wallpaper)"),
                },
                apps::Event::Disconnected => {
                    volume = None;
                    vol_pop.close(now);
                    if let Some(hm) = home.as_mut() {
                        hm.set_volume(None);
                    }
                    consent = None;
                    if let Some(l) = launch.as_mut().filter(|l| l.phase != launch::Phase::Closing) {
                        let card = home.as_ref().and_then(|hm| hm.card_visual(&l.id, now)).map(|v| (v.rect, v.badge_w, v.corner));
                        l.close(now, card);
                        force_full = true;
                    }
                    // appd went away while an app had the screen: take it back.
                    if suspended.take().is_some() {
                        if let Err(e) = kms.reacquire() {
                            eprintln!("UI_WARN reacquire after appd loss: {e}");
                        }
                        force_full = true;
                    }
                    need_build = true;
                }
                apps::Event::App(a) => {
                    if let Some(l) = launch.as_mut().filter(|l| l.id == a.id && matches!(l.phase, launch::Phase::Opening | launch::Phase::Waiting)) {
                        if a.state == "error" || a.state == "available" {
                            // Download/start failed or install cancelled: back onto the card.
                            let card = home.as_ref().and_then(|hm| hm.card_visual(&l.id, now)).map(|v| (v.rect, v.badge_w, v.corner));
                            l.close(now, card);
                        } else {
                            launch_status(l, &a);
                        }
                        need_build = true;
                    }
                    if a.state == "available" {
                        app_details.remove(&a.id); // uninstalled (it stays in the store)
                    }
                    match app_list.iter_mut().find(|x| x.id == a.id) {
                        Some(x) => *x = a,
                        None => {
                            app_list.push(a);
                            app_list.sort_by(|x, y| x.name.to_lowercase().cmp(&y.name.to_lowercase()));
                        }
                    }
                    apps_dirty = true;
                }
                apps::Event::Remove(id) => {
                    app_list.retain(|x| x.id != id);
                    app_details.remove(&id);
                    apps_dirty = true;
                }
                apps::Event::Recents(ids) => {
                    recents = ids;
                    apps_dirty = true;
                }
                apps::Event::Info(d) => {
                    app_details.insert(d.id.clone(), d);
                    need_build |= sv.is_open();
                }
                apps::Event::ListEnd => apps_dirty = true,
                apps::Event::Consent(c) => {
                    eprintln!("UI_APPS consent {} perms={}", c.id, c.perms.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(","));
                    consent = Some(home::Consent { id: c.id, name: c.name, perms: c.perms, update: c.update, focus: 0, shown_at: now });
                    need_build = true;
                }
                apps::Event::ReleaseDisplay(id) => {
                    consent = None;
                    // An app is coming to the front, perhaps by itself (a cast started and its
                    // background service asked): whatever the launcher was showing makes way.
                    // A dimmed or sleeping screen wakes; Settings and the pop-up close.
                    vol_pop.hide();
                    if dim.target() > 0.0 || dim.value(now) > 0.0 {
                        dim = util::Tween::fixed(0.0);
                    }
                    last_input = now;
                    if sv.is_open() || sv.visible(now) {
                        sv.close_now();
                        if let Some(hm) = home.as_mut() {
                            hm.set_away_now(false);
                        }
                    }
                    // Normally the opening transition is running: release after its fade.
                    // An app started elsewhere (appctl) gets the same transition if its card
                    // is on the home screen. appd waits up to 3 s for DISPLAY_RELEASED.
                    if launch.as_ref().is_none_or(|l| l.id != id || l.phase == launch::Phase::Closing) {
                        let app = app_list.iter().find(|a| a.id == id);
                        launch = home.as_ref().and_then(|hm| new_launch(hm, &id, now, launch::Phase::Opening, app));
                    }
                    match launch.as_mut() {
                        Some(l) => {
                            l.release_pending = true;
                            if l.phase == launch::Phase::Waiting {
                                l.set_phase(launch::Phase::Fading, now);
                            }
                            need_build = true;
                        }
                        None => {
                            match kms.release(None) {
                                Ok(()) => eprintln!("UI_APPS screen -> {id}"),
                                Err(e) => eprintln!("UI_WARN release display: {e}"),
                            }
                            suspended = Some(id);
                            appd.send("DISPLAY_RELEASED");
                        }
                    }
                }
                apps::Event::ReclaimDisplay(id) => {
                    match kms.reacquire() {
                        Ok(()) => eprintln!("UI_APPS screen <- {id}"),
                        Err(e) => eprintln!("UI_WARN reacquire display: {e}"),
                    }
                    suspended = None;
                    last_present = None;
                    force_full = true;
                    need_build = true;
                    last_input = now; // the idle timer starts when the launcher is back
                    appd.send("DISPLAY_RECLAIMED");
                    // Back from the app: the plain splash shrinks onto the card.
                    let card = home.as_ref().and_then(|hm| hm.card_visual(&id, now));
                    match launch.as_mut().filter(|l| l.id == id) {
                        Some(l) => l.close(now, card.map(|v| (v.rect, v.badge_w, v.corner))),
                        None => launch = home.as_ref().and_then(|hm| new_launch(hm, &id, now, launch::Phase::Closing, None)),
                    }
                }
                apps::Event::Foreground(f) => eprintln!("UI_APPS foreground {}", f.as_deref().unwrap_or("-")),
                apps::Event::Reply(r) => {
                    if let Some(name) = pending_uninstall.take() {
                        let t = match r {
                            Ok(_) => format!("{name} was uninstalled"),
                            Err(e) => format!("Could not uninstall {name}: {e}"),
                        };
                        eprintln!("UI_APPS {t}");
                        notice = Some((t, now));
                        need_build = true;
                    }
                }
                apps::Event::Volume(v, muted, show) => {
                    volume = Some((v, muted));
                    need_build |= sv.is_open();
                    if home.as_mut().is_some_and(|hm| hm.set_volume(volume)) {
                        need_build = true;
                    }
                    // While an app has the screen it draws the pop-up itself (the SDK's).
                    if show && suspended.is_none() {
                        vol_pop.show(v, muted, now);
                        need_build = true;
                    }
                }
                apps::Event::Notice(t) => {
                    eprintln!("UI_APPS notice {t}");
                    if let Some(l) = launch.as_mut().filter(|l| matches!(l.phase, launch::Phase::Opening | launch::Phase::Waiting)) {
                        let card = home.as_ref().and_then(|hm| hm.card_visual(&l.id, now)).map(|v| (v.rect, v.badge_w, v.corner));
                        l.close(now, card);
                    }
                    notice = Some((t, now));
                    need_build = true;
                }
            }
        }
        // Full font faces: ask for those some text needed, swap in what arrived.
        if let (Some(t), true) = (text.as_ref(), appd.connected()) {
            for f in t.missing_faces().collect::<Vec<_>>() {
                if fonts_asked & 1 << f as u8 == 0 {
                    fonts_asked |= 1 << f as u8;
                    eprintln!("UI_FONT asking for the full {}", text::FACES[f as usize]);
                    appd.send(&format!("ASSET\t{}", text::full_asset(f)));
                }
            }
        }
        while let Ok((face, r)) = font_rx.try_recv() {
            match r.and_then(|font| text.as_mut().ok_or_else(|| "no text yet".to_string())?.replace_font(face, font)) {
                Ok(()) => {
                    eprintln!("UI_FONT full {} in use", text::FACES[face as usize]);
                    need_build = true;
                    force_full = true;
                }
                Err(e) => eprintln!("UI_FONT {e}"),
            }
        }
        // Asset wallpaper: start uploading a processed set; upload one strip per loop.
        while let Ok((wgen, r)) = wset_rx.try_recv() {
            if wgen != wall_gen {
                continue; // for a wallpaper the user has moved on from
            }
            match r {
                // A preview that lost the race against the real wallpaper: drop it.
                Ok((_, false)) if wset_real || wset_q.as_ref().is_some_and(|q| q.4) => {}
                Ok((set, real)) => {
                    let sharp = gpu.texture(set.sharp.w, set.sharp.h, None);
                    let soft = gpu.texture(set.soft.w, set.soft.h, Some(&set.soft.data));
                    if let Some((_, a, b, _, _)) = wset_q.take() {
                        gpu.delete_texture(a);
                        gpu.delete_texture(b);
                    }
                    // Not uploaded yet: the built-in wallpaper is not needed any more.
                    if q_wall.take().is_some() {
                        if let Some(t) = wall_tex.take() {
                            gpu.delete_texture(t);
                        }
                        wall_rows = 0;
                    }
                    wset_q = Some((set, sharp, soft, 0, real));
                }
                Err(e) => eprintln!("UI_WALL {e}"),
            }
        }
        if let Some((set, sharp, _, rows, _)) = wset_q.as_mut() {
            let img = &set.sharp;
            let n = 72.min(img.h - *rows);
            if n > 0 {
                let row = (img.w * 4) as usize;
                let at = *rows as usize * row;
                gpu.upload(*sharp, 0, *rows, img.w, n, &img.data[at..at + n as usize * row]);
                *rows += n;
            }
            // Uploaded. A cross-fade still running (old wallpaper -> this one's preview, when
            // the download is quick) plays to its end first: one fade after the other, no jump.
            if *rows == img.h && wset_fade.is_none() {
                let (_, sharp, soft, _, real) = wset_q.take().unwrap();
                let shown = ready_at.is_some() && intro.as_ref().is_none_or(|it| it.handing_over());
                let old = wset.replace((sharp, soft));
                let from_preview = old.is_some() && wset_preview;
                // On screen already: cross-fade from what is there (the built-in wallpaper, or
                // the preview: its layer at the current depth); else just switch.
                if let (Some((a, b)), true) = (old, shown) {
                    let blurred = home.as_ref().is_some_and(|hm| hm.wall_blur(now) >= 0.5);
                    let (keep, drop) = if blurred { (b, a) } else { (a, b) };
                    gpu.delete_texture(drop);
                    if let Some(t) = wall_tex.replace(keep) {
                        gpu.delete_texture(t);
                    }
                } else if let Some((a, b)) = old {
                    gpu.delete_texture(a);
                    gpu.delete_texture(b);
                }
                if shown && wall_tex.is_some() {
                    let ms = if real && from_preview { WALL_FOCUS_MS } else { WALL_FADE_MS };
                    wset_fade = Some((now, if wall_quick { WALL_QUICK_MS } else { ms }));
                } else if let Some(t) = wall_tex.take() {
                    gpu.delete_texture(t);
                }
                wset_real = real;
                wset_preview = !real;
                need_build = true;
                eprintln!("UI_WALL ready real={real} fade={shown}");
            }
        }
        if wset_fade.is_some_and(|(t, ms)| now.duration_since(t) >= Duration::from_millis(ms)) {
            wset_fade = None;
            if let Some(t) = wall_tex.take() {
                gpu.delete_texture(t);
            }
            need_build = true;
        }
        // The gallery: ask for the catalog when Settings opens (its reply asks for the sheet),
        // so the Wallpaper page is usually complete before the user gets there.
        if sv.is_open() && appd.connected() && !gal.asked {
            gal.asked = true;
            appd.send(&format!("ASSET\t{}", gallery::CATALOG));
        }
        while let Ok((name, r)) = sheet_rx.try_recv() {
            match r {
                Ok(img) if gal.catalog.as_ref().is_some_and(|c| c.thumbs == name) => {
                    if let Some((_, t, _)) = sheet_q.take() {
                        gpu.delete_texture(t);
                    }
                    let tex = gpu.texture(img.w, img.h, None);
                    sheet_q = Some((img, tex, 0));
                }
                Ok(_) => {}
                Err(e) => eprintln!("UI_GALLERY sheet: {e}"),
            }
        }
        if let Some((img, tex, rows)) = sheet_q.as_mut() {
            let n = 64.min(img.h - *rows);
            let row = (img.w * 4) as usize;
            let at = *rows as usize * row;
            gpu.upload(*tex, 0, *rows, img.w, n, &img.data[at..at + n as usize * row]);
            *rows += n;
            if *rows == img.h {
                let (_, tex, _) = sheet_q.take().unwrap();
                // A newer sheet replaces the one shown without a second fade-in.
                let since = gal.sheet.map_or(now, |(old, t)| {
                    gpu.delete_texture(old);
                    t
                });
                gal.sheet = Some((tex, since));
                need_build = true;
                eprintln!("UI_GALLERY thumbnails ready");
            }
        }
        if apps_dirty {
            if let Some(hm) = home.as_mut() {
                hm.set_apps(&app_list, now);
                hm.set_recents(&app_list, &recents, now);
                apps_dirty = false;
                need_build = true;
            }
        }
        if notice.as_ref().is_some_and(|n| now > n.1 + Duration::from_secs_f32(home::NOTICE_SECS + 0.2)) {
            notice = None;
            need_build = true;
        }
        if demo_ms > 0 && intro.is_none() && ready_at.is_some() && now >= demo_at {
            events.push(input::Input::Key(demo_keys[demo_i % demo_keys.len()]));
            demo_i += 1;
            demo_at = now + Duration::from_millis(demo_ms);
        }
        let mut cmds: Vec<setview::Cmd> = Vec::new();
        for ev in events.drain(..) {
            match ev {
                input::Input::Key(k) => {
                    last_input = now;
                    if suspended.is_some() {
                        // An app has the screen: Home is the system's, everything else is the app's.
                        appd.send(&if k == Key::Home { "HOME".to_string() } else { format!("KEY\t{}", k.name()) });
                        continue;
                    }
                    if restart_at.is_some() {
                        continue; // fading out for a restart
                    }
                    if dim.target() > 0.0 || dim.value(now) > 0.0 {
                        // Dimmed or asleep: the first key only wakes the screen.
                        dim.go(0.0, now, Duration::from_millis(260));
                        need_build = true;
                        continue;
                    }
                    // (Before the transition check: an update's question comes while the
                    // launch transition of the app is already waiting.)
                    if let Some(d) = consent.as_mut() {
                        if let Some(allow) = d.key(k) {
                            appd.send(&format!("CONSENT\t{}\t{}", d.id, if allow { "allow" } else { "deny" }));
                            eprintln!("UI_APPS consent {} {}", d.id, if allow { "allow" } else { "deny" });
                            // Installing goes ahead unless a required permission was refused.
                            let proceeds = allow || !d.perms.iter().any(|p| p.sensitive && p.required);
                            if proceeds && launch.as_ref().is_none_or(|l| l.id != d.id) {
                                let app = app_list.iter().find(|a| a.id == d.id);
                                launch = home.as_ref().and_then(|hm| new_launch(hm, &d.id, now, launch::Phase::Opening, app));
                            }
                            consent = None;
                        }
                        need_build = true;
                        continue;
                    }
                    if launch.is_some() {
                        continue; // a transition is running: navigation waits
                    }
                    if rst.active() {
                        continue; // a factory-reset screen: only the button on the dongle counts
                    }
                    // The volume pop-up, opened from the top bar, has the keys.
                    if vol_pop.has_keys(now) {
                        need_build = true;
                        match vol_pop.key(k, now) {
                            volpop::PopKey::Request(what) => {
                                appd.send(&format!("VOLUME\t{what}"));
                                continue;
                            }
                            volpop::PopKey::Closed => continue,
                            volpop::PopKey::Pass => {} // Home: on to the view below
                        }
                    }
                    // The setup prompt covers everything: Back (or OK) hides it.
                    if ob.visible() {
                        if matches!(k, Key::Back | Key::Enter) {
                            ob.dismiss(&net, now);
                        }
                        need_build = true;
                        continue;
                    }
                    let ctx = setview::Ctx { settings: &cfg, info: &sysinfo, apps: &app_list, appd: appd.connected(), output_now: kms.name, planes, net: &net, gallery: &gal, wall_busy: !wset_real && wall_name != "procedural", volume: volume.map(|v| v.0), details: &app_details };
                    if sv.is_open() {
                        // Close at once: the next keys of the same burst go to the home screen.
                        match sv.key(k, now, &ctx) {
                            Some(setview::Cmd::Close) => {
                                sv.close(now);
                                if let Some(hm) = home.as_mut() {
                                    hm.set_away(false, now);
                                }
                            }
                            c => cmds.extend(c),
                        }
                        need_build = true;
                        continue;
                    }
                    // Navigation starts once the hand-over is complete.
                    if let (Some(hm), None) = (home.as_mut(), intro.as_ref()) {
                        match hm.key(k, now) {
                            Some(home::Action::Launch(id)) => {
                                if appd.connected() {
                                    eprintln!("UI_APPS launch {id}");
                                    appd.send(&format!("LAUNCH\t{id}"));
                                    // Not installed yet: the consent dialog comes first.
                                    let app = app_list.iter().find(|a| a.id == id);
                                    if app.is_some_and(|a| a.state != "available") {
                                        launch = new_launch(hm, &id, now, launch::Phase::Opening, app);
                                    }
                                }
                            }
                            Some(home::Action::Settings(cat, into)) => {
                                sysinfo = settings::sample();
                                info_at = now + Duration::from_secs(2);
                                let ctx = setview::Ctx { settings: &cfg, info: &sysinfo, apps: &app_list, appd: appd.connected(), output_now: kms.name, planes, net: &net, gallery: &gal, wall_busy: !wset_real && wall_name != "procedural", volume: volume.map(|v| v.0), details: &app_details };
                                let cat = cat.unwrap_or(sv.cat());
                                sv.open_at(cat, into, now, &ctx);
                                hm.set_away(true, now);
                                eprintln!("UI_SETTINGS open {cat:?}");
                            }
                            Some(home::Action::Volume) => match volume {
                                Some((v, muted)) => vol_pop.open(v, muted, now),
                                None => notice = Some(("The volume needs the app service, which is not running".into(), now)),
                            },
                            Some(home::Action::Sleep) => {
                                sleeping = true;
                                dim.go(1.0, now, Duration::from_millis(450));
                            }
                            None => {}
                        }
                        need_build = true;
                    }
                }
                input::Input::Button(b) => {
                    use input::Button;
                    eprintln!("UI_BUTTON {b:?}");
                    if restart_at.is_some() {
                        continue;
                    }
                    // Every step is shown: the screen wakes, and an app in front makes way
                    // (the daemon does not wait for either; the reset never depends on us).
                    last_input = now;
                    if !matches!(b, Button::Cancelled | Button::TooLong) {
                        if dim.target() > 0.0 || dim.value(now) > 0.0 {
                            dim.go(0.0, now, Duration::from_millis(260));
                        }
                        if suspended.is_some() {
                            appd.send("HOME");
                        }
                    }
                    need_build = true;
                    match b {
                        Button::Short if rst.active() => {}
                        Button::Short if ob.visible() => ob.dismiss(&net, now),
                        Button::Short if !net.running() => notice = Some(("The network service is not running".into(), now)),
                        Button::Short if !net.available() => notice = Some(("Pairing a phone needs Wi-Fi: use the dongle's original cable".into(), now)),
                        Button::Short => {
                            // No working Wi-Fi: the setup network comes first, as in Settings.
                            if net.adapter() && !net.connected() && !net.setup() {
                                onboard::ctl("setup");
                            }
                            vol_pop.hide();
                            ob.force(&net, now);
                        }
                        Button::Warn => {
                            vol_pop.hide();
                            rst.warn(now);
                        }
                        Button::Armed => rst.armed(now),
                        Button::Cancelled | Button::TooLong => {
                            if rst.active() {
                                rst.cancel(now);
                                notice = Some(("Factory reset cancelled: nothing was erased".into(), now));
                            }
                        }
                        Button::Erasing => rst.erasing(now),
                    }
                }
                input::Input::Quit => STOP.store(true, Ordering::Relaxed),
                input::Input::Shot => {
                    shot_now = true;
                    need_build = true;
                }
            }
        }
        cmds.extend(sv.tick(now));
        if rst.tick(now) {
            need_build = true;
        }
        for cmd in cmds {
            use setview::Cmd;
            need_build = true;
            match cmd {
                Cmd::Close => {
                    sv.close(now);
                    if let Some(hm) = home.as_mut() {
                        hm.set_away(false, now);
                    }
                }
                Cmd::SetOutput(o) => {
                    eprintln!("UI_SETTINGS output {} -> {o} (restart)", kms.name);
                    cfg.confirm = Some(kms.name.to_string());
                    cfg.output = o;
                    if let Err(e) = cfg.save_now() {
                        eprintln!("UI_WARN settings: {e}");
                    }
                    restart_at = Some(now + Duration::from_millis(300));
                    dim.go(1.0, now, Duration::from_millis(260));
                }
                Cmd::KeepOutput => {
                    cfg.confirm = None;
                    saver.save(&cfg);
                    notice = Some((format!("Resolution set to {}", cfg.output_label()), now));
                }
                Cmd::RevertOutput => {
                    let prev = cfg.confirm.take().unwrap_or_else(|| "1080p60".into());
                    eprintln!("UI_SETTINGS output {} reverted to {prev}", cfg.output);
                    cfg.output = prev;
                    if let Err(e) = cfg.save_now() {
                        eprintln!("UI_WARN settings: {e}");
                    }
                    restart_at = Some(now + Duration::from_millis(300));
                    dim.go(1.0, now, Duration::from_millis(260));
                }
                Cmd::SetZone(tz) => {
                    cfg.tz = tz;
                    cfg.apply_tz();
                    let next;
                    (clock_s, date_s, next) = clock(cfg.clock24);
                    minute_at = now + Duration::from_secs(next);
                    saver.save(&cfg);
                }
                Cmd::SetVolume(v) => {
                    // appd owns the volume: it saves it, tells every app, and reports it back.
                    appd.send(&format!("VOLUME\t{v}"));
                }
                Cmd::SetBackground(id, on) => {
                    if appd.connected() {
                        appd.send(&format!("BACKGROUND\t{id}\t{}", if on { "on" } else { "off" }));
                        // Show the switch as set at once; appd's APPINFO confirms (or corrects).
                        if let Some(d) = app_details.get_mut(&id) {
                            d.background = if on { apps::Background::On } else { apps::Background::Off };
                        }
                    } else {
                        notice = Some(("The app manager is not running".into(), now));
                    }
                }
                Cmd::SetDim(m) => {
                    cfg.dim_min = m;
                    last_input = now;
                    saver.save(&cfg);
                }
                Cmd::ToggleDepth => {
                    cfg.depth = !cfg.depth;
                    saver.save(&cfg);
                }
                Cmd::ToggleClock24 => {
                    cfg.clock24 = !cfg.clock24;
                    let next;
                    (clock_s, date_s, next) = clock(cfg.clock24);
                    minute_at = now + Duration::from_secs(next);
                    saver.save(&cfg);
                }
                Cmd::ToggleHud => {
                    cfg.hud = !cfg.hud;
                    (hud_at, hud_flips, hud_n, hud_late, hud_ticks, hud_t) = (now, kms.flips, 0, 0, cpu_ticks(), now);
                    hud_text.clear();
                    saver.save(&cfg);
                }
                Cmd::Restart => {
                    eprintln!("UI_SETTINGS restart");
                    restart_at = Some(now + Duration::from_millis(300));
                    dim.go(1.0, now, Duration::from_millis(260));
                }
                Cmd::WifiScan => {
                    sv.wifi_scan_started(now);
                    sysinfo = settings::sample();
                    eprintln!("UI_SETTINGS wifi adapters: {:?}", sysinfo.wifi);
                }
                Cmd::WifiSetup | Cmd::WifiPair => {
                    if matches!(cmd, Cmd::WifiSetup) && !onboard::ctl("setup") {
                        notice = Some(("The network service is not running".into(), now));
                    } else {
                        ob.force(&net, now);
                    }
                }
                Cmd::WifiForget => {
                    if onboard::ctl("forget") {
                        notice = Some((format!("Forgot {}", net.get("ssid")), now));
                    }
                }
                Cmd::Unpair => {
                    if onboard::ctl("unpair") {
                        notice = Some(("Phones unpaired".into(), now));
                    }
                }
                Cmd::ToggleNetDebug => {
                    let on = !net.debug();
                    if !onboard::ctl(if on { "debug on" } else { "debug off" }) {
                        notice = Some(("The network service is not running".into(), now));
                    }
                    eprintln!("UI_SETTINGS network debugging {}", if on { "on" } else { "off" });
                }
                Cmd::ToggleDebugLan => {
                    onboard::ctl(if net.get("debug_lan") == "1" { "debug lan off" } else { "debug lan on" });
                }
                Cmd::ToggleDebugCloud => {
                    onboard::ctl(if net.get("debug_cloud") == "1" { "debug cloud off" } else { "debug cloud on" });
                }
                Cmd::DebugPair => { onboard::ctl("debug pair"); }
                Cmd::DebugRevoke(id) => {
                    if onboard::ctl(&format!("debug revoke {id}")) { notice = Some(("Developer access revoked".into(), now)); }
                }
                Cmd::SetWallpaper(asset) => {
                    cfg.wallpaper = asset.clone();
                    saver.save(&cfg);
                    wall_switch = Some(asset);
                }
                Cmd::FactoryReset => {
                    eprintln!("UI_SETTINGS factory reset");
                    if reset::run("settings") {
                        // /bin/factory-reset stops this launcher in a second.
                        rst.erasing(now);
                    } else {
                        notice = Some(("Factory reset is not available in this build".into(), now));
                    }
                }
                Cmd::Uninstall(id) => {
                    let name = app_list.iter().find(|a| a.id == id).map_or(id.clone(), |a| a.name.clone());
                    if appd.connected() {
                        appd.send(&format!("UNINSTALL\t{id}"));
                        pending_uninstall = Some(name);
                    } else {
                        notice = Some(("The app manager is not running".into(), now));
                    }
                }
            }
        }
        // Settings that arrived late (/data mounted after the start).
        if cfg_retry.is_some_and(|t| now >= t) {
            cfg_retry = if settings::data_mounted() || now >= cfg_retry_end { None } else { Some(now + Duration::from_secs(1)) };
            if cfg_retry.is_none() {
                let (c, found) = settings::Settings::load(cfg.clone());
                if found {
                    // The output applies at the next start (no surprise restart now).
                    cfg = settings::Settings { output: cfg.output.clone(), confirm: None, ..c };
                    cfg.apply_tz();
                    let next;
                    (clock_s, date_s, next) = clock(cfg.clock24);
                    minute_at = now + Duration::from_secs(next);
                    need_build = true;
                    eprintln!("UI_SETTINGS loaded late: tz={} clock24={} wallpaper={}", cfg.tz, cfg.clock24, cfg.wallpaper);
                    if wall_env.is_none() {
                        wall_switch = Some(cfg.wallpaper.clone());
                    }
                }
            }
        }
        // Another wallpaper (chosen in Settings, settings loaded late, or the chosen one is
        // gone): ask appd for it. Its preview comes at once and cross-fades in over the
        // current wallpaper, then the downloaded picture pulls into focus.
        if let Some(name) = wall_switch.take().filter(|n| *n != wall_name) {
            eprintln!("UI_WALL {wall_name} -> {name}");
            wall_name = name;
            wall_gen += 1;
            wall_quick = !planes;
            (wset_real, wset_preview) = (false, false);
            if let Some((_, a, b, _, _)) = wset_q.take() {
                gpu.delete_texture(a);
                gpu.delete_texture(b);
            }
            appd.send(&format!("ASSET\t{wall_name}"));
            need_build = true;
        }
        // Back after a resolution change: keep it, or it reverts on its own.
        if confirm_check && ready_at.is_some() && intro.is_none() {
            confirm_check = false;
            if let (Some(prev), Some(hm)) = (cfg.confirm.clone(), home.as_mut()) {
                if kms.name != cfg.output {
                    // The TV does not offer the mode (kms.rs fell back).
                    notice = Some((format!("This TV does not offer {}", cfg.output_label()), now));
                    cfg.output = kms.name.to_string();
                    cfg.confirm = None;
                    saver.save(&cfg);
                } else {
                    sysinfo = settings::sample();
                    let ctx = setview::Ctx { settings: &cfg, info: &sysinfo, apps: &app_list, appd: appd.connected(), output_now: kms.name, planes, net: &net, gallery: &gal, wall_busy: !wset_real && wall_name != "procedural", volume: volume.map(|v| v.0), details: &app_details };
                    sv.open_at(setview::Cat::Display, true, now, &ctx);
                    sv.focus_id(&setview::RowId::Output, &ctx, now);
                    hm.set_away(true, now);
                    // The countdown starts when the TV can show a picture again.
                    let sync = if kms.kept_mode { 0.0 } else { tv_sync };
                    let deadline = now + Duration::from_secs_f64(sync + setview::KEEP_SECS as f64);
                    let body = format!("{}. Going back to {} in {{s}} s.", cfg.output_label(), settings::output_label(&prev));
                    sv.show_dialog(setview::DialogKind::KeepOutput, "Keep this resolution?".into(), body, ["Keep", "Revert"], Some(deadline), now);
                }
                need_build = true;
            }
        }
        if sv.dialog.as_ref().is_some_and(|d| d.deadline.is_some()) {
            need_build = true; // the countdown (only its line repaints)
        }
        if vol_pop.tick(now) {
            need_build = true; // it has faded out: its last frame goes
        }
        if let Some(hm) = home.as_mut() {
            let open = vol_pop.has_keys(now);
            if hm.volume_popup != open {
                hm.volume_popup = open;
                need_build = true;
            }
        }
        if now >= net_at {
            if let Some((seq, level, text)) = thermal_state() {
                if level != heat_level {
                    eprintln!("UI_HEAT level {level}");
                    heat_level = level;
                }
                if seq != heat_seq {
                    heat_seq = seq;
                    heat_notice = Some(text).filter(|t| !t.is_empty());
                }
            }
            // While an app has the screen the notice waits (the daemon closes the app first).
            if suspended.is_none() && let Some(t) = heat_notice.take() {
                eprintln!("UI_HEAT notice {t}");
                if dim.target() > 0.0 || dim.value(now) > 0.0 {
                    dim.go(0.0, now, Duration::from_millis(260));
                }
                last_input = now;
                notice = Some((t, now));
                need_build = true;
            }
            net_at = now + Duration::from_millis(700);
            if let Some(fresh) = onboard::NetState::load() {
                if fresh != net {
                    let pilot = home::Pilot::of(fresh.pilots(), fresh.get("online").parse().unwrap_or(0), fresh.get("pilot_connected"));
                    if home.as_mut().is_some_and(|hm| hm.set_pilot(pilot)) {
                        eprintln!("UI_NET pilot {pilot:?}");
                    }
                    if net.running() && fresh.pilots() > net.pilots() {
                        notice = Some(("Phone paired: it is now a remote for this TV".into(), now));
                    }
                    if fresh.get("wifi") != net.get("wifi") || fresh.get("setup") != net.get("setup") || fresh.pilots() != net.pilots() {
                        eprintln!("UI_NET wifi={} ssid={} ip={} setup={} pilots={} adapter={}", fresh.get("wifi"), fresh.get("ssid"), fresh.get("ip"), fresh.get("setup"), fresh.pilots(), fresh.get("adapter"));
                    }
                    net = fresh;
                    ob.update(&net, now);
                    need_build = true;
                }
            }
        }
        let want_window = ob.visible();
        if want_window != pair_window || (want_window && now >= pair_renew_at) {
            if onboard::ctl(if want_window { "pair-open" } else { "pair-close" }) || !want_window {
                pair_window = want_window;
            }
            pair_renew_at = now + Duration::from_secs(300);
        }
        if sv.is_open() && now >= info_at {
            info_at = now + Duration::from_secs(2);
            let fresh = settings::sample();
            if fresh != sysinfo {
                sysinfo = fresh;
                need_build = true;
            }
        }
        // Idle dimming.
        if cfg.dim_min > 0
            && dim.target() == 0.0
            && suspended.is_none()
            && launch.is_none()
            && consent.is_none()
            && sv.dialog.is_none()
            && now >= last_input + Duration::from_secs(cfg.dim_min as u64 * 60)
        {
            dim.go(DIM_IDLE, now, Duration::from_millis(1200));
            dimmed_at = Some(now);
            eprintln!("UI_DIM idle {} min", cfg.dim_min);
        }
        // Sleep. Whatever lit the screen again above (a key, a notice, an app coming to the
        // front) ends it; the output is back on before anything is drawn or handed over.
        if dim.target() == 0.0 {
            dimmed_at = None;
        }
        if sleeping && dim.target() < 1.0 {
            sleeping = false;
            if kms.is_off() {
                match kms.set_output(true) {
                    Ok(()) => eprintln!("UI_SLEEP wake: output on"),
                    Err(e) => eprintln!("UI_ERROR output on: {e}"),
                }
                if suspended.is_none() && launch.is_none() {
                    dim = util::Tween::fixed(1.0);
                    wake_fade_at = Some(now + WAKE_SYNC);
                    // A notice that woke the screen is shown once the picture is back.
                    if let Some(n) = notice.as_mut() {
                        n.1 = now + WAKE_SYNC;
                    }
                }
                force_full = true;
                need_build = true;
            }
        }
        if wake_fade_at.is_some_and(|t| now >= t) {
            wake_fade_at = None;
            if !sleeping && restart_at.is_none() {
                dim.go(0.0, now, Duration::from_millis(500));
                last_input = now;
            }
        }
        if !sleeping
            && restart_at.is_none()
            && suspended.is_none()
            && launch.is_none()
            && consent.is_none()
            && sv.dialog.is_none()
            && dim.target() == DIM_IDLE
            && dimmed_at.is_some_and(|t| now >= t + SLEEP_AFTER_DIM)
        {
            sleeping = true;
            dim.go(1.0, now, Duration::from_millis(1200));
            eprintln!("UI_SLEEP idle");
        }
        if sleeping && !kms.is_off() && dim.target() >= 1.0 && !dim.active(now) {
            if suspended.is_some() || launch.is_some() {
                sleeping = false;
            } else {
                match kms.set_output(false) {
                    Ok(()) => eprintln!("UI_SLEEP output off"),
                    Err(e) => {
                        // Stays a black frame, as before.
                        eprintln!("UI_WARN output off: {e}");
                        sleeping = false;
                    }
                }
            }
        }
        if restart_at.is_some_and(|t| now >= t) && !dim.active(now) {
            exit_code = EXIT_RESTART;
            break;
        }
        if cfg.hud && now >= hud_at {
            let dt = now.duration_since(hud_t).as_secs_f64();
            let ticks = cpu_ticks();
            hud_text = hud_line(kms.flips - hud_flips, dt, hud_n, hud_late, (ticks - hud_ticks) as f64 / dt.max(0.001));
            (hud_at, hud_flips, hud_n, hud_late, hud_ticks, hud_t) = (now + Duration::from_secs(1), kms.flips, 0, 0, ticks, now);
            need_build = true;
        }
        // Boost only while the panel moves or fades (not while waiting or in the app).
        // Also while the home screen animates (input boost): devfreq would otherwise start every
        // 300 ms scroll at the 200 MHz idle clock (full-screen frames then take ~21 ms).
        boost.set(suspended.is_none()
            && heat_level < 2
            && (launch.as_ref().is_some_and(|l| matches!(l.phase, launch::Phase::Opening | launch::Phase::Fading | launch::Phase::Closing))
                || wset_fade.is_some()
                || sv.animating(now)
                || vol_pop.animating(now)
                || dim.active(now)
                || home.as_ref().is_some_and(|h| h.animating(now))));
        if shot_times.front().is_some_and(|s| now >= *s) {
            shot_times.pop_front();
            shot_now = true;
            need_build = true;
        }
        // The wall clock is stepped when the network sets it (no RTC: boots start in 1970):
        // show the new time at once, not at the next minute of the old clock.
        let wall = wall_minus_mono();
        let stepped = (wall - wall_base).abs() > 0.5;
        if now >= minute_at || stepped {
            if stepped {
                eprintln!("UI_CLOCK stepped by {:+.1} s", wall - wall_base);
            }
            wall_base = wall;
            let next;
            (clock_s, date_s, next) = clock(cfg.clock24);
            minute_at = now + Duration::from_secs(next);
            need_build = true;
        }
        if now >= stats_at {
            let dt = now.duration_since(st_t).as_secs_f64();
            let nf = frames - st_frames;
            let ticks = cpu_ticks();
            eprintln!(
                "UI_STATS t={} fps={:.1} frames={} anim_ms={:.1} anim_max_ms={:.1} late_pct={:.0}{} repaint_pct={:.1} cpu_pct={:.0} rss_kb={} hwm_kb={} gpu_hz={} temp_mc={}",
                t0.elapsed().as_secs(),
                (kms.flips - st_flips) as f64 / dt,
                frames,
                if iv_n > 0 { iv_sum * 1000.0 / iv_n as f64 } else { 0.0 },
                iv_max * 1000.0,
                if iv_n > 0 { iv_late as f64 * 100.0 / iv_n as f64 } else { 0.0 },
                if profile && nf > 0 { format!(" build_ms={:.1} gpu_ms={:.1}", pr_cpu * 1000.0 / nf as f64, pr_gpu * 1000.0 / nf as f64) } else { String::new() },
                if nf > 0 { st_px as f64 * 100.0 / (nf as f64 * full.area() as f64) } else { 0.0 },
                (ticks - st_ticks) as f64 / dt, // CLK_TCK = 100 -> percent
                proc_status("VmRSS:"),
                proc_status("VmHWM:"),
                read_trim("/sys/class/devfreq/20000000.gpu/cur_freq"),
                read_trim("/sys/class/thermal/thermal_zone0/temp"),
            );
            (st_frames, st_flips, st_px, st_ticks, st_t) = (frames, kms.flips, 0, ticks, now);
            (tot_n, tot_late, tot_sum, tot_max) = (tot_n + iv_n, tot_late + iv_late, tot_sum + iv_sum, tot_max.max(iv_max));
            (tot_gpu, tot_pf) = (tot_gpu + pr_gpu, tot_pf + nf);
            (iv_n, iv_sum, iv_late, iv_max, pr_cpu, pr_gpu) = (0, 0.0, 0, 0.0, 0.0, 0.0);
            stats_at = now + Duration::from_secs(1);
        }
        if suspended.is_none() || surface.is_none() || !trim {
            trim_at = None;
            trim_tries = 0;
        } else if trim_at.is_none() {
            trim_at = Some(now + Duration::from_millis(300));
        } else if trim_at.is_some_and(|t| now >= t) {
            let extra: Vec<_> = wall_plane.as_ref().map(|p| p.1).into_iter().collect();
            if kms.foreign_scanout() && gpu.detach(&extra) {
                let n = kms.free_buffers();
                wall_plane = None;
                surface = None;
                unsafe { libc::malloc_trim(0) };
                eprintln!("UI_TRIM freed framebuffers={n}");
            } else {
                // The app has not drawn yet (or never will: a service, a crash).
                trim_tries += 1;
                trim_at = Some(now + Duration::from_millis(if trim_tries < 40 { 250 } else { 5000 }));
            }
        }
        // Nothing to show yet (no intro): keep loading. An app has the screen: draw nothing.
        if (intro.is_none() && ready_at.is_none()) || suspended.is_some() {
            continue;
        }
        if surface.is_none() {
            // Back from an app after a trim: new scanout buffers, everything repainted.
            let t_s = Instant::now();
            let s = kms
                .gbm
                .create_surface::<drm::control::framebuffer::Handle>(w, h, format, usage)
                .unwrap_or_else(|e| fail(&format!("GBM surface: {e}")));
            gpu.attach(&s).unwrap_or_else(|e| fail(&e));
            surface = Some(s);
            if planes {
                let s = kms
                    .gbm
                    .create_surface::<drm::control::framebuffer::Handle>(wall_size.0, wall_size.1, gbm::Format::Argb8888, usage)
                    .unwrap_or_else(|e| fail(&format!("GBM wallpaper surface: {e}")));
                let e = gpu.window_surface(&s).unwrap_or_else(|e| fail(&e));
                wall_plane = Some((s, e));
            }
            prev_wall_key = None;
            history.clear();
            force_full = true;
            need_build = true;
            prime = true;
            // The return animation starts now, not when the screen came back.
            if let Some(l) = launch.as_mut() {
                l.t0 += t_s.elapsed();
            }
            eprintln!("UI_TRIM surfaces back ms={:.1}", t_s.elapsed().as_secs_f64() * 1000.0);
        }
        let intro_live = intro.as_ref().is_some_and(|it| it.started(now));
        let overlay_anim = home.as_ref().is_some_and(|h| h.overlay_animating(now, consent.as_ref(), notice.as_ref()));
        let launch_anim = launch.as_ref().is_some_and(|l| l.animating(now));
        let view_anim = sv.animating(now) || ob.animating(now) || rst.animating(now) || dim.active(now) || (sv.visible(now) && gal.fading(now)) || vol_pop.animating(now);
        if kms.is_off() {
            // Asleep: nothing can be presented. `need_build` stays set for the wake-up frame.
            last_present = None;
            continue;
        }
        if !(need_build || intro_live || overlay_anim || launch_anim || view_anim || wset_fade.is_some() || home.as_ref().is_some_and(|h| h.animating(now))) {
            last_present = None; // idle: the next frame starts a new animation
            if suspended.is_none() && launch.is_none() && intro.is_none() {
                if let (Some(hm), Some(tx)) = (home.as_mut(), text.as_mut()) {
                    let t_pw = Instant::now();
                    if hm.prewarm_launch(&gpu, &mut atlas, tx) {
                        eprintln!("UI_LAUNCH prewarm ms={:.1}", t_pw.elapsed().as_secs_f64() * 1000.0);
                        continue;
                    }
                }
            }
            if suspended.is_none() && intro.is_none() && ready_at.is_some() && vol_pop.prewarm(&gpu, volume) {
                continue;
            }
            if prefetch && events.is_empty() {
                let t_pf = Instant::now();
                if let (Some(hm), Some(tx)) = (home.as_mut(), text.as_mut()) {
                    prefetching = hm.prefetch_one(&gpu, &mut atlas, tx);
                    if !prefetching {
                        eprintln!(
                            "UI_PREFETCH_DONE ms={} last_item_ms={:.1} atlas_used_pct={:.1} atlas_rows_pct={:.1} entries={} glyphs={} badges={}",
                            t0.elapsed().as_millis(),
                            t_pf.elapsed().as_secs_f64() * 1000.0,
                            atlas.used_px as f64 * 100.0 / (atlas.size as f64 * atlas.size as f64),
                            atlas.rows_used_pct(),
                            atlas.entries,
                            tx.rasterised,
                            hm.icons_rendered
                        );
                    }
                }
            }
            continue;
        }

        // Build, diff, and redraw only what changed.
        need_build = false;
        let t_build = Instant::now();
        // Stall tracing: phase end times of this frame (logged when an animated frame
        // interval exceeds 60 ms).
        let (t_wall, t_draw, t_swap);
        // The home screen is drawn once it is ready and (with an intro) handed over.
        let show_home = ready_at.is_some() && intro.as_ref().is_none_or(|it| it.handing_over());
        let build_cost = (text.as_ref().map_or(0, |t| t.rasterised), home.as_ref().map_or(0, |h| h.icons_rendered), atlas.entries);
        let mut hframe = match (show_home, home.as_mut(), text.as_mut()) {
            (true, _, Some(tx)) if sync_test => {
                need_build = true; // redraw every frame
                let up = read_trim("/proc/uptime");
                let up = up.split_whitespace().next().unwrap_or("?");
                let mut f = home::Frame::default();
                let st = f.verts.len();
                let px = 180.0 * scale;
                let tw = tx.width(text::Face::Light, px, up);
                tx.draw(&gpu, &mut atlas, &mut f.verts, text::Face::Light, px, (w as f32 - tw) / 2.0, h as f32 * 0.58, util::rgba(0xffffff, 1.0), up);
                let label = "seconds since kernel start";
                let lw = tx.width(text::Face::Regular, 40.0 * scale, label);
                tx.draw(&gpu, &mut atlas, &mut f.verts, text::Face::Regular, 40.0 * scale, (w as f32 - lw) / 2.0, h as f32 * 0.70, util::rgba(0xffffff, 0.7), label);
                f.end(1, st);
                f
            }
            (true, Some(hm), Some(tx)) => hm.build(&gpu, &mut atlas, tx, now, &clock_s, &date_s),
            _ => home::Frame::default(),
        };
        let mut occluder = Rect::EMPTY;
        if let (true, Some(l), Some(hm), Some(tx)) = (show_home && !sync_test, launch.as_mut(), home.as_mut(), text.as_mut()) {
            occluder = hm.build_launch(&gpu, &mut atlas, tx, now, l, w, h, &mut hframe);
        }
        if let (true, Some(hm), Some(tx)) = (show_home && !sync_test && sv.visible(now), home.as_ref(), text.as_mut()) {
            let ctx = setview::Ctx { settings: &cfg, info: &sysinfo, apps: &app_list, appd: appd.connected(), output_now: kms.name, planes, net: &net, gallery: &gal, wall_busy: !wset_real && wall_name != "procedural", volume: volume.map(|v| v.0), details: &app_details };
            sv.build(&gpu, &mut atlas, tx, now, &ctx, &hm.slices(), &mut hframe);
        }
        if let (true, Some(hm), Some(tx)) = (show_home && !sync_test, home.as_ref(), text.as_mut()) {
            ob.build(&gpu, &mut atlas, tx, now, &net, &hm.slices(), scale, &mut hframe);
        }
        if let (true, Some(hm), Some(tx)) = (show_home && !sync_test, home.as_mut(), text.as_mut()) {
            hm.build_overlay(&gpu, &mut atlas, tx, now, consent.as_ref(), notice.as_ref(), &mut hframe);
            let sl = hm.slices();
            vol_pop.build(&gpu, &atlas, now, (w, h), &sl, &mut hframe);
            // Above everything but the dim layer.
            rst.build(&gpu, &mut atlas, tx, now, &sl, scale, &mut hframe);
            let (cr, cm) = sl.card;
            if cfg.hud && !hud_text.is_empty() {
                let px = 21.0 * scale;
                let tw = tx.width(text::Face::Regular, px, &hud_text);
                let (x0, y0) = ((28.0 * scale).round(), ((1080.0 - 28.0 - 44.0) * scale).round());
                let st = hframe.begin();
                home::nine(&mut hframe.verts, &atlas, &cr, cm, util::RectF::new(x0, y0, tw + 36.0 * scale, 44.0 * scale), cm as f32 * scale / sl.raster, util::rgba(0x05060c, 0.72));
                tx.draw(&gpu, &mut atlas, &mut hframe.verts, text::Face::Regular, px, (x0 + 18.0 * scale).round(), (y0 + 29.0 * scale).round(), util::rgba(0xffffff, 0.92), &hud_text);
                hframe.end(ID_HUD, st);
            }
            let d = dim.value(now);
            if d > 0.0 {
                let st = hframe.begin();
                let a = if d >= 0.999 { 1.0 } else { d };
                home::solid(&mut hframe.verts, &atlas, &cr, cm, 0.0, 0.0, w as f32, h as f32, util::rgba(0x000000, a));
                hframe.end(ID_DIM, st);
            }
        }
        let iframe = match intro.as_ref() {
            Some(it) => it.build(now, home.as_ref().filter(|_| it.handing_over()).map(|hm| hm.focus_ring(now))),
            None => home::Frame::default(),
        };
        let wall_alpha = if !show_home || (wall_tex.is_none() && wset.is_none()) { 0.0 } else { intro.as_ref().map_or(1.0, |it| it.wall_alpha(now)) };
        // Wallpaper layers bottom-up: (texture, opacity). At rest one texture; two while the
        // depth (sharp <-> soft) cross-fades; plus the built-in one while the asset fades in.
        let mut layers: Vec<(glow::Texture, f32)> = Vec::with_capacity(3);
        let fade = match (wset, wset_fade) {
            (Some(_), Some((t, ms))) => {
                let x = (now.duration_since(t).as_secs_f32() * 1000.0 / ms as f32).min(1.0);
                x * x * (3.0 - 2.0 * x) // ease in and out
            }
            (Some(_), None) => 1.0,
            _ => 0.0,
        };
        if fade < 1.0 {
            layers.extend(wall_tex.map(|t| (t, 1.0)));
        }
        if let (Some((sharp, soft)), true) = (wset, fade > 0.0) {
            // Depth off (Settings): the sharp layer everywhere, except behind the Settings view.
            let blur = match (wall_depth && cfg.depth, home.as_ref()) {
                (true, Some(hm)) => hm.wall_blur(now),
                (false, Some(hm)) => hm.wall_blur(now).min(hm.away(now)),
                _ => 0.0,
            };
            if blur < 1.0 {
                layers.push((sharp, fade));
            }
            if blur > 0.0 {
                layers.push((soft, if blur >= 1.0 { fade } else { blur * fade }));
            }
        }
        let wall_key: Vec<(u32, u16)> = layers.iter().map(|(t, a)| (t.0.get(), (a * wall_alpha * 1000.0) as u16)).collect();
        let mut damage = diff(&mut prev_home, &hframe).union(&diff(&mut prev_intro, &iframe)).intersect(&full);
        // GPU-composited wallpaper: a wallpaper change repaints the whole screen. Plane mode:
        // only the wallpaper plane's quarter-size surface is redrawn; the UI keeps its frame.
        let wall_changed = prev_wall_key.as_ref() != Some(&wall_key);
        if wall_changed {
            if !planes {
                damage = full;
            }
            prev_wall_key = Some(wall_key);
        }
        if damage.is_empty() && shot_now {
            // Screenshots capture a normal *partial* frame (a 32x32 repaint), so the image
            // also verifies that buffer-age reconstruction left no stale tiles.
            damage = Rect::new(0, 0, 32, 32);
        }
        // force_full: back from an app, whose last frame is still on screen.
        let draw_ui = !damage.is_empty() || first_frame || force_full;
        let draw_wall = planes && (wall_changed || first_frame);
        if !draw_ui && !draw_wall {
            continue;
        }
        let t_built = Instant::now();
        if t_built.duration_since(t_build) > Duration::from_millis(12) {
            // A slow build: say what it rasterised (glyphs, badges, atlas entries).
            eprintln!(
                "UI_SLOW_BUILD ms={:.1} glyphs+={} badges+={} atlas_entries+={} atlas_used_pct={:.1}",
                t_built.duration_since(t_build).as_secs_f64() * 1000.0,
                text.as_ref().map_or(0, |t| t.rasterised) - build_cost.0,
                home.as_ref().map_or(0, |h| h.icons_rendered) - build_cost.1,
                atlas.entries - build_cost.2,
                atlas.used_px as f64 * 100.0 / (atlas.size as f64 * atlas.size as f64)
            );
        }
        if draw_wall {
            if let Some((_, ws)) = wall_plane.as_ref() {
                gpu.bind(Some(*ws), wall_size);
                draw_wall_layers(&mut gpu, &layers, wall_alpha, w, h, &mut wall_quads);
                if let Err(e) = gpu.swap_target(*ws) {
                    eprintln!("UI_ERROR {e}");
                    break;
                }
                gpu.bind(None, (w, h));
            }
        }
        if !draw_ui {
            if profile {
                let t_gpu = Instant::now();
                pr_cpu += t_gpu.duration_since(t_build).as_secs_f64();
                gpu.finish();
                pr_gpu += t_gpu.elapsed().as_secs_f64();
            }
            if let Err(e) = kms.present_planes(None, wall_plane.as_ref().map(|p| &p.0), wall_size) {
                eprintln!("UI_ERROR present: {e}");
                break;
            }
            frames += 1;
            let tp = Instant::now();
            if let Some(lp) = last_present {
                let iv = tp.duration_since(lp).as_secs_f64();
                iv_n += 1;
                iv_sum += iv;
                iv_max = iv_max.max(iv);
                hud_n += 1;
                if iv > period * 1.5 {
                    iv_late += 1;
                    hud_late += 1;
                }
                if iv > 0.060 {
                    let ms = |a: Instant, b: Instant| b.saturating_duration_since(a).as_secs_f64() * 1000.0;
                    eprintln!("UI_STALL iv_ms={:.1} poll_ms={poll_ms:.1} build_ms={:.1} wall_only_ms={:.1}", iv * 1000.0, ms(t_build, t_built), ms(t_built, tp));
                }
            }
            last_present = Some(tp);
            continue;
        }
        t_wall = Instant::now();
        let age = gpu.buffer_age() as usize;
        let mut repaint = damage;
        if full_redraw || first_frame || force_full || prime_full > 0 || age == 0 || age > history.len() + 1 {
            repaint = full;
            force_full = false;
            prime_full = prime_full.saturating_sub(1);
        } else {
            for d in &history[..age - 1] {
                repaint = repaint.union(d);
            }
        }
        repaint = repaint.align_out(32).intersect(&full);
        history.insert(0, damage);
        history.truncate(4);

        // The UI's quads in drawing order, as runs of one texture each: all atlas, except
        // the thumbnails of Settings > Wallpaper (their sheet texture).
        batch.clear();
        runs.clear();
        for it in &hframe.items {
            // Skip home items entirely behind the opaque launch panel (overlays always draw).
            if it.bounds.intersects(&repaint) && (it.id >= 900_000 || !occluder.contains(&it.bounds)) {
                let tex = it.tex.unwrap_or(atlas.tex);
                match runs.last_mut() {
                    Some((t, _, end)) if *t == tex => *end += it.len,
                    _ => runs.push((tex, batch.len(), batch.len() + it.len)),
                }
                batch.extend_from_slice(&hframe.verts[it.start..it.start + it.len]);
            }
        }
        ibatch.clear();
        for it in &iframe.items {
            if it.bounds.intersects(&repaint) {
                ibatch.extend_from_slice(&iframe.verts[it.start..it.start + it.len]);
            }
        }
        let mut batches: Vec<(glow::Texture, bool, &[gpu::Vertex])> = Vec::with_capacity(8);
        for q in wall_quads.iter_mut() {
            q.clear();
        }
        let mut wl: Vec<(glow::Texture, bool)> = Vec::with_capacity(3);
        gpu.mix = None;
        if wall_alpha > 0.0 && !planes {
            // Two layers whose first is opaque: one pass, mix(first, second, opacity).
            let mut ls = layers.clone();
            if ls.len() >= 2 && ls[0].1 >= 1.0 {
                gpu.mix = Some((ls[1].0, ls[1].1));
                ls.remove(1);
            }
            for (i, (t, a)) in ls.iter().enumerate() {
                // The first full-opacity layer is drawn without blending (intro fade = darker).
                let opaque = i == 0 && *a >= 1.0;
                wall_pieces(&mut wall_quads[i], w as f32, h as f32, occluder, if opaque { wall_alpha } else { a * wall_alpha });
                wl.push((*t, opaque));
            }
        }
        for (i, (t, opaque)) in wl.iter().enumerate() {
            batches.push((*t, *opaque, &wall_quads[i]));
        }
        for (t, a, b) in &runs {
            batches.push((*t, false, &batch[*a..*b]));
        }
        if let Some(it) = intro.as_ref() {
            batches.push((it.tex, false, &ibatch));
        }
        gpu.draw(&batches, repaint);
        t_draw = Instant::now();
        gpu.mix = None;
        if shot_now {
            shot_now = false;
            let path = if numbered { shot_path.replace(".ppm", &format!("-{shot_n}.ppm")) } else { shot_path.clone() };
            shot_n += 1;
            let r = if let Some((_, ws)) = wall_plane.as_ref() {
                // Plane mode: compose like the VOP does (UI premultiplied over the wallpaper).
                let ui = gpu.read_rgba();
                gpu.bind(Some(*ws), wall_size);
                draw_wall_layers(&mut gpu, &layers, wall_alpha, w, h, &mut wall_quads);
                let wall = gpu.read_rgba();
                gpu.bind(None, (w, h));
                write_composed_ppm(&path, &ui, (w, h), &wall, wall_size)
            } else {
                gpu.screenshot(&path)
            };
            match r {
                Ok(()) => eprintln!("UI_SHOT {path} ms={}", t0.elapsed().as_millis()),
                Err(e) => eprintln!("UI_WARN screenshot: {e}"),
            }
        }
        if profile {
            let t_gpu = Instant::now();
            pr_cpu += t_gpu.duration_since(t_build).as_secs_f64();
            gpu.finish();
            pr_gpu += t_gpu.elapsed().as_secs_f64();
        }
        if let Err(e) = gpu.swap() {
            eprintln!("UI_ERROR {e}");
            break;
        }
        t_swap = Instant::now();
        let presented = if planes {
            kms.present_planes(surface.as_ref(), wall_plane.as_ref().filter(|_| draw_wall).map(|p| &p.0), wall_size)
        } else {
            kms.present(surface.as_ref().unwrap())
        };
        if let Err(e) = presented {
            eprintln!("UI_ERROR present: {e}");
            break;
        }
        frames += 1;
        st_px += repaint.area();
        if let Some(l) = launch.as_ref().filter(|l| matches!(l.phase, launch::Phase::Opening | launch::Phase::Closing | launch::Phase::Fading)) {
            let tp = Instant::now();
            let iv = last_present.map_or(0.0, |lp| tp.duration_since(lp).as_secs_f64());
            if trace_launch {
                eprintln!("UI_LAUNCH_FRAME phase={:?} iv_ms={:.1} repaint={}x{} e={:.3}", l.phase, iv * 1000.0, repaint.x1 - repaint.x0, repaint.y1 - repaint.y0, l.expansion(tp));
            }
            match lstat.as_mut() {
                Some(st) if st.0 == l.phase => {
                    st.1 += 1;
                    st.2 += (iv > period * 1.5) as u32;
                    st.3 = st.3.max(iv);
                }
                _ => {
                    if let Some((ph, n, late, mx, t)) = lstat.take() {
                        eprintln!("UI_LAUNCH phase={ph:?} frames={n} late={late} max_iv_ms={:.1} ms={}", mx * 1000.0, t.elapsed().as_millis());
                    }
                    lstat = Some((l.phase, 1, 0, 0.0, tp));
                }
            }
        } else if let Some((ph, n, late, mx, t)) = lstat.take() {
            eprintln!("UI_LAUNCH phase={ph:?} frames={n} late={late} max_iv_ms={:.1} ms={}", mx * 1000.0, t.elapsed().as_millis());
        }
        let tp = Instant::now();
        if let Some(lp) = last_present {
            let iv = tp.duration_since(lp).as_secs_f64();
            iv_n += 1;
            iv_sum += iv;
            iv_max = iv_max.max(iv);
            hud_n += 1;
            if iv > period * 1.5 {
                iv_late += 1;
                hud_late += 1;
            }
            if iv > 0.060 {
                let ms = |a: Instant, b: Instant| b.saturating_duration_since(a).as_secs_f64() * 1000.0;
                eprintln!(
                    "UI_STALL iv_ms={:.1} poll_ms={poll_ms:.1} build_ms={:.1} wall_ms={:.1} draw_ms={:.1} swap_ms={:.1} present_ms={:.1} wall={} repaint={}x{}",
                    iv * 1000.0, ms(t_build, t_built), ms(t_built, t_wall), ms(t_wall, t_draw), ms(t_draw, t_swap), ms(t_swap, tp),
                    draw_wall as u8, repaint.x1 - repaint.x0, repaint.y1 - repaint.y0
                );
            }
        }
        last_present = Some(tp);
        if prime {
            // The first frame after a trim is on its way to the screen (the user sees the
            // UI again). GBM allocates each further buffer on its first use (8 MiB: ~27 ms,
            // two late frames in the middle of the animation): take both now, then start
            // the animation's clock.
            prime = false;
            let t_p = Instant::now();
            if let Some(s) = surface.as_ref() {
                let mut held = Vec::with_capacity(2);
                for _ in 0..2 {
                    if gpu.swap().is_err() {
                        break;
                    }
                    match unsafe { s.lock_front_buffer() } {
                        Ok(bo) => held.push(bo),
                        Err(_) => break,
                    }
                }
            }
            prime_full = 3;
            history.clear();
            if let Some(l) = launch.as_mut() {
                l.t0 += t_p.elapsed();
            }
            last_present = None;
            need_build = true;
            eprintln!("UI_TRIM swap chain primed ms={:.1}", t_p.elapsed().as_secs_f64() * 1000.0);
        }
        if first_frame {
            first_frame = false;
            eprintln!(
                "UI_FIRST_FRAME ms={} intro={} atlas={}x{} used_pct={:.1} entries={} glyphs={} badges={}",
                t0.elapsed().as_millis(),
                intro.is_some(),
                atlas.size,
                atlas.size,
                atlas.used_px as f64 * 100.0 / (atlas.size as f64 * atlas.size as f64),
                atlas.entries,
                text.as_ref().map_or(0, |t| t.rasterised),
                home.as_ref().map_or(0, |h| h.icons_rendered)
            );
        }
    }

    if let (true, Some(hm), Some(tx)) = (env_u64("UI_GPU_BENCH", 0) != 0, home.as_mut(), text.as_mut()) {
        // The asset wallpaper's layers when loaded, else the built-in one for both.
        if let Some((sharp, soft)) = wset.or(wall_tex.map(|t| (t, t))) {
            boost.set(true);
            let mut wq = Vec::with_capacity(4);
            home::quad(&mut wq, 0.0, 0.0, w as f32, h as f32, [0.0, 0.0, 1.0, 1.0], [255; 4]);
            gpu_bench(&mut gpu, hm, &mut atlas, tx, (sharp, soft), &wq, full, &clock_s, &date_s, wall_plane.as_ref().map(|p| (p.1, wall_size)));
        }
    }
    boost.set(false);
    if exit_code == EXIT_RESTART {
        blank_console();
    }
    eprintln!("{}", kms.restore());
    gpu.shutdown();
    drop(surface);
    let (tot_n, tot_late, tot_sum, tot_max) = (tot_n + iv_n, tot_late + iv_late, tot_sum + iv_sum, tot_max.max(iv_max));
    eprintln!(
        "UI_BENCH_SUMMARY planes={} anim_frames={tot_n} late={tot_late} late_pct={:.1} anim_mean_ms={:.2} anim_max_ms={:.1}{}",
        planes as u8,
        if tot_n > 0 { tot_late as f64 * 100.0 / tot_n as f64 } else { 0.0 },
        if tot_n > 0 { tot_sum * 1000.0 / tot_n as f64 } else { 0.0 },
        tot_max * 1000.0,
        if profile && tot_pf > 0 { format!(" gpu_ms={:.2}", tot_gpu * 1000.0 / tot_pf as f64) } else { String::new() },
    );
    eprintln!(
        "UI_EXIT frames={} flips={} secs={:.1} hwm_kb={} atlas_entries={} glyphs={} badges={}",
        frames,
        kms.flips,
        t0.elapsed().as_secs_f64(),
        proc_status("VmHWM:"),
        atlas.entries,
        text.as_ref().map_or(0, |t| t.rasterised),
        home.as_ref().map_or(0, |h| h.icons_rendered)
    );
    if exit_code != 0 {
        eprintln!("UI_RESTART output={}", cfg.output);
        std::process::exit(exit_code);
    }
}

/// Fixed-scene GPU cost breakdown: N full frames per variant, each finished with glFinish
/// (not presented). Prints ms/frame and the GPU clock. "ui" alone is what the GPU draws per
/// frame in plane mode; "wall-half-mix" is a plane-mode wallpaper dissolve frame.
#[allow(clippy::too_many_arguments)]
fn gpu_bench(
    gpu: &mut gpu::Gpu,
    home: &mut Home,
    atlas: &mut atlas::Atlas,
    text: &mut text::Text,
    (sharp, soft): (glow::Texture, glow::Texture),
    wall_quad: &[gpu::Vertex],
    full: Rect,
    clock: &str,
    date: &str,
    plane: Option<(khronos_egl::Surface, (u32, u32))>,
) {
    let frame = home.build(gpu, atlas, text, Instant::now(), clock, date);
    let ui = frame.verts.clone();
    let none: &[gpu::Vertex] = &[];
    // Partial variants: the card row band (a typical horizontal-scroll repaint).
    let band = Rect::new(0, full.y1 * 512 / 1080, full.x1, full.y1 * 960 / 1080).align_out(32);
    // (name, wallpaper quads, mix with soft, UI quads, clear, clip, on the wallpaper plane)
    let tiny = Rect::new(0, 0, 32, 32);
    let variants: [(&str, &[gpu::Vertex], bool, &[gpu::Vertex], bool, Rect, bool); 11] = [
        ("tiny-sclear", none, false, none, true, tiny, false),
        ("clear", none, false, none, true, full, false),
        ("clear+ui", none, false, &ui, true, full, false),
        ("clear+wall", wall_quad, false, none, true, full, false),
        ("clear+wall+ui", wall_quad, false, &ui, true, full, false),
        ("clear+mix+ui", wall_quad, true, &ui, true, full, false),
        ("reload+wall+ui", wall_quad, false, &ui, false, full, false),
        ("band-sclear+ui", none, false, &ui, true, band, false),
        ("band-sclear+wall+ui", wall_quad, false, &ui, true, band, false),
        ("wall-half", wall_quad, false, none, true, full, true),
        ("wall-half-mix", wall_quad, true, none, true, full, true),
    ];
    for (name, wq, mix, u, clear, clip, on_plane) in variants {
        let target = match (on_plane, plane) {
            (false, _) => None,
            (true, Some(p)) => Some(p),
            (true, None) => continue,
        };
        if let Some((s, size)) = target {
            gpu.bind(Some(s), size);
        }
        let clip = if target.is_some() { Rect::new(0, 0, gpu.width as i32, gpu.height as i32) } else { clip };
        let saved = gpu.clear_rgba;
        if target.is_some() {
            gpu.clear_rgba = [0.0, 0.0, 0.0, 1.0];
        }
        let run = |gpu: &mut gpu::Gpu| {
            gpu.mix = mix.then_some((soft, 0.5));
            gpu.draw_ex(&[(sharp, true, wq), (atlas.tex, false, u)], clip, clear);
            gpu.mix = None;
            gpu.finish();
        };
        run(gpu);
        let n = 20;
        let t = Instant::now();
        for _ in 0..n {
            run(gpu);
        }
        let lat = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        // Throughput: the same frames queued back to back (one glFinish at the end), as in
        // the real pipelined loop where the CPU does not wait for each frame.
        let t = Instant::now();
        for _ in 0..n {
            gpu.mix = mix.then_some((soft, 0.5));
            gpu.draw_ex(&[(sharp, true, wq), (atlas.tex, false, u)], clip, clear);
            gpu.mix = None;
            unsafe { glow::HasContext::flush(&gpu.gl) };
        }
        gpu.finish();
        let thr = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!(
            "UI_GPU_BENCH variant={name} ms={lat:.2} queued_ms={thr:.2} ui_quads={} gpu_hz={} temp_mc={}",
            u.len() / 4,
            read_trim("/sys/class/devfreq/20000000.gpu/cur_freq"),
            read_trim("/sys/class/thermal/thermal_zone0/temp")
        );
        gpu.clear_rgba = saved;
        if target.is_some() {
            gpu.bind(None, (full.x1 as u32, full.y1 as u32));
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("UI_ERROR {msg}");
    std::process::exit(1);
}
