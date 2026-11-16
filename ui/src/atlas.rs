//! One RGBA texture atlas holding all pre-rendered UI art and glyphs, filled lazily.
//! Entries are uploaded once with glTexSubImage2D and no CPU copy is kept. Each entry gets
//! a transparent 1-texel border so linear filtering never bleeds in neighbours.

use crate::gpu::Gpu;

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One step of the skyline: the used area's top edge is at `y` over [x, x + w).
#[derive(Clone, Copy)]
struct Seg {
    x: u32,
    y: u32,
    w: u32,
}

pub struct Atlas {
    pub tex: glow::Texture,
    pub size: u32,
    skyline: Vec<Seg>,
    pub used_px: u64,
    pub entries: u32,
    scratch: Vec<u8>,
}

/// An image to insert: premultiplied RGBA8, row-major.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

impl Atlas {
    pub fn new(gpu: &Gpu, size: u32) -> Atlas {
        // Left uninitialised: only uploaded entries (with their transparent borders) are ever
        // sampled, and this avoids a 16 MiB CPU buffer at 4K.
        let tex = gpu.texture(size, size, None);
        Atlas { tex, size, skyline: vec![Seg { x: 0, y: 0, w: size }], used_px: 0, entries: 0, scratch: Vec::new() }
    }

    /// Skyline (bottom-left) packing: each item goes where its top edge ends lowest, on top
    /// of the skyline of what is placed already. Mixed sizes (glyphs of many sizes, badges,
    /// icons) pack far tighter than on shelves, which ran the atlas out of rows at ~45 %
    /// texel use.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w > self.size || h > self.size {
            return None;
        }
        // Best start segment: lowest resulting top, then the leftmost.
        let mut best: Option<(u32, u32, usize)> = None; // (top, x, index)
        for i in 0..self.skyline.len() {
            let x = self.skyline[i].x;
            if x + w > self.size {
                break;
            }
            let (mut y, mut covered, mut j) = (0, 0, i);
            while covered < w {
                y = y.max(self.skyline[j].y);
                covered += self.skyline[j].w;
                j += 1;
            }
            if y + h <= self.size && best.is_none_or(|b| (y + h, x) < (b.0, b.1)) {
                best = Some((y + h, x, i));
            }
        }
        let (top, x, i) = best?;
        // Raise the skyline over [x, x + w) to `top`.
        let mut rest = w;
        while rest > 0 {
            let seg = &mut self.skyline[i];
            if seg.w <= rest {
                rest -= seg.w;
                self.skyline.remove(i);
            } else {
                seg.x += rest;
                seg.w -= rest;
                rest = 0;
            }
        }
        self.skyline.insert(i, Seg { x, y: top, w });
        // Merge neighbours at the same height.
        let mut k = 0;
        while k + 1 < self.skyline.len() {
            if self.skyline[k].y == self.skyline[k + 1].y {
                self.skyline[k].w += self.skyline[k + 1].w;
                self.skyline.remove(k + 1);
            } else {
                k += 1;
            }
        }
        Some((x, top - h))
    }

    /// Height of the highest skyline point, as a share of the atlas (how full it really is).
    pub fn rows_used_pct(&self) -> f64 {
        self.skyline.iter().map(|s| s.y).max().unwrap_or(0) as f64 * 100.0 / self.size as f64
    }

    pub fn insert(&mut self, gpu: &Gpu, img: &Image) -> Option<Region> {
        let (pw, ph) = (img.w + 2, img.h + 2);
        let Some((x, y)) = self.alloc(pw, ph) else {
            eprintln!("UI_WARN atlas full ({}x{} px, {} entries)", self.size, self.size, self.entries);
            return None;
        };
        self.scratch.clear();
        self.scratch.resize((pw * ph * 4) as usize, 0);
        for row in 0..img.h {
            let src = (row * img.w * 4) as usize;
            let dst = (((row + 1) * pw + 1) * 4) as usize;
            self.scratch[dst..dst + (img.w * 4) as usize]
                .copy_from_slice(&img.data[src..src + (img.w * 4) as usize]);
        }
        gpu.upload(self.tex, x, y, pw, ph, &self.scratch);
        self.used_px += (pw * ph) as u64;
        self.entries += 1;
        Some(Region { x: x + 1, y: y + 1, w: img.w, h: img.h })
    }

    /// Texture coordinates of a sub-rectangle (in texels, relative to the region).
    pub fn uv(&self, r: &Region, sx: f32, sy: f32, sw: f32, sh: f32) -> [f32; 4] {
        let s = self.size as f32;
        [
            (r.x as f32 + sx) / s,
            (r.y as f32 + sy) / s,
            (r.x as f32 + sx + sw) / s,
            (r.y as f32 + sy + sh) / s,
        ]
    }
}
