//! Pre-rendered artwork, drawn once on the CPU with tiny-skia at the active scale, then
//! uploaded to the atlas. The GPU only ever copies these texels (9-slices for rounded
//! cards, shadows and the focus ring; icons as plain sprites), so gradients, blur and
//! anti-aliased curves cost nothing per frame.

use crate::atlas::Image;
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, Path, PathBuilder,
    Pixmap, Point, SpreadMode, Stroke, Transform,
};

fn pixmap_image(p: Pixmap) -> Image {
    let (w, h) = (p.width(), p.height());
    Image { w, h, data: p.take() }
}

fn solid(rgb: u32, a: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, a);
    p.anti_alias = true;
    p
}

pub fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().expect("rounded rect")
}

/// A 9-slice source: square image whose `margin` texels on each side are the corners/edges
/// and whose centre (2 texels) is stretched.
pub struct Slice {
    pub img: Image,
    pub margin: u32,
}

/// White rounded rectangle (tinted per card by vertex colour).
pub fn slice_rounded(r: f32) -> Slice {
    let m = r.ceil() as u32 + 1;
    let s = 2 * m + 2;
    let mut p = Pixmap::new(s, s).unwrap();
    p.fill_path(&rounded_rect(0.0, 0.0, s as f32, s as f32, r), &solid(0xffffff, 255), FillRule::Winding, Transform::identity(), None);
    Slice { img: pixmap_image(p), margin: m }
}

/// White rounded ring of stroke `width`, outer radius `r` (tinted cyan for focus).
pub fn slice_ring(r: f32, width: f32) -> Slice {
    let m = r.ceil() as u32 + 1;
    let s = 2 * m + 2;
    let mut p = Pixmap::new(s, s).unwrap();
    let h = width / 2.0;
    let path = rounded_rect(h, h, s as f32 - width, s as f32 - width, (r - h).max(0.5));
    let stroke = Stroke { width, line_join: LineJoin::Round, ..Default::default() };
    p.stroke_path(&path, &solid(0xffffff, 255), &stroke, Transform::identity(), None);
    Slice { img: pixmap_image(p), margin: m }
}

/// Soft shadow of a rounded rect with corner radius `r`, blurred by ~`blur` px. The slice
/// extends `pad` texels beyond the casting rectangle on each side.
pub fn slice_shadow(r: f32, blur: f32) -> (Slice, u32) {
    let b = (blur / 3.0).ceil().max(1.0) as usize; // three box passes ~ gaussian
    let pad = (3 * b + 2) as u32;
    let core = r.ceil() as u32 + 3 * b as u32 + 2;
    let m = pad + core;
    let s = 2 * m + 2;
    let mut p = Pixmap::new(s, s).unwrap();
    p.fill_path(
        &rounded_rect(pad as f32, pad as f32, (s - 2 * pad) as f32, (s - 2 * pad) as f32, r),
        &solid(0x000000, 255),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let n = s as usize;
    let mut a: Vec<f32> = p.data().chunks(4).map(|c| c[3] as f32 / 255.0).collect();
    let mut tmp = vec![0.0f32; n * n];
    for _ in 0..3 {
        for (horizontal, src_is_a) in [(true, true), (false, false)] {
            let (src, dst) = if src_is_a { (&a, &mut tmp) } else { (&tmp, &mut a) };
            for line in 0..n {
                for i in 0..n {
                    let (lo, hi) = (i.saturating_sub(b), (i + b).min(n - 1));
                    let mut sum = 0.0;
                    for j in lo..=hi {
                        sum += if horizontal { src[line * n + j] } else { src[j * n + line] };
                    }
                    let v = sum / (2 * b + 1) as f32;
                    if horizontal { dst[line * n + i] = v } else { dst[i * n + line] = v }
                }
            }
        }
    }
    let data = a.iter().flat_map(|v| [0, 0, 0, (v * 255.0 + 0.5) as u8]).collect();
    (Slice { img: Image { w: s, h: s, data }, margin: m }, pad)
}

/// Monochrome line icons on a 24-unit grid (Breeze-like), plus app badge glyphs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Glyph {
    Search,
    Apps,
    Edit,
    Sliders,
    Volume,
    Wifi,
    Device,
    Power,
    Globe,
    Play,
    Music,
    Terminal,
    Cast,
    Photo,
    Folder,
    Archive,
    Headphones,
    Gear,
    Sun,
    Gamepad,
    Monitor,
    Clock,
    Info,
    Check,
    Chevron,
    Lan,
    /// The speaker with fewer waves, and crossed out (`Volume` is the loud one): the shapes
    /// of meow_app::osd, so the top bar and the volume pop-up show the same icon.
    VolumeMid,
    VolumeLow,
    VolumeMute,
    /// The phone, crossed out (no phone remote paired).
    DeviceOff,
}

fn arc(pb: &mut PathBuilder, cx: f32, cy: f32, r: f32, a0: f32, a1: f32, start: bool) {
    let steps = (((a1 - a0).abs() / 6.0).ceil() as usize).max(2);
    for i in 0..=steps {
        let a = (a0 + (a1 - a0) * i as f32 / steps as f32).to_radians();
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 && start { pb.move_to(x, y) } else { pb.line_to(x, y) }
    }
}

fn poly(pts: &[(f32, f32)], close: bool) -> Path {
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    for p in &pts[1..] {
        pb.line_to(p.0, p.1);
    }
    if close {
        pb.close();
    }
    pb.finish().unwrap()
}

fn circle(x: f32, y: f32, r: f32) -> Path {
    PathBuilder::from_circle(x, y, r).unwrap()
}

/// Draw `g` in white into `p` with transform `t` (24-unit grid -> pixels).
fn draw_glyph(p: &mut Pixmap, g: Glyph, t: Transform, line: f32) {
    let white = solid(0xffffff, 255);
    let stroke = Stroke { width: line, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
    let mut strokes: Vec<Path> = Vec::new();
    let mut fills: Vec<Path> = Vec::new();
    match g {
        Glyph::Search => {
            strokes.push(circle(10.0, 10.0, 6.5));
            strokes.push(poly(&[(15.0, 15.0), (20.5, 20.5)], false));
        }
        Glyph::Apps => {
            for gy in 0..3 {
                for gx in 0..3 {
                    let (x, y) = (4.0 + gx as f32 * 7.0, 4.0 + gy as f32 * 7.0);
                    fills.push(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x - 1.4, y - 1.4, 2.8, 2.8).unwrap()));
                }
            }
        }
        Glyph::Edit => {
            strokes.push(poly(&[(15.0, 4.0), (20.0, 9.0), (8.5, 20.5), (3.5, 20.5), (3.5, 15.5)], true));
            strokes.push(poly(&[(12.5, 6.5), (17.5, 11.5)], false));
        }
        Glyph::Sliders => {
            strokes.push(poly(&[(3.0, 8.0), (13.0, 8.0)], false));
            strokes.push(poly(&[(17.5, 8.0), (21.0, 8.0)], false));
            fills.push(circle(15.0, 8.0, 2.4));
            strokes.push(poly(&[(3.0, 17.0), (5.0, 17.0)], false));
            strokes.push(poly(&[(9.5, 17.0), (21.0, 17.0)], false));
            strokes.push(circle(7.2, 17.0, 2.1));
        }
        Glyph::Volume | Glyph::VolumeMid | Glyph::VolumeLow | Glyph::VolumeMute => {
            use meow_app::osd::{BODY, CROSS, WAVE_CENTRE, WAVES};
            fills.push(poly(&BODY, true));
            let waves = match g {
                Glyph::Volume => 3,
                Glyph::VolumeMid => 2,
                Glyph::VolumeLow => 1,
                _ => 0,
            };
            for (r, half) in WAVES.iter().take(waves) {
                let mut pb = PathBuilder::new();
                arc(&mut pb, WAVE_CENTRE.0, WAVE_CENTRE.1, *r, -half, *half, true);
                strokes.push(pb.finish().unwrap());
            }
            if g == Glyph::VolumeMute {
                for (a, b) in CROSS {
                    strokes.push(poly(&[a, b], false));
                }
            }
        }
        Glyph::Wifi => {
            for r in [5.0, 10.0, 15.0] {
                let mut pb = PathBuilder::new();
                arc(&mut pb, 12.0, 20.0, r, 225.0, 315.0, true);
                strokes.push(pb.finish().unwrap());
            }
            fills.push(circle(12.0, 19.5, 1.6));
        }
        Glyph::Device | Glyph::DeviceOff => {
            strokes.push(rounded_rect(6.5, 2.5, 11.0, 19.0, 2.0));
            strokes.push(poly(&[(11.0, 18.5), (13.0, 18.5)], false));
        }
        Glyph::Power => {
            let mut pb = PathBuilder::new();
            arc(&mut pb, 12.0, 13.0, 8.0, -60.0, 240.0, true);
            strokes.push(pb.finish().unwrap());
            strokes.push(poly(&[(12.0, 3.0), (12.0, 11.0)], false));
        }
        Glyph::Globe => {
            strokes.push(circle(12.0, 12.0, 8.5));
            let mut pb = PathBuilder::new();
            pb.push_oval(tiny_skia::Rect::from_xywh(8.0, 3.5, 8.0, 17.0).unwrap());
            strokes.push(pb.finish().unwrap());
            strokes.push(poly(&[(3.5, 12.0), (20.5, 12.0)], false));
            strokes.push(poly(&[(5.5, 7.5), (18.5, 7.5)], false));
            strokes.push(poly(&[(5.5, 16.5), (18.5, 16.5)], false));
        }
        Glyph::Play => fills.push(poly(&[(8.5, 5.5), (19.0, 12.0), (8.5, 18.5)], true)),
        Glyph::Music => {
            let mut pb = PathBuilder::new();
            pb.push_oval(tiny_skia::Rect::from_xywh(5.5, 14.5, 7.0, 5.5).unwrap());
            fills.push(pb.finish().unwrap());
            strokes.push(poly(&[(12.0, 17.0), (12.0, 4.0)], false));
            let mut pb = PathBuilder::new();
            pb.move_to(12.0, 4.0);
            pb.cubic_to(14.0, 6.5, 18.0, 6.5, 17.0, 11.0);
            strokes.push(pb.finish().unwrap());
        }
        Glyph::Terminal => {
            strokes.push(poly(&[(7.0, 7.0), (12.0, 12.0), (7.0, 17.0)], false));
            strokes.push(poly(&[(13.5, 17.0), (18.0, 17.0)], false));
        }
        Glyph::Cast => {
            strokes.push(poly(&[(3.0, 9.0), (3.0, 5.0), (21.0, 5.0), (21.0, 19.0), (14.0, 19.0)], false));
            for r in [4.0, 8.0] {
                let mut pb = PathBuilder::new();
                arc(&mut pb, 3.0, 20.0, r, -90.0, 0.0, true);
                strokes.push(pb.finish().unwrap());
            }
            fills.push(circle(3.5, 19.5, 1.6));
        }
        Glyph::Photo => {
            strokes.push(rounded_rect(3.0, 5.0, 18.0, 14.0, 1.5));
            fills.push(poly(&[(5.0, 17.0), (10.0, 11.0), (13.0, 14.5), (15.0, 12.5), (19.0, 17.0)], true));
            fills.push(circle(16.0, 9.0, 1.7));
        }
        Glyph::Folder => fills.push(poly(&[(3.0, 6.0), (9.0, 6.0), (11.0, 8.0), (21.0, 8.0), (21.0, 18.5), (3.0, 18.5)], true)),
        Glyph::Archive => {
            strokes.push(rounded_rect(5.5, 3.0, 13.0, 18.0, 1.5));
            for i in 0..5 {
                let y = 4.5 + i as f32 * 2.4;
                let x = if i % 2 == 0 { 10.5 } else { 12.0 };
                fills.push(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(x, y, 1.6, 1.4).unwrap()));
            }
            strokes.push(rounded_rect(10.0, 15.0, 4.0, 3.5, 0.8));
        }
        Glyph::Headphones => {
            let mut pb = PathBuilder::new();
            arc(&mut pb, 12.0, 13.0, 8.0, 180.0, 360.0, true);
            strokes.push(pb.finish().unwrap());
            fills.push(rounded_rect(3.0, 13.0, 4.5, 7.0, 1.5));
            fills.push(rounded_rect(16.5, 13.0, 4.5, 7.0, 1.5));
        }
        Glyph::Gear => {
            let mut pts = Vec::new();
            for i in 0..32 {
                let a = (i as f32 * 11.25 - 5.6).to_radians();
                let r = if (i / 2) % 2 == 0 { 9.0 } else { 6.8 };
                pts.push((12.0 + r * a.cos(), 12.0 + r * a.sin()));
            }
            fills.push(poly(&pts, true));
            fills.push(circle(12.0, 12.0, 2.8)); // punched out below
        }
        Glyph::Sun => {
            fills.push(circle(12.0, 12.0, 4.2));
            for i in 0..8 {
                let a = (i as f32 * 45.0).to_radians();
                strokes.push(poly(&[(12.0 + 6.5 * a.cos(), 12.0 + 6.5 * a.sin()), (12.0 + 9.0 * a.cos(), 12.0 + 9.0 * a.sin())], false));
            }
        }
        Glyph::Gamepad => {
            fills.push(rounded_rect(2.5, 7.0, 19.0, 11.0, 5.0));
        }
        Glyph::Monitor => {
            strokes.push(rounded_rect(2.5, 4.0, 19.0, 12.5, 1.5));
            strokes.push(poly(&[(12.0, 16.5), (12.0, 19.5)], false));
            strokes.push(poly(&[(8.0, 20.0), (16.0, 20.0)], false));
        }
        Glyph::Clock => {
            strokes.push(circle(12.0, 12.0, 8.5));
            strokes.push(poly(&[(12.0, 7.0), (12.0, 12.0), (15.5, 14.5)], false));
        }
        Glyph::Info => {
            strokes.push(circle(12.0, 12.0, 8.5));
            strokes.push(poly(&[(12.0, 11.0), (12.0, 16.5)], false));
            fills.push(circle(12.0, 7.9, 1.25));
        }
        Glyph::Check => strokes.push(poly(&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)], false)),
        Glyph::Chevron => strokes.push(poly(&[(9.0, 5.5), (15.5, 12.0), (9.0, 18.5)], false)),
        Glyph::Lan => {
            strokes.push(rounded_rect(9.0, 3.0, 6.0, 5.0, 1.0));
            strokes.push(rounded_rect(3.0, 16.0, 6.0, 5.0, 1.0));
            strokes.push(rounded_rect(15.0, 16.0, 6.0, 5.0, 1.0));
            strokes.push(poly(&[(12.0, 8.0), (12.0, 12.0)], false));
            strokes.push(poly(&[(6.0, 16.0), (6.0, 12.0), (18.0, 12.0), (18.0, 16.0)], false));
        }
    }
    for f in &fills {
        p.fill_path(f, &white, FillRule::EvenOdd, t, None);
    }
    if g == Glyph::Gear {
        // Punch the hub out again (EvenOdd across separate paths does not combine).
        let mut clear = Paint::default();
        clear.blend_mode = tiny_skia::BlendMode::Clear;
        clear.anti_alias = true;
        p.fill_path(&circle(12.0, 12.0, 2.8), &clear, FillRule::Winding, t, None);
    }
    if g == Glyph::Gamepad {
        let mut clear = Paint::default();
        clear.blend_mode = tiny_skia::BlendMode::Clear;
        clear.anti_alias = true;
        let cs = Stroke { width: 1.6, line_cap: LineCap::Round, ..Default::default() };
        p.stroke_path(&poly(&[(5.5, 12.5), (9.5, 12.5)], false), &clear, &cs, t, None);
        p.stroke_path(&poly(&[(7.5, 10.5), (7.5, 14.5)], false), &clear, &cs, t, None);
        p.fill_path(&circle(15.5, 11.0, 1.2), &clear, FillRule::Winding, t, None);
        p.fill_path(&circle(17.8, 13.8, 1.2), &clear, FillRule::Winding, t, None);
    }
    for s in &strokes {
        p.stroke_path(s, &white, &stroke, t, None);
    }
    if g == Glyph::DeviceOff {
        // A slash across the phone, with a gap cut around it so it reads at 38 px.
        let slash = poly(&[(4.0, 4.5), (20.0, 19.5)], false);
        let mut clear = Paint::default();
        clear.blend_mode = tiny_skia::BlendMode::Clear;
        clear.anti_alias = true;
        let gap = Stroke { width: line * 3.4, line_cap: LineCap::Butt, ..Default::default() };
        p.stroke_path(&slash, &clear, &gap, t, None);
        p.stroke_path(&slash, &white, &stroke, t, None);
    }
}

/// White line icon, `size` px square (top bar).
pub fn line_icon(g: Glyph, size: u32) -> Image {
    let mut p = Pixmap::new(size, size).unwrap();
    let k = size as f32 / 24.0;
    draw_glyph(&mut p, g, Transform::from_scale(k, k), 1.45);
    pixmap_image(p)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Badge {
    Circle,
    Square,
}

/// App badge: gradient disc (or rounded square) with a soft drop shadow and a white glyph.
pub fn app_icon(g: Glyph, d: u32, top: u32, bottom: u32, badge: Badge) -> Image {
    let pad = (d as f32 * 0.08).ceil() as u32;
    let s = d + 2 * pad;
    let mut p = Pixmap::new(s, s).unwrap();
    let (o, df) = (pad as f32, d as f32);
    let shape = |dx: f32, dy: f32| match badge {
        Badge::Circle => circle(o + df / 2.0 + dx, o + df / 2.0 + dy, df / 2.0),
        Badge::Square => rounded_rect(o + dx, o + dy, df, df, df * 0.08),
    };
    // Baked soft shadow: a few offset translucent copies.
    for i in (1..=4).rev() {
        let off = i as f32 * df * 0.012;
        p.fill_path(&shape(off * 0.6, off), &solid(0x000000, 18), FillRule::Winding, Transform::identity(), None);
    }
    let c = |rgb: u32| Color::from_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255);
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.shader = LinearGradient::new(
        Point::from_xy(o, o),
        Point::from_xy(o + df * 0.7, o + df),
        vec![GradientStop::new(0.0, c(top)), GradientStop::new(1.0, c(bottom))],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    p.fill_path(&shape(0.0, 0.0), &paint, FillRule::Winding, Transform::identity(), None);
    let gs = df * 0.56;
    let k = gs / 24.0;
    let t = Transform::from_scale(k, k).post_translate(o + (df - gs) / 2.0, o + (df - gs) / 2.0);
    draw_glyph(&mut p, g, t, 1.7);
    pixmap_image(p)
}

/// Procedural pre-blurred wallpaper (dark navy/purple with warm out-of-focus glows),
/// generated once at low resolution; the GPU's bilinear upscale keeps it smooth.
pub fn wallpaper(w: u32, h: u32) -> Image {
    // (x, y, radius) in units of the height, colour, strength
    const BLOBS: &[(f32, f32, f32, u32, f32)] = &[
        (0.25, 0.15, 0.30, 0xb06a3a, 0.30),
        (0.62, 0.20, 0.26, 0xc27838, 0.30),
        (0.95, 0.28, 0.30, 0xa0603a, 0.26),
        (1.30, 0.22, 0.28, 0xb87040, 0.24),
        (1.60, 0.30, 0.32, 0x9a5a3c, 0.22),
        (0.45, 0.45, 0.45, 0x5a3a7a, 0.40),
        (1.15, 0.55, 0.45, 0x4b3170, 0.40),
        (0.05, 0.70, 0.40, 0x2e2a5c, 0.35),
        (1.70, 0.75, 0.45, 0x2a2858, 0.35),
        (0.90, 1.05, 0.70, 0x10142c, 0.45),
    ];
    let aspect = w as f32 / h as f32;
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let fy = y as f32 / h as f32;
        let base = [0x18 as f32, 0x14 as f32, 0x2c as f32];
        let bottom = [0x0c as f32, 0x0e as f32, 0x1c as f32];
        let mut row_base = [0.0f32; 3];
        for i in 0..3 {
            row_base[i] = base[i] + (bottom[i] - base[i]) * fy;
        }
        // Darken towards the bottom where cards and text sit.
        let t = ((fy - 0.45) / 0.55).clamp(0.0, 1.0);
        let dim = 1.0 - 0.35 * t * t * (3.0 - 2.0 * t);
        for x in 0..w {
            let fx = x as f32 / h as f32;
            let mut c = row_base;
            for &(bx, by, br, rgb, s) in BLOBS {
                let (dx, dy) = (fx - bx * aspect / 1.777, fy - by);
                let q = 1.0 - (dx * dx + dy * dy) / (br * br);
                if q > 0.0 {
                    let f = q * q * s;
                    c[0] += ((rgb >> 16) & 0xff) as f32 * f;
                    c[1] += ((rgb >> 8) & 0xff) as f32 * f;
                    c[2] += (rgb & 0xff) as f32 * f;
                }
            }
            // Ordered dither (+-0.5 LSB) against banding in the dark gradients.
            let n = ((x.wrapping_mul(7) ^ y.wrapping_mul(13)) & 15) as f32 / 16.0 - 0.47;
            for v in c.iter_mut() {
                *v = (*v * dim + n).clamp(0.0, 255.0);
            }
            data.extend_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, 255]);
        }
    }
    Image { w, h, data }
}

/// Soft radial glow (premultiplied white, smoothstep falloff), small: drawn stretched
/// behind the launch splash icon, where bilinear upscaling keeps it smooth.
pub fn glow(d: u32) -> Image {
    let mut data = vec![0u8; (d * d * 4) as usize];
    let c = (d as f32 - 1.0) / 2.0;
    for y in 0..d {
        for x in 0..d {
            let r = (((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / c).min(1.0);
            let t = 1.0 - r;
            let a = (t * t * (3.0 - 2.0 * t) * 255.0) as u8;
            let i = ((y * d + x) * 4) as usize;
            data[i..i + 4].copy_from_slice(&[a, a, a, a]);
        }
    }
    Image { w: d, h: d, data }
}

/// Anti-aliased white disc (spinner dots).
pub fn dot(d: u32) -> Image {
    let mut p = Pixmap::new(d, d).unwrap();
    let r = d as f32 / 2.0 - 0.5;
    p.fill_path(&circle(d as f32 / 2.0, d as f32 / 2.0, r), &solid(0xffffff, 255), FillRule::Winding, Transform::identity(), None);
    pixmap_image(p)
}
