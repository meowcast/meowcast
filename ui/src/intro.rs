//! Boot intro and its hand-over to the home screen, in the same process and GL context.
//!
//! The mark is the UI's own focus ring in its purest form: a point of light traces a cyan
//! ring, the ring closes with a soft ripple, a centre dot pops in and the wordmark settles
//! underneath. When the home screen is ready, the wallpaper fades in, the cards rise in a
//! cascade (home.rs entrance) and the ring morphs from a circle into the focus ring of the
//! first card, where the home screen takes it over pixel for pixel.
//!
//! Cost model (Mali-400): every element is a pre-rendered sprite in a small atlas of its
//! own (freed when the intro ends), drawn by the same `texture * colour` shader. Glows are
//! additive (premultiplied colour with alpha 0), the ring "drawing itself" is a fan of
//! wedges cut out of one ring sprite, the morph is a 9-slice of one of a few ring sprites
//! with nearby radii. Before the wallpaper fades in, only the halo's area is repainted.

use crate::atlas::{Atlas, Image, Region};
use crate::gpu::{Gpu, Vertex};
use crate::home::{FOCUS_COLOR, Frame, nine_ex};
use crate::text::{self, Face, Raster};
use crate::util::{RectF, rgba};
use ab_glyph::{Font, ScaleFont};
use std::time::Instant;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

/// Product wordmark under the mark (Noto Sans Light, tracked out).
pub const WORDMARK: &str = "meow";

// Layout, logical 1920x1080 px.
const CX: f32 = 960.0;
const CY: f32 = 482.0;
const R: f32 = 56.0; // ring centre line
const STROKE: f32 = 4.0;
const DOT_R: f32 = 8.5;
const WORD_PX: f32 = 44.0;
const WORD_BASE: f32 = 652.0;
const TRACK_FROM: f32 = 0.70; // letter spacing, em
const TRACK_TO: f32 = 0.34;
const HALO_D: f32 = 1100.0;
const CORE_D: f32 = 560.0;

// Timeline, ms after the first intro frame.
const HALO_IN_MS: f32 = 1100.0;
const HEAD_IN: f32 = 120.0;
const SWEEP_AT: f32 = 220.0;
const SWEEP_MS: f32 = 800.0;
const CLOSE: f32 = SWEEP_AT + SWEEP_MS; // ring closes
const RIPPLE_MS: f32 = 950.0;
const DOT_MS: f32 = 380.0;
const WORD_AT: f32 = CLOSE + 60.0;
const WORD_MS: f32 = 760.0;
const WORD_STAGGER: f32 = 75.0;
/// The intro has played completely; the hand-over may start from here on.
pub const MIN_MS: f32 = 1850.0;
const BREATHE_MS: f32 = 2600.0;

// Hand-over, ms after it starts.
const WALL_MS: f32 = 1000.0;
const HALO_OUT_MS: f32 = 650.0;
const WORD_OUT_MS: f32 = 320.0;
const DOT_OUT_MS: f32 = 260.0;
const MORPH_AT: f32 = 60.0;
/// Hand-over length: the ring lands when the first card has risen (home.rs).
pub const HANDOVER_MS: f32 = crate::home::ENTRANCE_FOCUS_MS + 40.0;

fn clamp01(t: f32) -> f32 {
    t.clamp(0.0, 1.0)
}
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - clamp01(t)).powi(3)
}
fn ease_in_out(t: f32) -> f32 {
    let t = clamp01(t);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}
/// Ease-out with a slight overshoot (the dot "pops").
fn back_out(t: f32) -> f32 {
    let (c1, t) = (1.2f32, clamp01(t) - 1.0);
    1.0 + (c1 + 1.0) * t * t * t + c1 * t * t
}
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
/// Additive glow colour: premultiplied `rgb * k` with alpha 0 (ONE, ONE_MINUS_SRC_ALPHA).
fn glow(rgb: u32, k: f32) -> [u8; 4] {
    let mut c = rgba(rgb, clamp01(k));
    c[3] = 0;
    c
}
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| lerp(((a >> s) & 0xff) as f32, ((b >> s) & 0xff) as f32, clamp01(t)).round() as u32;
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// CPU-side intro artwork (any thread).
pub struct IntroImages {
    ring: Image,
    slices: Vec<(crate::art::Slice, f32)>, // ring 9-slices for the morph, with outer radius
    halo: Image,
    ripple: Image,
    dot: Image,
    glyphs: Vec<(Raster, f32)>, // wordmark glyphs and advances (physical px)
    letter_em: f32,
}

fn white() -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(255, 255, 255, 255);
    p.anti_alias = true;
    p
}

/// Radial sprite (premultiplied white), alpha = f(normalised radius 0..1).
fn radial(n: u32, f: impl Fn(f32) -> f32) -> Image {
    let mut data = Vec::with_capacity((n * n * 4) as usize);
    let c = n as f32 / 2.0;
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = ((x as f32 + 0.5 - c) / c, (y as f32 + 0.5 - c) / c);
            let a = (clamp01(f((dx * dx + dy * dy).sqrt())) * 255.0 + 0.5) as u8;
            data.extend_from_slice(&[a, a, a, a]);
        }
    }
    Image { w: n, h: n, data }
}

/// Lowest morph ring = the home screen's focused ring exactly (home.rs raster scale 1.05).
fn morph_end(scale: f32) -> (f32, f32) {
    (13.0 * scale * 1.05, 4.0 * scale * 1.05)
}

pub fn render(scale: f32, fonts: &str) -> Result<IntroImages, String> {
    let s = scale;
    // Ring sprite: stroke centred on radius R; half-size R + STROKE so wedges cover it.
    let n = (2.0 * (R + STROKE) * s).ceil() as u32 + 2;
    let mut p = Pixmap::new(n, n).unwrap();
    let c = n as f32 / 2.0;
    let circle = PathBuilder::from_circle(c, c, R * s).unwrap();
    p.stroke_path(&circle, &white(), &Stroke { width: STROKE * s, ..Default::default() }, Transform::identity(), None);
    let ring = Image { w: n, h: n, data: p.take() };

    // Morph rings: outer radius from the logo's down to the focus ring's, geometric steps.
    let (r0, w0) = ((R + STROKE / 2.0) * s, STROKE * s);
    let (r1, w1) = morph_end(s);
    let steps = 9;
    let slices = (0..steps)
        .map(|i| {
            let t = i as f32 / (steps - 1) as f32;
            let r = r0 * (r1 / r0).powf(t);
            (crate::art::slice_ring(r, lerp(w0, w1, t)), r)
        })
        .collect();

    let g = (256.0 * s.min(1.5)).round() as u32;
    // Soft bloom: gaussian core with a long tail, zero at the edge.
    let halo = radial(g, |r| (-5.0 * r * r).exp() * (1.0 - r * r).max(0.0).powi(2));
    // Shock-wave ring at 0.78 of the sprite radius.
    let ripple = radial(g, |r| (-((r - 0.78) / 0.075).powi(2) / 2.0).exp() * (1.0 - r).max(0.0).min(0.1) * 10.0);

    let dn = (2.0 * DOT_R * s).ceil() as u32 + 4;
    let mut p = Pixmap::new(dn, dn).unwrap();
    let dc = dn as f32 / 2.0;
    p.fill_path(&PathBuilder::from_circle(dc, dc, DOT_R * s).unwrap(), &white(), FillRule::Winding, Transform::identity(), None);
    let dot = Image { w: dn, h: dn, data: p.take() };

    let font = text::load_font(fonts, Face::Light)?;
    let px = WORD_PX * s;
    let sf = font.as_scaled(text::px_scale(&font, px));
    let glyphs = WORDMARK
        .chars()
        .map(|ch| {
            let id = font.glyph_id(ch);
            (text::rasterise(&font, px, id), sf.h_advance(id))
        })
        .collect();
    Ok(IntroImages { ring, slices, halo, ripple, dot, glyphs, letter_em: px })
}

pub struct Intro {
    pub tex: glow::Texture,
    atlas: Atlas,
    s: f32,
    ring: Region,
    slices: Vec<(Region, u32, f32)>,
    halo: Region,
    ripple: Region,
    dot: Region,
    glyphs: Vec<(Option<Region>, i32, i32, f32)>,
    letter_em: f32,
    start: Instant,
    handover: Option<Instant>,
}

impl Intro {
    pub fn new(gpu: &Gpu, img: &IntroImages, scale: f32, now: Instant) -> Intro {
        // Freed when the intro ends (4 MiB at 1080p, 16 MiB at 4K).
        let mut atlas = Atlas::new(gpu, if scale > 1.0 { 2048 } else { 1024 });
        let mut ins = |i: &Image| atlas.insert(gpu, i).expect("intro atlas space");
        let ring = ins(&img.ring);
        let slices = img.slices.iter().map(|(sl, r)| (ins(&sl.img), sl.margin, *r)).collect();
        let halo = ins(&img.halo);
        let ripple = ins(&img.ripple);
        let dot = ins(&img.dot);
        let glyphs = img
            .glyphs
            .iter()
            .map(|(r, adv)| (r.img.as_ref().map(&mut ins), r.left, r.top, *adv))
            .collect();
        Intro {
            tex: atlas.tex,
            atlas,
            s: scale,
            ring,
            slices,
            halo,
            ripple,
            dot,
            glyphs,
            letter_em: img.letter_em,
            start: now,
            handover: None,
        }
    }

    /// The intro waits (black) until the TV can show a picture.
    pub fn started(&self, now: Instant) -> bool {
        now >= self.start
    }
    pub fn start_at(&self) -> Instant {
        self.start
    }

    fn ms(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.start).as_secs_f32() * 1000.0
    }
    fn handover_ms(&self, now: Instant) -> Option<f32> {
        self.handover.map(|h| now.saturating_duration_since(h).as_secs_f32() * 1000.0)
    }
    /// The mark has fully played (the hand-over may start).
    pub fn played(&self, now: Instant) -> bool {
        self.ms(now) >= MIN_MS
    }
    pub fn handing_over(&self) -> bool {
        self.handover.is_some()
    }
    pub fn begin_handover(&mut self, now: Instant) {
        self.handover.get_or_insert(now);
    }
    /// The hand-over is complete: the home screen owns the focus ring from now on.
    pub fn finished(&self, now: Instant) -> bool {
        self.handover_ms(now).is_some_and(|t| t >= HANDOVER_MS)
    }
    /// Wallpaper opacity (black during the intro).
    pub fn wall_alpha(&self, now: Instant) -> f32 {
        self.handover_ms(now).map_or(0.0, |t| ease_in_out(t / WALL_MS))
    }
    pub fn free(self, gpu: &Gpu) {
        gpu.delete_texture(self.tex);
    }

    fn sprite(&self, f: &mut Frame, id: u32, r: &Region, cx: f32, cy: f32, size: f32, c: [u8; 4]) {
        let st = f.begin();
        let uv = self.atlas.uv(r, 0.0, 0.0, r.w as f32, r.h as f32);
        let h = size / 2.0;
        crate::home::quad(&mut f.verts, cx - h, cy - h, cx + h, cy + h, uv, c);
        f.end(id, st);
    }

    /// The intro's draw list at `now`. `target` is the home screen's focus ring (outer
    /// rectangle, outer corner radius) during the hand-over.
    pub fn build(&self, now: Instant, target: Option<(RectF, f32)>) -> Frame {
        let s = self.s;
        let m = self.ms(now);
        let ho = self.handover_ms(now);
        let (cx, cy) = (CX * s, CY * s);
        let mut f = Frame::default();

        // Halo: a violet bloom (the wallpaper's hue) with a cyan core, breathing gently
        // while the home screen is still loading.
        let breathe = if m > MIN_MS { 0.86 + 0.14 * (std::f32::consts::TAU * (m - MIN_MS) / BREATHE_MS).cos() } else { 1.0 };
        let ha = ease_out(m / HALO_IN_MS) * breathe * (1.0 - ho.map_or(0.0, |t| ease_out(t / HALO_OUT_MS)));
        if ha > 0.0 {
            self.sprite(&mut f, 9_000_001, &self.halo, cx, cy, HALO_D * s, glow(0x4a2f86, 0.55 * ha));
            self.sprite(&mut f, 9_000_002, &self.halo, cx, cy, CORE_D * s, glow(FOCUS_COLOR, 0.30 * ha));
        }

        match (ho, target) {
            (Some(t), Some(target)) => self.morph(&mut f, t, target),
            _ => self.sweep(&mut f, m),
        }

        // Ripple from the closing ring.
        if (CLOSE..CLOSE + RIPPLE_MS).contains(&m) {
            let p = (m - CLOSE) / RIPPLE_MS;
            let radius = lerp((R + STROKE) * s, R * 5.2 * s, ease_out(p));
            let a = 0.6 * (1.0 - p).powf(1.6) * (1.0 - ho.map_or(0.0, |t| clamp01(t / 300.0)));
            self.sprite(&mut f, 9_000_010, &self.ripple, cx, cy, 2.0 * radius / 0.78, glow(FOCUS_COLOR, a));
        }

        // Centre dot pops in as the ring closes; shrinks away when the hand-over starts.
        if m >= CLOSE - 20.0 {
            let k = back_out((m - CLOSE + 20.0) / DOT_MS) * (1.0 - ho.map_or(0.0, |t| ease_in_out(t / DOT_OUT_MS)));
            if k > 0.001 {
                self.sprite(&mut f, 9_000_011, &self.dot, cx, cy, self.dot.w as f32 * k, rgba(0xeaf7ff, 1.0));
            }
        }

        self.wordmark(&mut f, m, ho);
        f
    }

    /// The ring drawing itself: wedges of the ring sprite from 12 o'clock clockwise up to the
    /// head angle, brighter near the head (a comet tail), then a short flash as it closes.
    fn sweep(&self, f: &mut Frame, m: f32) {
        let s = self.s;
        let (cx, cy) = (CX * s, CY * s);
        let theta = 360.0 * ease_in_out((m - SWEEP_AT) / SWEEP_MS);
        let n = self.ring.w as f32;
        let (ox, oy) = ((cx - n / 2.0).round(), (cy - n / 2.0).round());
        let (cx, cy) = (ox + n / 2.0, oy + n / 2.0);
        let rv = n / 2.0;
        let uv = |x: f32, y: f32| {
            let q = self.atlas.uv(&self.ring, x - ox, y - oy, 0.0, 0.0);
            (q[0], q[1])
        };
        let at = |deg: f32| {
            let a = deg.to_radians();
            (cx + rv * a.sin(), cy - rv * a.cos())
        };
        let settle = ease_out((m - CLOSE) / 380.0);
        let flash = if m >= CLOSE { (-(m - CLOSE) / 200.0).exp() } else { 0.0 };
        let colour = mix(FOCUS_COLOR, 0xffffff, 0.55 * flash);
        if theta > 0.0 {
            let st = f.begin();
            let seg = 5.0;
            let mut a0 = 0.0f32;
            while a0 < theta {
                let a1 = (a0 + seg).min(theta);
                let tail = 0.42 + 0.58 * (-(theta - (a0 + a1) / 2.0) / 75.0).exp();
                let c = rgba(colour, lerp(tail, 1.0, settle));
                let (p0, p1) = (at(a0), at(a1));
                let (uc, u0, u1) = (uv(cx, cy), uv(p0.0, p0.1), uv(p1.0, p1.1));
                // Degenerate quad = one triangle (centre, p0, p1) through the quad index buffer.
                for (x, y, (u, v)) in [(cx, cy, uc), (cx, cy, uc), (p0.0, p0.1, u0), (p1.0, p1.1, u1)] {
                    f.verts.push(Vertex { x, y, u, v, rgba: c });
                }
                a0 = a1;
            }
            f.end(9_000_003, st);
        }
        // The point of light leading the sweep.
        let ha = ease_out((m - HEAD_IN) / 260.0) * (1.0 - ease_out((m - CLOSE) / 240.0));
        if ha > 0.0 {
            let a = theta.to_radians();
            let (hx, hy) = (cx + R * s * a.sin(), cy - R * s * a.cos());
            self.sprite(f, 9_000_004, &self.halo, hx, hy, 120.0 * s, glow(0x9fe0ff, 0.9 * ha));
            self.sprite(f, 9_000_005, &self.dot, hx, hy, self.dot.w as f32 * 0.62, rgba(0xffffff, ha));
        }
    }

    /// Hand-over: the ring travels and reshapes from the logo circle into the focus ring
    /// `target` (which moves with the rising first card), using the ring 9-slice whose
    /// radius is closest to the current one.
    fn morph(&self, f: &mut Frame, t: f32, target: (RectF, f32)) {
        let s = self.s;
        let r0 = (R + STROKE / 2.0) * s;
        let n = self.ring.w as f32;
        let (cx, cy) = ((CX * s - n / 2.0).round() + n / 2.0, (CY * s - n / 2.0).round() + n / 2.0);
        let from = RectF::new(cx - r0, cy - r0, 2.0 * r0, 2.0 * r0);
        let p = ease_in_out((t - MORPH_AT) / (HANDOVER_MS - 40.0 - MORPH_AT));
        let (to, r1) = target;
        let mut rect = RectF::new(lerp(from.x, to.x, p), lerp(from.y, to.y, p), lerp(from.w, to.w, p), lerp(from.h, to.h, p));
        if p >= 1.0 {
            rect = to;
        }
        let radius = lerp(r0, r1, p).min(rect.w / 2.0).min(rect.h / 2.0);
        let (reg, margin, art_r) = self
            .slices
            .iter()
            .min_by(|a, b| (a.2 / radius).ln().abs().total_cmp(&(b.2 / radius).ln().abs()))
            .copied()
            .unwrap();
        let st = f.begin();
        nine_ex(&mut f.verts, &self.atlas, &reg, margin, rect, radius * margin as f32 / art_r, rgba(FOCUS_COLOR, 1.0), true);
        f.end(9_000_006, st);
    }

    /// Wordmark: letters fade up one after another while the tracking tightens.
    fn wordmark(&self, f: &mut Frame, m: f32, ho: Option<f32>) {
        let s = self.s;
        if m < WORD_AT {
            return;
        }
        let out = ho.map_or(0.0, |t| ease_out(t / WORD_OUT_MS));
        if out >= 1.0 {
            return;
        }
        let tp = ease_out((m - WORD_AT) / (WORD_MS + 300.0));
        let track = lerp(TRACK_FROM, TRACK_TO, tp) * self.letter_em + lerp(0.0, 0.1, out) * self.letter_em;
        let width: f32 = self.glyphs.iter().map(|g| g.3).sum::<f32>() + track * (self.glyphs.len() as f32 - 1.0);
        let mut pen = CX * s - width / 2.0;
        let settled = tp >= 1.0 && ho.is_none();
        let st = f.begin();
        for (i, (reg, left, top, adv)) in self.glyphs.iter().enumerate() {
            let e = ease_out((m - WORD_AT - WORD_STAGGER * i as f32) / WORD_MS);
            if let Some(r) = reg {
                if e > 0.0 {
                    let x = pen + *left as f32;
                    let y = WORD_BASE * s + *top as f32 + 10.0 * s * (1.0 - e) - 12.0 * s * out;
                    let (x, y) = if settled { (x.round(), y.round()) } else { (x, y) };
                    let uv = self.atlas.uv(r, 0.0, 0.0, r.w as f32, r.h as f32);
                    crate::home::quad(&mut f.verts, x, y, x + r.w as f32, y + r.h as f32, uv, rgba(0xffffff, 0.9 * e * (1.0 - out)));
                }
            }
            pen += adv + track;
        }
        f.end(9_000_020, st);
    }
}
