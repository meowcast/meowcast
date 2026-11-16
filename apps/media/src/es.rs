//! Elementary-stream framing, for sources that are not MP4: H.264 as an Annex B byte stream
//! (MPEG-TS, RTP, AirPlay mirroring) or with an `avcC` record, AAC with ADTS headers.
//!
//! The decoder side of this crate takes H.264 access units as length-prefixed NAL units (as in
//! MP4) and AAC as raw frames plus an AudioSpecificConfig; this module converts to that.

use crate::h264::{NAL_PPS, NAL_SPS};

/// The NAL units of an Annex B byte stream (start codes `00 00 01` or `00 00 00 01`), without
/// the start codes and trailing zero bytes. Bytes before the first start code are ignored.
pub fn annexb_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = start_code(data, 0).map(|(_, after)| after);
    std::iter::from_fn(move || {
        loop {
            let begin = at?;
            let (end, next) = match start_code(data, begin) {
                Some((code, after)) => (code, Some(after)),
                None => (data.len(), None),
            };
            at = next;
            let mut nal = &data[begin..end];
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            if !nal.is_empty() {
                return Some(nal);
            }
        }
    })
}

/// The next `00 00 01` at or after `from`: (where it starts, where the NAL unit begins).
fn start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let position = data.get(from..)?.windows(3).position(|w| w == [0, 0, 1])? + from;
    Some((position, position + 3))
}

/// One access unit from Annex B to 4-byte length prefixes, appended to `out`.
pub fn annexb_to_avcc(data: &[u8], out: &mut Vec<u8>) {
    for nal in annexb_units(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
}

/// The SPS and PPS NAL units in a piece of Annex B stream, SPS first; None until it holds
/// both (a live stream repeats them before every IDR picture: wait for that).
pub fn annexb_parameter_sets(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let kind = |wanted: u8| annexb_units(data).filter(move |nal| nal[0] & 0x1f == wanted).map(<[u8]>::to_vec);
    let sets: Vec<Vec<u8>> = kind(NAL_SPS).chain(kind(NAL_PPS)).collect();
    let has = |wanted: u8| sets.iter().any(|nal| nal[0] & 0x1f == wanted);
    (has(NAL_SPS) && has(NAL_PPS)).then_some(sets)
}

/// An AVCDecoderConfigurationRecord (the payload of an `avcC` box; AirPlay mirroring sends
/// one before the pictures): (bytes per NAL length prefix, SPS and PPS NAL units).
pub fn avcc_record(data: &[u8]) -> Result<(usize, Vec<Vec<u8>>), String> {
    // The record starts with a version byte; the MP4 parser expects the box's payload.
    match crate::mp4::parse_avcc(data, 0, 0)? {
        crate::mp4::Codec::Avc { sps, pps, nal_length_size, .. } => Ok((nal_length_size, sps.into_iter().chain(pps).collect())),
        _ => Err("not an avcC record".to_string()),
    }
}

/// One ADTS header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Adts {
    pub sample_rate: u32,
    pub channels: u8,
    /// The AudioSpecificConfig saying the same, for `audio::Aac::new`.
    pub config: [u8; 2],
    /// Header bytes before the raw AAC frame, and the whole frame with its header.
    pub header_size: usize,
    pub frame_size: usize,
}

const ADTS_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// Parse the ADTS header at the start of `data`.
pub fn adts_header(data: &[u8]) -> Option<Adts> {
    let h = data.get(..7)?;
    if h[0] != 0xff || h[1] & 0xf6 != 0xf0 {
        return None;
    }
    let object_type = (h[2] >> 6) + 1;
    let rate_index = h[2] >> 2 & 0xf;
    let channels = (h[2] & 1) << 2 | h[3] >> 6;
    let frame_size = (h[3] as usize & 3) << 11 | (h[4] as usize) << 3 | h[5] as usize >> 5;
    let header_size = if h[1] & 1 == 1 { 7 } else { 9 }; // a CRC follows unless "protection absent"
    let sample_rate = *ADTS_RATES.get(rate_index as usize)?;
    if frame_size < header_size {
        return None;
    }
    // AudioSpecificConfig: 5 bits object type, 4 bits rate index, 4 bits channel configuration.
    let config = [object_type << 3 | rate_index >> 1, (rate_index & 1) << 7 | channels << 3];
    Some(Adts { sample_rate, channels, config, header_size, frame_size })
}

/// The frames of a run of ADTS data (a PES packet usually holds several): each header with
/// its raw AAC frame. Stops at the first byte that is not a complete frame.
pub fn adts_frames(data: &[u8]) -> impl Iterator<Item = (Adts, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let header = adts_header(data.get(at..)?)?;
        let frame = data.get(at + header.header_size..at + header.frame_size)?;
        at += header.frame_size;
        Some((header, frame))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annexb_splits_on_both_start_codes_and_drops_padding() {
        let stream = [9, 9, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4, 5, 6, 0, 0];
        let units: Vec<&[u8]> = annexb_units(&stream).collect();
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5, 6]]);
        assert_eq!(annexb_units(&[1, 2, 3]).count(), 0);
        assert_eq!(annexb_units(&[0, 0, 1]).count(), 0);

        let mut avcc = Vec::new();
        annexb_to_avcc(&stream, &mut avcc);
        assert_eq!(avcc, [0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 4, 0x65, 4, 5, 6]);
        let back: Vec<&[u8]> = crate::h264::nal_units(&avcc, 4).map(Result::unwrap).collect();
        assert_eq!(back, units);
    }

    #[test]
    fn parameter_sets_need_both_kinds_and_come_sps_first() {
        assert_eq!(annexb_parameter_sets(&[0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 4]), None);
        let sets = annexb_parameter_sets(&[0, 0, 1, 0x68, 3, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 4]).unwrap();
        assert_eq!(sets, [vec![0x67, 1], vec![0x68, 3]]);
    }

    #[test]
    fn avcc_record_gives_length_size_and_sets() {
        let record = [1, 100, 0, 40, 0xff, 0xe1, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 3];
        assert_eq!(avcc_record(&record).unwrap(), (4, vec![vec![0x67, 1], vec![0x68, 3]]));
        assert!(avcc_record(&record[..9]).is_err());
    }

    #[test]
    fn adts_header_and_frames() {
        // AAC-LC, 44.1 kHz, stereo, no CRC, 7 + 3 bytes; then the same at 48 kHz with 2 bytes.
        let mut data = vec![0xff, 0xf1, 0x50, 0x80, 0x01, 0x5f, 0xfc, 1, 2, 3];
        data.extend_from_slice(&[0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc, 4, 5]);
        data.extend_from_slice(&[0xff, 0xf1, 0x4c]); // a truncated third frame
        let frames: Vec<(Adts, &[u8])> = adts_frames(&data).collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, Adts { sample_rate: 44100, channels: 2, config: [0x12, 0x10], header_size: 7, frame_size: 10 });
        assert_eq!(frames[0].1, [1, 2, 3]);
        assert_eq!((frames[1].0.sample_rate, frames[1].0.config, frames[1].1), (48000, [0x11, 0x90], &[4, 5][..]));
        assert_eq!(adts_header(&[0, 0, 0, 0, 0, 0, 0]), None);
    }
}
