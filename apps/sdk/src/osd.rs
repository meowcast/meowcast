//! The system volume pop-up: one look for the whole device.
//!
//! Whoever owns the screen draws it: the launcher on the home screen, the app in front
//! otherwise (while an app has the screen the launcher cannot draw at all). So that it is the
//! same picture everywhere, the pixels come from here: `Renderer` draws the panel (speaker
//! icon, level bar, number) on the CPU into a small premultiplied ARGB sprite, and `place`
//! says where on the screen it goes.
//!
//! `App` calls `notify` whenever the system volume is set (from the phone, a remote, Settings,
//! another app, or this app itself), so an app only has to draw:
//!
//! - With the SDK's `display::Display` (features "display" + "osd") nothing is needed:
//!   `present` draws the pop-up over the frame. An app that draws only when something
//!   changed keeps presenting while `active()` is true.
//! - An app with its own output (a video overlay plane) asks `current()` a few times a
//!   second and blends `Renderer::render` at `place` with the returned opacity; when it
//!   returns None the pop-up is gone.
//!
//! The state part (when, how opaque, where) needs no feature; the renderer needs "osd"
//! (ab_glyph for the number).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long the pop-up stays after the last change, and its fades.
pub const HOLD: Duration = Duration::from_millis(2000);
pub const FADE_IN: Duration = Duration::from_millis(120);
pub const FADE_OUT: Duration = Duration::from_millis(280);

// Layout, in 1080-line units (everything scales with the screen's height).
/// The panel.
pub const PANEL_W: f32 = 440.0;
pub const PANEL_H: f32 = 96.0;
/// Room for the shadow around the panel inside the sprite.
pub const PAD: f32 = 28.0;
/// Top-right corner of the panel: under the launcher's top bar, at its right margin.
pub const PANEL_RIGHT: f32 = 1920.0 - 110.0;
pub const PANEL_TOP: f32 = 168.0;
/// The accent of the system (the launcher's focus colour).
pub const ACCENT: u32 = 0x3daee9;

/// What the speaker icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Speaker {
    Muted,
    Low,
    Medium,
    High,
}

impl Speaker {
    /// The icon for a volume: crossed out when muted or at 0, then one, two or three waves.
    pub fn of(level: u8, muted: bool) -> Speaker {
        match level {
            _ if muted => Speaker::Muted,
            0 => Speaker::Muted,
            1..=33 => Speaker::Low,
            34..=66 => Speaker::Medium,
            _ => Speaker::High,
        }
    }
}

/// The speaker's shapes on a 24-unit grid, for every renderer of it (the launcher's top bar
/// draws the same icon): the body polygon, the wave arcs as (radius, half angle in degrees)
/// around `WAVE_CENTRE`, and the two strokes of the cross.
pub const BODY: [(f32, f32); 6] = [(2.0, 9.0), (6.0, 9.0), (11.0, 4.5), (11.0, 19.5), (6.0, 15.0), (2.0, 15.0)];
pub const WAVE_CENTRE: (f32, f32) = (11.0, 12.0);
pub const WAVES: [(f32, f32); 3] = [(3.8, 42.0), (7.3, 42.0), (10.8, 42.0)];
pub const CROSS: [((f32, f32), (f32, f32)); 2] = [((15.0, 9.0), (21.0, 15.0)), ((21.0, 9.0), (15.0, 15.0))];
pub const STROKE: f32 = 1.45;

impl Speaker {
    pub fn waves(self) -> usize {
        match self {
            Speaker::Muted => 0,
            Speaker::Low => 1,
            Speaker::Medium => 2,
            Speaker::High => 3,
        }
    }
}

/// Where the sprite goes on a screen of this size: (x, y, width, height) in pixels. On a
/// screen that is not 16:9 it keeps its distance from the right edge.
pub fn place(screen_w: u32, screen_h: u32) -> (i32, i32, u32, u32) {
    let s = screen_h as f32 / 1080.0;
    let (w, h) = sprite_size(screen_h);
    let right = screen_w as f32 - (1920.0 - PANEL_RIGHT) * s;
    ((right - (PANEL_W + PAD) * s).round() as i32, ((PANEL_TOP - PAD) * s).round() as i32, w, h)
}

/// Size of the sprite (panel plus shadow room) for a screen `screen_h` pixels high.
pub fn sprite_size(screen_h: u32) -> (u32, u32) {
    let s = screen_h as f32 / 1080.0;
    (((PANEL_W + 2.0 * PAD) * s).round() as u32, ((PANEL_H + 2.0 * PAD) * s).round() as u32)
}

/// Opacity of a pop-up shown `elapsed` ago that stays for `hold`: fades in, holds, fades out.
/// None once it is gone.
pub fn opacity(elapsed: Duration, hold: Duration) -> Option<f32> {
    if elapsed >= hold + FADE_OUT {
        return None;
    }
    let fade_in = (elapsed.as_secs_f32() / FADE_IN.as_secs_f32()).min(1.0);
    let fade_out = if elapsed > hold { 1.0 - (elapsed - hold).as_secs_f32() / FADE_OUT.as_secs_f32() } else { 1.0 };
    // Ease out on the way in, ease in on the way out.
    Some((1.0 - (1.0 - fade_in).powi(3)) * (fade_out * fade_out * (3.0 - 2.0 * fade_out)))
}

// The last volume the system announced: level (bits 0-7), muted (bit 8), a change counter
// (bits 16-31) and when (ms since `epoch`, bits 32-63). One word, so readers never see a mix.
static LAST: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The system volume was set: show the pop-up (called by `App` for every `VOLUME` message).
pub fn notify(level: u8, muted: bool) {
    // +1: "0 ms" means "never".
    let at = epoch().elapsed().as_millis() as u64 + 1;
    let count = (LAST.load(Ordering::Relaxed) >> 16 & 0xffff) + 1;
    LAST.store(at << 32 | (count & 0xffff) << 16 | (muted as u64) << 8 | level.min(100) as u64, Ordering::Relaxed);
}

/// The pop-up as it should be drawn now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shown {
    pub level: u8,
    pub muted: bool,
    /// 0..1, with the fades.
    pub opacity: f32,
    /// Changes with every `notify` (redraw the sprite when it does).
    pub change: u16,
}

/// What to draw now, or None when no pop-up is due.
pub fn current() -> Option<Shown> {
    let v = LAST.load(Ordering::Relaxed);
    let at = v >> 32;
    if at == 0 {
        return None;
    }
    let elapsed = (epoch().elapsed().as_millis() as u64 + 1).saturating_sub(at);
    let opacity = opacity(Duration::from_millis(elapsed), HOLD)?;
    Some(Shown { level: (v & 0xff) as u8, muted: v >> 8 & 1 == 1, opacity, change: (v >> 16 & 0xffff) as u16 })
}

/// A pop-up is on screen (or fading): keep presenting frames.
pub fn active() -> bool {
    current().is_some()
}

#[cfg(feature = "osd")]
pub use render::{Renderer, Sprite};

#[cfg(feature = "osd")]
mod render {
    use super::*;
    use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

    // Inside the panel (1080-line units).
    const RADIUS: f32 = 14.0;
    const ICON: f32 = 40.0;
    const BAR_X: f32 = 92.0;
    const BAR_W: f32 = 236.0;
    const BAR_H: f32 = 8.0;
    const PANEL_RGB: u32 = 0x1d2233;

    /// Premultiplied ARGB8888 pixels (0xAARRGGBB, the DRM format of that name), row by row.
    #[derive(Clone, Debug, Default, PartialEq)]
    pub struct Sprite {
        pub width: u32,
        pub height: u32,
        pub pixels: Vec<u32>,
    }

    impl Sprite {
        /// The same pixels as premultiplied RGBA bytes (an OpenGL texture).
        pub fn rgba_bytes(&self) -> Vec<u8> {
            self.pixels.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8]).collect()
        }

        /// Blend the sprite over premultiplied ARGB `dest` (`dest_w` pixels per row) at
        /// (x, y) with `opacity`, clipped to the destination.
        pub fn blend_into(&self, dest: &mut [u32], dest_w: usize, x: i32, y: i32, opacity: f32) {
            let dest_h = dest.len() / dest_w.max(1);
            let o = (opacity.clamp(0.0, 1.0) * 256.0) as u32;
            for sy in 0..self.height as i32 {
                let dy = y + sy;
                if dy < 0 || dy as usize >= dest_h {
                    continue;
                }
                for sx in 0..self.width as i32 {
                    let dx = x + sx;
                    if dx < 0 || dx as usize >= dest_w {
                        continue;
                    }
                    let src = self.pixels[(sy as u32 * self.width + sx as u32) as usize];
                    if src >> 24 == 0 {
                        continue;
                    }
                    let d = &mut dest[dy as usize * dest_w + dx as usize];
                    let scaled = |shift: u32| ((src >> shift & 0xff) * o) >> 8;
                    let keep = 255 - scaled(24);
                    let channel = |shift: u32| (scaled(shift) + ((*d >> shift & 0xff) * keep + 127) / 255).min(255);
                    *d = channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0);
                }
            }
        }
    }

    /// Draws the pop-up for one screen height. The panel with its shadow and the four
    /// speaker icons are drawn once; a change of the level redraws only the bar and the
    /// number (well under a millisecond on the dongle).
    pub struct Renderer {
        font: FontVec,
        scale: f32,
        base: Sprite,
        icons: [Vec<u8>; 4],
        icon_px: u32,
        sprite: Sprite,
        drawn: Option<(u8, bool)>,
    }

    /// Coverage (0..1) of a pixel whose centre is `d` pixels outside (negative: inside) an edge.
    fn cover(d: f32) -> f32 {
        (0.5 - d).clamp(0.0, 1.0)
    }

    /// Signed distance from (px, py) to a rounded rectangle.
    fn rounded(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
        let r = r.min(w / 2.0).min(h / 2.0);
        let (qx, qy) = ((px - (x + w / 2.0)).abs() - (w / 2.0 - r), (py - (y + h / 2.0)).abs() - (h / 2.0 - r));
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
    }

    fn segment(px: f32, py: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let t = (((px - a.0) * dx + (py - a.1) * dy) / (dx * dx + dy * dy).max(1e-6)).clamp(0.0, 1.0);
        (px - (a.0 + dx * t)).hypot(py - (a.1 + dy * t))
    }

    fn inside(px: f32, py: f32, poly: &[(f32, f32)]) -> bool {
        let mut inside = false;
        for i in 0..poly.len() {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            if (a.1 > py) != (b.1 > py) && px < (b.0 - a.0) * (py - a.1) / (b.1 - a.1) + a.0 {
                inside = !inside;
            }
        }
        inside
    }

    /// The speaker icon as a coverage mask of `size` x `size` pixels (the shapes of
    /// `BODY`/`WAVES`/`CROSS`, strokes with round ends).
    pub(super) fn speaker_mask(kind: Speaker, size: u32) -> Vec<u8> {
        let k = size as f32 / 24.0;
        let half = STROKE / 2.0;
        let mut out = vec![0u8; (size * size) as usize];
        for y in 0..size {
            for x in 0..size {
                // The body: 4x4 samples per pixel.
                let mut hits = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let (ux, uy) = ((x as f32 + (sx as f32 + 0.5) / 4.0) / k, (y as f32 + (sy as f32 + 0.5) / 4.0) / k);
                        hits += inside(ux, uy, &BODY) as u32;
                    }
                }
                let mut c = hits as f32 / 16.0;
                let (ux, uy) = ((x as f32 + 0.5) / k, (y as f32 + 0.5) / k);
                for (radius, half_angle) in WAVES.iter().take(kind.waves()) {
                    let (rx, ry) = (ux - WAVE_CENTRE.0, uy - WAVE_CENTRE.1);
                    let angle = ry.atan2(rx).to_degrees();
                    let d = if angle.abs() <= *half_angle {
                        (rx.hypot(ry) - radius).abs()
                    } else {
                        let end = half_angle.to_radians() * angle.signum();
                        (rx - radius * end.cos()).hypot(ry - radius * end.sin())
                    };
                    c = c.max(cover((d - half) * k));
                }
                if kind == Speaker::Muted {
                    for (a, b) in CROSS {
                        c = c.max(cover((segment(ux, uy, a, b) - half) * k));
                    }
                }
                out[(y * size + x) as usize] = (c * 255.0 + 0.5) as u8;
            }
        }
        out
    }

    /// Source-over of a straight colour with `alpha` (0..1) onto a premultiplied pixel.
    fn over(dest: &mut u32, rgb: u32, alpha: f32) {
        let a = (alpha.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        if a == 0 {
            return;
        }
        let keep = 255 - a;
        let channel = |shift: u32| (((rgb >> shift & 0xff) * a + (*dest >> shift & 0xff) * keep + 127) / 255).min(255);
        let alpha_out = (a + ((*dest >> 24) * keep + 127) / 255).min(255);
        *dest = alpha_out << 24 | channel(16) << 16 | channel(8) << 8 | channel(0);
    }

    impl Renderer {
        /// `font`: the bytes of NotoSans-Regular.ttf (the number). `screen_h`: the height of
        /// the screen the sprite is for.
        pub fn new(font: Vec<u8>, screen_h: u32) -> Result<Renderer, String> {
            let font = FontVec::try_from_vec(font).map_err(|e| format!("volume pop-up font: {e}"))?;
            let scale = screen_h as f32 / 1080.0;
            let (width, height) = sprite_size(screen_h);
            let mut base = Sprite { width, height, pixels: vec![0; (width * height) as usize] };
            // The shadow (soft, a little below the panel), then the panel.
            let (px, py, pw, ph, r) = (PAD * scale, PAD * scale, PANEL_W * scale, PANEL_H * scale, RADIUS * scale);
            let blur = 18.0 * scale;
            for y in 0..height {
                for x in 0..width {
                    let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                    let p = &mut base.pixels[(y * width + x) as usize];
                    let shadow = (1.0 - rounded(cx, cy - 5.0 * scale, px, py, pw, ph, r).max(0.0) / blur).clamp(0.0, 1.0);
                    over(p, 0x000000, 0.5 * shadow * shadow);
                    over(p, PANEL_RGB, 0.95 * cover(rounded(cx, cy, px, py, pw, ph, r)));
                }
            }
            let icon_px = (ICON * scale).round().max(8.0) as u32;
            let icons = [Speaker::Muted, Speaker::Low, Speaker::Medium, Speaker::High].map(|k| speaker_mask(k, icon_px));
            Ok(Renderer { font, scale, sprite: base.clone(), base, icons, icon_px, drawn: None })
        }

        /// Load the font from the system's font directory (`MEOW_FONTS` in a sandbox).
        pub fn from_font_dir(dir: &str, screen_h: u32) -> Result<Renderer, String> {
            let path = format!("{dir}/NotoSans-Regular.ttf");
            Renderer::new(std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?, screen_h)
        }

        fn bar(&mut self, x: f32, y: f32, w: f32, h: f32, rgb: u32, alpha: f32) {
            let s = self.scale;
            let (x, y, w, h) = (PAD * s + x * s, PAD * s + y * s, w * s, h * s);
            let width = self.sprite.width;
            for py in (y.floor() as u32).saturating_sub(1)..((y + h).ceil() as u32 + 1).min(self.sprite.height) {
                for px in (x.floor() as u32).saturating_sub(1)..((x + w).ceil() as u32 + 1).min(width) {
                    let c = cover(rounded(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, h / 2.0));
                    over(&mut self.sprite.pixels[(py * width + px) as usize], rgb, alpha * c);
                }
            }
        }

        /// The pop-up for this volume. The same sprite is returned until the volume changes.
        pub fn render(&mut self, level: u8, muted: bool) -> &Sprite {
            let level = level.min(100);
            if self.drawn == Some((level, muted)) {
                return &self.sprite;
            }
            self.drawn = Some((level, muted));
            self.sprite.pixels.copy_from_slice(&self.base.pixels);
            let s = self.scale;
            let width = self.sprite.width;
            // The speaker.
            let kind = Speaker::of(level, muted);
            let mask = &self.icons[kind.waves()];
            let (ix, iy) = (((PAD + 28.0) * s).round() as u32, ((PAD + (PANEL_H - ICON) / 2.0) * s).round() as u32);
            let dim = if muted { 0.62 } else { 1.0 };
            for y in 0..self.icon_px {
                for x in 0..self.icon_px {
                    let c = mask[(y * self.icon_px + x) as usize];
                    if c > 0 {
                        over(&mut self.sprite.pixels[((iy + y) * width + ix + x) as usize], 0xffffff, dim * c as f32 / 255.0);
                    }
                }
            }
            // The level: a track, and the part up to the level (grey while muted).
            let bar_y = (PANEL_H - BAR_H) / 2.0;
            self.bar(BAR_X, bar_y, BAR_W, BAR_H, 0xffffff, 0.2);
            if level > 0 {
                let fill = (BAR_W * level as f32 / 100.0).max(BAR_H);
                self.bar(BAR_X, bar_y, fill, BAR_H, if muted { 0xffffff } else { ACCENT }, if muted { 0.42 } else { 1.0 });
            }
            // The number, right-aligned.
            let text = level.to_string();
            let size = PxScale::from(30.0 * s * self.font.height_unscaled() / self.font.units_per_em().unwrap_or(1000.0));
            let scaled = self.font.as_scaled(size);
            let text_w: f32 = text.chars().map(|c| scaled.h_advance(self.font.glyph_id(c))).sum();
            let mut pen = (PAD + PANEL_W - 28.0) * s - text_w;
            let baseline = (PAD + PANEL_H / 2.0 + 11.0) * s;
            for c in text.chars() {
                let id = self.font.glyph_id(c);
                let glyph = id.with_scale_and_position(size, ab_glyph::point(pen.round(), baseline.round()));
                pen += scaled.h_advance(id);
                let Some(outline) = self.font.outline_glyph(glyph) else { continue };
                let bounds = outline.px_bounds();
                let (sprite, height) = (&mut self.sprite.pixels, self.sprite.height);
                outline.draw(|gx, gy, coverage| {
                    let (x, y) = (bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32);
                    if x >= 0 && y >= 0 && (x as u32) < width && (y as u32) < height {
                        over(&mut sprite[(y as u32 * width + x as u32) as usize], 0xffffff, dim * coverage);
                    }
                });
            }
            &self.sprite
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_speaker_follows_the_volume() {
        assert_eq!(Speaker::of(70, true), Speaker::Muted);
        assert_eq!(Speaker::of(0, false), Speaker::Muted);
        assert_eq!((Speaker::of(1, false), Speaker::of(33, false)), (Speaker::Low, Speaker::Low));
        assert_eq!((Speaker::of(34, false), Speaker::of(66, false)), (Speaker::Medium, Speaker::Medium));
        assert_eq!((Speaker::of(67, false), Speaker::of(100, false)), (Speaker::High, Speaker::High));
    }

    #[test]
    fn it_fades_in_holds_and_fades_out() {
        let at = |ms: u64| opacity(Duration::from_millis(ms), HOLD);
        assert_eq!(at(0), Some(0.0));
        assert!(at(60).unwrap() > 0.5, "ease out: most of the way after half the fade");
        assert_eq!(at(120), Some(1.0));
        assert_eq!(at(1999), Some(1.0));
        let leaving = at(2140).unwrap();
        assert!((0.4..0.6).contains(&leaving));
        assert!(at(2279).unwrap() < 0.01);
        assert_eq!(at(2280), None);
        assert_eq!(at(60_000), None);
    }

    #[test]
    fn it_sits_under_the_top_bar_at_the_right_margin() {
        let (x, y, w, h) = place(1920, 1080);
        assert_eq!((w, h), (496, 152));
        assert_eq!(x + w as i32 - 28, 1810, "the panel ends at the right margin");
        assert_eq!(y + 28, 168);
        // Other sizes scale; the sprite stays on the screen.
        for (sw, sh) in [(1280, 720), (3840, 2160), (1024, 768)] {
            let (x, y, w, h) = place(sw, sh);
            assert!(x >= 0 && y >= 0 && x as u32 + w <= sw && y as u32 + h <= sh, "{sw}x{sh}");
        }
        assert_eq!(sprite_size(2160), (992, 304));
    }

    #[test]
    fn notify_shows_it() {
        notify(40, false);
        let a = current().expect("just shown");
        assert_eq!((a.level, a.muted), (40, false));
        notify(140, true);
        let b = current().unwrap();
        assert_eq!((b.level, b.muted), (100, true));
        assert_ne!(a.change, b.change);
        assert!(active());
    }

    #[cfg(feature = "osd")]
    #[test]
    fn the_icon_masks_differ_and_stay_inside() {
        let masks: Vec<Vec<u8>> = [Speaker::Muted, Speaker::Low, Speaker::Medium, Speaker::High].iter().map(|k| render::speaker_mask(*k, 40)).collect();
        let ink = |m: &Vec<u8>| m.iter().map(|v| *v as u32).sum::<u32>();
        assert!(ink(&masks[1]) < ink(&masks[2]) && ink(&masks[2]) < ink(&masks[3]), "each wave adds ink");
        assert_ne!(masks[0], masks[1]);
        for m in &masks {
            // Nothing touches the mask's edge (it would be cut off).
            for i in 0..40 {
                assert_eq!((m[i], m[39 * 40 + i], m[i * 40], m[i * 40 + 39]), (0, 0, 0, 0));
            }
            // The body is solid.
            assert_eq!(m[20 * 40 + 14], 255);
        }
    }

    #[cfg(feature = "osd")]
    #[test]
    fn blending_respects_opacity_and_clipping() {
        let sprite = Sprite { width: 2, height: 1, pixels: vec![0xffff_ffff, 0x8080_8080] };
        let mut dest = vec![0xff00_0000u32; 4];
        sprite.blend_into(&mut dest, 2, 1, 0, 1.0); // second pixel falls off the right edge
        assert_eq!(dest, [0xff00_0000, 0xffff_ffff, 0xff00_0000, 0xff00_0000]);
        let mut dest = vec![0xff00_0000u32; 2];
        sprite.blend_into(&mut dest, 2, 0, 0, 0.5);
        assert_eq!(dest[0] >> 24, 0xff);
        assert!((0x7e..=0x80).contains(&(dest[0] & 0xff)));
        sprite.blend_into(&mut dest, 2, -5, 7, 1.0); // entirely outside: nothing happens
        assert_eq!(sprite.rgba_bytes(), [255, 255, 255, 255, 128, 128, 128, 128]);
    }
}
