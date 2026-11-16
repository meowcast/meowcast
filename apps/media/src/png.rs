//! A small PNG decoder for the pictures apps show in their UI (channel logos, cover art):
//! every colour type at 8 and 16 bits (and the 1-, 2- and 4-bit grey and palette forms), with
//! transparency, not interlaced. Inflate is written out here (RFC 1951, a few dozen lines) so
//! that no dependency is needed; it is the simple bit-by-bit kind, fast enough for icons
//! (a 300x300 logo takes a few milliseconds), not meant for photographs.
//!
//! ```no_run
//! # let bytes: &[u8] = &[];
//! let image = meow_media::png::decode(bytes)?;          // straight RGBA, 8 bits
//! let tile = image.resized(96, 96).premultiplied();     // 0xAARRGGBB, as the canvas blends
//! # Ok::<(), String>(())
//! ```

/// Pictures larger than this (in pixels) are refused: an app's UI has no use for them and
/// the memory is the device's.
const MAX_PIXELS: usize = 4 << 20;

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    buffer: u32,
    count: u32,
}

impl Bits<'_> {
    fn take(&mut self, count: u32) -> Result<u32, String> {
        while self.count < count {
            let byte = *self.data.get(self.at).ok_or("png: the compressed data ends early")?;
            self.at += 1;
            self.buffer |= (byte as u32) << self.count;
            self.count += 8;
        }
        let value = self.buffer & ((1u32 << count) - 1);
        self.buffer >>= count;
        self.count -= count;
        Ok(value)
    }
}

/// A canonical Huffman code: how many codes of each length, and the symbols in code order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &length in lengths {
            counts[length as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for length in 1..16 {
            offsets[length] = offsets[length - 1] + counts[length - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[length as usize] as usize] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, String> {
        // Codes of each length are consecutive: walk the lengths, one bit at a time.
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for length in 1..16 {
            code |= bits.take(1)? as i32;
            let count = self.counts[length] as i32;
            if code - count < first {
                return self.symbols.get((index + code - first) as usize).copied().ok_or_else(|| "png: bad Huffman code".to_string());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("png: bad Huffman code".to_string())
    }
}

const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DISTANCE_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DISTANCE_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Decompress a raw deflate stream; more than `limit` bytes of output is an error.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut bits = Bits { data, at: 0, buffer: 0, count: 0 };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => {
                // Stored: byte-aligned length, its complement, the bytes.
                (bits.buffer, bits.count) = (0, 0);
                let header = data.get(bits.at..bits.at + 4).ok_or("png: the compressed data ends early")?;
                let length = u16::from_le_bytes([header[0], header[1]]) as usize;
                if length != !u16::from_le_bytes([header[2], header[3]]) as usize {
                    return Err("png: damaged stored block".to_string());
                }
                let bytes = data.get(bits.at + 4..bits.at + 4 + length).ok_or("png: the compressed data ends early")?;
                if out.len() + length > limit {
                    return Err("png: more data than the picture needs".to_string());
                }
                out.extend_from_slice(bytes);
                bits.at += 4 + length;
            }
            kind @ (1 | 2) => {
                let (literals, distances) = if kind == 1 {
                    let mut lengths = [8u8; 288];
                    lengths[144..256].fill(9);
                    lengths[256..280].fill(7);
                    (Huffman::new(&lengths), Huffman::new(&[5u8; 30]))
                } else {
                    let literal_count = bits.take(5)? as usize + 257;
                    let distance_count = bits.take(5)? as usize + 1;
                    let code_count = bits.take(4)? as usize + 4;
                    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut code_lengths = [0u8; 19];
                    for &position in &ORDER[..code_count] {
                        code_lengths[position] = bits.take(3)? as u8;
                    }
                    let codes = Huffman::new(&code_lengths);
                    let mut lengths = vec![0u8; literal_count + distance_count];
                    let mut at = 0;
                    while at < lengths.len() {
                        let (value, repeat) = match codes.decode(&mut bits)? {
                            symbol @ 0..=15 => (symbol as u8, 1),
                            16 => (*lengths.get(at.wrapping_sub(1)).ok_or("png: bad code lengths")?, 3 + bits.take(2)? as usize),
                            17 => (0, 3 + bits.take(3)? as usize),
                            _ => (0, 11 + bits.take(7)? as usize),
                        };
                        let run = lengths.get_mut(at..at + repeat).ok_or("png: bad code lengths")?;
                        run.fill(value);
                        at += repeat;
                    }
                    (Huffman::new(&lengths[..literal_count]), Huffman::new(&lengths[literal_count..]))
                };
                loop {
                    match literals.decode(&mut bits)? as usize {
                        symbol @ 0..=255 => {
                            if out.len() >= limit {
                                return Err("png: more data than the picture needs".to_string());
                            }
                            out.push(symbol as u8);
                        }
                        256 => break,
                        symbol => {
                            let index = symbol - 257;
                            let length = *LENGTH_BASE.get(index).ok_or("png: bad length code")? as usize + bits.take(LENGTH_EXTRA[index] as u32)? as usize;
                            let index = distances.decode(&mut bits)? as usize;
                            let distance = *DISTANCE_BASE.get(index).ok_or("png: bad distance code")? as usize + bits.take(DISTANCE_EXTRA[index] as u32)? as usize;
                            if distance > out.len() {
                                return Err("png: a copy from before the start".to_string());
                            }
                            if out.len() + length > limit {
                                return Err("png: more data than the picture needs".to_string());
                            }
                            // Byte by byte: the copy may overlap what it writes.
                            for _ in 0..length {
                                out.push(out[out.len() - distance]);
                            }
                        }
                    }
                }
            }
            _ => return Err("png: bad block type".to_string()),
        }
        if last {
            return Ok(out);
        }
    }
}

/// A decoded picture: 8-bit RGBA, not premultiplied, rows top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl Image {
    /// Scaled to `width` x `height` by averaging the source pixels under each new one (in
    /// premultiplied form, so transparent pixels do not bleed their colour).
    pub fn resized(&self, width: usize, height: usize) -> Image {
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            let (y0, y1) = (y * self.height / height.max(1), ((y + 1) * self.height).div_ceil(height.max(1)).clamp(1, self.height.max(1)));
            for x in 0..width {
                let (x0, x1) = (x * self.width / width.max(1), ((x + 1) * self.width).div_ceil(width.max(1)).clamp(1, self.width.max(1)));
                let (mut sum, mut count) = ([0u32; 4], 0u32);
                for row in y0.min(y1 - 1)..y1 {
                    for pixel in self.rgba[(row * self.width + x0.min(x1 - 1)) * 4..(row * self.width + x1) * 4].chunks_exact(4) {
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
        Image { width, height, rgba }
    }

    /// Made smaller so that no side is longer than `side` pixels, keeping its shape (0, or
    /// a picture that fits already: unchanged).
    pub fn fitted(self, side: usize) -> Image {
        let longest = self.width.max(self.height);
        if side == 0 || longest <= side {
            return self;
        }
        self.resized((self.width * side).div_ceil(longest).clamp(1, side), (self.height * side).div_ceil(longest).clamp(1, side))
    }

    /// The pixels as premultiplied 0xAARRGGBB, which is what `canvas::Canvas` holds.
    pub fn premultiplied(&self) -> Vec<u32> {
        self.rgba
            .chunks_exact(4)
            .map(|p| {
                let alpha = p[3] as u32;
                let channel = |value: u8| (value as u32 * alpha + 127) / 255;
                alpha << 24 | channel(p[0]) << 16 | channel(p[1]) << 8 | channel(p[2])
            })
            .collect()
    }
}

/// Decode a PNG file.
pub fn decode(data: &[u8]) -> Result<Image, String> {
    if data.get(..8) != Some(b"\x89PNG\r\n\x1a\n") {
        return Err("png: not a PNG file".to_string());
    }
    let (mut header, mut palette, mut transparency, mut compressed) = (None, &[][..], &[][..], Vec::new());
    let mut at = 8;
    while let Some(head) = data.get(at..at + 8) {
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let body = data.get(at + 8..at + 8 + length).ok_or("png: a chunk runs past the end of the file")?;
        match &head[4..8] {
            b"IHDR" if length == 13 => header = Some(body),
            b"PLTE" => palette = body,
            b"tRNS" => transparency = body,
            b"IDAT" => compressed.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        at += 12 + length;
    }
    let header = header.ok_or("png: no header")?;
    let width = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let height = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let (depth, colour, interlace) = (header[8] as usize, header[9], header[12]);
    if width == 0 || height == 0 || width.saturating_mul(height) > MAX_PIXELS {
        return Err(format!("png: {width}x{height} is not a size this decoder takes"));
    }
    if interlace != 0 {
        return Err("png: interlaced pictures are not supported".to_string());
    }
    let channels = match (colour, depth) {
        (0, 1 | 2 | 4 | 8 | 16) | (3, 1 | 2 | 4 | 8) => 1,
        (4, 8 | 16) => 2,
        (2, 8 | 16) => 3,
        (6, 8 | 16) => 4,
        _ => return Err(format!("png: colour type {colour} with {depth} bits is not valid")),
    };
    let stride = (width * channels * depth).div_ceil(8);
    // Bytes between a byte and the one "to its left" for the filters.
    let step = (channels * depth / 8).max(1);
    let expected = (stride + 1) * height;
    // A zlib stream: two header bytes, deflate, four bytes of checksum.
    if compressed.len() < 6 || compressed[0] & 0x0f != 8 {
        return Err("png: no compressed data".to_string());
    }
    let mut raw = inflate(&compressed[2..], expected)?;
    if raw.len() != expected {
        return Err("png: the picture's data is incomplete".to_string());
    }
    // Undo the filters, in place: each row is predicted from the bytes left of and above it.
    for row in 0..height {
        let (before, rest) = raw.split_at_mut(row * (stride + 1));
        let filter = rest[0];
        let line = &mut rest[1..stride + 1];
        let above = if row == 0 { None } else { Some(&before[(row - 1) * (stride + 1) + 1..]) };
        for i in 0..stride {
            let left = if i >= step { line[i - step] as i32 } else { 0 };
            let up = above.map_or(0, |a| a[i] as i32);
            let up_left = if i >= step { above.map_or(0, |a| a[i - step] as i32) } else { 0 };
            let predicted = match filter {
                0 => 0,
                1 => left,
                2 => up,
                3 => (left + up) / 2,
                4 => {
                    let estimate = left + up - up_left;
                    let (a, b, c) = ((estimate - left).abs(), (estimate - up).abs(), (estimate - up_left).abs());
                    if a <= b && a <= c { left } else if b <= c { up } else { up_left }
                }
                _ => return Err("png: unknown row filter".to_string()),
            };
            line[i] = line[i].wrapping_add(predicted as u8);
        }
    }
    // To RGBA, 8 bits.
    let mut rgba = Vec::with_capacity(width * height * 4);
    let key = |channel: usize| transparency.get(channel * 2..channel * 2 + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    for row in 0..height {
        let line = &raw[row * (stride + 1) + 1..][..stride];
        // A sample of this row: its 8-bit value (scaled up from fewer bits, the high byte of
        // 16) and its full value (for the transparent-colour key).
        let sample = |index: usize| -> (u8, u16) {
            match depth {
                8 => (line[index], line[index] as u16),
                16 => (line[index * 2], u16::from_be_bytes([line[index * 2], line[index * 2 + 1]])),
                _ => {
                    let value = line[index * depth / 8] >> (8 - depth - index * depth % 8) & ((1 << depth) - 1);
                    ((value as u32 * 255 / ((1 << depth) - 1)) as u8, value as u16)
                }
            }
        };
        for x in 0..width {
            match colour {
                0 => {
                    let (grey, full) = sample(x);
                    rgba.extend_from_slice(&[grey, grey, grey, if key(0) == Some(full) { 0 } else { 255 }]);
                }
                2 => {
                    let (r, g, b) = (sample(x * 3), sample(x * 3 + 1), sample(x * 3 + 2));
                    let keyed = key(0).is_some() && (key(0), key(1), key(2)) == (Some(r.1), Some(g.1), Some(b.1));
                    rgba.extend_from_slice(&[r.0, g.0, b.0, if keyed { 0 } else { 255 }]);
                }
                3 => {
                    let index = sample(x).1 as usize;
                    let colour = palette.get(index * 3..index * 3 + 3).ok_or("png: a colour outside the palette")?;
                    rgba.extend_from_slice(&[colour[0], colour[1], colour[2], transparency.get(index).copied().unwrap_or(255)]);
                }
                4 => rgba.extend_from_slice(&[sample(x * 2).0, sample(x * 2).0, sample(x * 2).0, sample(x * 2 + 1).0]),
                _ => rgba.extend_from_slice(&[sample(x * 4).0, sample(x * 4 + 1).0, sample(x * 4 + 2).0, sample(x * 4 + 3).0]),
            }
        }
    }
    Ok(Image { width, height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap()).collect()
    }

    #[test]
    fn inflate_reads_stored_fixed_and_dynamic_blocks() {
        // zlib.compress(b"meow " * 40, 9)[2:-4]: a fixed-code block with a long self-overlapping copy.
        assert_eq!(inflate(&hex("cb4dcd2f57c81dfa0400"), 1000).unwrap(), b"meow ".repeat(40));
        // A stored block.
        assert_eq!(inflate(&hex("010500faff6d656f7721"), 10).unwrap(), b"meow!");
        // Dynamic codes: zlib.compress(bytes(range(256)) * 3 + b"the quick brown fox " * 9, 9)[2:-4].
        let data = hex(DYNAMIC);
        let expected: Vec<u8> = (0..=255u8).collect::<Vec<_>>().repeat(3).into_iter().chain(b"the quick brown fox ".repeat(9)).collect();
        assert_eq!(inflate(&data, 2000).unwrap(), expected);
        // Limits and damage are errors, not panics.
        assert!(inflate(&data, 100).is_err());
        assert!(inflate(&data[..data.len() / 2], 2000).is_err());
        assert!(inflate(&hex("07"), 10).is_err());
        assert!(inflate(&[], 10).is_err());
    }

    #[test]
    fn png_colour_types_filters_and_transparency() {
        // The canvas's own writer: RGBA in stored blocks.
        let mut canvas = crate::canvas::Canvas::new(5, 3);
        canvas.pixels[0] = 0xffff_0000;
        canvas.pixels[7] = 0xff00_ff00;
        canvas.pixels[14] = 0xff12_3456;
        let image = decode(&canvas.png()).unwrap();
        assert_eq!((image.width, image.height, image.rgba.len()), (5, 3, 60));
        assert_eq!((&image.rgba[..4], &image.rgba[28..32], &image.rgba[56..60]), (&[255, 0, 0, 255][..], &[0, 255, 0, 255][..], &[0x12, 0x34, 0x56, 255][..]));

        // Written by a real encoder (Python's zlib, every row filter in turn): 4x5 RGBA.
        let image = decode(&hex(RGBA_FILTERS)).unwrap();
        assert_eq!((image.width, image.height), (4, 5));
        for y in 0..5 {
            for x in 0..4 {
                assert_eq!(image.rgba[(y * 4 + x) * 4..][..4], [(x * 60) as u8, (y * 50) as u8, (x * y * 20) as u8, (255 - x * 40) as u8], "pixel {x},{y}");
            }
        }
        // A 2-bit palette with a transparent entry: 5x2.
        let image = decode(&hex(PALETTE)).unwrap();
        assert_eq!(image.rgba, [[255, 0, 0, 255], [0, 255, 0, 128], [0, 0, 255, 255], [9, 9, 9, 0], [255, 0, 0, 255], [9, 9, 9, 0], [0, 0, 255, 255], [0, 255, 0, 128], [255, 0, 0, 255], [0, 255, 0, 128]].concat());
        // 16-bit grey with alpha: 2x1.
        let image = decode(&hex(GREY_ALPHA_16)).unwrap();
        assert_eq!(image.rgba, [0x12, 0x12, 0x12, 0xff, 0xab, 0xab, 0xab, 0x80]);

        assert!(decode(b"GIF89a").is_err());
        let mut interlaced = hex(PALETTE);
        interlaced[28] = 1;
        assert!(decode(&interlaced).is_err());
        let truncated = hex(RGBA_FILTERS);
        assert!(decode(&truncated[..truncated.len() - 30]).is_err());
    }

    #[test]
    fn resizing_averages_and_premultiplies() {
        // A 4x2 picture: red, red, transparent blue, transparent blue / the same.
        let row = [[255, 0, 0, 255], [255, 0, 0, 255], [0, 0, 255, 0], [0, 0, 255, 0]].concat();
        let image = Image { width: 4, height: 2, rgba: [row.clone(), row].concat() };
        let small = image.resized(2, 1);
        assert_eq!(small.rgba, [255, 0, 0, 255, 0, 0, 0, 0]);
        // One pixel for everything: half covered, and the colour is the visible half's.
        let one = image.resized(1, 1);
        assert_eq!(one.rgba, [255, 0, 0, 128]);
        assert_eq!(one.premultiplied(), [0x8080_0000]);
        // Larger than the source: pixels repeat.
        assert_eq!(image.resized(8, 2).rgba.len(), 64);
        assert_eq!(image.resized(8, 4).rgba[..8], [255, 0, 0, 255, 255, 0, 0, 255]);
    }

    const DYNAMIC: &str = "6360646266616563e7e0e4e2e6e1e5e3171014121611151397909492969195935750545256515553d7d0d4d2d6d1d5d33730343236313533b7b0b4b2b6b1b5b37770747276717573f7f0f4f2f6f1f5f30f080c0a0e090d0b8f888c8a8e898d8b4f484c4a4e494d4bcfc8cccacec9cdcb2f282c2a2e292d2bafa8acaaaea9adab6f686c6a6e696d6befe8eceaeee9edeb9f3071d2e42953a74d9f3173d6ec3973e7cd5fb070d1e2254b972d5fb172d5ea356bd7addfb071d3e62d5bb76ddfb173d7ee3d7bf7ed3f70f0d0e123478f1d3f71f2d4e93367cf9dbf70f1d2e52b57af5dbf71f3d6ed3b77efdd7ff0f0d1e3274f9f3d7ff1f2d5eb376fdfbdfff0f1d3e72f5fbf7dfff1f3d7ef3f7ffffd6718f5ff88f67f4946aa4261696672b64252517e799e425a7e85c260170300";
    const RGBA_FILTERS: &str = "89504e470d0a1a0a0000000d494844520000000400000005080600000062ad4ddb0000004a4944415478da63606060f86fc3c070bd828161fd160686764606239080c80d1866020a3030188900b10610db303033a43034c8496abc91933402629b372c60150c40150c40150c360c0011e00ff517a82f8f0000000049454e44ae426082";
    const PALETTE: &str = "89504e470d0a1a0a0000000d4948445200000005000000020203000000ed04fece0000000c504c5445ff000000ff000000ff0909095c717e860000000474524e53ff80ff001f8786990000000e4944415478da6390666078e2000002950140167c9f970000000049454e44ae426082";
    const GREY_ALPHA_16: &str = "89504e470d0a1a0a0000000d49484452000000020000000110040000000ebb6b42000000114944415478da631032f9ff7ff5d9060600130d043d0344cdd80000000049454e44ae426082";
}
