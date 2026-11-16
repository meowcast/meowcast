//! Bytes of a picture file in, straight 8-bit RGBA out. Everything here works on data from the
//! internet that nobody checked: the limits below are enforced before anything is allocated.

use std::io::Cursor;

pub use meow_app::image::{MAX_INPUT, MAX_PIXELS, MAX_SIDE};

/// A decoded picture: 8-bit RGBA, not premultiplied, rows top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Format {
    Webp,
    Jpeg,
}

pub fn sniff(data: &[u8]) -> Option<Format> {
    match data {
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(Format::Webp),
        [0xff, 0xd8, 0xff, ..] => Some(Format::Jpeg),
        _ => None,
    }
}

fn check_size(width: usize, height: usize) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("the picture is empty".to_string());
    }
    if width > MAX_SIDE || height > MAX_SIDE || width * height > MAX_PIXELS {
        return Err(format!("{width}x{height} is too large (at most {MAX_PIXELS} pixels, {MAX_SIDE} a side)"));
    }
    Ok(())
}

/// The chunk list of a WebP file, checked before the decoder sees it: the sizes add up, the
/// picture data is there, and an animation has its first frame. (The decoder library reads
/// the same list; a file that passes here cannot send it looking for a chunk that is absent.)
fn check_webp(data: &[u8]) -> Result<(), String> {
    let word = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let riff = word(4).ok_or("webp: the file ends early")?;
    if riff < 4 || riff + 8 > data.len() {
        return Err("webp: the file ends early".to_string());
    }
    let end = riff + 8;
    let (mut at, mut first, mut animated, mut frames, mut picture) = (12, true, false, 0, false);
    while at + 8 <= end {
        let (name, size) = (&data[at..at + 4], word(at + 4).unwrap_or(usize::MAX));
        let body = at + 8;
        if size > end - body {
            return Err("webp: a chunk runs past the end of the file".to_string());
        }
        match name {
            b"VP8X" if first && size >= 10 => animated = data[body] & 0x02 != 0,
            b"VP8X" => return Err("webp: a misplaced header".to_string()),
            b"VP8 " | b"VP8L" => picture = true,
            b"ANMF" => frames += 1,
            _ => {}
        }
        first = false;
        at = body + size + (size & 1);
    }
    match (animated, frames, picture) {
        (true, 0, _) => Err("webp: an animation without frames".to_string()),
        (false, _, false) => Err("webp: no picture data".to_string()),
        _ => Ok(()),
    }
}

fn webp(data: &[u8]) -> Result<Picture, String> {
    check_webp(data)?;
    let mut decoder = image_webp::WebPDecoder::new(Cursor::new(data)).map_err(|e| format!("webp: {e}"))?;
    let (width, height) = decoder.dimensions();
    let (width, height) = (width as usize, height as usize);
    check_size(width, height)?;
    decoder.set_memory_limit(MAX_PIXELS * 8);
    let alpha = decoder.has_alpha();
    let size = decoder.output_buffer_size().filter(|size| *size == width * height * if alpha { 4 } else { 3 }).ok_or("webp: bad size")?;
    let mut pixels = vec![0u8; size];
    // (An animation gives its first frame.)
    decoder.read_image(&mut pixels).map_err(|e| format!("webp: {e}"))?;
    let rgba = if alpha { pixels } else { pixels.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
    Ok(Picture { width, height, rgba })
}

fn jpeg(data: &[u8]) -> Result<Picture, String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    // No hand-written SIMD: logos are small, and the plain code is the checked code.
    let options = DecoderOptions::default().set_use_unsafe(false).set_strict_mode(false).set_max_width(MAX_SIDE).set_max_height(MAX_SIDE).jpeg_set_max_scans(64).jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(data), options);
    decoder.decode_headers().map_err(|e| format!("jpeg: {e:?}"))?;
    let info = decoder.info().ok_or("jpeg: no header")?;
    let (width, height) = (info.width as usize, info.height as usize);
    check_size(width, height)?;
    let rgba = decoder.decode().map_err(|e| format!("jpeg: {e:?}"))?;
    if rgba.len() != width * height * 4 {
        return Err("jpeg: bad size".to_string());
    }
    Ok(Picture { width, height, rgba })
}

impl Picture {
    /// Made smaller to fit in `fit` (0 = no limit on that side), keeping its shape, by
    /// averaging the source pixels under each new one (weighted by their opacity, so
    /// transparent pixels do not bleed their colour). Never made larger.
    pub fn fitted(self, fit: (usize, usize)) -> Picture {
        let limit = |side: usize| if side == 0 { usize::MAX } else { side };
        let (max_width, max_height) = (limit(fit.0), limit(fit.1));
        if self.width <= max_width && self.height <= max_height {
            return self;
        }
        // The side that needs the stronger reduction decides.
        let (width, height) = if self.width as u64 * max_height.min(MAX_SIDE) as u64 >= self.height as u64 * max_width.min(MAX_SIDE) as u64 {
            (max_width, (self.height * max_width).div_ceil(self.width).clamp(1, max_height))
        } else {
            ((self.width * max_height).div_ceil(self.height).clamp(1, max_width), max_height)
        };
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            let (y0, y1) = (y * self.height / height, ((y + 1) * self.height).div_ceil(height).min(self.height));
            for x in 0..width {
                let (x0, x1) = (x * self.width / width, ((x + 1) * self.width).div_ceil(width).min(self.width));
                let (mut sum, mut count) = ([0u32; 4], 0u32);
                for row in y0..y1 {
                    for pixel in self.rgba[(row * self.width + x0) * 4..(row * self.width + x1) * 4].chunks_exact(4) {
                        let alpha = pixel[3] as u32;
                        for channel in 0..3 {
                            sum[channel] += pixel[channel] as u32 * alpha;
                        }
                        sum[3] += alpha;
                        count += 1;
                    }
                }
                let alpha = sum[3];
                match alpha {
                    0 => rgba.extend_from_slice(&[0, 0, 0, 0]),
                    _ => rgba.extend_from_slice(&[((sum[0] + alpha / 2) / alpha) as u8, ((sum[1] + alpha / 2) / alpha) as u8, ((sum[2] + alpha / 2) / alpha) as u8, ((alpha + count / 2) / count.max(1)) as u8]),
                }
            }
        }
        Picture { width, height, rgba }
    }
}

/// Decode a WebP or JPEG file and make it fit in `fit` (see `Picture::fitted`).
pub fn decode(data: &[u8], fit: (usize, usize)) -> Result<Picture, String> {
    if data.len() > MAX_INPUT {
        return Err(format!("{} bytes is too large (at most {MAX_INPUT})", data.len()));
    }
    let picture = match sniff(data) {
        Some(Format::Webp) => webp(data)?,
        Some(Format::Jpeg) => jpeg(data)?,
        None => return Err("not a WebP or JPEG file".to_string()),
    };
    Ok(picture.fitted(fit))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made by testdata/make.sh with libwebp's cwebp and img2webp from one drawing with soft,
    // partly transparent edges (`*.ref.png`: what libwebp's own dwebp decodes them to).
    const LOSSY: &[u8] = include_bytes!("../testdata/lossy.webp");
    const LOSSY_ALPHA: &[u8] = include_bytes!("../testdata/lossy-alpha.webp");
    const LOSSY_ALPHA_QUANTIZED: &[u8] = include_bytes!("../testdata/lossy-alpha-q40.webp");
    const LOSSLESS: &[u8] = include_bytes!("../testdata/lossless.webp");
    const LOSSLESS_ALPHA: &[u8] = include_bytes!("../testdata/lossless-alpha.webp");
    const ANIMATION: &[u8] = include_bytes!("../testdata/animation.webp");
    // A made-up badge of the size and kinds of file a station logo comes in (testdata/make.sh):
    // a 300-pixel lossy WebP, one with an alpha channel, and a 96-pixel JPEG.
    const LOGO_WEBP: &[u8] = include_bytes!("../testdata/logo.webp");
    const LOGO_WEBP_ALPHA: &[u8] = include_bytes!("../testdata/logo-alpha.webp");
    const LOGO_JPEG: &[u8] = include_bytes!("../testdata/logo-96.jpg");
    const ALL: &[(&str, &[u8])] = &[
        ("lossy", LOSSY),
        ("lossy-alpha", LOSSY_ALPHA),
        ("lossy-alpha-q40", LOSSY_ALPHA_QUANTIZED),
        ("lossless", LOSSLESS),
        ("lossless-alpha", LOSSLESS_ALPHA),
        ("animation", ANIMATION),
        ("logo", LOGO_WEBP),
        ("logo-alpha", LOGO_WEBP_ALPHA),
        ("logo-96.jpg", LOGO_JPEG),
    ];

    /// What the reference decoder made of a fixture (`NAME.ref.png`), as RGBA.
    fn reference(name: &str) -> Vec<u8> {
        let png = std::fs::read(format!("{}/testdata/{name}.ref.png", env!("CARGO_MANIFEST_DIR"))).unwrap();
        meow_media::png::decode(&png).unwrap().rgba
    }

    /// The largest difference of any channel, and the mean one.
    fn difference(a: &[u8], b: &[u8]) -> (u8, f64) {
        assert_eq!(a.len(), b.len());
        let worst = a.iter().zip(b).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        (worst, a.iter().zip(b).map(|(a, b)| a.abs_diff(*b) as f64).sum::<f64>() / a.len().max(1) as f64)
    }

    #[test]
    fn lossless_pictures_are_exact() {
        for name in ["lossless", "lossless-alpha"] {
            let data = ALL.iter().find(|(n, _)| *n == name).unwrap().1;
            let picture = decode(data, (0, 0)).unwrap();
            assert_eq!((picture.width, picture.height), (64, 64), "{name}");
            assert!(picture.rgba == reference(name), "{name}");
        }
        // Without an alpha channel in the file every pixel is opaque.
        assert!(decode(LOSSLESS, (0, 0)).unwrap().rgba.chunks_exact(4).all(|p| p[3] == 255));
        assert!(decode(LOSSLESS_ALPHA, (0, 0)).unwrap().rgba.chunks_exact(4).any(|p| p[3] == 0));
    }

    #[test]
    fn lossy_pictures_match_libwebp() {
        // Colour: the reference decoder's output to within rounding of the chroma upsampling
        // (measured: at most 3 of 255 on these files). Alpha is stored losslessly: exact.
        for name in ["lossy", "lossy-alpha", "lossy-alpha-q40", "logo", "logo-alpha"] {
            let data = ALL.iter().find(|(n, _)| *n == name).unwrap().1;
            let picture = decode(data, (0, 0)).unwrap();
            let expected = reference(name);
            assert_eq!(picture.rgba.len(), expected.len(), "{name}");
            let alpha = |pixels: &[u8]| pixels.chunks_exact(4).map(|p| p[3]).collect::<Vec<u8>>();
            assert!(alpha(&picture.rgba) == alpha(&expected), "{name}: alpha differs");
            // libwebp leaves the colour of fully transparent pixels as the file has it; so
            // does this decoder, but only what can be seen is compared.
            let visible = |pixels: &[u8]| pixels.chunks_exact(4).flat_map(|p| if p[3] == 0 { [0, 0, 0] } else { [p[0], p[1], p[2]] }).collect::<Vec<u8>>();
            let (worst, mean) = difference(&visible(&picture.rgba), &visible(&expected));
            assert!(worst <= 4 && mean < 0.5, "{name}: differs from libwebp by up to {worst}, {mean:.3} on average");
        }
        assert!(decode(LOSSY, (0, 0)).unwrap().rgba.chunks_exact(4).all(|p| p[3] == 255));
        let logo = decode(LOGO_WEBP_ALPHA, (0, 0)).unwrap();
        assert_eq!((logo.width, logo.height), (300, 300));
        // (Its alpha channel is all but opaque: some pixels at 253 or 254, none below.)
        assert!(logo.rgba.chunks_exact(4).any(|p| p[3] < 255) && logo.rgba.chunks_exact(4).all(|p| p[3] >= 253));
        assert!(decode(LOSSY_ALPHA, (0, 0)).unwrap().rgba.chunks_exact(4).any(|p| p[3] == 0));
    }

    #[test]
    fn an_animation_gives_its_first_frame() {
        let picture = decode(ANIMATION, (0, 0)).unwrap();
        assert_eq!((picture.width, picture.height), (64, 64));
        // The first frame is the drawing (lossless here), the second a plain colour.
        assert!(picture.rgba == reference("lossless-alpha"));
    }

    #[test]
    fn a_jpeg_decodes() {
        let picture = decode(LOGO_JPEG, (0, 0)).unwrap();
        assert_eq!((picture.width, picture.height, picture.rgba.len()), (96, 96, 96 * 96 * 4));
        assert!(picture.rgba.chunks_exact(4).all(|p| p[3] == 255));
        // JPEG decoders differ in how they smooth the colour planes (here by up to 34 of 255
        // at an edge, 0.5 on average); sips (ImageIO) made the reference.
        let (worst, mean) = difference(&picture.rgba, &reference("logo-96.jpg"));
        assert!(worst <= 40 && mean < 1.0, "differs from the reference by up to {worst}, {mean:.3} on average");
    }

    #[test]
    fn pictures_are_made_to_fit() {
        let whole = decode(LOGO_WEBP, (0, 0)).unwrap();
        assert_eq!((whole.width, whole.height), (300, 300));
        let small = decode(LOGO_WEBP, (96, 96)).unwrap();
        assert_eq!((small.width, small.height, small.rgba.len()), (96, 96, 96 * 96 * 4));
        // Never larger, and one limit is enough.
        assert_eq!(decode(LOGO_JPEG, (300, 300)).unwrap().width, 96);
        let narrow = decode(LOGO_WEBP, (150, 0)).unwrap();
        assert_eq!((narrow.width, narrow.height), (150, 150));
        // The shape is kept.
        let wide = Picture { width: 200, height: 100, rgba: vec![255; 200 * 100 * 4] };
        let fitted = wide.clone().fitted((50, 50));
        assert_eq!((fitted.width, fitted.height), (50, 25));
        assert!(fitted.rgba.iter().all(|b| *b == 255));
        assert_eq!(wide.clone().fitted((0, 10)).width, 20);
        let tall = Picture { width: 3, height: 4000, rgba: vec![9; 3 * 4000 * 4] }.fitted((96, 96));
        assert_eq!((tall.width, tall.height), (1, 96));
        // Transparent pixels do not bleed their colour into their neighbours.
        let mut half = Picture { width: 2, height: 1, rgba: vec![255, 0, 0, 255, 0, 255, 0, 0] };
        half = half.fitted((1, 1));
        assert_eq!(half.rgba, [255, 0, 0, 128]);
    }

    #[test]
    fn other_files_are_refused() {
        for data in [&b""[..], b"RIFF", b"<html>", b"\x89PNG\r\n\x1a\n", b"GIF89a", &[0xff, 0xd8]] {
            assert!(decode(data, (0, 0)).is_err());
        }
        assert!(decode(&vec![0; MAX_INPUT + 1], (0, 0)).unwrap_err().contains("too large"));
    }

    #[test]
    fn huge_pictures_are_refused_before_any_work() {
        // A lossless header that claims 16383x16383, and a lossy one with the same.
        let mut lossless = b"RIFF\x1a\0\0\0WEBPVP8L\x0e\0\0\0\x2f".to_vec();
        lossless.extend_from_slice(&(0x3ffeu32 | 0x3ffe << 14).to_le_bytes());
        lossless.extend_from_slice(&[0; 9]);
        let error = decode(&lossless, (96, 96)).unwrap_err();
        assert!(error.contains("too large"), "{error}");
        let mut lossy = LOSSY.to_vec();
        let start = lossy.windows(3).position(|w| w == [0x9d, 0x01, 0x2a]).unwrap() + 3;
        lossy[start..start + 4].copy_from_slice(&[0xff, 0x3f, 0xff, 0x3f]);
        let error = decode(&lossy, (96, 96)).unwrap_err();
        assert!(error.contains("too large"), "{error}");
        // A JPEG that claims 65535x65535.
        let mut jpeg = LOGO_JPEG.to_vec();
        let frame = jpeg.windows(2).position(|w| w == [0xff, 0xc0] || w == [0xff, 0xc2]).unwrap();
        jpeg[frame + 5..frame + 9].copy_from_slice(&[0xff; 4]);
        assert!(decode(&jpeg, (96, 96)).is_err());
        // 1025x1024 is one row of pixels over the limit.
        assert!(check_size(1024, 1024).is_ok() && check_size(1025, 1024).is_err() && check_size(MAX_SIDE + 1, 1).is_err() && check_size(0, 5).is_err());
    }

    /// A small generator with a fixed seed: the same damage on every run.
    struct Random(u64);
    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, limit: usize) -> usize {
            (self.next() % limit.max(1) as u64) as usize
        }
    }

    /// Whatever the bytes are, the answer is a picture within the limits or an error: no
    /// panic (the test would fail), no endless loop (the test would not end), no allocation
    /// beyond what the limits allow (checked on the result; the size check comes first).
    fn survives(data: &[u8]) {
        if let Ok(picture) = decode(data, (96, 96)) {
            assert!(picture.width >= 1 && picture.height >= 1 && picture.width <= 96 && picture.height <= 96);
            assert_eq!(picture.rgba.len(), picture.width * picture.height * 4);
        }
    }

    #[test]
    fn truncated_files_are_errors_or_pictures_never_a_crash() {
        for (name, data) in ALL {
            let step = (data.len() / 700).max(1);
            for cut in (0..data.len()).step_by(step) {
                survives(&data[..cut]);
            }
            // Without its last byte no file here is whole... except where the last byte is
            // padding or the tail of the last partition, which decoders may tolerate.
            let _ = name;
        }
        assert!(decode(&LOSSLESS[..LOSSLESS.len() / 2], (0, 0)).is_err());
        assert!(decode(&LOGO_WEBP[..40], (0, 0)).is_err());
    }

    #[test]
    fn damaged_files_are_errors_or_pictures_never_a_crash() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let started = std::time::Instant::now();
        let mut tried = 0;
        for (_, data) in ALL {
            // Single bytes changed, with the headers (where a few bits mean the most) hit
            // more often than the rest.
            for round in 0..600 {
                let mut damaged = data.to_vec();
                let changes = 1 + random.below(4);
                for _ in 0..changes {
                    let at = if round % 3 == 0 { random.below(damaged.len().min(64)) } else { random.below(damaged.len()) };
                    damaged[at] = match random.below(4) {
                        0 => 0,
                        1 => 0xff,
                        2 => damaged[at] ^ (1 << random.below(8)),
                        _ => random.next() as u8,
                    };
                }
                survives(&damaged);
                tried += 1;
            }
            // Pieces of the file moved, repeated or dropped.
            for _ in 0..150 {
                let mut damaged = data.to_vec();
                let (from, to, length) = (random.below(damaged.len()), random.below(damaged.len()), 1 + random.below(64));
                let piece: Vec<u8> = damaged[from..(from + length).min(damaged.len())].to_vec();
                match random.below(3) {
                    0 => drop(damaged.splice(to..to, piece)),
                    1 => drop(damaged.drain(from..(from + length).min(data.len()))),
                    _ => damaged[to..(to + piece.len()).min(data.len())].copy_from_slice(&piece[..piece.len().min(data.len() - to)]),
                }
                survives(&damaged);
                tried += 1;
            }
        }
        // Noise behind each kind of header.
        for head in [&b"RIFF\xf4\x01\0\0WEBPVP8 \xe8\x01\0\0"[..], b"RIFF\xf4\x01\0\0WEBPVP8L\xe8\x01\0\0\x2f", b"RIFF\xf4\x01\0\0WEBPVP8X\x0a\0\0\0\x12\0\0\0\x3f\0\0\x3f\0\0", &[0xff, 0xd8, 0xff, 0xe0]] {
            for _ in 0..300 {
                let mut data = head.to_vec();
                data.extend((0..508 - head.len()).map(|_| random.next() as u8));
                survives(&data);
                tried += 1;
            }
        }
        assert!(tried > 7000);
        // Nothing here may take long: a file of a few kilobytes is refused or decoded in
        // milliseconds (the whole test takes a few seconds unoptimised).
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "{:?}", started.elapsed());
    }

    #[test]
    fn chunk_lists_are_checked() {
        assert!(check_webp(LOSSY).is_ok() && check_webp(ANIMATION).is_ok() && check_webp(LOGO_WEBP_ALPHA).is_ok());
        // An animation flag without a frame (the decoder library would look for one).
        let mut flagged = LOSSY_ALPHA.to_vec();
        assert_eq!(&flagged[12..16], b"VP8X");
        flagged[20] |= 0x02;
        assert!(decode(&flagged, (0, 0)).unwrap_err().contains("without frames"));
        // A chunk that claims more than the file has.
        let mut long = LOSSY.to_vec();
        long[16..20].copy_from_slice(&0x00ff_ffffu32.to_le_bytes());
        assert!(decode(&long, (0, 0)).unwrap_err().contains("past the end"));
        // Only a header.
        assert!(decode(&LOSSY_ALPHA[..30], (0, 0)).is_err());
        assert!(decode(b"RIFF\x04\0\0\0WEBP", (0, 0)).unwrap_err().contains("no picture"));
    }
}
