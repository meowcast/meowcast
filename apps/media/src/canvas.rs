//! A small software canvas for the on-screen UI: premultiplied ARGB8888 (what the display's
//! overlay plane blends per pixel), with the few shapes the UI is made of — gradients,
//! anti-aliased rounded rectangles, and text.
//!
//! The UI changes a few times a second at most, on its own thread, so the CPU draws it; the
//! GPU stays off (less heat) and the video path is never touched.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

/// Straight (not premultiplied) colour: 0xAARRGGBB.
pub type Color = u32;

pub const fn rgba(rgb: u32, alpha: u8) -> Color {
    (alpha as u32) << 24 | (rgb & 0xff_ffff)
}

/// Scale a colour's alpha (0.0 to 1.0).
pub fn fade(color: Color, by: f32) -> Color {
    let alpha = ((color >> 24) as f32 * by.clamp(0.0, 1.0)).round() as u32;
    alpha << 24 | (color & 0xff_ffff)
}

fn premultiply(color: Color, coverage: u32) -> u32 {
    // coverage 0..=256
    let alpha = (color >> 24) * coverage >> 8;
    let channel = |shift: u32| ((color >> shift & 0xff) * alpha + 127) / 255;
    alpha << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
}

/// Source-over for premultiplied pixels.
fn over(source: u32, target: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 255 {
        return source;
    }
    if alpha == 0 {
        return target;
    }
    let keep = 255 - alpha;
    let channel = |shift: u32| (source >> shift & 0xff) + ((target >> shift & 0xff) * keep + 127) / 255;
    channel(24).min(255) << 24 | channel(16).min(255) << 16 | channel(8).min(255) << 8 | channel(0).min(255)
}

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Canvas {
        Canvas { width, height, pixels: vec![0; width * height] }
    }

    /// Replace everything in the rows `from..to` with transparency.
    pub fn clear_rows(&mut self, from: usize, to: usize) {
        let (from, to) = (from.min(self.height), to.min(self.height));
        self.pixels[from * self.width..to * self.width].fill(0);
    }

    fn blend(&mut self, x: i32, y: i32, color: Color, coverage: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height && coverage > 0 {
            let at = y as usize * self.width + x as usize;
            self.pixels[at] = over(premultiply(color, coverage.min(256)), self.pixels[at]);
        }
    }

    /// Fill rows with a colour that changes from `top` to `bottom` (replacing, not blending):
    /// backgrounds and the scrim behind the playback bar. Dithered, so a slow gradient on a
    /// big TV does not show bands.
    pub fn vertical_gradient(&mut self, from: usize, to: usize, top: Color, bottom: Color) {
        let (from, to) = (from.min(self.height), to.min(self.height));
        let rows = (to - from).max(1) as f32;
        for y in from..to {
            let t = (y - from) as f32 / (rows - 1.0).max(1.0);
            // 8 fractional bits per channel carry the dither.
            let mix = |shift: u32| {
                let (a, b) = ((top >> shift & 0xff) as f32, (bottom >> shift & 0xff) as f32);
                ((a + (b - a) * t) * 256.0) as u32
            };
            let (a, r, g, b) = (mix(24), mix(16), mix(8), mix(0));
            let row = &mut self.pixels[y * self.width..(y + 1) * self.width];
            for (x, pixel) in row.iter_mut().enumerate() {
                // A 4x4 ordered dither pattern.
                const BAYER: [u32; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
                let threshold = BAYER[(y & 3) * 4 + (x & 3)] * 16 + 8;
                let step = |value: u32| ((value + threshold) >> 8).min(255);
                let alpha = step(a);
                let channel = |value: u32| (step(value) * alpha + 127) / 255;
                *pixel = alpha << 24 | channel(r) << 16 | channel(g) << 8 | channel(b);
            }
        }
    }

    /// A soft round glow (blended): colour at the centre fading to nothing at `radius`.
    /// Computed in floating point and dithered: a glow is a very slow gradient, and plain
    /// 8-bit blending would draw it as rings.
    pub fn glow(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        let (x0, x1) = ((cx - radius).floor().max(0.0) as usize, ((cx + radius).ceil().max(0.0) as usize).min(self.width));
        let (y0, y1) = ((cy - radius).floor().max(0.0) as usize, ((cy + radius).ceil().max(0.0) as usize).min(self.height));
        let alpha = (color >> 24) as f32 / 255.0;
        let source = [(color >> 16 & 0xff) as f32, (color >> 8 & 0xff) as f32, (color & 0xff) as f32];
        for y in y0..y1 {
            for x in x0..x1 {
                let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let distance = (dx * dx + dy * dy).sqrt() / radius;
                if distance >= 1.0 {
                    continue;
                }
                // Smooth falloff, no visible edge.
                let near = 1.0 - distance;
                let strength = alpha * near * near * (3.0 - 2.0 * near);
                const BAYER: [f32; 16] = [0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0];
                let threshold = (BAYER[(y & 3) * 4 + (x & 3)] + 0.5) / 16.0;
                let pixel = &mut self.pixels[y * self.width + x];
                let channel = |shift: u32, value: f32| {
                    let under = (*pixel >> shift & 0xff) as f32;
                    ((value * strength + under * (1.0 - strength) + threshold) as u32).min(255) << shift
                };
                *pixel = channel(24, 255.0) | channel(16, source[0]) | channel(8, source[1]) | channel(0, source[2]);
            }
        }
    }

    /// An anti-aliased rounded rectangle; `top` to `bottom` is a vertical gradient (pass the
    /// same colour twice for a flat fill). A radius of half the size gives a circle or a pill.
    pub fn rounded_rect(&mut self, x: f32, y: f32, width: f32, height: f32, radius: f32, top: Color, bottom: Color) {
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
        let (cx, cy, hx, hy) = (x + width / 2.0, y + height / 2.0, width / 2.0 - radius, height / 2.0 - radius);
        for py in y.floor() as i32..(y + height).ceil() as i32 {
            let t = ((py as f32 + 0.5 - y) / height).clamp(0.0, 1.0);
            let color = if top == bottom { top } else { mix(top, bottom, t) };
            for px in x.floor() as i32..(x + width).ceil() as i32 {
                // Signed distance to the rounded box.
                let (dx, dy) = ((px as f32 + 0.5 - cx).abs() - hx, (py as f32 + 0.5 - cy).abs() - hy);
                let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0) - radius;
                let coverage = (0.5 - outside).clamp(0.0, 1.0);
                self.blend(px, py, color, (coverage * 256.0) as u32);
            }
        }
    }

    /// An anti-aliased filled triangle (the play glyph).
    pub fn triangle(&mut self, points: [(f32, f32); 3], color: Color) {
        let edge = |a: (f32, f32), b: (f32, f32), p: (f32, f32)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        let orientation = edge(points[0], points[1], points[2]).signum();
        let bound = |pick: fn(f32, f32) -> f32, axis: fn(&(f32, f32)) -> f32| points.iter().map(axis).fold(axis(&points[0]), pick);
        let (x0, x1) = (bound(f32::min, |p| p.0).floor() as i32, bound(f32::max, |p| p.0).ceil() as i32);
        let (y0, y1) = (bound(f32::min, |p| p.1).floor() as i32, bound(f32::max, |p| p.1).ceil() as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                // 4x4 samples per pixel.
                let mut inside = 0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let p = (px as f32 + (sx as f32 + 0.5) / 4.0, py as f32 + (sy as f32 + 0.5) / 4.0);
                        if (0..3).all(|i| edge(points[i], points[(i + 1) % 3], p) * orientation >= 0.0) {
                            inside += 1;
                        }
                    }
                }
                self.blend(px, py, color, inside * 16);
            }
        }
    }

    /// Draw one line of text with its baseline at `baseline`; returns the width drawn.
    /// Characters the font lacks are left out.
    pub fn text(&mut self, font: &FontVec, size: f32, x: f32, baseline: f32, color: Color, text: &str) -> f32 {
        let scaled = font.as_scaled(PxScale::from(size));
        let mut pen = x;
        let mut previous = None;
        for character in text.chars() {
            let id = scaled.glyph_id(character);
            if id.0 == 0 {
                continue;
            }
            if let Some(previous) = previous {
                pen += scaled.kern(previous, id);
            }
            let glyph = id.with_scale_and_position(PxScale::from(size), ab_glyph::point(pen, baseline));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| self.blend(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, color, (coverage * 256.0) as u32));
            }
            pen += scaled.h_advance(id);
            previous = Some(id);
        }
        pen - x
    }

    /// The canvas as a PNG file (uncompressed inside): for looking at a design off the device.
    pub fn png(&self) -> Vec<u8> {
        let mut raw = Vec::with_capacity((self.width * 4 + 1) * self.height);
        for row in self.pixels.chunks(self.width) {
            raw.push(0);
            for &pixel in row {
                // Un-premultiply over black: what the screen shows where no video is underneath.
                raw.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8, 255]);
            }
        }
        let mut zlib = vec![0x78, 0x01];
        let mut chunks = raw.chunks(65535).peekable();
        while let Some(chunk) = chunks.next() {
            zlib.push(chunks.peek().is_none() as u8);
            zlib.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            zlib.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
            zlib.extend_from_slice(chunk);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        zlib.extend_from_slice(&(b << 16 | a).to_be_bytes());
        let crc = |bytes: &[u8]| {
            !bytes.iter().fold(!0u32, |crc, &byte| (0..8).fold(crc ^ byte as u32, |c, _| if c & 1 == 1 { c >> 1 ^ 0xedb8_8320 } else { c >> 1 }))
        };
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8; 4], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let body = [kind.as_slice(), data].concat();
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc(&body).to_be_bytes());
        };
        let header = [&(self.width as u32).to_be_bytes()[..], &(self.height as u32).to_be_bytes()[..], &[8, 6, 0, 0, 0]].concat();
        chunk(b"IHDR", &header);
        chunk(b"IDAT", &zlib);
        chunk(b"IEND", &[]);
        out
    }
}

/// Between two straight colours.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let channel = |shift: u32| {
        let (x, y) = ((a >> shift & 0xff) as f32, (b >> shift & 0xff) as f32);
        ((x + (y - x) * t).round() as u32).min(255) << shift
    };
    channel(24) | channel(16) | channel(8) | channel(0)
}

/// How wide `text` is at `size`.
pub fn measure(font: &FontVec, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let id = scaled.glyph_id(character);
        if id.0 == 0 {
            continue;
        }
        if let Some(previous) = previous {
            width += scaled.kern(previous, id);
        }
        width += scaled.h_advance(id);
        previous = Some(id);
    }
    width
}

/// `text` cut to fit `width`, ending in an ellipsis when it had to be cut.
pub fn fit(font: &FontVec, size: f32, text: &str, width: f32) -> String {
    if measure(font, size, text) <= width {
        return text.to_string();
    }
    let mut kept: Vec<char> = text.chars().collect();
    while !kept.is_empty() {
        kept.pop();
        let candidate: String = kept.iter().collect::<String>().trim_end().to_string() + "…";
        if measure(font, size, &candidate) <= width {
            return candidate;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_is_premultiplied_source_over() {
        assert_eq!(premultiply(0x80ff8040, 256), 0x80804020);
        assert_eq!(premultiply(0xffffffff, 128), 0x7f7f7f7f);
        assert_eq!(over(0xff102030, 0xffffffff), 0xff102030);
        assert_eq!(over(0, 0x80112233), 0x80112233);
        // Half-transparent white over opaque black: mid grey, still opaque.
        assert_eq!(over(0x80808080, 0xff000000), 0xff808080);
    }

    #[test]
    fn rounded_rect_covers_its_middle_and_spares_its_corners() {
        let mut canvas = Canvas::new(40, 20);
        canvas.rounded_rect(0.0, 0.0, 40.0, 20.0, 10.0, 0xffffffff, 0xffffffff);
        assert_eq!(canvas.pixels[10 * 40 + 20], 0xffffffff);
        assert_eq!(canvas.pixels[0], 0); // the very corner is outside the curve
        let edge = canvas.pixels[10 * 40] >> 24; // the left end of the pill: partly covered
        assert!(edge > 0x20 && edge < 0xff, "{edge:#x}");
    }

    #[test]
    fn gradient_runs_from_top_to_bottom_and_png_is_well_formed() {
        let mut canvas = Canvas::new(8, 64);
        canvas.vertical_gradient(0, 64, 0x00000000, 0xff000000);
        assert_eq!(canvas.pixels[0] >> 24, 0);
        assert_eq!(canvas.pixels[63 * 8] >> 24, 255);
        assert!((canvas.pixels[32 * 8] >> 24).abs_diff(128) < 8);
        let png = canvas.png();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        assert_eq!((fade(0xff123456, 0.5), mix(0xff000000, 0xffffffff, 0.5)), (0x80123456, 0xff808080));
    }
}
