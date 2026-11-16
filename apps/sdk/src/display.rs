//! Full-screen GLES2 output on the brokered DRM master fd (feature "display").
//!
//! Keeps the mode the launcher left on the CRTC (no full modeset, so the TV does not
//! re-sync), renders through GBM/EGL (Mesa Lima) and page-flips. The screen can be revoked
//! by the system at any time: `present` then returns an error and the app should exit.
//!
//! With the "osd" feature `present` also draws the system's volume pop-up (osd.rs) over the
//! frame while one is due. It uses its own small program and texture and puts back the GL
//! state it touches (program, texture unit 0, array buffer, blending, scissor and depth
//! test). Its vertices go through the last two vertex attributes the GPU has
//! (GL_MAX_VERTEX_ATTRIBS - 2 and - 1), which it leaves disabled: an app's own attributes
//! and their pointers stay as they are. An app that renders only when something changed
//! keeps presenting while `meow_app::osd::active()`.

use crate::{App, Error};
use drm::control::{Device as _, PageFlipFlags, connector, crtc, framebuffer};
use khronos_egl as egl;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

#[link(name = "EGL")]
unsafe extern "C" {}

const PLATFORM_GBM_KHR: egl::Enum = 0x31D7;

pub struct Card(OwnedFd);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

type Bo = gbm::BufferObject<framebuffer::Handle>;

pub struct Display {
    pub gl: glow::Context,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    egl: egl::Instance<egl::Static>,
    dpy: egl::Display,
    surf: egl::Surface,
    _ctx: egl::Context,
    surface: gbm::Surface<framebuffer::Handle>,
    gbm: gbm::Device<Card>,
    crtc: crtc::Handle,
    conn: connector::Handle,
    mode: drm::control::Mode,
    modeset: bool,
    pending: bool,
    shown: Option<Bo>,
    queued: Option<Bo>,
    #[cfg(feature = "osd")]
    osd: Option<Osd>,
    /// The pop-up could not be set up (no font): do not try every frame.
    #[cfg(feature = "osd")]
    osd_failed: bool,
}

/// GL objects of the volume pop-up.
#[cfg(feature = "osd")]
struct Osd {
    renderer: crate::osd::Renderer,
    program: glow::Program,
    texture: glow::Texture,
    buffer: glow::Buffer,
    /// The first of its two vertex attributes.
    attribute: u32,
    /// The volume the texture holds.
    uploaded: Option<(u8, bool)>,
}

#[cfg(feature = "osd")]
impl Osd {
    fn new(gl: &glow::Context, screen_h: u32) -> Result<Osd, String> {
        use glow::HasContext;
        let fonts = std::env::var("MEOW_FONTS").unwrap_or_else(|_| "/usr/share/fonts/noto".into());
        let renderer = crate::osd::Renderer::from_font_dir(&fonts, screen_h)?;
        unsafe {
            let program = gl.create_program()?;
            let sources = [
                (glow::VERTEX_SHADER, "attribute vec2 p; attribute vec2 t; varying vec2 v; void main() { v = t; gl_Position = vec4(p, 0.0, 1.0); }"),
                (glow::FRAGMENT_SHADER, "precision mediump float; varying vec2 v; uniform sampler2D s; uniform float a; void main() { gl_FragColor = texture2D(s, v) * a; }"),
            ];
            for (kind, source) in sources {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    return Err(gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            // Out of the way of the app's attributes (ES 2 has at least 8).
            let attribute = (gl.get_parameter_i32(glow::MAX_VERTEX_ATTRIBS).max(8) - 2) as u32;
            gl.bind_attrib_location(program, attribute, "p");
            gl.bind_attrib_location(program, attribute + 1, "t");
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(gl.get_program_info_log(program));
            }
            Ok(Osd { renderer, program, texture: gl.create_texture()?, buffer: gl.create_buffer()?, attribute, uploaded: None })
        }
    }

    fn draw(&mut self, gl: &glow::Context, shown: crate::osd::Shown, (width, height): (u32, u32)) {
        use glow::HasContext;
        let (x, y, w, h) = crate::osd::place(width, height);
        unsafe {
            // What the app may rely on: saved here, put back below.
            let program = gl.get_parameter_i32(glow::CURRENT_PROGRAM);
            let buffer = gl.get_parameter_i32(glow::ARRAY_BUFFER_BINDING);
            let unit = gl.get_parameter_i32(glow::ACTIVE_TEXTURE);
            gl.active_texture(glow::TEXTURE0);
            let texture = gl.get_parameter_i32(glow::TEXTURE_BINDING_2D);
            let (blend, scissor, depth) = (gl.is_enabled(glow::BLEND), gl.is_enabled(glow::SCISSOR_TEST), gl.is_enabled(glow::DEPTH_TEST));
            let func = [glow::BLEND_SRC_RGB, glow::BLEND_DST_RGB, glow::BLEND_SRC_ALPHA, glow::BLEND_DST_ALPHA].map(|f| gl.get_parameter_i32(f) as u32);

            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            if self.uploaded != Some((shown.level, shown.muted)) {
                let sprite = self.renderer.render(shown.level, shown.muted);
                let bytes = sprite.rgba_bytes();
                for (name, value) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
                }
                gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32, sprite.width as i32, sprite.height as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&bytes)));
                self.uploaded = Some((shown.level, shown.muted));
            }
            // It rises a few pixels while it fades in, like the launcher's.
            let rise = 8.0 * height as f32 / 1080.0 * (1.0 - shown.opacity);
            let ndc = |px: f32, py: f32| [px / width as f32 * 2.0 - 1.0, 1.0 - py / height as f32 * 2.0];
            let (x0, y0, x1, y1) = (x as f32, y as f32 + rise, (x + w as i32) as f32, (y + h as i32) as f32 + rise);
            let mut vertices = [0f32; 16];
            for (i, (px, py, u, v)) in [(x0, y0, 0.0, 0.0), (x1, y0, 1.0, 0.0), (x0, y1, 0.0, 1.0), (x1, y1, 1.0, 1.0)].into_iter().enumerate() {
                let [nx, ny] = ndc(px, py);
                vertices[i * 4..i * 4 + 4].copy_from_slice(&[nx, ny, u, v]);
            }
            let bytes = std::slice::from_raw_parts(vertices.as_ptr() as *const u8, std::mem::size_of_val(&vertices));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.buffer));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            gl.use_program(Some(self.program));
            gl.uniform_1_i32(gl.get_uniform_location(self.program, "s").as_ref(), 0);
            gl.uniform_1_f32(gl.get_uniform_location(self.program, "a").as_ref(), shown.opacity);
            gl.enable_vertex_attrib_array(self.attribute);
            gl.enable_vertex_attrib_array(self.attribute + 1);
            gl.vertex_attrib_pointer_f32(self.attribute, 2, glow::FLOAT, false, 16, 0);
            gl.vertex_attrib_pointer_f32(self.attribute + 1, 2, glow::FLOAT, false, 16, 8);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA); // premultiplied
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.disable_vertex_attrib_array(self.attribute);
            gl.disable_vertex_attrib_array(self.attribute + 1);

            let name = |v: i32| std::num::NonZeroU32::new(v as u32);
            gl.blend_func_separate(func[0], func[1], func[2], func[3]);
            for (capability, on) in [(glow::BLEND, blend), (glow::SCISSOR_TEST, scissor), (glow::DEPTH_TEST, depth)] {
                if on { gl.enable(capability) } else { gl.disable(capability) }
            }
            gl.use_program(name(program).map(glow::NativeProgram));
            gl.bind_buffer(glow::ARRAY_BUFFER, name(buffer).map(glow::NativeBuffer));
            gl.bind_texture(glow::TEXTURE_2D, name(texture).map(glow::NativeTexture));
            gl.active_texture(unit as u32);
        }
    }
}

fn err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Protocol(format!("{what}: {e}"))
}

impl Display {
    pub fn open(app: &mut App) -> Result<Display, Error> {
        let card = Card(app.display_fd()?);
        let _ = drm::Device::set_client_capability(&card, drm::ClientCapability::AspectRatio, true);
        let res = card.resource_handles().map_err(|e| err("KMS resources", e))?;
        let conn = res
            .connectors()
            .iter()
            .filter_map(|h| card.get_connector(*h, false).ok())
            .find(|c| c.state() == connector::State::Connected)
            .ok_or_else(|| err("KMS", "no connected output"))?;
        let crtc = conn
            .current_encoder()
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc())
            .or_else(|| res.crtcs().first().copied())
            .ok_or_else(|| err("KMS", "no CRTC"))?;
        // The launcher's mode stays: only the framebuffer changes, no TV re-sync.
        let mode = card
            .get_crtc(crtc)
            .ok()
            .and_then(|c| c.mode())
            .or_else(|| conn.modes().iter().find(|m| m.size() == (1920, 1080) && m.vrefresh() == 60).copied())
            .or_else(|| conn.modes().first().copied())
            .ok_or_else(|| err("KMS", "no mode"))?;
        let (w, h) = mode.size();
        let gbm = gbm::Device::new(card).map_err(|e| err("GBM", e))?;
        let usage = gbm::BufferObjectFlags::SCANOUT | gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR;
        let surface = gbm.create_surface::<framebuffer::Handle>(w as u32, h as u32, gbm::Format::Xrgb8888, usage).map_err(|e| err("GBM surface", e))?;

        use gbm::AsRaw;
        let egl = egl::Instance::new(egl::Static);
        let dpy = unsafe { egl.get_platform_display(PLATFORM_GBM_KHR, gbm.as_raw() as *mut _, &[egl::ATTRIB_NONE]) }.map_err(|e| err("eglGetPlatformDisplay", e))?;
        egl.initialize(dpy).map_err(|e| err("eglInitialize", e))?;
        egl.bind_api(egl::OPENGL_ES_API).map_err(|e| err("eglBindAPI", e))?;
        let attrs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT, egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT,
            egl::RED_SIZE, 8, egl::GREEN_SIZE, 8, egl::BLUE_SIZE, 8, egl::ALPHA_SIZE, 0, egl::NONE,
        ];
        let mut configs = Vec::with_capacity(32);
        egl.choose_config(dpy, &attrs, &mut configs).map_err(|e| err("eglChooseConfig", e))?;
        let cfg = configs
            .into_iter()
            .find(|c| egl.get_config_attrib(dpy, *c, egl::NATIVE_VISUAL_ID).ok() == Some(gbm::Format::Xrgb8888 as egl::Int))
            .ok_or_else(|| err("EGL", "no XRGB8888 config"))?;
        let ctx = egl.create_context(dpy, cfg, None, &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE]).map_err(|e| err("eglCreateContext", e))?;
        let surf = unsafe { egl.create_window_surface(dpy, cfg, surface.as_raw() as *mut _, None) }.map_err(|e| err("eglCreateWindowSurface", e))?;
        egl.make_current(dpy, Some(surf), Some(surf), Some(ctx)).map_err(|e| err("eglMakeCurrent", e))?;
        let gl = unsafe { glow::Context::from_loader_function(|n| egl.get_proc_address(n).map_or(std::ptr::null(), |f| f as *const _)) };
        Ok(Display {
            gl,
            width: w as u32,
            height: h as u32,
            refresh_hz: mode.vrefresh(),
            egl,
            dpy,
            surf,
            _ctx: ctx,
            surface,
            gbm,
            crtc,
            conn: conn.handle(),
            mode,
            modeset: false,
            pending: false,
            shown: None,
            queued: None,
            #[cfg(feature = "osd")]
            osd: None,
            #[cfg(feature = "osd")]
            osd_failed: false,
        })
    }

    /// Draw the system's volume pop-up over the current frame, if one is due. `present`
    /// does this by itself; an app calls it only to have the pop-up in a frame it reads back
    /// (a screenshot) before presenting.
    #[cfg(feature = "osd")]
    pub fn draw_volume_popup(&mut self) {
        let Some(shown) = crate::osd::current() else { return };
        if self.osd.is_none() && !self.osd_failed {
            match Osd::new(&self.gl, self.height) {
                Ok(osd) => self.osd = Some(osd),
                Err(e) => {
                    eprintln!("volume pop-up: {e}");
                    self.osd_failed = true;
                }
            }
        }
        if let Some(osd) = self.osd.as_mut() {
            osd.draw(&self.gl, shown, (self.width, self.height));
        }
    }

    fn wait_flip(&mut self, timeout: Duration) -> Result<(), Error> {
        let deadline = Instant::now() + timeout;
        while self.pending {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(err("KMS", "page flip timeout"));
            }
            let mut p = libc::pollfd { fd: self.gbm.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut p, 1, left.as_millis().min(100) as i32) } > 0 {
                for ev in self.gbm.receive_events().map_err(|e| err("KMS events", e))? {
                    if let drm::control::Event::PageFlip(_) = ev {
                        self.pending = false;
                        self.shown = self.queued.take();
                    }
                }
            }
        }
        Ok(())
    }

    /// Show the frame just drawn. Blocks until the previous flip is done (vsync pacing).
    pub fn present(&mut self) -> Result<(), Error> {
        #[cfg(feature = "osd")]
        self.draw_volume_popup();
        self.egl.swap_buffers(self.dpy, self.surf).map_err(|e| err("eglSwapBuffers", e))?;
        self.wait_flip(Duration::from_secs(2))?;
        let mut bo = unsafe { self.surface.lock_front_buffer() }.map_err(|e| err("lock_front_buffer", e))?;
        let fb = match bo.userdata() {
            Some(fb) => *fb,
            None => {
                let fb = self.gbm.add_framebuffer(&bo, 24, 32).map_err(|e| err("AddFB", e))?;
                bo.set_userdata(fb);
                fb
            }
        };
        if !self.modeset {
            self.gbm.set_crtc(self.crtc, Some(fb), (0, 0), &[self.conn], Some(self.mode)).map_err(|e| err("SetCrtc (screen revoked?)", e))?;
            self.modeset = true;
            self.shown = Some(bo);
            return Ok(());
        }
        self.gbm.page_flip(self.crtc, fb, PageFlipFlags::EVENT, None).map_err(|e| err("PageFlip (screen revoked?)", e))?;
        self.pending = true;
        self.queued = Some(bo);
        Ok(())
    }
}
