//! Text: Noto Sans (Light/Regular/Medium), glyphs rasterised lazily by ab_glyph on first
//! use into the shared atlas, then drawn as textured quads. Pens are snapped to whole pixels
//! so glyph texels map 1:1 to screen pixels (crisp with linear filtering).
//!
//! The firmware carries Latin subsets of the faces (assets/fonts/subset.py: same glyph ids as
//! the full faces). Text with a character a subset lacks (an SSID or a name in another script)
//! marks its face as `missing`; the launcher then fetches the full face from the store
//! (asset fonts/<face>) and `replace_font` swaps it in. Until then such characters show the
//! .notdef box.

use crate::atlas::{Atlas, Image, Region};
use crate::gpu::{Gpu, Vertex};
use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont, point};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[allow(dead_code)]
pub enum Face {
    Light = 0,
    Regular = 1,
    Medium = 2,
}

#[derive(Clone, Copy)]
struct Glyph {
    region: Option<Region>, // None for blank glyphs (space)
    left: i32,
    top: i32,
}

pub struct Text {
    fonts: Vec<FontVec>,
    cache: HashMap<(Face, u16, u32), Glyph>,
    pub rasterised: u32,
    /// Bit per face: drawn text had a character this face does not have.
    missing: std::cell::Cell<u8>,
    /// Bit per face: the full face replaced the subset.
    full: u8,
    /// Counts font replacements: widths measured before one may be stale (`Fits`).
    generation: u32,
}

/// The store asset holding the full version of `face`.
pub fn full_asset(face: Face) -> String {
    format!("fonts/{}", FACES[face as usize])
}

pub const ALL_FACES: [Face; 3] = [Face::Light, Face::Regular, Face::Medium];

pub const FACES: [&str; 3] = ["NotoSans-Light.ttf", "NotoSans-Regular.ttf", "NotoSans-Medium.ttf"];

pub fn load_font(dir: &str, face: Face) -> Result<FontVec, String> {
    let path = format!("{dir}/{}", FACES[face as usize]);
    let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
    FontVec::try_from_vec(bytes).map_err(|e| format!("{path}: {e}"))
}

pub fn load_fonts(dir: &str) -> Result<Vec<FontVec>, String> {
    [Face::Light, Face::Regular, Face::Medium].iter().map(|f| load_font(dir, *f)).collect()
}

/// CSS-like size: `px` is the em size in physical pixels.
pub fn px_scale(font: &FontVec, px: f32) -> PxScale {
    PxScale::from(px * font.height_unscaled() / font.units_per_em().unwrap_or(1000.0))
}

/// A glyph rasterised on any thread: coverage as premultiplied white RGBA plus its offset
/// from the pen position (None for blank glyphs such as space).
pub struct Raster {
    pub img: Option<Image>,
    pub left: i32,
    pub top: i32,
}

pub fn rasterise(font: &FontVec, px: f32, id: GlyphId) -> Raster {
    let scale = px_scale(font, px);
    let mut r = Raster { img: None, left: 0, top: 0 };
    if let Some(og) = font.outline_glyph(id.with_scale_and_position(scale, point(0.0, 0.0))) {
        let b = og.px_bounds();
        let (w, h) = ((b.max.x - b.min.x) as u32, (b.max.y - b.min.y) as u32);
        if w > 0 && h > 0 {
            let mut data = vec![0u8; (w * h * 4) as usize];
            og.draw(|x, y, c| {
                // Slight gamma lift: thin light weights on a dark TV background.
                let a = (c.clamp(0.0, 1.0).powf(0.8) * 255.0 + 0.5) as u8;
                let i = ((y * w + x) * 4) as usize;
                data[i..i + 4].copy_from_slice(&[a, a, a, a]);
            });
            r = Raster { img: Some(Image { w, h, data }), left: b.min.x as i32, top: b.min.y as i32 };
        }
    }
    r
}

/// Cache key of a glyph: (face, glyph id, size in quarter pixels).
pub type Key = (Face, u16, u32);
pub fn key(face: Face, px: f32, id: GlyphId) -> Key {
    (face, id.0, (px * 4.0).round() as u32)
}

/// Rasterise every distinct glyph of `runs` ((face, physical px, string)) off the render
/// thread, so the UI's first frame finds them all cached (see `Text::preload`).
pub fn rasterise_runs(fonts: &[FontVec], runs: &[(Face, f32, String)]) -> Vec<(Key, Raster)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (face, px, s) in runs {
        let font = &fonts[*face as usize];
        for c in s.chars() {
            let id = font.glyph_id(c);
            let k = key(*face, *px, id);
            if seen.insert(k) {
                out.push((k, rasterise(font, *px, id)));
            }
        }
    }
    out
}

impl Text {
    pub fn new(fonts: Vec<FontVec>) -> Text {
        Text { fonts, cache: HashMap::new(), rasterised: 0, missing: std::cell::Cell::new(0), full: 0, generation: 0 }
    }

    fn note(&self, face: Face, c: char, id: GlyphId) {
        if id.0 == 0 && !c.is_control() {
            self.missing.set(self.missing.get() | 1 << face as u8);
        }
    }

    /// Faces whose full version some text needs (and which are not full yet).
    pub fn missing_faces(&self) -> impl Iterator<Item = Face> + '_ {
        let m = self.missing.get() & !self.full;
        ALL_FACES.into_iter().filter(move |f| m & 1 << *f as u8 != 0)
    }

    /// Swap in the full version of `face` (glyph ids are the same, so cached glyphs and
    /// layouts stay valid). Refused if it does not look like the same face.
    pub fn replace_font(&mut self, face: Face, font: FontVec) -> Result<(), String> {
        let old = &self.fonts[face as usize];
        let same = "Ag0".chars().all(|c| old.glyph_id(c) == font.glyph_id(c) && old.h_advance_unscaled(old.glyph_id(c)) == font.h_advance_unscaled(font.glyph_id(c)));
        if !same || old.units_per_em() != font.units_per_em() {
            return Err(format!("{}: not the same face as the built-in one", FACES[face as usize]));
        }
        self.fonts[face as usize] = font;
        self.full |= 1 << face as u8;
        self.generation += 1;
        // Glyph 0 (.notdef) entries stay valid; characters that drew it now map to real glyphs.
        Ok(())
    }

    fn scale(&self, face: Face, px: f32) -> PxScale {
        px_scale(&self.fonts[face as usize], px)
    }

    /// Insert glyphs rasterised elsewhere (`rasterise_runs`) into the atlas and cache.
    pub fn preload(&mut self, gpu: &Gpu, atlas: &mut Atlas, glyphs: Vec<(Key, Raster)>) {
        for (k, r) in glyphs {
            if self.cache.contains_key(&k) {
                continue;
            }
            let region = r.img.as_ref().and_then(|img| atlas.insert(gpu, img));
            if r.img.is_some() {
                self.rasterised += 1;
            }
            self.cache.insert(k, Glyph { region, left: r.left, top: r.top });
        }
    }

    pub fn width(&self, face: Face, px: f32, s: &str) -> f32 {
        let font = &self.fonts[face as usize];
        let sf = font.as_scaled(self.scale(face, px));
        let mut w = 0.0;
        let mut prev: Option<GlyphId> = None;
        for c in s.chars() {
            let id = font.glyph_id(c);
            self.note(face, c, id);
            if let Some(p) = prev {
                w += sf.kern(p, id);
            }
            w += sf.h_advance(id);
            prev = Some(id);
        }
        w
    }

    /// `s`, cut with an ellipsis to `width` pixels: the longest prefix that fits, found by
    /// bisection (a handful of measurements; draw code keeps the result in a `Fits`).
    pub fn fit(&self, face: Face, px: f32, s: &str, width: f32) -> String {
        if self.width(face, px, s) <= width {
            return s.to_string();
        }
        let ends: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
        let cut = |n: usize| format!("{}…", s[..ends[n]].trim_end());
        // Invariant: `lo` characters fit (0 always "fits": just the ellipsis), `hi` do not.
        let (mut lo, mut hi) = (0, ends.len());
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.width(face, px, &cut(mid)) <= width { lo = mid } else { hi = mid }
        }
        cut(lo)
    }

    /// `s` broken at spaces into at most `max` lines of `width` pixels. The last line ends
    /// in an ellipsis when text is left over; a word wider than a line is broken inside.
    /// Always at least one line (an empty one for an empty string).
    pub fn wrap(&self, face: Face, px: f32, s: &str, width: f32, max: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut rest = s.trim();
        while !rest.is_empty() {
            if lines.len() + 1 >= max || self.width(face, px, rest) <= width {
                lines.push(self.fit(face, px, rest, width));
                break;
            }
            // The most whole words that fit.
            let mut cut = 0;
            for (i, _) in rest.match_indices(' ') {
                if self.width(face, px, &rest[..i]) > width {
                    break;
                }
                cut = i;
            }
            if cut == 0 {
                // Not even the first word: as many of its characters as fit (at least one).
                let word = rest.find(' ').unwrap_or(rest.len());
                cut = rest.char_indices().map(|(i, c)| i + c.len_utf8()).take_while(|&e| e <= word && self.width(face, px, &rest[..e]) <= width).last().unwrap_or(rest.chars().next().map_or(0, char::len_utf8));
            }
            lines.push(rest[..cut].trim_end().to_string());
            rest = rest[cut..].trim_start();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    fn glyph(&mut self, gpu: &Gpu, atlas: &mut Atlas, face: Face, px: f32, id: GlyphId) -> Glyph {
        let k = key(face, px, id);
        if let Some(g) = self.cache.get(&k) {
            return *g;
        }
        let r = rasterise(&self.fonts[face as usize], px, id);
        let mut g = Glyph { region: None, left: r.left, top: r.top };
        if let Some(img) = &r.img {
            g.region = atlas.insert(gpu, img);
            self.rasterised += 1;
        }
        self.cache.insert(k, g);
        g
    }

    /// Emit quads for `s` with its baseline at (x, y) in physical pixels. `rgba` is
    /// premultiplied. Returns the advance width.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        atlas: &mut Atlas,
        out: &mut Vec<Vertex>,
        face: Face,
        px: f32,
        x: f32,
        y: f32,
        rgba: [u8; 4],
        s: &str,
    ) -> f32 {
        let scale = self.scale(face, px);
        let (by, mut pen) = (y.round(), x);
        let mut prev: Option<GlyphId> = None;
        for c in s.chars() {
            let (id, adv, kern) = {
                let font = &self.fonts[face as usize];
                let sf = font.as_scaled(scale);
                let id = font.glyph_id(c);
                (id, sf.h_advance(id), prev.map_or(0.0, |p| sf.kern(p, id)))
            };
            self.note(face, c, id);
            pen += kern;
            let g = self.glyph(gpu, atlas, face, px, id);
            if let Some(r) = g.region {
                let x0 = pen.round() + g.left as f32;
                let y0 = by + g.top as f32;
                let uv = atlas.uv(&r, 0.0, 0.0, r.w as f32, r.h as f32);
                crate::home::quad(out, x0, y0, x0 + r.w as f32, y0 + r.h as f32, uv, rgba);
            }
            pen += adv;
            prev = Some(id);
        }
        pen - x
    }
}

/// Text fitted to its room, kept between frames: `Text::fit` and `Text::wrap` measure a
/// string many times, so every view asks here instead and pays only a hash of the string
/// per frame (no allocation, no layout). The widths asked for must not animate. Emptied
/// when a font is replaced (the widths of its new characters change) and when it grows
/// large (live values such as a countdown leave their old texts behind).
#[derive(Default)]
pub struct Fits {
    map: HashMap<u64, (Box<str>, Vec<String>)>,
    generation: u32,
}

impl Fits {
    const MAX: usize = 512;

    /// `s` on one line of at most `width` pixels (cut with an ellipsis).
    pub fn line(&mut self, text: &Text, face: Face, px: f32, s: &str, width: f32) -> &str {
        &self.lines(text, face, px, s, width, 1)[0]
    }

    /// `s` on at most `max` lines of `width` pixels (`Text::wrap`); never empty.
    pub fn lines(&mut self, text: &Text, face: Face, px: f32, s: &str, width: f32, max: usize) -> &[String] {
        use std::hash::{Hash, Hasher};
        if self.generation != text.generation || self.map.len() >= Self::MAX {
            self.map.clear();
            self.generation = text.generation;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (s, face, (px * 4.0).round() as u32, width.max(0.0) as u32, max).hash(&mut h);
        let e = self.map.entry(h.finish()).or_insert_with(|| (s.into(), text.wrap(face, px, s, width, max)));
        if &*e.0 != s {
            // Two texts with one hash: the newer one wins.
            *e = (s.into(), text.wrap(face, px, s, width, max));
        }
        &e.1
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Text {
        Text::new(load_fonts(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/latin")).expect("the firmware's fonts"))
    }

    #[test]
    fn fit_cuts_with_an_ellipsis_only_when_needed() {
        let t = text();
        let s = "Cast from the YouTube app on your phone";
        let full = t.width(Face::Regular, 21.0, s);
        assert_eq!(t.fit(Face::Regular, 21.0, s, full), s);
        for room in [20.0, 100.0, 250.0, full - 1.0] {
            let cut = t.fit(Face::Regular, 21.0, s, room);
            assert!(cut.ends_with('…') && cut.chars().count() < s.chars().count(), "{cut}");
            assert!(t.width(Face::Regular, 21.0, &cut) <= room, "{cut} in {room}");
            assert!(!cut.ends_with(" …"), "no space before the ellipsis: {cut}");
        }
        assert_eq!(t.fit(Face::Regular, 21.0, s, 0.0), "…");
        assert_eq!(t.fit(Face::Regular, 21.0, "", 0.0), "");
    }

    #[test]
    fn wrap_breaks_at_words_and_ends_in_an_ellipsis() {
        let t = text();
        let w = |s: &str, room: f32, max: usize| t.wrap(Face::Regular, 21.0, s, room, max);
        assert_eq!(w("", 350.0, 2), [""]);
        assert_eq!(w("Running", 350.0, 2), ["Running"]);
        let two = w("Cast from the YouTube app on your phone", 350.0, 2);
        assert_eq!(two.len(), 2, "{two:?}");
        assert_eq!(two.join(" "), "Cast from the YouTube app on your phone");
        let long = "Live TV from your sweet.tv subscription, with a guide, catch-up and more than two hundred channels in every room of the house";
        for max in 1..4 {
            let l = w(long, 350.0, max);
            assert_eq!(l.len(), max, "{l:?}");
            assert!(l.iter().all(|x| t.width(Face::Regular, 21.0, x) <= 350.0), "{l:?}");
            assert!(l[max - 1].ends_with('…'), "{l:?}");
            assert!(l[..max - 1].iter().all(|x| !x.ends_with('…') && !x.starts_with(' ') && !x.ends_with(' ')), "{l:?}");
        }
        // Everything fits in fewer lines than allowed: no ellipsis, nothing lost.
        let all = w(long, 350.0, 9);
        assert!(all.len() < 9 && all.join(" ") == long, "{all:?}");
        // A word wider than the line is broken inside, and nothing is lost either.
        let url = w("https://meowcast-dev.example.workers.dev/a/very/long/path/that/goes/on", 200.0, 9);
        assert!(url.len() > 1 && url.iter().all(|x| t.width(Face::Regular, 21.0, x) <= 200.0), "{url:?}");
        assert_eq!(url.concat(), "https://meowcast-dev.example.workers.dev/a/very/long/path/that/goes/on");
        // No room at all still terminates, with one character per line.
        assert_eq!(w("abc def", 0.0, 3), ["a", "b", "…"]);
    }

    #[test]
    fn fits_remembers_and_follows_its_arguments() {
        let t = text();
        let mut f = Fits::default();
        let s = "Live TV from your sweet.tv subscription";
        let one = f.line(&t, Face::Regular, 21.0, s, 200.0).to_string();
        assert!(one.ends_with('…'));
        assert_eq!(f.line(&t, Face::Regular, 21.0, s, 200.0), one);
        assert_eq!(f.line(&t, Face::Regular, 21.0, s, 2000.0), s, "another width is another entry");
        assert_eq!(f.lines(&t, Face::Regular, 21.0, s, 200.0, 2).len(), 2);
        assert_eq!(f.map.len(), 3);
        for i in 0..2 * Fits::MAX {
            f.line(&t, Face::Regular, 21.0, &format!("Going back in {i} s"), 300.0);
        }
        assert!(f.map.len() <= Fits::MAX, "bounded: {}", f.map.len());
    }
}
