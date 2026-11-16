//! Sandbox Inspector — example meow app. It probes its own sandbox and shows the results:
//! what it is (uid, namespaces, Landlock, seccomp), what it can see (processes, files) and
//! what each permission changes (network, storage, display, input). Up/Down select a row,
//! Enter re-runs the probes, Back exits. A launch counter in /data shows that storage
//! survives restarts and reboots.

use glow::HasContext;
use meow_app::{App, Event, Key};
use std::time::{Duration, Instant};

struct Probe {
    label: String,
    result: String,
    good: Option<bool>, // Some(true) = allowed/working, Some(false) = blocked, None = info
}

fn p(label: &str, result: impl Into<String>, good: Option<bool>) -> Probe {
    Probe { label: label.into(), result: result.into(), good }
}

fn errno_str(e: std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EPERM) => "EPERM (not permitted)".into(),
        Some(libc::EACCES) => "EACCES (access denied)".into(),
        Some(libc::ENOENT) => "ENOENT (not in the sandbox)".into(),
        Some(libc::EROFS) => "EROFS (read-only)".into(),
        Some(libc::ENETUNREACH) => "ENETUNREACH (no network)".into(),
        _ => e.to_string(),
    }
}

fn tcp_probe(addr: &str) -> Result<(), std::io::Error> {
    let a: std::net::SocketAddr = addr.parse().unwrap();
    std::net::TcpStream::connect_timeout(&a, Duration::from_millis(1500)).map(|_| ())
}

fn run_probes(app: &mut App, gl_renderer: &str) -> Vec<Probe> {
    let mut v = Vec::new();
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    v.push(p("Identity", format!("uid {uid}, gid {gid}, pid {}", std::process::id()), Some(uid != 0)));
    v.push(p("Sandbox", App::sandbox_info(), None));
    v.push(p("Granted permissions", app.permissions.join(", "), None));
    let procs = std::fs::read_dir("/proc").map(|rd| rd.flatten().filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit())).count()).unwrap_or(0);
    v.push(p("Processes visible", format!("{procs} (own PID namespace)"), Some(procs <= 3)));
    let cg = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    v.push(p("cgroup", cg.trim().to_string(), None));
    v.push(match std::fs::metadata("/bin/sh") {
        Ok(_) => p("System shell /bin/sh", "visible", Some(false)),
        Err(e) => p("System shell /bin/sh", errno_str(e), Some(true)),
    });
    v.push(match std::fs::File::open("/dev/mem") {
        Ok(_) => p("Raw memory /dev/mem", "OPEN", Some(false)),
        Err(e) => p("Raw memory /dev/mem", errno_str(e), Some(true)),
    });
    v.push(match std::fs::write("/app/tamper", b"x") {
        Ok(_) => p("Write own code /app", "WRITABLE", Some(false)),
        Err(e) => p("Write own code /app", errno_str(e), Some(true)),
    });
    let r = unsafe { libc::setuid(0) };
    v.push(p("Become root (setuid 0)", if r == 0 { "SUCCEEDED".into() } else { errno_str(std::io::Error::last_os_error()) }, Some(r != 0)));
    let (s, t) = (std::ffi::CString::new("none").unwrap(), std::ffi::CString::new("/tmp").unwrap());
    let r = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), c"tmpfs".as_ptr(), 0, std::ptr::null()) };
    v.push(p("Mount a filesystem", if r == 0 { "SUCCEEDED".into() } else { errno_str(std::io::Error::last_os_error()) }, Some(r != 0)));
    // Network: the store on the Mac (USB link).
    let net = tcp_probe("192.168.2.1:8090");
    let want = app.has("network");
    v.push(match net {
        Ok(()) => p("Network (store 192.168.2.1)", "connected", Some(want)),
        Err(e) => p("Network (store 192.168.2.1)", errno_str(e), Some(!want)),
    });
    // Storage: launch counter.
    if app.has("storage") {
        let n: u64 = std::fs::read_to_string("/data/launches").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0) + 1;
        let w = std::fs::write("/data/launches", format!("{n}\n"));
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        unsafe { libc::statvfs(c"/data".as_ptr(), &mut st) };
        let quota = st.f_blocks as u64 * st.f_frsize as u64 / 1024;
        let sync = app.sync_storage().map_err(|e| e.to_string());
        v.push(p(
            "Storage /data",
            match w {
                Ok(()) => format!("launch #{n}, quota {quota} KiB, sync {}", sync.unwrap_or_else(|e| e)),
                Err(e) => errno_str(e),
            },
            Some(true),
        ));
        let exec = std::fs::write("/data/run.sh", b"#!/bin/sh\n").and_then(|_| {
            std::fs::set_permissions("/data/run.sh", std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            std::process::Command::new("/data/run.sh").status().map(|_| ())
        });
        let _ = std::fs::remove_file("/data/run.sh");
        // EACCES = the kernel refused (noexec mount). Anything else got past that check.
        v.push(match exec {
            Ok(()) => p("Run code from /data", "EXECUTED", Some(false)),
            Err(e) if e.raw_os_error() == Some(libc::EACCES) => p("Run code from /data", "EACCES (noexec)", Some(true)),
            Err(e) => p("Run code from /data", format!("exec allowed, then {}", errno_str(e)), Some(false)),
        });
    } else {
        v.push(p("Storage /data", "no storage permission", None));
    }
    v.push(p("GPU", gl_renderer.to_string(), Some(!gl_renderer.is_empty())));
    v
}

// ---------------------------------------------------------------- tiny GLES2 renderer

const VS: &str = "attribute vec2 a_pos; attribute vec2 a_uv; uniform vec2 u_view; varying vec2 v_uv;
void main() { v_uv = a_uv; gl_Position = vec4(a_pos.x * u_view.x - 1.0, 1.0 - a_pos.y * u_view.y, 0.0, 1.0); }";
const FS: &str = "precision mediump float; uniform sampler2D u_tex; uniform vec4 u_col; varying vec2 v_uv;
void main() { gl_FragColor = texture2D(u_tex, v_uv) * u_col; }";

struct Label {
    tex: glow::Texture,
    w: u32,
    h: u32,
    text: String,
    px: u32,
}

struct Gfx {
    prog: glow::Program,
    vbo: glow::Buffer,
    white: glow::Texture,
    u_col: glow::UniformLocation,
    fonts: Vec<ab_glyph::FontVec>,
    labels: Vec<Label>,
}

fn tex(gl: &glow::Context, w: u32, h: u32, rgba: &[u8]) -> glow::Texture {
    unsafe {
        let t = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        for (k, v) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, k, v as i32);
        }
        gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(rgba)));
        t
    }
}

impl Gfx {
    fn new(gl: &glow::Context, fonts: Vec<ab_glyph::FontVec>) -> Gfx {
        unsafe {
            let prog = gl.create_program().unwrap();
            for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
                let s = gl.create_shader(kind).unwrap();
                gl.shader_source(s, src);
                gl.compile_shader(s);
                assert!(gl.get_shader_compile_status(s), "{}", gl.get_shader_info_log(s));
                gl.attach_shader(prog, s);
            }
            gl.bind_attrib_location(prog, 0, "a_pos");
            gl.bind_attrib_location(prog, 1, "a_uv");
            gl.link_program(prog);
            assert!(gl.get_program_link_status(prog), "{}", gl.get_program_info_log(prog));
            let u_col = gl.get_uniform_location(prog, "u_col").unwrap();
            let vbo = gl.create_buffer().unwrap();
            let white = tex(gl, 1, 1, &[255; 4]);
            Gfx { prog, vbo, white, u_col, fonts, labels: Vec::new() }
        }
    }

    /// A text line as a texture (cached by text and size).
    fn label(&mut self, gl: &glow::Context, face: usize, px: u32, text: &str) -> usize {
        use ab_glyph::{Font, PxScale, ScaleFont, point};
        if let Some(i) = self.labels.iter().position(|l| l.text == text && l.px == px * 4 + face as u32) {
            return i;
        }
        let font = &self.fonts[face];
        let scale = PxScale::from(px as f32 * font.height_unscaled() / font.units_per_em().unwrap_or(1000.0));
        let sf = font.as_scaled(scale);
        let mut x = 0.0f32;
        let mut glyphs = Vec::new();
        let mut prev = None;
        for c in text.chars() {
            let id = sf.glyph_id(c);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            glyphs.push(id.with_scale_and_position(scale, point(x, sf.ascent())));
            x += sf.h_advance(id);
            prev = Some(id);
        }
        let (w, h) = ((x.ceil() as u32 + 2).max(1), (sf.height().ceil() as u32 + 2).max(1));
        let mut img = vec![0u8; (w * h * 4) as usize];
        for g in glyphs {
            if let Some(og) = font.outline_glyph(g) {
                let b = og.px_bounds();
                og.draw(|gx, gy, c| {
                    let (px_, py_) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px_ >= 0 && py_ >= 0 && (px_ as u32) < w && (py_ as u32) < h {
                        let i = ((py_ as u32 * w + px_ as u32) * 4) as usize;
                        let a = (c.min(1.0) * 255.0) as u8;
                        let a = a.max(img[i + 3]);
                        img[i..i + 4].copy_from_slice(&[a, a, a, a]);
                    }
                });
            }
        }
        let t = tex(gl, w, h, &img);
        self.labels.push(Label { tex: t, w, h, text: text.into(), px: px * 4 + face as u32 });
        self.labels.len() - 1
    }

    fn quad(&self, gl: &glow::Context, t: glow::Texture, x: f32, y: f32, w: f32, h: f32, col: [f32; 4]) {
        let v: [f32; 16] = [x, y, 0.0, 0.0, x + w, y, 1.0, 0.0, x, y + h, 0.0, 1.0, x + w, y + h, 1.0, 1.0];
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            gl.uniform_4_f32(Some(&self.u_col), col[0] * col[3], col[1] * col[3], col[2] * col[3], col[3]);
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, std::slice::from_raw_parts(v.as_ptr() as *const u8, 64), glow::STREAM_DRAW);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn text(&mut self, gl: &glow::Context, face: usize, px: f32, x: f32, y: f32, s: &str, col: [f32; 4]) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let i = self.label(gl, face, px.round() as u32, s);
        let (t, w, h) = (self.labels[i].tex, self.labels[i].w, self.labels[i].h);
        self.quad(gl, t, x.round(), y.round(), w as f32, h as f32, col);
        w as f32
    }
}

fn rgb(c: u32, a: f32) -> [f32; 4] {
    [((c >> 16) & 255) as f32 / 255.0, ((c >> 8) & 255) as f32 / 255.0, (c & 255) as f32 / 255.0, a]
}

fn main() {
    let mut app = match App::connect() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("inspector: {e}");
            std::process::exit(2);
        }
    };
    println!("inspector: connected as {} with [{}]", app.id, app.permissions.join(","));
    let mut screen = match meow_app::display::Display::open(&mut app) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("inspector: no display: {e}");
            std::process::exit(3);
        }
    };
    let dir = std::env::var("MEOW_FONTS").unwrap_or_else(|_| "/usr/share/fonts/noto".into());
    let fonts: Vec<ab_glyph::FontVec> = ["NotoSans-Light.ttf", "NotoSans-Regular.ttf", "NotoSans-Medium.ttf"]
        .iter()
        .map(|f| ab_glyph::FontVec::try_from_vec(std::fs::read(format!("{dir}/{f}")).expect("font")).expect("font"))
        .collect();
    let gl = &screen.gl;
    let renderer = unsafe { gl.get_parameter_string(glow::RENDERER) };
    let mut g = Gfx::new(gl, fonts);
    let (w, h) = (screen.width as f32, screen.height as f32);
    let s = h / 1080.0;
    unsafe {
        gl.viewport(0, 0, w as i32, h as i32);
        gl.use_program(Some(g.prog));
        let u_view = gl.get_uniform_location(g.prog, "u_view").unwrap();
        gl.uniform_2_f32(Some(&u_view), 2.0 / w, 2.0 / h);
        gl.enable_vertex_attrib_array(0);
        gl.enable_vertex_attrib_array(1);
        gl.enable(glow::BLEND);
        gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
    }
    let mut probes = run_probes(&mut app, &renderer);
    for pr in &probes {
        println!("probe {:<28} {:<6} {}", pr.label, match pr.good { Some(true) => "ok", Some(false) => "BAD", None => "info" }, pr.result);
    }
    let (mut sel, mut keys, mut dirty, mut shot) = (0usize, 0u32, true, false);
    // Fade in from the launcher's splash colour (the screen shows it until our first frame).
    let splash = App::splash_color();
    let mut shown_at: Option<Instant> = None;
    const FADE_IN_MS: f32 = 260.0;
    let started = Instant::now();
    // The system's volume pop-up: `screen.present` draws it; this loop only has to keep
    // presenting while it is up, and once more to take it away.
    let mut popup = false;
    loop {
        for ev in app.wait(Some(Duration::from_millis(if dirty || popup { 0 } else { 1000 }))) {
            match ev {
                Event::Quit | Event::Key(Key::Back) => {
                    println!("inspector: exit after {:.1}s, {keys} keys", started.elapsed().as_secs_f32());
                    return;
                }
                Event::Key(k) => {
                    keys += 1;
                    match k {
                        Key::Up => sel = sel.saturating_sub(1),
                        Key::Down => sel = (sel + 1).min(probes.len() - 1),
                        Key::Enter => probes = run_probes(&mut app, &renderer),
                        Key::Other(k) if k == "shot" => shot = true,
                        _ => {}
                    }
                    dirty = true;
                }
                _ => {} // the system volume: this app makes no sound (the pop-up shows by itself)
            }
        }
        let popup_now = meow_app::osd::active();
        if !dirty && !popup && !popup_now {
            continue;
        }
        popup = popup_now;
        dirty = false;
        let gl = &screen.gl;
        unsafe {
            gl.clear_color(0.06, 0.07, 0.13, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let white = g.white;
        g.quad(gl, white, 0.0, 0.0, w, 6.0 * s, rgb(0x3daee9, 1.0));
        g.text(gl, 0, 64.0 * s, 110.0 * s, 70.0 * s, "Sandbox Inspector", rgb(0xffffff, 1.0));
        g.text(gl, 1, 28.0 * s, 114.0 * s, 160.0 * s, &format!("{}  ·  what this app can and cannot do", app.id), rgb(0xffffff, 0.6));
        let (x0, y0, row) = (110.0 * s, 230.0 * s, 52.0 * s);
        for (i, pr) in probes.iter().enumerate() {
            let y = y0 + i as f32 * row;
            if i == sel {
                g.quad(gl, white, x0 - 20.0 * s, y - 6.0 * s, w - 2.0 * x0 + 40.0 * s, row - 4.0 * s, rgb(0x3daee9, 0.22));
            }
            let dot = match pr.good {
                Some(true) => 0x4cd07d,
                Some(false) => 0xec5470,
                None => 0x7d848b,
            };
            g.quad(gl, white, x0 + 2.0 * s, y + 14.0 * s, 14.0 * s, 14.0 * s, rgb(dot, 1.0));
            g.text(gl, 1, 28.0 * s, x0 + 40.0 * s, y, &pr.label, rgb(0xffffff, 0.95));
            let r: String = pr.result.chars().take(90).collect();
            g.text(gl, 1, 28.0 * s, x0 + 520.0 * s, y, &r, rgb(0xffffff, 0.7));
        }
        let foot = format!("Up/Down select  ·  Enter re-run  ·  Back exit  ·  keys received: {keys}  ·  {}x{}@{}", screen.width, screen.height, screen.refresh_hz);
        g.text(gl, 1, 24.0 * s, 110.0 * s, h - 80.0 * s, &foot, rgb(0xffffff, 0.5));
        let fade = shown_at.map_or(0.0, |t| (t.elapsed().as_secs_f32() * 1000.0 / FADE_IN_MS).min(1.0));
        if let (Some(c), true) = (splash, fade < 1.0) {
            let a = 1.0 - fade * fade * (3.0 - 2.0 * fade);
            g.quad(gl, white, 0.0, 0.0, w, h, [c[0], c[1], c[2], a]);
            dirty = true; // keep drawing until the fade is done
        }
        if shot {
            // Debug: half-resolution PPM in /tmp (appd's side reads /proc/<pid>/root/tmp).
            shot = false;
            screen.draw_volume_popup(); // normally drawn by `present`: here so that it is in the picture
            let gl = &screen.gl;
            let (sw, sh) = (screen.width as usize, screen.height as usize);
            let mut px = vec![0u8; sw * sh * 4];
            unsafe { gl.read_pixels(0, 0, sw as i32, sh as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px))) };
            let mut out = format!("P6\n{} {}\n255\n", sw / 2, sh / 2).into_bytes();
            for y in (0..sh / 2).rev() {
                for x in 0..sw / 2 {
                    let i = ((y * 2) * sw + x * 2) * 4;
                    out.extend_from_slice(&px[i..i + 3]);
                }
            }
            let _ = std::fs::write("/tmp/shot.ppm", out);
            println!("inspector: screenshot /tmp/shot.ppm");
        }
        if let Err(e) = screen.present() {
            eprintln!("inspector: screen gone ({e}); exiting");
            return;
        }
        shown_at.get_or_insert_with(Instant::now);
    }
}
