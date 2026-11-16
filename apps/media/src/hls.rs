//! HTTP Live Streaming, the part without a network: playlists (master and media), the choice
//! of a variant this device can decode, following a live playlist's moving window, and
//! AES-128 segment decryption. Fetching is the caller's (apps reach the network through
//! `meow_app::net`); segments are MPEG-TS (`ts`) or fragmented MP4 (`mp4`).
//!
//! ```no_run
//! use meow_media::hls::{self, Playlist};
//! # fn get(url: &str) -> String { String::new() }
//! let url = "https://example.com/live/master.m3u8";
//! let media_url = match hls::parse(&get(url), url)? {
//!     Playlist::Master(master) => master.pick(&hls::Limits::default()).ok_or("nothing playable")?.url.clone(),
//!     Playlist::Media(_) => url.to_string(),
//! };
//! let mut window = hls::Window::new();
//! loop {
//!     let Playlist::Media(media) = hls::parse(&get(&media_url), &media_url)? else { break };
//!     for segment in window.advance(&media) { /* fetch segment.url, demux, play */ }
//!     if media.ended { break }
//!     std::thread::sleep(media.reload_after(true));
//! }
//! # Ok::<(), String>(())
//! ```

use std::time::Duration;

/// One `#EXT-X-STREAM-INF` entry of a master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub url: String,
    /// Peak bits per second.
    pub bandwidth: u64,
    /// 0 when the playlist does not say.
    pub width: u32,
    pub height: u32,
    pub frame_rate: f32,
    /// As written: `avc1.4d4028,mp4a.40.2`. Empty when absent.
    pub codecs: String,
    pub name: String,
    /// The group of `#EXT-X-MEDIA` audio renditions this variant plays with.
    pub audio_group: Option<String>,
}

impl Variant {
    /// The video codec is H.264, or not named (old playlists: H.264 is what HLS began with).
    pub fn is_h264(&self) -> bool {
        let mut video = self.codecs.split(',').map(str::trim).filter(|c| !c.is_empty() && !is_audio_codec(c)).peekable();
        video.peek().is_none() || video.all(|c| c.starts_with("avc1") || c.starts_with("avc3"))
    }

    /// Sound only (a radio channel, or the audio-only fallback variant).
    pub fn is_audio_only(&self) -> bool {
        !self.codecs.is_empty() && self.codecs.split(',').map(str::trim).all(is_audio_codec)
    }
}

fn is_audio_codec(codec: &str) -> bool {
    ["mp4a", "ac-3", "ec-3", "mp3", "opus", "flac"].iter().any(|prefix| codec.starts_with(prefix))
}

/// One `#EXT-X-MEDIA` entry: an alternative sound track or subtitles.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendition {
    /// `AUDIO`, `SUBTITLES`, `VIDEO`, `CLOSED-CAPTIONS`.
    pub kind: String,
    pub group: String,
    pub name: String,
    pub language: String,
    /// None: the rendition is in the variant's own segments.
    pub url: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

/// What the device can decode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub max_height: u32,
    pub max_width: u32,
    pub max_frame_rate: f32,
    /// 0: no limit (the best that fits the other limits).
    pub max_bandwidth: u64,
}

impl Default for Limits {
    /// The dongle's decoder: H.264 up to 1080p60.
    fn default() -> Limits {
        Limits { max_height: 1088, max_width: 1920, max_frame_rate: 61.0, max_bandwidth: 0 }
    }
}

impl Master {
    /// The best H.264 variant within `limits`; when none fits, the smallest H.264 one (it may
    /// still play); None when there is no H.264 video at all.
    pub fn pick(&self, limits: &Limits) -> Option<&Variant> {
        let playable = || self.variants.iter().filter(|v| v.is_h264() && !v.is_audio_only());
        let fits = |v: &&Variant| v.height <= limits.max_height && v.width <= limits.max_width && v.frame_rate <= limits.max_frame_rate && (limits.max_bandwidth == 0 || v.bandwidth <= limits.max_bandwidth);
        playable().filter(fits).max_by_key(|v| (v.height, v.bandwidth)).or_else(|| playable().min_by_key(|v| (v.height, v.bandwidth)))
    }

    /// The sound of `variant` when it comes as a playlist of its own (not in the video's segments).
    pub fn separate_audio(&self, variant: &Variant) -> Option<&Rendition> {
        let group = variant.audio_group.as_deref()?;
        let mut tracks = self.renditions.iter().filter(|r| r.kind == "AUDIO" && r.group == group && r.url.is_some());
        let first = tracks.clone().next();
        tracks.find(|r| r.default).or(first)
    }
}

/// How the segments from an `#EXT-X-KEY` on are encrypted.
#[derive(Debug, Clone, PartialEq)]
pub enum Encryption {
    /// Whole segments, AES-128 in CBC mode (see `decrypt_aes128`).
    Aes128 { key_url: String, iv: Option<[u8; 16]> },
    /// SAMPLE-AES (FairPlay and friends) or something newer: not playable here.
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Its media sequence number: the identity of a segment across reloads.
    pub sequence: u64,
    pub url: String,
    pub duration: f64,
    /// Timestamps and stream parameters start anew at this segment.
    pub discontinuity: bool,
    pub encryption: Option<Encryption>,
    /// The initialization section for this segment (fragmented MP4): URL and byte range.
    pub map: Option<(String, Option<(u64, u64)>)>,
    /// (first byte, length) when the segment is a part of the resource.
    pub byte_range: Option<(u64, u64)>,
    /// `#EXT-X-PROGRAM-DATE-TIME` as written, when given for this segment.
    pub date_time: Option<String>,
}

impl Segment {
    /// The IV for AES-128: the playlist's, or the sequence number (big endian) by default.
    pub fn iv(&self) -> [u8; 16] {
        match &self.encryption {
            Some(Encryption::Aes128 { iv: Some(iv), .. }) => *iv,
            _ => (self.sequence as u128).to_be_bytes(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Media {
    pub target_duration: f64,
    pub media_sequence: u64,
    pub discontinuity_sequence: u64,
    pub segments: Vec<Segment>,
    /// `#EXT-X-ENDLIST`: nothing will be added (a recording, or a finished broadcast).
    pub ended: bool,
    /// `#EXT-X-PLAYLIST-TYPE` is VOD or EVENT: the playlist starts at the beginning of the content.
    pub from_start: bool,
}

impl Media {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }

    /// When to load a live playlist again: after a target duration if it had something new,
    /// after half of one if not (RFC 8216, 6.3.4).
    pub fn reload_after(&self, changed: bool) -> Duration {
        let target = if self.target_duration > 0.0 { self.target_duration } else { 4.0 };
        Duration::from_secs_f64((if changed { target } else { target / 2.0 }).clamp(0.5, 15.0))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(Master),
    Media(Media),
}

/// `ATTR=value,ATTR="quoted, with commas"` -> pairs (quotes removed).
fn attributes(list: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = list.trim();
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        let (value, tail) = match after.strip_prefix('"') {
            Some(quoted) => match quoted.split_once('"') {
                Some((value, tail)) => (value, tail.trim_start().strip_prefix(',').unwrap_or(tail)),
                None => (quoted, ""),
            },
            None => after.split_once(',').unwrap_or((after, "")),
        };
        out.push((name.trim(), value.trim()));
        rest = tail.trim_start();
    }
    out
}

fn attribute<'a>(pairs: &[(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| *value)
}

/// `length[@offset]` of a BYTERANGE; without an offset the range follows the previous one.
fn byte_range(text: &str, follows: u64) -> Option<(u64, u64)> {
    let (length, offset) = match text.split_once('@') {
        Some((length, offset)) => (length.trim().parse().ok()?, offset.trim().parse().ok()?),
        None => (text.trim().parse().ok()?, follows),
    };
    Some((offset, length))
}

fn hex_iv(text: &str) -> Option<[u8; 16]> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    u128::from_str_radix(digits, 16).ok().map(u128::to_be_bytes)
}

/// Parse a playlist loaded from `url` (relative references are resolved against it).
pub fn parse(text: &str, url: &str) -> Result<Playlist, String> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    if !lines.next().is_some_and(|first| first.trim_start_matches('\u{feff}').starts_with("#EXTM3U")) {
        return Err("not an HLS playlist (no #EXTM3U)".to_string());
    }
    let mut master = Master::default();
    let mut media = Media::default();
    let mut is_master = false;
    let mut is_media = false;
    // Pending state for the next URI line.
    let mut variant: Option<Variant> = None;
    let (mut duration, mut discontinuity, mut range, mut date_time) = (None::<f64>, false, None, None);
    let (mut encryption, mut map, mut range_end) = (None::<Encryption>, None, 0u64);
    for line in lines {
        if let Some(tag) = line.strip_prefix("#EXT") {
            let (name, value) = tag.split_once(':').unwrap_or((tag, ""));
            match name {
                "-X-STREAM-INF" => {
                    is_master = true;
                    let pairs = attributes(value);
                    let (width, height) = attribute(&pairs, "RESOLUTION").and_then(|r| r.split_once(['x', 'X'])).map_or((0, 0), |(w, h)| (w.parse().unwrap_or(0), h.parse().unwrap_or(0)));
                    variant = Some(Variant {
                        url: String::new(),
                        bandwidth: attribute(&pairs, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                        width,
                        height,
                        frame_rate: attribute(&pairs, "FRAME-RATE").and_then(|f| f.parse().ok()).unwrap_or(0.0),
                        codecs: attribute(&pairs, "CODECS").unwrap_or("").to_string(),
                        name: attribute(&pairs, "NAME").unwrap_or("").to_string(),
                        audio_group: attribute(&pairs, "AUDIO").map(str::to_string),
                    });
                }
                "-X-MEDIA" => {
                    is_master = true;
                    let pairs = attributes(value);
                    let text = |name: &str| attribute(&pairs, name).unwrap_or("").to_string();
                    master.renditions.push(Rendition { kind: text("TYPE"), group: text("GROUP-ID"), name: text("NAME"), language: text("LANGUAGE"), url: attribute(&pairs, "URI").map(|u| resolve(url, u)), default: attribute(&pairs, "DEFAULT") == Some("YES") });
                }
                "INF" => {
                    is_media = true;
                    duration = Some(value.split(',').next().and_then(|d| d.trim().parse().ok()).unwrap_or(0.0));
                }
                "-X-TARGETDURATION" => {
                    is_media = true;
                    media.target_duration = value.trim().parse().unwrap_or(0.0);
                }
                "-X-MEDIA-SEQUENCE" => media.media_sequence = value.trim().parse().unwrap_or(0),
                "-X-DISCONTINUITY-SEQUENCE" => media.discontinuity_sequence = value.trim().parse().unwrap_or(0),
                "-X-DISCONTINUITY" => discontinuity = true,
                "-X-ENDLIST" => media.ended = true,
                "-X-PLAYLIST-TYPE" => media.from_start = matches!(value.trim(), "VOD" | "EVENT"),
                "-X-PROGRAM-DATE-TIME" => date_time = Some(value.trim().to_string()),
                "-X-BYTERANGE" => range = byte_range(value, range_end),
                "-X-KEY" => {
                    let pairs = attributes(value);
                    encryption = match attribute(&pairs, "METHOD") {
                        Some("NONE") | None => None,
                        Some("AES-128") => Some(Encryption::Aes128 { key_url: resolve(url, attribute(&pairs, "URI").unwrap_or("")), iv: attribute(&pairs, "IV").and_then(hex_iv) }),
                        Some(other) => Some(Encryption::Other(other.to_string())),
                    };
                }
                "-X-MAP" => {
                    let pairs = attributes(value);
                    map = attribute(&pairs, "URI").map(|u| (resolve(url, u), attribute(&pairs, "BYTERANGE").and_then(|r| byte_range(r, 0))));
                }
                _ => {}
            }
        } else if line.starts_with('#') {
            // A comment.
        } else if let Some(mut variant) = variant.take() {
            variant.url = resolve(url, line);
            master.variants.push(variant);
        } else if let Some(duration) = duration.take() {
            if let Some((offset, length)) = range {
                range_end = offset + length;
            }
            media.segments.push(Segment {
                sequence: media.media_sequence + media.segments.len() as u64,
                url: resolve(url, line),
                duration,
                discontinuity: std::mem::take(&mut discontinuity),
                encryption: encryption.clone(),
                map: map.clone(),
                byte_range: range.take(),
                date_time: date_time.take(),
            });
        }
    }
    match (is_master, is_media) {
        (true, false) if !master.variants.is_empty() => Ok(Playlist::Master(master)),
        (true, _) => Err("a master playlist without playable variants".to_string()),
        (false, true) => Ok(Playlist::Media(media)),
        (false, false) => Err("an empty playlist".to_string()),
    }
}

/// Resolve a URI reference from a playlist against the playlist's own URL (RFC 3986, for
/// http(s): absolute URLs, `//host/...`, `/path`, and relative paths with `.` and `..`).
pub fn resolve(base: &str, reference: &str) -> String {
    let reference = reference.trim();
    if reference.contains("://") && reference.split("://").next().is_some_and(|scheme| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))) {
        return reference.to_string();
    }
    let Some((scheme, rest)) = base.split_once("://") else { return reference.to_string() };
    if let Some(network) = reference.strip_prefix("//") {
        return format!("{scheme}://{network}");
    }
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let path = &path[..path.find(['?', '#']).unwrap_or(path.len())];
    if reference.starts_with('/') {
        return format!("{scheme}://{authority}{reference}");
    }
    if reference.is_empty() || reference.starts_with(['?', '#']) {
        return format!("{scheme}://{authority}{}{reference}", if path.is_empty() { "/" } else { path });
    }
    // Merge with the base's directory, then remove dot segments (in the path only).
    let (reference_path, suffix) = reference.split_at(reference.find(['?', '#']).unwrap_or(reference.len()));
    let directory = &path[..path.rfind('/').map_or(0, |at| at + 1)];
    let mut parts: Vec<&str> = Vec::new();
    let merged = format!("{}{reference_path}", if directory.is_empty() { "/" } else { directory });
    let trailing = merged.ends_with('/') || merged.ends_with("/.") || merged.ends_with("/..");
    for part in merged.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("{scheme}://{authority}/{}{}{suffix}", parts.join("/"), if trailing && !parts.is_empty() { "/" } else { "" })
}

/// Follows a media playlist across reloads: which segments are new. For a live playlist
/// playback starts a few segments behind its end, so the next reload is never needed in a hurry.
#[derive(Debug, Clone)]
pub struct Window {
    next: Option<u64>,
    /// Segments from the live end to start at (3, as RFC 8216 advises).
    pub start_behind: usize,
    /// The next segment handed out starts a new timeline (segments were missed).
    jumped: bool,
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}

impl Window {
    pub fn new() -> Window {
        Window { next: None, start_behind: 3, jumped: false }
    }

    /// The sequence number of the next segment wanted.
    pub fn next(&self) -> Option<u64> {
        self.next
    }

    /// The segments of `media` not handed out yet, oldest first. When segments were missed
    /// (the window moved past them, or the stream started again) the first one is marked
    /// as a discontinuity.
    pub fn advance(&mut self, media: &Media) -> Vec<Segment> {
        let (first, count) = (media.media_sequence, media.segments.len() as u64);
        if count == 0 {
            return Vec::new();
        }
        let live_start = || if media.ended || media.from_start { first } else { first + count.saturating_sub(self.start_behind.max(1) as u64) };
        let from = match self.next {
            None => live_start(),
            Some(next) if next < first => {
                // Too slow: segments left the window before they were fetched.
                self.jumped = true;
                first
            }
            // The numbering went back: the stream was restarted. (One reload may show an
            // older copy from another server: a few segments back is not a restart.)
            Some(next) if next > first + count + 3 => {
                self.jumped = true;
                live_start()
            }
            Some(next) => next,
        };
        let mut fresh: Vec<Segment> = media.segments.iter().filter(|s| s.sequence >= from).cloned().collect();
        if let Some(segment) = fresh.first_mut() {
            segment.discontinuity |= std::mem::take(&mut self.jumped);
        }
        self.next = Some(fresh.last().map_or(from, |s| s.sequence + 1));
        fresh
    }
}

// AES-128 decryption (FIPS 197), enough for CBC-encrypted segments: small rather than fast
// (a 2 MB segment takes a fraction of a second on the dongle).
const fn gf_double(value: u8) -> u8 {
    value << 1 ^ if value & 0x80 != 0 { 0x1b } else { 0 }
}

const fn gf_multiply(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        a = gf_double(a);
        b >>= 1;
    }
    product
}

/// The S-box and its inverse, computed from the field arithmetic at compile time.
const fn sboxes() -> ([u8; 256], [u8; 256]) {
    let (mut forward, mut inverse) = ([0u8; 256], [0u8; 256]);
    // 3 generates the multiplicative group: walk p = 3^i and q = 3^-i together.
    let (mut p, mut q) = (1u8, 1u8);
    loop {
        p = p ^ gf_double(p);
        // q /= 3
        q ^= q << 1;
        q ^= q << 2;
        q ^= q << 4;
        if q & 0x80 != 0 {
            q ^= 0x09;
        }
        let value = q ^ q.rotate_left(1) ^ q.rotate_left(2) ^ q.rotate_left(3) ^ q.rotate_left(4) ^ 0x63;
        forward[p as usize] = value;
        inverse[value as usize] = p;
        if p == 1 {
            break;
        }
    }
    forward[0] = 0x63;
    inverse[0x63] = 0;
    (forward, inverse)
}

const SBOXES: ([u8; 256], [u8; 256]) = sboxes();

/// Products with 9, 11, 13 and 14 in the field (the inverse MixColumns matrix), as tables.
const fn inverse_mix() -> [[u8; 256]; 4] {
    let mut tables = [[0u8; 256]; 4];
    let factors = [9, 11, 13, 14];
    let mut table = 0;
    while table < 4 {
        let mut value = 0;
        while value < 256 {
            tables[table][value] = gf_multiply(value as u8, factors[table]);
            value += 1;
        }
        table += 1;
    }
    tables
}

const INVERSE_MIX: [[u8; 256]; 4] = inverse_mix();

fn expand_key(key: &[u8; 16]) -> [[u8; 16]; 11] {
    let mut words = [[0u8; 4]; 44];
    for (word, bytes) in words.iter_mut().zip(key.chunks_exact(4)) {
        word.copy_from_slice(bytes);
    }
    let mut round_constant = 1u8;
    for i in 4..44 {
        let mut word = words[i - 1];
        if i % 4 == 0 {
            word = [SBOXES.0[word[1] as usize] ^ round_constant, SBOXES.0[word[2] as usize], SBOXES.0[word[3] as usize], SBOXES.0[word[0] as usize]];
            round_constant = gf_double(round_constant);
        }
        for (byte, previous) in word.iter_mut().zip(words[i - 4]) {
            *byte ^= previous;
        }
        words[i] = word;
    }
    let mut keys = [[0u8; 16]; 11];
    for (round, key) in keys.iter_mut().enumerate() {
        for column in 0..4 {
            key[column * 4..column * 4 + 4].copy_from_slice(&words[round * 4 + column]);
        }
    }
    keys
}

fn decrypt_block(keys: &[[u8; 16]; 11], block: &mut [u8; 16]) {
    let add = |block: &mut [u8; 16], key: &[u8; 16]| block.iter_mut().zip(key).for_each(|(b, k)| *b ^= k);
    add(block, &keys[10]);
    for round in (0..10).rev() {
        // Inverse ShiftRows (row r moves right by r) and inverse SubBytes.
        let old = *block;
        for column in 0..4 {
            for row in 0..4 {
                block[column * 4 + row] = SBOXES.1[old[((column + 4 - row) % 4) * 4 + row] as usize];
            }
        }
        add(block, &keys[round]);
        if round > 0 {
            for column in block.chunks_exact_mut(4) {
                let [a, b, c, d] = [column[0], column[1], column[2], column[3]];
                let [a, b, c, d] = [a as usize, b as usize, c as usize, d as usize];
                let [by9, by11, by13, by14] = &INVERSE_MIX;
                column[0] = by14[a] ^ by11[b] ^ by13[c] ^ by9[d];
                column[1] = by9[a] ^ by14[b] ^ by11[c] ^ by13[d];
                column[2] = by13[a] ^ by9[b] ^ by14[c] ^ by11[d];
                column[3] = by11[a] ^ by13[b] ^ by9[c] ^ by14[d];
            }
        }
    }
}

/// Decrypt a whole segment in place (AES-128-CBC with PKCS#7 padding, as `METHOD=AES-128`
/// segments are) and cut the padding off.
pub fn decrypt_aes128(key: &[u8; 16], iv: &[u8; 16], data: &mut Vec<u8>) -> Result<(), String> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err(format!("an encrypted segment of {} bytes (not whole AES blocks)", data.len()));
    }
    let keys = expand_key(key);
    let mut previous = *iv;
    for chunk in data.chunks_exact_mut(16) {
        let block: &mut [u8; 16] = chunk.try_into().unwrap();
        let cipher = *block;
        decrypt_block(&keys, block);
        block.iter_mut().zip(previous).for_each(|(b, p)| *b ^= p);
        previous = cipher;
    }
    let padding = *data.last().unwrap() as usize;
    if padding == 0 || padding > 16 || data[data.len() - padding..].iter().any(|&b| b as usize != padding) {
        return Err("wrong key or damaged segment (bad padding after decryption)".to_string());
    }
    data.truncate(data.len() - padding);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:3\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"avc1.4d401e,mp4a.40.2\",RESOLUTION=640x360,NAME=\"360p\",FRAME-RATE=25.000\n\
        https://hls.example.tv/stream/aaa.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2700000,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720,NAME=\"720p\",FRAME-RATE=25.000\n\
        mid/bbb.m3u8?token=1\n\
        #EXT-X-STREAM-INF:BANDWIDTH=4700000,CODECS=\"avc1.4d4028,mp4a.40.2\",RESOLUTION=1920x1080,NAME=\"1080p\",FRAME-RATE=25.000\n\
        /abs/ccc.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=9000000,CODECS=\"hvc1.2.4.L150.B0,mp4a.40.2\",RESOLUTION=3840x2160,FRAME-RATE=50.000\n\
        uhd.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=7000000,CODECS=\"avc1.640033,mp4a.40.2\",RESOLUTION=3840x2160\n\
        uhd-avc.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=96000,CODECS=\"mp4a.40.2\"\n\
        audio.m3u8\n";

    #[test]
    fn master_playlist_and_variant_choice() {
        let Playlist::Master(master) = parse(MASTER, "https://cdn.example.tv/live/ch1/master.m3u8?x=1").unwrap() else { panic!("not a master") };
        assert_eq!(master.variants.len(), 6);
        let urls: Vec<&str> = master.variants.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(urls[..3], ["https://hls.example.tv/stream/aaa.m3u8", "https://cdn.example.tv/live/ch1/mid/bbb.m3u8?token=1", "https://cdn.example.tv/abs/ccc.m3u8"]);
        let top = &master.variants[2];
        assert_eq!((top.bandwidth, top.width, top.height, top.frame_rate, top.name.as_str(), top.codecs.as_str()), (4_700_000, 1920, 1080, 25.0, "1080p", "avc1.4d4028,mp4a.40.2"));
        assert!(top.is_h264() && !master.variants[3].is_h264() && master.variants[5].is_audio_only());
        // 1080p H.264: not the HEVC or the 4K H.264 variant, not the audio-only one.
        assert_eq!(master.pick(&Limits::default()).unwrap().name, "1080p");
        assert_eq!(master.pick(&Limits { max_height: 720, ..Limits::default() }).unwrap().name, "720p");
        assert_eq!(master.pick(&Limits { max_bandwidth: 1_000_000, ..Limits::default() }).unwrap().name, "360p");
        // Nothing fits: the smallest H.264 variant rather than nothing.
        assert_eq!(master.pick(&Limits { max_height: 100, ..Limits::default() }).unwrap().name, "360p");
        let hevc_only = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"hev1.1.6.L93.B0\"\nx.m3u8\n";
        let Playlist::Master(hevc) = parse(hevc_only, "http://h/").unwrap() else { panic!() };
        assert_eq!(hevc.pick(&Limits::default()), None);
        // No CODECS at all: taken as H.264.
        let plain = "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1280000\nlow.m3u8\n";
        let Playlist::Master(plain) = parse(plain, "http://h/a/b.m3u8").unwrap() else { panic!() };
        assert_eq!(plain.pick(&Limits::default()).unwrap().url, "http://h/a/low.m3u8");
    }

    #[test]
    fn renditions_and_separate_audio() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English, stereo\",LANGUAGE=\"en\",DEFAULT=NO,URI=\"a/en.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Polski\",LANGUAGE=\"pl\",DEFAULT=YES,URI=\"a/pl.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub\",NAME=\"English\",URI=\"s/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.64001f\",RESOLUTION=1280x720,AUDIO=\"aud\",SUBTITLES=\"sub\"\n\
            v/720.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"avc1.64001e,mp4a.40.2\",RESOLUTION=640x360\n\
            v/360.m3u8\n";
        let Playlist::Master(master) = parse(text, "https://h/x/master.m3u8").unwrap() else { panic!() };
        assert_eq!(master.renditions.len(), 3);
        assert_eq!(master.renditions[0].name, "English, stereo");
        let audio = master.separate_audio(&master.variants[0]).unwrap();
        assert_eq!((audio.language.as_str(), audio.url.as_deref()), ("pl", Some("https://h/x/a/pl.m3u8")));
        assert_eq!(master.separate_audio(&master.variants[1]), None);
    }

    fn live(first: u64, count: u64) -> Media {
        let mut text = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:{first}\n#EXT-X-TARGETDURATION:4\n");
        for n in first..first + count {
            text.push_str(&format!("#EXTINF:3.840,\nhttps://h/hls/1108/high/{n}.ts\n"));
        }
        let Playlist::Media(media) = parse(&text, "https://h/stream/token.m3u8").unwrap() else { panic!() };
        media
    }

    #[test]
    fn media_playlist_tags() {
        let text = "\u{feff}#EXTM3U\r\n#EXT-X-TARGETDURATION:10\r\n#EXT-X-MEDIA-SEQUENCE:7\r\n#EXT-X-DISCONTINUITY-SEQUENCE:2\r\n#EXT-X-PLAYLIST-TYPE:VOD\r\n\
            #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\r\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example/k1\",IV=0x000102030405060708090A0B0C0D0E0F\r\n\
            #EXT-X-PROGRAM-DATE-TIME:2026-11-14T18:00:00.000Z\r\n\
            #EXTINF:9.009,first\r\n#EXT-X-BYTERANGE:1000@720\r\nmedia.mp4\r\n\
            #EXTINF:8,\r\n#EXT-X-BYTERANGE:500\r\nmedia.mp4\r\n\
            # a comment\r\n\
            #EXT-X-DISCONTINUITY\r\n#EXT-X-KEY:METHOD=AES-128,URI=\"k2\"\r\n\
            #EXTINF:4.5,\r\n../other/seg.ts?a=b\r\n\
            #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\r\n#EXTINF:1,\r\nlast.ts\r\n#EXT-X-ENDLIST\r\n";
        let Playlist::Media(media) = parse(text, "https://h/a/b/list.m3u8").unwrap() else { panic!() };
        assert_eq!((media.target_duration, media.media_sequence, media.discontinuity_sequence, media.ended, media.from_start), (10.0, 7, 2, true, true));
        assert_eq!(media.segments.len(), 4);
        let s = &media.segments;
        assert_eq!((s[0].sequence, s[0].url.as_str(), s[0].duration, s[0].byte_range, s[0].discontinuity), (7, "https://h/a/b/media.mp4", 9.009, Some((720, 1000)), false));
        assert_eq!(s[0].map, Some(("https://h/a/b/init.mp4".to_string(), Some((0, 720)))));
        assert_eq!(s[0].date_time.as_deref(), Some("2026-11-14T18:00:00.000Z"));
        assert_eq!(s[0].iv(), [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!((s[1].byte_range, s[1].date_time.as_deref()), (Some((1720, 500)), None));
        assert_eq!((s[2].sequence, s[2].url.as_str(), s[2].discontinuity), (9, "https://h/a/other/seg.ts?a=b", true));
        assert_eq!(s[2].encryption, Some(Encryption::Aes128 { key_url: "https://h/a/b/k2".to_string(), iv: None }));
        assert_eq!(s[2].iv(), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);
        assert_eq!(s[3].encryption, Some(Encryption::Other("SAMPLE-AES".to_string())));
        assert!((media.duration() - 22.509).abs() < 1e-9);
        assert_eq!((media.reload_after(true), media.reload_after(false)), (Duration::from_secs(10), Duration::from_secs(5)));

        assert!(parse("<html>", "http://h/").is_err());
        assert!(parse("#EXTM3U\n", "http://h/").is_err());
    }

    #[test]
    fn urls_resolve_like_a_browser() {
        let base = "https://h.example:8443/a/b/c.m3u8?token=1#frag";
        assert_eq!(resolve(base, "seg.ts"), "https://h.example:8443/a/b/seg.ts");
        assert_eq!(resolve(base, "./x/../seg.ts?v=2"), "https://h.example:8443/a/b/seg.ts?v=2");
        assert_eq!(resolve(base, "../../../up.ts"), "https://h.example:8443/up.ts");
        assert_eq!(resolve(base, "/root.ts"), "https://h.example:8443/root.ts");
        assert_eq!(resolve(base, "//other.example/x.ts"), "https://other.example/x.ts");
        assert_eq!(resolve(base, "http://plain.example/y.ts?u=https://z/"), "http://plain.example/y.ts?u=https://z/");
        assert_eq!(resolve(base, "?token=2"), "https://h.example:8443/a/b/c.m3u8?token=2");
        assert_eq!(resolve(base, "dir/"), "https://h.example:8443/a/b/dir/");
        assert_eq!(resolve("https://h.example", "seg.ts"), "https://h.example/seg.ts");
        assert_eq!(resolve("https://h.example?x=1", "seg.ts?u=a://b"), "https://h.example/seg.ts?u=a://b");
    }

    #[test]
    fn live_window_is_followed_across_reloads() {
        let mut window = Window::new();
        // The first load starts three segments from the live end.
        let fresh = window.advance(&live(100, 11));
        assert_eq!(fresh.iter().map(|s| s.sequence).collect::<Vec<_>>(), [108, 109, 110]);
        assert!(fresh.iter().all(|s| !s.discontinuity));
        // Nothing new, then one new segment, then two.
        assert!(window.advance(&live(100, 11)).is_empty());
        assert_eq!(window.advance(&live(101, 11)).iter().map(|s| s.sequence).collect::<Vec<_>>(), [111]);
        assert_eq!(window.advance(&live(103, 11)).iter().map(|s| s.url.as_str()).collect::<Vec<_>>(), ["https://h/hls/1108/high/112.ts", "https://h/hls/1108/high/113.ts"]);
        // A stale copy from another server (older than what we have): nothing, no jump.
        assert!(window.advance(&live(101, 11)).is_empty());
        assert_eq!(window.next(), Some(114));
        // Stalled for a minute: the window moved on; continue at its start, marked.
        let fresh = window.advance(&live(130, 11));
        assert_eq!((fresh.len(), fresh[0].sequence, fresh[0].discontinuity, fresh[1].discontinuity), (11, 130, true, false));
        // The stream restarted with small numbers: from the live end again, marked.
        let fresh = window.advance(&live(0, 6));
        assert_eq!((fresh.iter().map(|s| s.sequence).collect::<Vec<_>>(), fresh[0].discontinuity), (vec![3, 4, 5], true));
        assert!(window.advance(&Media::default()).is_empty());

        // A recording plays from its first segment.
        let mut ended = live(5, 4);
        ended.ended = true;
        assert_eq!(Window::new().advance(&ended).len(), 4);
        // A short live playlist: all of it.
        assert_eq!(Window::new().advance(&live(9, 2)).len(), 2);
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2).map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap()).collect()
    }

    #[test]
    fn aes_matches_the_standard_vectors() {
        assert_eq!((SBOXES.0[0x00], SBOXES.0[0x53], SBOXES.0[0xff], SBOXES.1[0xed]), (0x63, 0xed, 0x16, 0x53));
        // FIPS 197, appendix C.1.
        let key: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut block: [u8; 16] = hex("69c4e0d86a7b0430d8cdb78070b4c55a").try_into().unwrap();
        decrypt_block(&expand_key(&key), &mut block);
        assert_eq!(block.to_vec(), hex("00112233445566778899aabbccddeeff"));

        // NIST SP 800-38A, F.2.2 (CBC-AES128.Decrypt), with a block of PKCS#7 padding added:
        // E(k, 0x10 * 16 XOR last cipher block).
        let key: [u8; 16] = hex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        let iv: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let plain = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710");
        let mut data = hex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b273bed6b8e3c1743b7116e69e222295163ff1caa1681fac09120eca307586e1a7");
        // Without the padding block the last byte (0x10) is not valid padding for this text.
        let mut unpadded = data.clone();
        assert!(decrypt_aes128(&key, &iv, &mut unpadded).is_err());
        assert_eq!(unpadded, plain);
        // The padding block for this message under this key (computed with OpenSSL).
        data.extend(hex("8cb82807230e1321d3fae00d18cc2012"));
        decrypt_aes128(&key, &iv, &mut data).unwrap();
        assert_eq!(data, plain);
        assert!(decrypt_aes128(&key, &iv, &mut vec![0; 15]).is_err());
    }
}
