//! EGL (GBM platform) + OpenGL ES 2.0 drawing, shaped for Mali-400 / Lima:
//! - one shader program: `texture2D(tex, uv) * colour`, texture coordinate passed straight
//!   from a varying (no fragment-side arithmetic, mediump only) -> ~1-2 PP instructions;
//! - everything is a textured quad (premultiplied alpha), drawn in at most two batches per
//!   frame (wallpaper texture, then the UI atlas);
//! - partial redraw: frames only touch a scissor rectangle; Lima then generates its PP stream
//!   (and reloads/writes back) only for the 16x16 tiles inside it. Buffer age tells us how
//!   stale the back buffer is.

use glow::HasContext;
use khronos_egl as egl;

// khronos-egl's `static` feature expects the application to link libEGL (Mesa's, from the
// initramfs; at build time from /build/mesa-install).
#[link(name = "EGL")]
unsafe extern "C" {}

const PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const BUFFER_AGE_EXT: egl::Int = 0x313D;
pub const MAX_QUADS: usize = 8192;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
    pub rgba: [u8; 4],
}

const VS: &str = r#"
attribute vec2 a_pos;
attribute vec2 a_uv;
attribute vec4 a_col;
uniform vec2 u_view;
varying vec2 v_uv;
varying lowp vec4 v_col;
void main() {
    v_uv = a_uv;
    v_col = a_col;
    gl_Position = vec4(a_pos.x * u_view.x - 1.0, 1.0 - a_pos.y * u_view.y, 0.0, 1.0);
}
"#;

const FS: &str = r#"
precision mediump float;
uniform sampler2D u_tex;
varying vec2 v_uv;
varying lowp vec4 v_col;
void main() {
    gl_FragColor = texture2D(u_tex, v_uv) * v_col;
}
"#;

/// Wallpaper cross-fade in one pass: two texture fetches instead of a second blended
/// full-screen pass (which measured ~30 % late frames on vertical scrolls).
const FS_MIX: &str = r#"
precision mediump float;
uniform sampler2D u_tex;
uniform sampler2D u_tex2;
uniform mediump float u_mix;
varying vec2 v_uv;
varying lowp vec4 v_col;
void main() {
    gl_FragColor = mix(texture2D(u_tex, v_uv), texture2D(u_tex2, v_uv), u_mix) * v_col;
}
"#;

pub struct Gpu {
    egl: egl::Instance<egl::Static>,
    dpy: egl::Display,
    ctx: egl::Context,
    cfg: egl::Config,
    surf: egl::Surface,
    /// Surface being drawn (the UI surface, or a wallpaper plane surface).
    cur: egl::Surface,
    pub gl: glow::Context,
    prog: glow::Program,
    prog_mix: glow::Program,
    u_mix: Option<glow::UniformLocation>,
    /// Set for one draw: the first batch becomes mix(its texture, this texture, t).
    pub mix: Option<(glow::Texture, f32)>,
    vbo: glow::Buffer,
    ibo: glow::Buffer,
    pub width: u32,
    pub height: u32,
    pub info: String,
    bytes: Vec<u8>,
    /// Clear the scissor rectangle on partial frames. With the project's Lima patch
    /// (patches/mesa-lima-scissored-clear.patch) a tile-aligned scissored clear makes
    /// Lima skip reloading those tiles; our frames repaint the whole rectangle anyway.
    pub scissor_clear: bool,
    /// Clear colour (premultiplied): opaque black, or transparent for the UI overlay plane.
    pub clear_rgba: [f32; 4],
    main_size: (u32, u32),
    /// No window surface exists (an app has the screen): the context is current surfaceless.
    detached: bool,
}

impl Gpu {
    pub fn new(
        gbm: &gbm::Device<crate::kms::Card>,
        surface: &gbm::Surface<drm::control::framebuffer::Handle>,
        width: u32,
        height: u32,
        rgb565: bool,
        alpha: bool,
    ) -> Result<Gpu, String> {
        use gbm::AsRaw;
        let egl = egl::Instance::new(egl::Static);
        let dpy = unsafe {
            egl.get_platform_display(PLATFORM_GBM_KHR, gbm.as_raw() as *mut _, &[egl::ATTRIB_NONE])
        }
        .map_err(|e| format!("eglGetPlatformDisplay: {e}"))?;
        let (major, minor) = egl.initialize(dpy).map_err(|e| format!("eglInitialize: {e}"))?;
        egl.bind_api(egl::OPENGL_ES_API).map_err(|e| format!("eglBindAPI: {e}"))?;
        let attrs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT,
            egl::RED_SIZE, if rgb565 { 5 } else { 8 },
            egl::GREEN_SIZE, if rgb565 { 6 } else { 8 },
            egl::BLUE_SIZE, if rgb565 { 5 } else { 8 },
            egl::ALPHA_SIZE, if alpha { 8 } else { 0 }, egl::DEPTH_SIZE, 0, egl::STENCIL_SIZE, 0,
            egl::NONE,
        ];
        let mut configs = Vec::with_capacity(64);
        egl.choose_config(dpy, &attrs, &mut configs).map_err(|e| format!("eglChooseConfig: {e}"))?;
        let visual = if rgb565 { gbm::Format::Rgb565 } else if alpha { gbm::Format::Argb8888 } else { gbm::Format::Xrgb8888 } as egl::Int;
        let cfg = configs
            .into_iter()
            .find(|c| egl.get_config_attrib(dpy, *c, egl::NATIVE_VISUAL_ID).ok() == Some(visual))
            .ok_or("no EGL config matching the scanout format")?;
        let ctx = egl
            .create_context(dpy, cfg, None, &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE])
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        let surf = unsafe { egl.create_window_surface(dpy, cfg, surface.as_raw() as *mut _, None) }
            .map_err(|e| format!("eglCreateWindowSurface: {e}"))?;
        egl.make_current(dpy, Some(surf), Some(surf), Some(ctx))
            .map_err(|e| format!("eglMakeCurrent: {e}"))?;
        let egl_ext = egl.query_string(Some(dpy), egl::EXTENSIONS).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();

        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const _)
            })
        };
        let renderer = unsafe { gl.get_parameter_string(glow::RENDERER) };
        if !renderer.contains("Mali400") && !renderer.contains("Mali-400") {
            return Err(format!("GL renderer '{renderer}' is not Lima Mali-400 (software fallback refused)"));
        }
        let info = format!(
            "UI_GL egl={major}.{minor} renderer=\"{renderer}\" format={} buffer_age={} partial_update={}",
            if rgb565 { "RGB565" } else if alpha { "ARGB8888" } else { "XRGB8888" },
            egl_ext.contains("EGL_EXT_buffer_age"),
            egl_ext.contains("EGL_KHR_partial_update")
        );

        unsafe {
            let build = |fs: &str| -> Result<glow::Program, String> {
                let prog = gl.create_program()?;
                for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, fs)] {
                    let sh = gl.create_shader(kind)?;
                    gl.shader_source(sh, src);
                    gl.compile_shader(sh);
                    if !gl.get_shader_compile_status(sh) {
                        return Err(format!("shader: {}", gl.get_shader_info_log(sh)));
                    }
                    gl.attach_shader(prog, sh);
                }
                gl.bind_attrib_location(prog, 0, "a_pos");
                gl.bind_attrib_location(prog, 1, "a_uv");
                gl.bind_attrib_location(prog, 2, "a_col");
                gl.link_program(prog);
                if !gl.get_program_link_status(prog) {
                    return Err(format!("link: {}", gl.get_program_info_log(prog)));
                }
                gl.use_program(Some(prog));
                let view = gl.get_uniform_location(prog, "u_view");
                gl.uniform_2_f32(view.as_ref(), 2.0 / width as f32, 2.0 / height as f32);
                let tex = gl.get_uniform_location(prog, "u_tex");
                gl.uniform_1_i32(tex.as_ref(), 0);
                let tex2 = gl.get_uniform_location(prog, "u_tex2");
                gl.uniform_1_i32(tex2.as_ref(), 1);
                Ok(prog)
            };
            let prog_mix = build(FS_MIX)?;
            let u_mix = gl.get_uniform_location(prog_mix, "u_mix");
            let prog = build(FS)?;

            // Static index buffer: two triangles per quad.
            let mut idx: Vec<u8> = Vec::with_capacity(MAX_QUADS * 12);
            for q in 0..MAX_QUADS as u16 {
                for i in [0u16, 1, 2, 2, 1, 3] {
                    idx.extend_from_slice(&(q * 4 + i).to_ne_bytes());
                }
            }
            let ibo = gl.create_buffer()?;
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ibo));
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, &idx, glow::STATIC_DRAW);
            let vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let stride = std::mem::size_of::<Vertex>() as i32;
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 8);
            gl.vertex_attrib_pointer_f32(2, 4, glow::UNSIGNED_BYTE, true, stride, 16);
            for i in 0..3 {
                gl.enable_vertex_attrib_array(i);
            }
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.disable(glow::DEPTH_TEST);
            // Dither only when quantising to RGB565 (hides banding in dark gradients).
            if rgb565 { gl.enable(glow::DITHER) } else { gl.disable(glow::DITHER) }
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            gl.viewport(0, 0, width as i32, height as i32);

            Ok(Gpu {
                egl, dpy, ctx, cfg, surf, cur: surf, detached: false, gl, prog, prog_mix, u_mix, mix: None, vbo, ibo, width, height, info,
                bytes: Vec::new(), scissor_clear: true, clear_rgba: [0.0, 0.0, 0.0, 1.0], main_size: (width, height),
            })
        }
    }

    /// RGBA8 texture, linear filtering, clamped (NPOT-safe on GLES2). `data` None leaves it
    /// uninitialised (filled later with sub-image uploads).
    pub fn texture(&self, w: u32, h: u32, data: Option<&[u8]>) -> glow::Texture {
        unsafe {
            let t = self.gl.create_texture().expect("texture");
            self.gl.bind_texture(glow::TEXTURE_2D, Some(t));
            for (p, v) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                self.gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            self.gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA as i32, w as i32, h as i32, 0,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(data),
            );
            t
        }
    }

    pub fn upload(&self, t: glow::Texture, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(t));
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D, 0, x as i32, y as i32, w as i32, h as i32,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(data)),
            );
        }
    }

    pub fn delete_texture(&self, t: glow::Texture) {
        unsafe { self.gl.delete_texture(t) };
    }

    /// Age of the back buffer in frames (0 = unknown contents -> full redraw).
    pub fn buffer_age(&self) -> i32 {
        self.egl.query_surface(self.dpy, self.surf, BUFFER_AGE_EXT).unwrap_or(0)
    }

    /// Draw `batches` (texture, quads as 4 vertices each) in order, limited to `clip`
    /// (physical pixels, y down). Batches flagged opaque are drawn without blending.
    pub fn draw(&mut self, batches: &[(glow::Texture, bool, &[Vertex])], clip: crate::util::Rect) {
        self.draw_ex(batches, clip, true)
    }

    /// `clear_full`: clear before a full-surface draw (skips Lima's tile reload).
    pub fn draw_ex(&mut self, batches: &[(glow::Texture, bool, &[Vertex])], clip: crate::util::Rect, clear_full: bool) {
        let total: usize = batches.iter().map(|b| b.2.len()).sum();
        assert!(total / 4 <= MAX_QUADS, "too many quads");
        self.bytes.clear();
        for (_, _, v) in batches {
            let raw = unsafe {
                std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(*v))
            };
            self.bytes.extend_from_slice(raw);
        }
        let gl = &self.gl;
        unsafe {
            gl.use_program(Some(self.prog));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(self.ibo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &self.bytes, glow::STREAM_DRAW);
            let full = clip.x0 <= 0 && clip.y0 <= 0 && clip.x1 >= self.width as i32 && clip.y1 >= self.height as i32;
            if full && !clear_full {
                gl.disable(glow::SCISSOR_TEST);
            } else if full {
                // A clear makes Lima skip reloading the old contents into its tile buffer.
                // (Lima's clear always spans the whole surface, so partial frames don't clear
                // and pay the reload only for their damaged tiles.)
                gl.disable(glow::SCISSOR_TEST);
                let [r, g, b, a] = self.clear_rgba;
                gl.clear_color(r, g, b, a);
                gl.clear(glow::COLOR_BUFFER_BIT);
            } else {
                gl.enable(glow::SCISSOR_TEST);
                gl.scissor(clip.x0, self.height as i32 - clip.y1, clip.x1 - clip.x0, clip.y1 - clip.y0);
                if self.scissor_clear && clear_full {
                    let [r, g, b, a] = self.clear_rgba;
                    gl.clear_color(r, g, b, a);
                    gl.clear(glow::COLOR_BUFFER_BIT);
                }
            }
            let mut quad = 0usize;
            for (tex, opaque, v) in batches {
                let n = v.len() / 4;
                if n == 0 {
                    continue;
                }
                if *opaque { gl.disable(glow::BLEND) } else { gl.enable(glow::BLEND) }
                let mixed = quad == 0 && self.mix.is_some();
                if let (true, Some((t2, m))) = (mixed, self.mix) {
                    gl.use_program(Some(self.prog_mix));
                    gl.uniform_1_f32(self.u_mix.as_ref(), m);
                    gl.active_texture(glow::TEXTURE1);
                    gl.bind_texture(glow::TEXTURE_2D, Some(t2));
                    gl.active_texture(glow::TEXTURE0);
                }
                gl.bind_texture(glow::TEXTURE_2D, Some(*tex));
                gl.draw_elements(glow::TRIANGLES, (n * 6) as i32, glow::UNSIGNED_SHORT, (quad * 12) as i32);
                if mixed {
                    gl.use_program(Some(self.prog));
                }
                quad += n;
            }
        }
    }

    /// A second window surface (same config) on another GBM surface: the wallpaper plane.
    pub fn window_surface(&self, surface: &gbm::Surface<drm::control::framebuffer::Handle>) -> Result<egl::Surface, String> {
        use gbm::AsRaw;
        unsafe { self.egl.create_window_surface(self.dpy, self.cfg, surface.as_raw() as *mut _, None) }
            .map_err(|e| format!("eglCreateWindowSurface (plane): {e}"))
    }

    /// Destroy the UI window surface and `extra` (the wallpaper plane's), keeping the context
    /// (textures, programs) current without a surface. False (nothing changed) when the
    /// driver refuses a surfaceless context.
    pub fn detach(&mut self, extra: &[egl::Surface]) -> bool {
        if self.detached {
            return true;
        }
        self.finish();
        if let Err(e) = self.egl.make_current(self.dpy, None, None, Some(self.ctx)) {
            eprintln!("UI_WARN surfaceless context: {e}");
            let _ = self.egl.make_current(self.dpy, Some(self.cur), Some(self.cur), Some(self.ctx));
            return false;
        }
        let _ = self.egl.destroy_surface(self.dpy, self.surf);
        for s in extra {
            let _ = self.egl.destroy_surface(self.dpy, *s);
        }
        self.detached = true;
        true
    }

    /// New UI window surface after `detach`.
    pub fn attach(&mut self, surface: &gbm::Surface<drm::control::framebuffer::Handle>) -> Result<(), String> {
        let surf = self.window_surface(surface)?;
        self.egl.make_current(self.dpy, Some(surf), Some(surf), Some(self.ctx)).map_err(|e| format!("eglMakeCurrent: {e}"))?;
        self.surf = surf;
        self.cur = surf;
        self.detached = false;
        Ok(())
    }

    /// Draw into `target` (None = the UI surface) of `size` physical pixels. Vertex
    /// coordinates stay in UI pixels: a smaller target just samples the scene coarser.
    pub fn bind(&mut self, target: Option<egl::Surface>, size: (u32, u32)) {
        let s = target.unwrap_or(self.surf);
        if s != self.cur {
            if let Err(e) = self.egl.make_current(self.dpy, Some(s), Some(s), Some(self.ctx)) {
                eprintln!("UI_WARN eglMakeCurrent: {e}");
            }
            self.cur = s;
        }
        let size = if target.is_none() { self.main_size } else { size };
        (self.width, self.height) = size;
        unsafe { self.gl.viewport(0, 0, size.0 as i32, size.1 as i32) };
    }

    pub fn swap_target(&self, target: egl::Surface) -> Result<(), String> {
        self.egl.swap_buffers(self.dpy, target).map_err(|e| format!("eglSwapBuffers (plane): {e}"))
    }

    /// RGBA pixels of the bound surface's back buffer, top row first.
    pub fn read_rgba(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut px = vec![0u8; w * h * 4];
        unsafe {
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
        }
        let row = w * 4;
        let mut out = vec![0u8; px.len()];
        for y in 0..h {
            out[y * row..(y + 1) * row].copy_from_slice(&px[(h - 1 - y) * row..(h - y) * row]);
        }
        out
    }

    /// Read the back buffer and write it as a binary PPM (screenshots for review).
    pub fn screenshot(&self, path: &str) -> std::io::Result<()> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut px = vec![0u8; w * h * 4];
        unsafe {
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
        }
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        out.reserve(w * h * 3);
        for row in (0..h).rev() {
            for p in px[row * w * 4..(row + 1) * w * 4].chunks(4) {
                out.extend_from_slice(&p[..3]);
            }
        }
        std::fs::write(path, out)
    }

    /// Wait for the GPU (profiling only: serialises CPU and GPU).
    pub fn finish(&self) {
        unsafe { self.gl.finish() };
    }

    pub fn swap(&self) -> Result<(), String> {
        self.egl.swap_buffers(self.dpy, self.surf).map_err(|e| format!("eglSwapBuffers: {e}"))
    }

    pub fn shutdown(&self) {
        let _ = self.egl.make_current(self.dpy, None, None, None);
        if !self.detached {
            let _ = self.egl.destroy_surface(self.dpy, self.surf);
        }
        let _ = self.egl.destroy_context(self.dpy, self.ctx);
        let _ = self.egl.terminate(self.dpy);
    }
}
