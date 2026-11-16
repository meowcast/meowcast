//! Fragmented MP4 as YouTube serves it (DASH, one track per file): the init segment (`moov`),
//! the segment index (`sidx`) and media fragments (`moof` + `mdat`).
//!
//! Nothing here allocates per sample beyond one `Vec<Sample>` per fragment, and nothing copies
//! media data: samples are offsets into the fragment the caller downloaded.

#[derive(Debug, Clone, PartialEq)]
pub enum Codec {
    /// H.264 from an `avcC` box: parameter sets and the size of the NAL length prefix.
    Avc { sps: Vec<Vec<u8>>, pps: Vec<Vec<u8>>, nal_length_size: usize, width: u32, height: u32 },
    /// AAC from an `esds` box: the AudioSpecificConfig bytes.
    Aac { config: Vec<u8>, sample_rate: u32, channels: u32 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: u32,
    /// Units per second of every time in this track.
    pub timescale: u32,
    pub codec: Codec,
    /// The media time that plays at time zero (from the edit list): composition offsets and
    /// the AAC encoder delay shift a track's first sample away from zero.
    pub media_start: u64,
    /// `trex` defaults, used when a fragment does not say otherwise.
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
}

impl Track {
    /// A sample time of this track in seconds on the presentation timeline.
    pub fn seconds(&self, time: u64) -> f64 {
        (time as f64 - self.media_start as f64) / self.timescale as f64
    }
}

/// One entry of the segment index: a `moof` + `mdat` pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    /// Absolute byte offset in the file, and length.
    pub offset: u64,
    pub size: u32,
    /// Start and length in the index's timescale.
    pub start: u64,
    pub duration: u32,
}

#[derive(Debug, PartialEq)]
pub struct Index {
    pub timescale: u32,
    pub segments: Vec<Segment>,
}

impl Index {
    /// The segment that contains `seconds` (the last one beyond the end).
    pub fn find(&self, seconds: f64) -> usize {
        let time = (seconds.max(0.0) * self.timescale as f64) as u64;
        self.segments.partition_point(|s| s.start + s.duration as u64 <= time).min(self.segments.len().saturating_sub(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Offset in the fragment buffer, and length.
    pub offset: usize,
    pub size: usize,
    /// Decode and presentation times in the track's timescale.
    pub dts: u64,
    pub pts: u64,
    pub duration: u32,
    /// A sync sample (IDR for H.264): decoding can start here.
    pub sync: bool,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }
    fn left(&self) -> usize {
        self.data.len() - self.at
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.left() < n {
            return Err("mp4: box is shorter than its fields".to_string());
        }
        let out = &self.data[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }
    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.bytes(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().expect("2 bytes")))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().expect("4 bytes")))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into().expect("8 bytes")))
    }
}

/// The boxes directly inside `data`: (type, payload, offset of the box header in `data`).
fn boxes(data: &[u8]) -> impl Iterator<Item = Result<([u8; 4], &[u8], usize), String>> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at + 8 > data.len() {
            return None;
        }
        let start = at;
        let size32 = u32::from_be_bytes(data[at..at + 4].try_into().expect("4 bytes")) as u64;
        let kind: [u8; 4] = data[at + 4..at + 8].try_into().expect("4 bytes");
        let (header, size) = match size32 {
            1 if at + 16 <= data.len() => (16, u64::from_be_bytes(data[at + 8..at + 16].try_into().expect("8 bytes"))),
            0 => (8, (data.len() - at) as u64),
            n => (8, n),
        };
        if size < header as u64 || size > (data.len() - at) as u64 {
            at = data.len();
            return Some(Err(format!("mp4: box '{}' of {size} bytes does not fit", String::from_utf8_lossy(&kind))));
        }
        at += size as usize;
        Some(Ok((kind, &data[start + header..start + size as usize], start)))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>, String> {
    for item in boxes(data) {
        let (found, payload, _) = item?;
        if &found == kind {
            return Ok(Some(payload));
        }
    }
    Ok(None)
}

fn need<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<&'a [u8], String> {
    child(data, kind)?.ok_or_else(|| format!("mp4: no '{}' box", String::from_utf8_lossy(kind)))
}

/// Parse the init segment (`ftyp` + `moov`): the one track of the file.
pub fn parse_init(data: &[u8]) -> Result<Track, String> {
    let moov = need(data, b"moov")?;
    let trak = need(moov, b"trak")?;
    let mdia = need(trak, b"mdia")?;

    let mut tkhd = Reader::new(need(trak, b"tkhd")?);
    let version = tkhd.u8()?;
    tkhd.skip(3 + if version == 1 { 16 } else { 8 })?;
    let id = tkhd.u32()?;

    let mut mdhd = Reader::new(need(mdia, b"mdhd")?);
    let version = mdhd.u8()?;
    mdhd.skip(3 + if version == 1 { 16 } else { 8 })?;
    let timescale = mdhd.u32()?;
    if timescale == 0 {
        return Err("mp4: timescale 0".to_string());
    }

    // The first edit that plays media (media_time -1 is an empty edit).
    let mut media_start = 0;
    if let Some(elst) = child(trak, b"edts")?.map(|edts| child(edts, b"elst")).transpose()?.flatten() {
        let mut reader = Reader::new(elst);
        let version = reader.u8()?;
        reader.skip(3)?;
        for _ in 0..reader.u32()? {
            let media_time = if version == 1 {
                reader.skip(8)?;
                reader.u64()? as i64
            } else {
                reader.skip(4)?;
                reader.u32()? as i32 as i64
            };
            reader.skip(4)?;
            if media_time >= 0 {
                media_start = media_time as u64;
                break;
            }
        }
    }

    let stbl = need(need(mdia, b"minf")?, b"stbl")?;
    let stsd = need(stbl, b"stsd")?;
    let entries = stsd.get(8..).ok_or("mp4: short stsd")?;
    let (kind, entry, _) = boxes(entries).next().ok_or("mp4: empty stsd")??;
    let codec = match &kind {
        b"avc1" | b"avc3" => {
            // VisualSampleEntry: 6 reserved, 2 data ref, 16 predefined, width, height, 50 more.
            let mut reader = Reader::new(entry);
            reader.skip(24)?;
            let (width, height) = (reader.u16()? as u32, reader.u16()? as u32);
            reader.skip(50)?;
            parse_avcc(need(&entry[reader.at..], b"avcC")?, width, height)?
        }
        b"mp4a" => {
            // AudioSampleEntry: 6 reserved, 2 data ref, 8 reserved, channels, sample size, 4, rate 16.16.
            let mut reader = Reader::new(entry);
            reader.skip(16)?;
            let channels = reader.u16()? as u32;
            reader.skip(6)?;
            let rate = reader.u32()? >> 16;
            parse_esds(need(&entry[reader.at..], b"esds")?, rate, channels)?
        }
        other => return Err(format!("mp4: unsupported sample entry '{}'", String::from_utf8_lossy(other))),
    };

    let (mut default_duration, mut default_size, mut default_flags) = (0, 0, 0);
    if let Some(mvex) = child(moov, b"mvex")? {
        if let Some(trex) = child(mvex, b"trex")? {
            let mut reader = Reader::new(trex);
            reader.skip(4 + 4 + 4)?; // version/flags, track id, sample description index
            default_duration = reader.u32()?;
            default_size = reader.u32()?;
            default_flags = reader.u32()?;
        }
    }
    Ok(Track { id, timescale, codec, media_start, default_duration, default_size, default_flags })
}

pub(crate) fn parse_avcc(data: &[u8], width: u32, height: u32) -> Result<Codec, String> {
    let mut reader = Reader::new(data);
    reader.skip(4)?; // version, profile, compatibility, level
    let nal_length_size = (reader.u8()? & 3) as usize + 1;
    let sets = |count: usize, reader: &mut Reader| -> Result<Vec<Vec<u8>>, String> {
        (0..count).map(|_| reader.u16().and_then(|n| reader.bytes(n as usize)).map(<[u8]>::to_vec)).collect()
    };
    let sps_count = (reader.u8()? & 0x1f) as usize;
    let sps = sets(sps_count, &mut reader)?;
    let pps_count = reader.u8()? as usize;
    let pps = sets(pps_count, &mut reader)?;
    if sps.is_empty() || pps.is_empty() {
        return Err("mp4: avcC without parameter sets".to_string());
    }
    Ok(Codec::Avc { sps, pps, nal_length_size, width, height })
}

/// `esds`: descriptors nested as ES(3) > DecoderConfig(4) > DecoderSpecificInfo(5).
fn parse_esds(data: &[u8], rate: u32, channels: u32) -> Result<Codec, String> {
    fn descriptor<'a>(reader: &mut Reader<'a>) -> Result<(u8, &'a [u8]), String> {
        let tag = reader.u8()?;
        let mut size = 0usize;
        for _ in 0..4 {
            let byte = reader.u8()?;
            size = size << 7 | (byte & 0x7f) as usize;
            if byte & 0x80 == 0 {
                break;
            }
        }
        Ok((tag, reader.bytes(size.min(reader.left()))?))
    }
    let mut reader = Reader::new(data.get(4..).ok_or("mp4: short esds")?);
    let (tag, es) = descriptor(&mut reader)?;
    if tag != 3 {
        return Err("mp4: esds without an ES descriptor".to_string());
    }
    let mut es = Reader::new(es);
    es.skip(2)?;
    let flags = es.u8()?;
    if flags & 0x80 != 0 {
        es.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let n = es.u8()? as usize;
        es.skip(n)?;
    }
    if flags & 0x20 != 0 {
        es.skip(2)?;
    }
    let (tag, config) = descriptor(&mut es)?;
    if tag != 4 {
        return Err("mp4: esds without a decoder config".to_string());
    }
    let mut config = Reader::new(config);
    let object_type = config.u8()?;
    if object_type != 0x40 {
        return Err(format!("mp4: audio object type {object_type:#x} is not AAC"));
    }
    config.skip(12)?;
    let (tag, specific) = descriptor(&mut config)?;
    if tag != 5 || specific.len() < 2 {
        return Err("mp4: esds without an AudioSpecificConfig".to_string());
    }
    Ok(Codec::Aac { config: specific.to_vec(), sample_rate: rate, channels })
}

/// Parse the `sidx` box found in `data`, which starts at absolute file offset `data_offset`.
pub fn parse_sidx(data: &[u8], data_offset: u64) -> Result<Index, String> {
    for item in boxes(data) {
        let (kind, payload, start) = item?;
        if &kind != b"sidx" {
            continue;
        }
        let mut reader = Reader::new(payload);
        let version = reader.u8()?;
        reader.skip(3 + 4)?; // flags, reference id
        let timescale = reader.u32()?;
        let (mut time, first_offset) = if version == 0 { (reader.u32()? as u64, reader.u32()? as u64) } else { (reader.u64()?, reader.u64()?) };
        reader.skip(2)?;
        let count = reader.u16()? as usize;
        // Offsets count from the first byte after this box.
        let mut offset = data_offset + (start + 8 + payload.len()) as u64 + first_offset;
        let mut segments = Vec::with_capacity(count);
        for _ in 0..count {
            let reference = reader.u32()?;
            let duration = reader.u32()?;
            reader.skip(4)?;
            if reference >> 31 != 0 {
                return Err("mp4: nested segment index".to_string());
            }
            segments.push(Segment { offset, size: reference, start: time, duration });
            offset += reference as u64;
            time += duration as u64;
        }
        if timescale == 0 || segments.is_empty() {
            return Err("mp4: empty segment index".to_string());
        }
        return Ok(Index { timescale, segments });
    }
    Err("mp4: no 'sidx' box".to_string())
}

const TRUN_DATA_OFFSET: u32 = 0x1;
const TRUN_FIRST_FLAGS: u32 = 0x4;
const TRUN_DURATION: u32 = 0x100;
const TRUN_SIZE: u32 = 0x200;
const TRUN_FLAGS: u32 = 0x400;
const TRUN_CTS: u32 = 0x800;
/// `sample_is_non_sync_sample` in the sample flags.
const NON_SYNC: u32 = 0x10000;

/// The samples of one fragment (`moof` + `mdat`, as one segment-index entry delivers it).
pub fn parse_fragment(data: &[u8], track: &Track) -> Result<Vec<Sample>, String> {
    let mut samples = Vec::new();
    for item in boxes(data) {
        let (kind, moof, moof_start) = item?;
        if &kind != b"moof" {
            continue;
        }
        for item in boxes(moof) {
            let (kind, traf, _) = item?;
            if &kind != b"traf" {
                continue;
            }
            let mut tfhd = Reader::new(need(traf, b"tfhd")?);
            let flags = tfhd.u32()? & 0xff_ffff;
            if tfhd.u32()? != track.id {
                continue;
            }
            // Only moof-relative data offsets are supported: what DASH requires.
            let base = if flags & 0x1 != 0 { return Err("mp4: fragment with an absolute base offset".to_string()) } else { moof_start };
            if flags & 0x2 != 0 {
                tfhd.skip(4)?;
            }
            let default_duration = if flags & 0x8 != 0 { tfhd.u32()? } else { track.default_duration };
            let default_size = if flags & 0x10 != 0 { tfhd.u32()? } else { track.default_size };
            let default_flags = if flags & 0x20 != 0 { tfhd.u32()? } else { track.default_flags };

            let mut dts = match child(traf, b"tfdt")? {
                Some(tfdt) => {
                    let mut reader = Reader::new(tfdt);
                    let version = reader.u8()?;
                    reader.skip(3)?;
                    if version == 1 { reader.u64()? } else { reader.u32()? as u64 }
                }
                None => 0,
            };
            let mut next_offset = None;
            for item in boxes(traf) {
                let (kind, trun, _) = item?;
                if &kind != b"trun" {
                    continue;
                }
                let mut reader = Reader::new(trun);
                let word = reader.u32()?;
                let (version, flags) = (word >> 24, word & 0xff_ffff);
                let count = reader.u32()? as usize;
                let mut offset = match (flags & TRUN_DATA_OFFSET != 0, next_offset) {
                    (true, _) => (base as i64 + reader.u32()? as i32 as i64) as usize,
                    (false, Some(offset)) => offset,
                    (false, None) => return Err("mp4: first run without a data offset".to_string()),
                };
                let first_flags = if flags & TRUN_FIRST_FLAGS != 0 { Some(reader.u32()?) } else { None };
                samples.reserve(count.min(4096));
                for i in 0..count {
                    let duration = if flags & TRUN_DURATION != 0 { reader.u32()? } else { default_duration };
                    let size = if flags & TRUN_SIZE != 0 { reader.u32()? } else { default_size } as usize;
                    let sample_flags = match (flags & TRUN_FLAGS != 0, first_flags) {
                        (true, _) => reader.u32()?,
                        (false, Some(first)) if i == 0 => first,
                        _ => default_flags,
                    };
                    let cts = if flags & TRUN_CTS != 0 {
                        let raw = reader.u32()?;
                        if version == 0 { raw as i64 } else { raw as i32 as i64 }
                    } else {
                        0
                    };
                    if offset + size > data.len() {
                        return Err("mp4: sample outside the fragment".to_string());
                    }
                    samples.push(Sample { offset, size, dts, pts: (dts as i64 + cts).max(0) as u64, duration, sync: sample_flags & NON_SYNC == 0 });
                    offset += size;
                    dts += duration as u64;
                }
                next_offset = Some(offset);
            }
        }
    }
    if samples.is_empty() {
        return Err("mp4: fragment without samples".to_string());
    }
    Ok(samples)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn video_init() -> Vec<u8> {
        let mut tkhd = vec![0; 12];
        tkhd.extend_from_slice(&7u32.to_be_bytes());
        let mut mdhd = vec![0; 12];
        mdhd.extend_from_slice(&15360u32.to_be_bytes());
        let avcc = cat(&[&[1, 0x64, 0, 0x2a, 0xff, 0xe1, 0, 3, 0x67, 1, 2, 1, 0, 2, 0x68, 3]]);
        let mut entry = vec![0; 24];
        entry.extend_from_slice(&[0x07, 0x80, 0x04, 0x38]);
        entry.extend_from_slice(&[0; 50]);
        entry.extend_from_slice(&boxed(b"avcC", &avcc));
        let stsd = cat(&[&[0; 4], &1u32.to_be_bytes(), &boxed(b"avc1", &entry)]);
        let trex = cat(&[&[0; 4], &7u32.to_be_bytes(), &1u32.to_be_bytes(), &256u32.to_be_bytes(), &0u32.to_be_bytes(), &NON_SYNC.to_be_bytes()]);
        let stbl = boxed(b"stbl", &boxed(b"stsd", &stsd));
        let mdia = boxed(b"mdia", &cat(&[&boxed(b"mdhd", &mdhd), &boxed(b"minf", &stbl)]));
        let elst = cat(&[&[0; 4], &2u32.to_be_bytes(), &9u32.to_be_bytes(), &(-1i32).to_be_bytes(), &[0, 1, 0, 0], &9u32.to_be_bytes(), &512u32.to_be_bytes(), &[0, 1, 0, 0]]);
        let trak = boxed(b"trak", &cat(&[&boxed(b"tkhd", &tkhd), &boxed(b"edts", &boxed(b"elst", &elst)), &mdia]));
        cat(&[&boxed(b"ftyp", b"dash"), &boxed(b"moov", &cat(&[&trak, &boxed(b"mvex", &boxed(b"trex", &trex))]))])
    }

    #[test]
    fn reads_the_video_track() {
        let track = parse_init(&video_init()).unwrap();
        assert_eq!((track.id, track.timescale, track.default_duration, track.media_start), (7, 15360, 256, 512));
        assert_eq!(track.seconds(15360 + 512), 1.0);
        assert_eq!(track.codec, Codec::Avc { sps: vec![vec![0x67, 1, 2]], pps: vec![vec![0x68, 3]], nal_length_size: 4, width: 1920, height: 1080 });
    }

    #[test]
    fn reads_the_segment_index() {
        let mut sidx = vec![0; 4];
        sidx.extend_from_slice(&1u32.to_be_bytes());
        sidx.extend_from_slice(&1000u32.to_be_bytes());
        sidx.extend_from_slice(&[0; 8]); // earliest time 0, first offset 0
        sidx.extend_from_slice(&[0, 0, 0, 2]);
        for (size, duration) in [(500u32, 5000u32), (700, 4000)] {
            sidx.extend_from_slice(&size.to_be_bytes());
            sidx.extend_from_slice(&duration.to_be_bytes());
            sidx.extend_from_slice(&0x9000_0000u32.to_be_bytes());
        }
        let data = boxed(b"sidx", &sidx);
        let index = parse_sidx(&data, 742).unwrap();
        let end = 742 + data.len() as u64;
        assert_eq!(index.segments, vec![
            Segment { offset: end, size: 500, start: 0, duration: 5000 },
            Segment { offset: end + 500, size: 700, start: 5000, duration: 4000 },
        ]);
        assert_eq!((index.find(0.0), index.find(4.999), index.find(5.0), index.find(99.0)), (0, 0, 1, 1));
    }

    #[test]
    fn reads_fragment_samples_with_composition_offsets() {
        let track = parse_init(&video_init()).unwrap();
        let tfhd = cat(&[&0x02_0000u32.to_be_bytes(), &7u32.to_be_bytes()]);
        let tfdt = cat(&[&[1, 0, 0, 0], &1024u64.to_be_bytes()]);
        let flags = TRUN_DATA_OFFSET | TRUN_FIRST_FLAGS | TRUN_SIZE | TRUN_CTS;
        let mut trun = cat(&[&(1u32 << 24 | flags).to_be_bytes(), &3u32.to_be_bytes(), &[0; 4], &0u32.to_be_bytes()]);
        for (size, cts) in [(10u32, 512i32), (4, 1024), (6, -256)] {
            trun.extend_from_slice(&size.to_be_bytes());
            trun.extend_from_slice(&cts.to_be_bytes());
        }
        let moof_len = 8 + 8 + tfhd.len() + 8 + tfdt.len() + 8 + trun.len() + 8;
        let data_offset = (moof_len + 8) as u32;
        trun[8..12].copy_from_slice(&data_offset.to_be_bytes());
        let traf = boxed(b"traf", &cat(&[&boxed(b"tfhd", &tfhd), &boxed(b"tfdt", &tfdt), &boxed(b"trun", &trun)]));
        let moof = boxed(b"moof", &traf);
        assert_eq!(moof.len(), moof_len);
        let data = cat(&[&moof, &boxed(b"mdat", &[0xaa; 20])]);
        let samples = parse_fragment(&data, &track).unwrap();
        let at = moof_len + 8;
        assert_eq!(samples, vec![
            Sample { offset: at, size: 10, dts: 1024, pts: 1536, duration: 256, sync: true },
            Sample { offset: at + 10, size: 4, dts: 1280, pts: 2304, duration: 256, sync: false },
            Sample { offset: at + 14, size: 6, dts: 1536, pts: 1280, duration: 256, sync: false },
        ]);
        // A sample that would run past the data is refused, not read.
        assert!(parse_fragment(&data[..data.len() - 1], &track).is_err());
    }

    #[test]
    fn refuses_truncated_boxes() {
        let mut init = video_init();
        init.truncate(init.len() - 5);
        assert!(parse_init(&init).is_err());
        assert!(parse_sidx(&boxed(b"sidx", &[0; 6]), 0).is_err());
    }
}
