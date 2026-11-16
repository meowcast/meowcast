//! The home wallpaper from an asset (appd ASSET, a JPEG from the meow asset server), turned
//! once on a worker thread into two textures, like Plasma Bigscreen's home background:
//!
//! - `sharp`: half resolution (upscaled 2x by the GPU), a light blur, and a
//!   vertical darkening from ~60 % at the top to ~36 % behind the card rows, dithered so the
//!   dark gradients don't band. Shown while the top row is focused.
//! - `soft`: a quarter-resolution copy, strongly blurred (~16 px at 1080p) and darker. Shown
//!   in the rows further down; the launcher cross-fades between the two as it scrolls.
//!
//! Per frame the GPU only copies texels (one pass at rest, two while cross-fading); the soft
//! texture is 480x270, so the blurred state is also the cheaper one to draw.
//!
//! The server publishes the wallpaper at 960x540 (the sharp layer's size at 1080p and 4K
//! alike); larger sources are box-filtered down by an integer factor first.
//!
//! `preview`: the asset index carries a ~1 KB 32x18 JPEG of the wallpaper. Smoothly upscaled
//! and blurred, it is an out-of-focus version of the real picture: shown at once (also
//! offline) and cross-faded into the real one when that arrives, like a focus pull.

use crate::atlas::Image;

pub struct WallSet {
    pub sharp: Image,
    pub soft: Image,
}

/// Brightness at relative height `fy` (0 = top): `top` until 35 %, easing to `bottom`.
fn shade(fy: f32, top: f32, bottom: f32) -> f32 {
    let t = ((fy - 0.35) / 0.65).clamp(0.0, 1.0);
    top + (bottom - top) * t * t * (3.0 - 2.0 * t)
}

const THREADS: usize = 4;

/// Run `f(first_row, rows_slice)` on THREADS bands of rows of `out` (row = `stride` bytes).
fn bands(out: &mut [u8], stride: usize, f: impl Fn(usize, &mut [u8]) + Sync) {
    let rows = out.len() / stride;
    let per = rows.div_ceil(THREADS).max(1);
    std::thread::scope(|sc| {
        for (k, band) in out.chunks_mut(per * stride).enumerate() {
            let f = &f;
            sc.spawn(move || f(k * per, band));
        }
    });
}

/// Separable box blur of an RGB image (radius `r`, edges clamped), src -> new buffer.
/// Both passes slide a running sum along memory order (rows), on 4 threads.
fn box_blur(src: &[u8], w: usize, h: usize, r: usize) -> Vec<u8> {
    let n = (2 * r + 1) as u32;
    // Horizontal: each row independently.
    let mut tmp = vec![0u8; src.len()];
    bands(&mut tmp, w * 3, |y0, band| {
        for (k, out) in band.chunks_mut(w * 3).enumerate() {
            let row = &src[(y0 + k) * w * 3..(y0 + k + 1) * w * 3];
            let get = |x: isize, c: usize| row[x.clamp(0, w as isize - 1) as usize * 3 + c] as u32;
            for c in 0..3 {
                let mut sum: u32 = (-(r as isize)..=r as isize).map(|x| get(x, c)).sum();
                for x in 0..w {
                    out[x * 3 + c] = ((sum + n / 2) / n) as u8;
                    sum += get(x as isize + r as isize + 1, c);
                    sum -= get(x as isize - r as isize, c);
                }
            }
        }
    });
    // Vertical: running column sums, advanced one whole row at a time.
    let mut dst = vec![0u8; src.len()];
    let tmp = &tmp;
    bands(&mut dst, w * 3, |y0, band| {
        let row = |y: isize| &tmp[y.clamp(0, h as isize - 1) as usize * w * 3..][..w * 3];
        let mut sum = vec![0u32; w * 3];
        for d in -(r as isize)..=r as isize {
            for (s, v) in sum.iter_mut().zip(row(y0 as isize + d)) {
                *s += *v as u32;
            }
        }
        for (k, out) in band.chunks_mut(w * 3).enumerate() {
            let y = (y0 + k) as isize;
            for (o, s) in out.iter_mut().zip(&sum) {
                *o = ((s + n / 2) / n) as u8;
            }
            let (add, sub) = (row(y + r as isize + 1), row(y - r as isize));
            for i in 0..w * 3 {
                sum[i] = sum[i] + add[i] as u32 - sub[i] as u32;
            }
        }
    });
    dst
}

/// Darken with the vertical curve, dither, and expand to opaque RGBA (4 threads).
fn finish(px: &[u8], w: usize, h: usize, top: f32, bottom: f32) -> Image {
    let mut data = vec![0u8; w * h * 4];
    bands(&mut data, w * 4, |y0, band| {
        for (k, out) in band.chunks_mut(w * 4).enumerate() {
            let y = y0 + k;
            let f = shade(y as f32 / h as f32, top, bottom);
            for x in 0..w {
                // Ordered dither (+-0.5 LSB) against banding in the darkened gradients.
                let d = ((x.wrapping_mul(7) ^ y.wrapping_mul(13)) & 15) as f32 / 16.0 - 0.47;
                let i = (y * w + x) * 3;
                for c in 0..3 {
                    out[x * 4 + c] = (px[i + c] as f32 * f + d).clamp(0.0, 255.0) as u8;
                }
                out[x * 4 + 3] = 255;
            }
        }
    });
    Image { w: w as u32, h: h as u32, data }
}

/// Width of the sharp layer (half of 1080p; the GPU scales it 2x, or 4x at 4K).
const SHARP_W: usize = 960;
/// The preview layers' size (smooth enough for the GPU's bilinear upscale after the blur).
const PREVIEW_W: usize = 320;
const PREVIEW_H: usize = 180;

/// Sharp layer brightness, top -> bottom; the soft layer is darker (it sits behind rows).
const SHARP_SHADE: (f32, f32) = (0.60, 0.36);
const SOFT_SHADE: (f32, f32) = (0.46, 0.28);

pub(crate) fn decode(jpeg: &[u8], max: usize) -> Result<(Vec<u8>, usize, usize), String> {
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg));
    dec.decode_headers().map_err(|e| format!("wallpaper: {e:?}"))?;
    let info = dec.info().ok_or("wallpaper: no image info")?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w < 2 || h < 2 || w > max || h > max {
        return Err(format!("wallpaper: {w}x{h} not supported"));
    }
    let px = dec.decode().map_err(|e| format!("wallpaper: {e:?}"))?;
    if px.len() != w * h * 3 {
        return Err("wallpaper: expected RGB output".into());
    }
    Ok((px, w, h))
}

/// Box average of f x f blocks (f = 1: a copy).
fn shrink(px: &[u8], w: usize, h: usize, f: usize) -> (Vec<u8>, usize, usize) {
    if f == 1 {
        return (px.to_vec(), w, h);
    }
    let (ow, oh) = (w / f, h / f);
    let mut out = vec![0u8; ow * oh * 3];
    let n = (f * f) as u32;
    for y in 0..oh {
        for x in 0..ow {
            for c in 0..3 {
                let mut s = 0u32;
                for dy in 0..f {
                    for dx in 0..f {
                        s += px[((y * f + dy) * w + x * f + dx) * 3 + c] as u32;
                    }
                }
                out[(y * ow + x) * 3 + c] = ((s + n / 2) / n) as u8;
            }
        }
    }
    (out, ow, oh)
}

pub fn process(jpeg: &[u8]) -> Result<WallSet, String> {
    let t = std::time::Instant::now();
    let (px, w, h) = decode(jpeg, 4096)?;
    let t_dec = t.elapsed().as_millis();
    // Sharp layer at half output resolution: the GPU's 2x bilinear upscale plus a radius-1
    // blur here give the slight softness of the reference, at a quarter of the texels per
    // frame (a full-resolution layer measured ~28 ms vertical-scroll frames: texel bandwidth).
    let (half, hw, hh) = shrink(&px, w, h, (w / SHARP_W).max(1));
    drop(px);
    // Soft layer: a quarter of the output resolution, strongly blurred.
    let (mut q, qw, qh) = shrink(&half, hw, hh, 2);
    for _ in 0..3 {
        q = box_blur(&q, qw, qh, 3);
    }
    let soft = finish(&q, qw, qh, SOFT_SHADE.0, SOFT_SHADE.1);
    let half = box_blur(&half, hw, hh, 1);
    let sharp = finish(&half, hw, hh, SHARP_SHADE.0, SHARP_SHADE.1);
    eprintln!("UI_WALL decode_ms={t_dec} total_ms={} {w}x{h}", t.elapsed().as_millis());
    Ok(WallSet { sharp, soft })
}

/// The placeholder from a tiny preview JPEG: bilinear upscale to PREVIEW_W x PREVIEW_H, a
/// Gaussian-like blur (three box passes) so no texel blocks show, then the usual shading.
pub fn preview(jpeg: &[u8]) -> Result<WallSet, String> {
    let t = std::time::Instant::now();
    let (px, w, h) = decode(jpeg, 128)?;
    let (ow, oh) = (PREVIEW_W, PREVIEW_H);
    let mut up = vec![0u8; ow * oh * 3];
    for y in 0..oh {
        // Pixel centres map to pixel centres (edges clamped).
        let fy = ((y as f32 + 0.5) * h as f32 / oh as f32 - 0.5).clamp(0.0, (h - 1) as f32);
        let (y0, ty) = (fy as usize, fy.fract());
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..ow {
            let fx = ((x as f32 + 0.5) * w as f32 / ow as f32 - 0.5).clamp(0.0, (w - 1) as f32);
            let (x0, tx) = (fx as usize, fx.fract());
            let x1 = (x0 + 1).min(w - 1);
            for c in 0..3 {
                let p = |xx: usize, yy: usize| px[(yy * w + xx) * 3 + c] as f32;
                let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
                let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
                up[(y * ow + x) * 3 + c] = (top + (bot - top) * ty + 0.5) as u8;
            }
        }
    }
    // Blur radius ~ half a preview pixel: enough to hide the bilinear grid, while the
    // highlights stay where they are in the real picture (a defocused look, not a smear).
    let r = (ow / w / 2).max(2);
    for _ in 0..3 {
        up = box_blur(&up, ow, oh, r);
    }
    let sharp = finish(&up, ow, oh, SHARP_SHADE.0, SHARP_SHADE.1);
    let soft = finish(&up, ow, oh, SOFT_SHADE.0, SOFT_SHADE.1);
    eprintln!("UI_WALL preview {w}x{h} -> {ow}x{oh} in {} ms", t.elapsed().as_millis());
    Ok(WallSet { sharp, soft })
}
