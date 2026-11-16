//! The wallpaper gallery of Settings > Wallpaper: what the meow store offers, and the
//! thumbnails of all of it in one texture.
//!
//! Two signed assets (appd ASSET, like the wallpaper itself; server/prepare-wallpapers.py):
//!
//! - `wallpapers/catalog.tsv` (~1 KB): the thumbnail sheet's asset name and geometry, then per
//!   wallpaper its asset, title, author and mean colour;
//! - the thumbnail sheet (`wallpapers/thumbs.jpg`, ~130 KB): every wallpaper's 264x148
//!   thumbnail in one JPEG. One download and one decode (on a worker thread) fill the grid.
//!
//! They are asked for when Settings opens, so the grid is usually complete before the user
//! reaches it; until the sheet is there each tile is a rounded card in its wallpaper's mean
//! colour, and the thumbnails fade in over those. The sheet becomes ONE extra texture (not
//! part of the UI atlas: 1.9 MB of texels would not fit beside the glyphs), with every cell's
//! rounded corners in its alpha and a transparent 1-texel frame around it, so the tiles are
//! plain quads: one more draw call while the page is shown, none otherwise.

use crate::atlas::Image;
use std::time::{Duration, Instant};

pub const CATALOG: &str = "wallpapers/catalog.tsv";
/// The wallpaper of a new device, and the fallback when the chosen one left the store.
pub const DEFAULT: &str = "wallpapers/default.jpg";
pub const MAX_WALLS: usize = 24;
/// Corner radius of a tile, in sheet texels (= logical px at 1080p; the cards' radius).
pub const RADIUS: f32 = 8.0;
const FADE_MS: f32 = 260.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Wall {
    pub asset: String,
    pub title: String,
    pub author: String,
    pub color: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    pub thumbs: String,
    /// Thumbnail size in the sheet, and thumbnails per sheet row.
    pub cell: (u32, u32),
    pub cols: u32,
    pub walls: Vec<Wall>,
}

/// An asset name the launcher will ask appd for: a plain relative path.
fn valid_name(v: &str, ext: &str) -> bool {
    v.len() <= 80
        && v.starts_with("wallpapers/")
        && v.ends_with(ext)
        && !v.contains("..")
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
}

/// A wallpaper asset name (also checked when read from the settings file).
pub fn valid_asset(v: &str) -> bool {
    valid_name(v, ".jpg")
}

impl Catalog {
    pub fn parse(text: &str) -> Result<Catalog, String> {
        let mut c = Catalog { thumbs: String::new(), cell: (0, 0), cols: 0, walls: Vec::new() };
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f[0] {
                "thumbs" if f.len() >= 5 => {
                    let n = |i: usize| f[i].parse::<u32>().unwrap_or(0);
                    c.thumbs = f[1].to_string();
                    c.cell = (n(2), n(3));
                    c.cols = n(4);
                }
                "w" if f.len() >= 5 && c.walls.len() < MAX_WALLS => {
                    let text_ok = |s: &str| !s.is_empty() && s.chars().count() <= 40 && !s.chars().any(char::is_control);
                    if !valid_asset(f[1]) || !text_ok(f[2]) || !text_ok(f[3]) {
                        return Err(format!("catalog: bad wallpaper line {line:?}"));
                    }
                    let color = u32::from_str_radix(f[4], 16).unwrap_or(0x30343c) & 0xffffff;
                    c.walls.push(Wall { asset: f[1].into(), title: f[2].into(), author: f[3].into(), color });
                }
                _ => {} // comments, and lines of newer catalogs
            }
        }
        let cell_ok = (16..=512).contains(&c.cell.0) && (16..=512).contains(&c.cell.1) && (1..=8).contains(&c.cols);
        if !valid_name(&c.thumbs, ".jpg") || !cell_ok || c.walls.is_empty() {
            return Err("catalog: no thumbnail sheet or no wallpapers".into());
        }
        Ok(c)
    }

    pub fn rows(&self) -> u32 {
        (self.walls.len() as u32).div_ceil(self.cols)
    }

    /// Size of the sheet texture: every cell with its transparent 1-texel frame.
    fn tex_size(&self) -> (u32, u32) {
        (self.cols * (self.cell.0 + 2), self.rows() * (self.cell.1 + 2))
    }

    /// Texture coordinates of wallpaper `i`'s thumbnail in the sheet texture.
    pub fn uv(&self, i: usize) -> [f32; 4] {
        let (tw, th) = self.tex_size();
        let (cw, ch) = self.cell;
        let x = (i as u32 % self.cols * (cw + 2) + 1) as f32;
        let y = (i as u32 / self.cols * (ch + 2) + 1) as f32;
        [x / tw as f32, y / th as f32, (x + cw as f32) / tw as f32, (y + ch as f32) / th as f32]
    }

    /// The sheet texture's pixels from the sheet JPEG (worker thread): premultiplied RGBA,
    /// rounded corners (anti-aliased) in the alpha.
    pub fn sheet(&self, jpeg: &[u8]) -> Result<Image, String> {
        let (px, w, h) = crate::wall::decode(jpeg, 4096)?;
        let (cw, ch) = (self.cell.0 as usize, self.cell.1 as usize);
        let cols = self.cols as usize;
        if w < cols.min(self.walls.len()) * cw || h < self.rows() as usize * ch {
            return Err(format!("thumbnail sheet {w}x{h} does not match the catalog"));
        }
        let (tw, th) = self.tex_size();
        let (tw, th) = (tw as usize, th as usize);
        let mut data = vec![0u8; tw * th * 4];
        // Coverage of a texel by the rounded rectangle, from its distance to the corner arc.
        let edge = |v: usize, n: usize| (RADIUS - (v as f32 + 0.5)).max(v as f32 + 0.5 - (n as f32 - RADIUS)).max(0.0);
        for i in 0..self.walls.len() {
            let (sx, sy) = (i % cols * cw, i / cols * ch);
            let (dx, dy) = (i % cols * (cw + 2) + 1, i / cols * (ch + 2) + 1);
            for y in 0..ch {
                let ey = edge(y, ch);
                let src = &px[((sy + y) * w + sx) * 3..][..cw * 3];
                let dst = &mut data[((dy + y) * tw + dx) * 4..][..cw * 4];
                for x in 0..cw {
                    let ex = edge(x, cw);
                    let a = if ex > 0.0 && ey > 0.0 { (RADIUS + 0.5 - (ex * ex + ey * ey).sqrt()).clamp(0.0, 1.0) } else { 1.0 };
                    for c in 0..3 {
                        dst[x * 4 + c] = (src[x * 3 + c] as f32 * a + 0.5) as u8;
                    }
                    dst[x * 4 + 3] = (a * 255.0 + 0.5) as u8;
                }
            }
        }
        Ok(Image { w: tw as u32, h: th as u32, data })
    }
}

/// What Settings > Wallpaper shows (owned by main.rs, which talks to appd and the GPU).
pub struct Gallery {
    pub catalog: Option<Catalog>,
    /// The thumbnail sheet's texture, and when it became complete (the tiles fade in).
    pub sheet: Option<(glow::Texture, Instant)>,
    /// The catalog was asked from appd on this connection.
    pub asked: bool,
    /// Why there is no catalog (appd's words; logged, the page shows a plain sentence).
    pub error: Option<String>,
}

impl Gallery {
    pub fn new() -> Gallery {
        Gallery { catalog: None, sheet: None, asked: false, error: None }
    }

    /// Opacity of the thumbnails (0 until the sheet is uploaded, then fading in).
    pub fn thumbs_alpha(&self, now: Instant) -> f32 {
        self.sheet.map_or(0.0, |(_, t)| (now.saturating_duration_since(t).as_secs_f32() * 1000.0 / FADE_MS).min(1.0))
    }

    pub fn fading(&self, now: Instant) -> bool {
        self.sheet.is_some_and(|(_, t)| now.saturating_duration_since(t) < Duration::from_millis(FADE_MS as u64 + 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# comment\nthumbs\twallpapers/thumbs.jpg\t16\t16\t2\nw\twallpapers/a.jpg\tA\tAnn\t102030\nw\twallpapers/b.jpg\tB\tBob\tff0000\nw\twallpapers/c.jpg\tC\tCy\t00ff00\nfuture\tline\n";

    #[test]
    fn parses_and_places_cells() {
        let c = Catalog::parse(TEXT).unwrap();
        assert_eq!((c.cell, c.cols, c.walls.len(), c.rows()), ((16, 16), 2, 3, 2));
        assert_eq!(c.walls[1], Wall { asset: "wallpapers/b.jpg".into(), title: "B".into(), author: "Bob".into(), color: 0xff0000 });
        // Sheet texture 36x36: cell 2 (second row, first column) starts at texel (1, 19).
        assert_eq!(c.uv(2), [1.0 / 36.0, 19.0 / 36.0, 17.0 / 36.0, 35.0 / 36.0]);
    }

    /// The store's own catalog and sheet (server/www, built by prepare-wallpapers.py).
    #[test]
    fn store_gallery_loads() {
        let c = Catalog::parse(include_str!("../../cloud/store/www/assets/wallpapers/catalog.tsv")).unwrap();
        assert!(c.walls.iter().any(|w| w.asset == DEFAULT));
        let img = c.sheet(include_bytes!("../../cloud/store/www/assets/wallpapers/thumbs.jpg")).unwrap();
        assert_eq!((img.w, img.h), (c.cols * (c.cell.0 + 2), c.rows() * (c.cell.1 + 2)));
        let alpha = |x: u32, y: u32| img.data[((y * img.w + x) * 4 + 3) as usize];
        // The frame is transparent, a cell's corner texel nearly so, its inside opaque.
        assert_eq!((alpha(0, 0), alpha(1 + c.cell.0 / 2, 1 + c.cell.1 / 2)), (0, 255));
        assert!(alpha(1, 1) < 40 && alpha(1 + RADIUS as u32, 1) == 255);
    }

    #[test]
    fn rejects_bad_catalogs() {
        assert!(Catalog::parse("w\twallpapers/a.jpg\tA\tAnn\t102030\n").is_err()); // no sheet
        assert!(Catalog::parse("thumbs\twallpapers/thumbs.jpg\t264\t148\t4\n").is_err()); // no wallpapers
        assert!(Catalog::parse(&TEXT.replace("wallpapers/a.jpg", "wallpapers/../a.jpg")).is_err());
        assert!(Catalog::parse(&TEXT.replace("wallpapers/a.jpg", "fonts/a.jpg")).is_err());
        assert!(Catalog::parse(&TEXT.replace("\t16\t16\t2", "\t16\t16\t0")).is_err());
        assert!(valid_asset(DEFAULT) && !valid_asset("wallpapers/a b.jpg") && !valid_asset("wallpapers/thumbs.png"));
    }
}
