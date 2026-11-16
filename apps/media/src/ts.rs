//! MPEG transport stream demultiplexer: 188-byte packets in, whole PES packets out, with their
//! timestamps. For HLS segments (live TV), Miracast (RTP carries TS) and DLNA.
//!
//! ```no_run
//! let mut demuxer = meow_media::ts::Demuxer::new();
//! let mut packets = Vec::new();
//! # let chunk: &[u8] = &[];
//! demuxer.push(chunk, &mut packets); // any amount of bytes, cut anywhere
//! for pes in packets.drain(..) {
//!     match pes.kind {
//!         meow_media::ts::Kind::H264 => { /* pes.data: one access unit, Annex B (see es.rs) */ }
//!         meow_media::ts::Kind::AacAdts => { /* pes.data: ADTS frames (es::adts_frames) */ }
//!         _ => {}
//!     }
//! }
//! ```
//!
//! What it does: finds packet sync again after garbage, follows the PAT to the first programme's
//! PMT (sections may span packets; their CRC is checked), assembles the PES packets of every
//! elementary stream of that programme, and reports loss (continuity counter) and signalled
//! discontinuities on the next packet of the stream. A PES packet with a declared length
//! (sound, usually) is handed out the moment it is complete; one without (video) when the next
//! one starts, or on `flush`. Encoders put one H.264 access unit in a PES packet, which is what
//! the rest of this crate expects; the payload is passed on untouched for every stream type, so
//! LPCM (Miracast) or anything else can be handled by the caller.

/// Bytes per transport packet.
pub const PACKET: usize = 188;
/// Timestamps (PTS, DTS) count in 1/90000 s and wrap at 2^33.
pub const CLOCK: u64 = 90_000;
const WRAP: u64 = 1 << 33;
/// A PES packet larger than this is not a picture: the stream is damaged.
const MAX_PES: usize = 8 << 20;

/// What an elementary stream carries, from its PMT entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    H264,
    Hevc,
    Mpeg2Video,
    /// AAC with ADTS headers (the HLS and Miracast form).
    AacAdts,
    /// AAC in LATM/LOAS (DVB).
    AacLatm,
    /// MPEG-1/2 layer I-III ("MP2", "MP3").
    MpegAudio,
    Ac3,
    Eac3,
    /// Linear PCM (stream type 0x83: Miracast, Blu-ray). The payload starts with the 4-byte
    /// LPCM header, then big-endian samples.
    Lpcm,
    Other,
}

impl Kind {
    /// `descriptors`: the ES_info bytes of the PMT entry (private streams say what they are there).
    pub fn from_stream_type(stream_type: u8, descriptors: &[u8]) -> Kind {
        match stream_type {
            0x01 | 0x02 => Kind::Mpeg2Video,
            0x03 | 0x04 => Kind::MpegAudio,
            0x0f => Kind::AacAdts,
            0x11 => Kind::AacLatm,
            0x1b => Kind::H264,
            0x24 => Kind::Hevc,
            0x80 | 0x83 => Kind::Lpcm,
            0x81 => Kind::Ac3,
            0x84 | 0x87 => Kind::Eac3,
            0x06 => {
                // PES private data: DVB descriptors or a registration descriptor name the codec.
                let mut found = Kind::Other;
                let mut rest = descriptors;
                while let [tag, length, tail @ ..] = rest {
                    let Some(body) = tail.get(..*length as usize) else { break };
                    found = match (*tag, body) {
                        (0x6a, _) => Kind::Ac3,
                        (0x7a, _) => Kind::Eac3,
                        (0x05, b"AC-3") => Kind::Ac3,
                        (0x05, b"EAC3") => Kind::Eac3,
                        (0x05, b"HEVC") => Kind::Hevc,
                        _ => found,
                    };
                    rest = &tail[*length as usize..];
                }
                found
            }
            _ => Kind::Other,
        }
    }

    pub fn is_video(self) -> bool {
        matches!(self, Kind::H264 | Kind::Hevc | Kind::Mpeg2Video)
    }

    pub fn is_audio(self) -> bool {
        matches!(self, Kind::AacAdts | Kind::AacLatm | Kind::MpegAudio | Kind::Ac3 | Kind::Eac3 | Kind::Lpcm)
    }

    /// A name for messages ("this channel's sound is AC-3").
    pub fn name(self) -> &'static str {
        match self {
            Kind::H264 => "H.264",
            Kind::Hevc => "HEVC",
            Kind::Mpeg2Video => "MPEG-2 video",
            Kind::AacAdts => "AAC",
            Kind::AacLatm => "AAC (LATM)",
            Kind::MpegAudio => "MPEG audio",
            Kind::Ac3 => "AC-3",
            Kind::Eac3 => "E-AC-3",
            Kind::Lpcm => "LPCM",
            Kind::Other => "unknown",
        }
    }
}

/// One elementary stream of the programme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub pid: u16,
    pub stream_type: u8,
    pub kind: Kind,
    /// The ES_info descriptors of the PMT entry (language, registration, ...).
    pub descriptors: Vec<u8>,
}

/// One PES packet: for video an access unit, for sound a few frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pes {
    pub pid: u16,
    pub kind: Kind,
    /// 0xe0.. video, 0xc0.. MPEG sound, 0xbd private (AC-3, LPCM).
    pub stream_id: u8,
    /// Presentation and decoding time in 1/90000 s, as sent (33 bits: see `Timeline`).
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    pub data: Vec<u8>,
    /// The packet starts at a random access point (the adaptation field says so; not every
    /// muxer does: look for an IDR picture to be sure).
    pub random_access: bool,
    /// Something was lost or the stream jumped before this packet (continuity error, signalled
    /// discontinuity, resync): decoders start again from the next key picture, clocks re-anchor.
    pub discontinuity: bool,
}

struct Assembly {
    pid: u16,
    kind: Kind,
    counter: Option<u8>,
    data: Vec<u8>,
    started: bool,
    random_access: bool,
    /// Reported on the next packet handed out.
    discontinuity: bool,
}

/// A PSI section being collected (PAT or PMT).
#[derive(Default)]
struct Section {
    data: Vec<u8>,
    collecting: bool,
}

/// Counters for diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub packets: u64,
    /// Bytes skipped looking for packet sync.
    pub skipped: u64,
    pub continuity_errors: u64,
    pub transport_errors: u64,
    pub bad_sections: u64,
}

pub struct Demuxer {
    carry: Vec<u8>,
    pmt_pid: Option<u16>,
    pat: Section,
    pmt: Section,
    pmt_version: Option<u8>,
    program: Option<u16>,
    pcr_pid: Option<u16>,
    pcr: Option<u64>,
    streams: Vec<Stream>,
    assemblies: Vec<Assembly>,
    /// Bumped whenever the set of streams changes.
    generation: u32,
    pub stats: Stats,
}

impl Default for Demuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Demuxer {
    pub fn new() -> Demuxer {
        Demuxer {
            carry: Vec::new(),
            pmt_pid: None,
            pat: Section::default(),
            pmt: Section::default(),
            pmt_version: None,
            program: None,
            pcr_pid: None,
            pcr: None,
            streams: Vec::new(),
            assemblies: Vec::new(),
            generation: 0,
            stats: Stats::default(),
        }
    }

    /// The elementary streams of the programme (empty until its PMT has arrived).
    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    /// Changes when `streams` does (a new PMT with other streams).
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The programme being followed (the first one of the PAT).
    pub fn program(&self) -> Option<u16> {
        self.program
    }

    /// The last programme clock reference seen, in 1/90000 s (the 27 MHz extension dropped).
    pub fn pcr(&self) -> Option<u64> {
        self.pcr
    }

    /// Forget everything but the tables: after a seek or a gap in the input the caller knows
    /// about. Unfinished PES packets are dropped; the next ones are marked `discontinuity`.
    pub fn discontinuity(&mut self) {
        self.carry.clear();
        self.pcr = None;
        for assembly in &mut self.assemblies {
            assembly.data.clear();
            assembly.started = false;
            assembly.counter = None;
            assembly.discontinuity = true;
        }
    }

    /// Feed bytes of the stream (any amount, cut anywhere); finished PES packets are appended
    /// to `out` in stream order.
    pub fn push(&mut self, mut data: &[u8], out: &mut Vec<Pes>) {
        // Finish the packet left over from the last call.
        if !self.carry.is_empty() {
            let need = PACKET - self.carry.len();
            if data.len() < need {
                self.carry.extend_from_slice(data);
                return;
            }
            let mut packet = [0u8; PACKET];
            packet[..self.carry.len()].copy_from_slice(&self.carry);
            packet[self.carry.len()..].copy_from_slice(&data[..need]);
            self.carry.clear();
            // Only bytes from a sync byte on are ever carried over.
            self.packet(&packet, out);
            data = &data[need..];
        }
        let mut at = 0;
        while at < data.len() {
            if data[at] != 0x47 {
                // Lost sync: the next 0x47 that is followed by another one a packet later.
                let found = (at..data.len()).find(|&i| data[i] == 0x47 && data.get(i + PACKET).is_none_or(|&b| b == 0x47));
                let next = found.unwrap_or(data.len());
                self.stats.skipped += (next - at) as u64;
                at = next;
                self.lost_sync();
                continue;
            }
            if at + PACKET > data.len() {
                self.carry.extend_from_slice(&data[at..]);
                return;
            }
            let packet: &[u8; PACKET] = data[at..at + PACKET].try_into().unwrap();
            self.packet(packet, out);
            at += PACKET;
        }
    }

    /// The end of the input (a file, a segment before a discontinuity): hand out the packets
    /// still being collected (the last picture has no successor to finish it).
    pub fn flush(&mut self, out: &mut Vec<Pes>) {
        self.carry.clear();
        for index in 0..self.assemblies.len() {
            self.finish(index, out);
        }
    }

    /// The end of one HLS segment when the next follows without a discontinuity: the packets
    /// being collected are handed out (segments hold whole pictures), and the next segment's
    /// continuity counters are taken as they come (muxers restart them per segment), with
    /// nothing marked as lost.
    pub fn end_of_segment(&mut self, out: &mut Vec<Pes>) {
        self.flush(out);
        for assembly in &mut self.assemblies {
            assembly.counter = None;
        }
    }

    fn lost_sync(&mut self) {
        for assembly in &mut self.assemblies {
            if assembly.started {
                assembly.data.clear();
                assembly.started = false;
            }
            assembly.counter = None;
            assembly.discontinuity = true;
        }
    }

    fn packet(&mut self, packet: &[u8; PACKET], out: &mut Vec<Pes>) {
        self.stats.packets += 1;
        let pid = (packet[1] as u16 & 0x1f) << 8 | packet[2] as u16;
        let start = packet[1] & 0x40 != 0;
        let (has_adaptation, has_payload) = (packet[3] & 0x20 != 0, packet[3] & 0x10 != 0);
        let counter = packet[3] & 0x0f;
        if packet[1] & 0x80 != 0 {
            // The demodulator could not correct this packet: its contents are not to be trusted.
            self.stats.transport_errors += 1;
            if let Some(assembly) = self.assemblies.iter_mut().find(|a| a.pid == pid) {
                assembly.data.clear();
                assembly.started = false;
                assembly.counter = None;
                assembly.discontinuity = true;
            }
            return;
        }
        let (mut payload_at, mut signalled, mut random_access) = (4, false, false);
        if has_adaptation {
            let length = packet[4] as usize;
            if length > PACKET - 5 {
                return;
            }
            if length > 0 {
                let flags = packet[5];
                signalled = flags & 0x80 != 0;
                random_access = flags & 0x40 != 0;
                if flags & 0x10 != 0 && length >= 7 && self.pcr_pid.is_none_or(|p| p == pid) {
                    let b = &packet[6..11];
                    self.pcr = Some((b[0] as u64) << 25 | (b[1] as u64) << 17 | (b[2] as u64) << 9 | (b[3] as u64) << 1 | (b[4] as u64) >> 7);
                }
            }
            payload_at = 5 + length;
        }
        let payload = if has_payload { &packet[payload_at..] } else { &[][..] };

        if pid == 0 {
            if let Some(section) = Self::section(&mut self.pat, payload, start, &mut self.stats) {
                self.read_pat(&section);
            }
            return;
        }
        if Some(pid) == self.pmt_pid {
            if let Some(section) = Self::section(&mut self.pmt, payload, start, &mut self.stats) {
                self.read_pmt(&section, out);
            }
            return;
        }
        let Some(index) = self.assemblies.iter().position(|a| a.pid == pid) else { return };
        let assembly = &mut self.assemblies[index];
        if signalled {
            // The sender says the counter and the clock jump here.
            assembly.counter = None;
            assembly.discontinuity = true;
            assembly.data.clear();
            assembly.started = false;
        }
        if !has_payload {
            return; // the counter does not advance without a payload
        }
        match assembly.counter {
            Some(last) if counter == last => return, // a duplicate packet (allowed once)
            Some(last) if counter != (last + 1) & 0x0f => {
                self.stats.continuity_errors += 1;
                assembly.data.clear();
                assembly.started = false;
                assembly.discontinuity = true;
            }
            _ => {}
        }
        assembly.counter = Some(counter);
        if start {
            self.finish(index, out);
            let assembly = &mut self.assemblies[index];
            assembly.started = true;
            assembly.random_access = random_access;
        }
        let assembly = &mut self.assemblies[index];
        if !assembly.started {
            return; // the tail of a packet whose start was never seen
        }
        assembly.data.extend_from_slice(payload);
        if assembly.data.len() > MAX_PES {
            assembly.data.clear();
            assembly.started = false;
            assembly.discontinuity = true;
            return;
        }
        // A packet that declares its length is complete without waiting for the next one.
        if assembly.data.len() >= 6 {
            let declared = (assembly.data[4] as usize) << 8 | assembly.data[5] as usize;
            if declared != 0 && assembly.data.len() >= 6 + declared {
                assembly.data.truncate(6 + declared);
                self.finish(index, out);
            }
        }
    }

    /// Hand out the PES packet collected for `assemblies[index]`, if there is one.
    fn finish(&mut self, index: usize, out: &mut Vec<Pes>) {
        let assembly = &mut self.assemblies[index];
        if !assembly.started {
            return;
        }
        assembly.started = false;
        let data = std::mem::take(&mut assembly.data);
        let Some((stream_id, pts, dts, header)) = pes_header(&data) else {
            assembly.discontinuity = true;
            return;
        };
        let mut data = data;
        data.drain(..header);
        out.push(Pes { pid: assembly.pid, kind: assembly.kind, stream_id, pts, dts, data, random_access: assembly.random_access, discontinuity: std::mem::take(&mut assembly.discontinuity) });
    }

    /// Collect a PSI section from the payloads of its PID; Some when one is complete and intact.
    fn section(state: &mut Section, payload: &[u8], start: bool, stats: &mut Stats) -> Option<Vec<u8>> {
        if start {
            let pointer = *payload.first()? as usize;
            state.data.clear();
            state.data.extend_from_slice(payload.get(1 + pointer..)?);
            state.collecting = true;
        } else if state.collecting {
            state.data.extend_from_slice(payload);
        } else {
            return None;
        }
        if state.data.len() < 3 {
            return None;
        }
        let length = 3 + ((state.data[1] as usize & 0x0f) << 8 | state.data[2] as usize);
        if state.data.len() < length {
            return None;
        }
        state.collecting = false;
        let section = state.data[..length].to_vec();
        if length < 12 || crc32(&section) != 0 {
            stats.bad_sections += 1;
            return None;
        }
        Some(section)
    }

    fn read_pat(&mut self, section: &[u8]) {
        if section[0] != 0x00 || section[5] & 1 == 0 {
            return; // not a PAT, or not yet in force
        }
        // (programme number, PID) pairs; programme 0 is the network table.
        let first = section[8..section.len() - 4].chunks_exact(4).map(|e| ((e[0] as u16) << 8 | e[1] as u16, (e[2] as u16 & 0x1f) << 8 | e[3] as u16)).find(|(number, _)| *number != 0);
        let Some((program, pid)) = first else { return };
        if self.pmt_pid != Some(pid) || self.program != Some(program) {
            self.pmt_pid = Some(pid);
            self.program = Some(program);
            self.pmt = Section::default();
            self.pmt_version = None;
        }
    }

    fn read_pmt(&mut self, section: &[u8], out: &mut Vec<Pes>) {
        if section[0] != 0x02 || section[5] & 1 == 0 {
            return;
        }
        let version = section[5] >> 1 & 0x1f;
        if self.pmt_version == Some(version) {
            return;
        }
        let body = &section[..section.len() - 4];
        let program_info = (body[10] as usize & 0x0f) << 8 | body[11] as usize;
        let mut streams = Vec::new();
        let mut at = 12 + program_info;
        while at + 5 <= body.len() {
            let stream_type = body[at];
            let pid = (body[at + 1] as u16 & 0x1f) << 8 | body[at + 2] as u16;
            let info = (body[at + 3] as usize & 0x0f) << 8 | body[at + 4] as usize;
            let Some(descriptors) = body.get(at + 5..at + 5 + info) else { break };
            streams.push(Stream { pid, stream_type, kind: Kind::from_stream_type(stream_type, descriptors), descriptors: descriptors.to_vec() });
            at += 5 + info;
        }
        self.pmt_version = Some(version);
        self.pcr_pid = Some((body[8] as u16 & 0x1f) << 8 | body[9] as u16);
        if streams == self.streams {
            return;
        }
        // Streams that stay keep what they have collected; those that went are handed out.
        for index in 0..self.assemblies.len() {
            let assembly = &self.assemblies[index];
            if !streams.iter().any(|s| s.pid == assembly.pid && s.kind == assembly.kind) {
                self.finish(index, out);
            }
        }
        let mut old = std::mem::take(&mut self.assemblies);
        for stream in &streams {
            let kept = old.iter().position(|a| a.pid == stream.pid && a.kind == stream.kind).map(|i| old.swap_remove(i));
            self.assemblies.push(kept.unwrap_or(Assembly { pid: stream.pid, kind: stream.kind, counter: None, data: Vec::new(), started: false, random_access: false, discontinuity: false }));
        }
        self.streams = streams;
        self.generation += 1;
    }
}

/// (stream id, PTS, DTS, bytes before the payload) of a PES packet.
fn pes_header(data: &[u8]) -> Option<(u8, Option<u64>, Option<u64>, usize)> {
    if data.len() < 6 || data[..3] != [0, 0, 1] {
        return None;
    }
    let stream_id = data[3];
    // Padding, private stream 2 and the control streams have no optional header.
    if matches!(stream_id, 0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff) {
        return Some((stream_id, None, None, 6));
    }
    if data.len() < 9 || data[6] & 0xc0 != 0x80 {
        return None;
    }
    let header = 9 + data[8] as usize;
    if data.len() < header {
        return None;
    }
    let stamp = |at: usize| -> Option<u64> {
        let b = data.get(at..at + 5)?;
        Some((b[0] as u64 >> 1 & 7) << 30 | (b[1] as u64) << 22 | (b[2] as u64 >> 1) << 15 | (b[3] as u64) << 7 | b[4] as u64 >> 1)
    };
    let (pts, dts) = match data[7] >> 6 {
        2 if data[8] >= 5 => (stamp(9), None),
        3 if data[8] >= 10 => (stamp(9), stamp(14)),
        _ => (None, None),
    };
    Some((stream_id, pts, dts, header))
}

/// The CRC of PSI sections (CRC-32/MPEG-2); over a section including its CRC it is 0.
pub fn crc32(data: &[u8]) -> u32 {
    data.iter().fold(!0u32, |crc, &byte| (0..8).fold(crc ^ (byte as u32) << 24, |c, _| if c & 0x8000_0000 != 0 { c << 1 ^ 0x04c1_1db7 } else { c << 1 }))
}

/// 33-bit timestamps as a time that never wraps: each stamp is taken as the value nearest to
/// the one before it (they are at most seconds apart, a wrap is 26.5 hours).
#[derive(Debug, Default, Clone)]
pub struct Timeline {
    last: Option<i64>,
}

impl Timeline {
    /// The unwrapped stamp, in 1/90000 s. The first one is returned as it is.
    pub fn unwrap(&mut self, stamp: u64) -> i64 {
        let stamp = (stamp % WRAP) as i64;
        let value = match self.last {
            None => stamp,
            Some(last) => {
                let wrap = WRAP as i64;
                let base = last - last.rem_euclid(wrap);
                // The candidate in this wrap period, the one before or the one after.
                [base + stamp - wrap, base + stamp, base + stamp + wrap].into_iter().min_by_key(|c| (c - last).abs()).unwrap()
            }
        };
        self.last = Some(value);
        value
    }

    /// Start again (after a discontinuity the next stamp is unrelated to the last).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Seconds for a difference of unwrapped stamps.
pub fn seconds(ticks: i64) -> f64 {
    ticks as f64 / CLOCK as f64
}

/// Builders for transport streams, for the tests of this module and of its users.
pub mod build {
    use super::{PACKET, crc32};

    /// Cut `payload` into packets of `pid` (the first one marked as a unit start), with the
    /// counter kept in `counter`. `random_access` sets the flag on the first packet; short
    /// payloads are padded with an adaptation field, as muxers do.
    pub fn packets(pid: u16, payload: &[u8], counter: &mut u8, random_access: bool, pcr: Option<u64>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = payload;
        let mut first = true;
        while first || !rest.is_empty() {
            let mut adaptation = Vec::new();
            if first && (random_access || pcr.is_some()) {
                adaptation.push((random_access as u8) << 6 | (pcr.is_some() as u8) << 4);
                if let Some(pcr) = pcr {
                    adaptation.extend_from_slice(&[(pcr >> 25) as u8, (pcr >> 17) as u8, (pcr >> 9) as u8, (pcr >> 1) as u8, (pcr as u8 & 1) << 7 | 0x7e, 0]);
                }
            }
            let mut room = PACKET - 4 - if adaptation.is_empty() { 0 } else { 1 + adaptation.len() };
            if rest.len() < room {
                // Stuffing: the adaptation field grows until the payload ends the packet.
                if adaptation.is_empty() && room - rest.len() == 1 {
                    room -= 1; // an adaptation field of length 0: one byte
                } else {
                    if adaptation.is_empty() {
                        adaptation.push(0);
                        room -= 2;
                    }
                    adaptation.resize(adaptation.len() + room - rest.len(), 0xff);
                    room = rest.len();
                }
            }
            let has_adaptation = room < PACKET - 4;
            out.extend_from_slice(&[0x47, (first as u8) << 6 | (pid >> 8) as u8, pid as u8, (has_adaptation as u8) << 5 | 0x10 | *counter & 0x0f]);
            if has_adaptation {
                out.push(adaptation.len() as u8);
                out.extend_from_slice(&adaptation);
            }
            out.extend_from_slice(&rest[..room]);
            rest = &rest[room..];
            *counter = (*counter + 1) & 0x0f;
            first = false;
        }
        out
    }

    /// A PSI section (table id, the id it is about, body) as a packet payload with its
    /// pointer field and CRC.
    pub fn section(table: u8, id: u16, version: u8, body: &[u8]) -> Vec<u8> {
        let length = 5 + body.len() + 4;
        let mut section = vec![table, 0xb0 | (length >> 8) as u8, length as u8, (id >> 8) as u8, id as u8, 0xc1 | version << 1, 0, 0];
        section.extend_from_slice(body);
        section.extend_from_slice(&crc32(&section).to_be_bytes());
        let mut payload = vec![0];
        payload.extend_from_slice(&section);
        payload
    }

    /// A PAT with one programme.
    pub fn pat(program: u16, pmt_pid: u16) -> Vec<u8> {
        section(0, 1, 0, &[(program >> 8) as u8, program as u8, 0xe0 | (pmt_pid >> 8) as u8, pmt_pid as u8])
    }

    /// A PMT: (stream type, PID, descriptors) per stream.
    pub fn pmt(program: u16, version: u8, pcr_pid: u16, streams: &[(u8, u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![0xe0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xf0, 0];
        for (stream_type, pid, descriptors) in streams {
            body.extend_from_slice(&[*stream_type, 0xe0 | (pid >> 8) as u8, *pid as u8, 0xf0 | (descriptors.len() >> 8) as u8, descriptors.len() as u8]);
            body.extend_from_slice(descriptors);
        }
        section(2, program, version, &body)
    }

    fn stamp(marker: u8, value: u64) -> [u8; 5] {
        [marker << 4 | ((value >> 30) as u8 & 7) << 1 | 1, (value >> 22) as u8, ((value >> 15) as u8 & 0x7f) << 1 | 1, (value >> 7) as u8, (value as u8 & 0x7f) << 1 | 1]
    }

    /// A PES packet. `bounded`: declare the length (sound) or leave it 0 (video).
    pub fn pes(stream_id: u8, pts: Option<u64>, dts: Option<u64>, payload: &[u8], bounded: bool) -> Vec<u8> {
        let mut header = Vec::new();
        match (pts, dts) {
            (Some(pts), Some(dts)) => {
                header.extend_from_slice(&stamp(3, pts));
                header.extend_from_slice(&stamp(1, dts));
            }
            (Some(pts), None) => header.extend_from_slice(&stamp(2, pts)),
            _ => {}
        }
        let flags = match (pts, dts) {
            (Some(_), Some(_)) => 0xc0,
            (Some(_), None) => 0x80,
            _ => 0,
        };
        let length = if bounded { 3 + header.len() + payload.len() } else { 0 };
        let mut out = vec![0, 0, 1, stream_id, (length >> 8) as u8, length as u8, 0x80, flags, header.len() as u8];
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::build::{packets, pat, pes, pmt};
    use super::*;

    const PMT: u16 = 0x1001;
    const VIDEO: u16 = 0x100;
    const AUDIO: u16 = 0x101;

    /// PAT, PMT, then `count` pictures (each with one sound packet), as one byte stream.
    fn stream(count: u64, counters: &mut [u8; 4]) -> Vec<u8> {
        let mut ts = packets(0, &pat(1, PMT), &mut counters[0], false, None);
        ts.extend(packets(PMT, &pmt(1, 0, VIDEO, &[(0x1b, VIDEO, &[]), (0x0f, AUDIO, &[0x0a, 4, b'p', b'o', b'l', 0])]), &mut counters[1], false, None));
        for n in 0..count {
            let picture: Vec<u8> = [0, 0, 0, 1, 9, 0xf0, 0, 0, 1, if n == 0 { 0x65 } else { 0x41 }].into_iter().chain((0..400 + n * 50).map(|i| (i + n) as u8)).collect();
            let pts = 900_000 + n * 3600;
            ts.extend(packets(VIDEO, &pes(0xe0, Some(pts + 7200), Some(pts), &picture, false), &mut counters[2], n == 0, Some(pts)));
            let sound: Vec<u8> = (0..300).map(|i| (i * 7 + n) as u8).collect();
            ts.extend(packets(AUDIO, &pes(0xc0, Some(pts), None, &sound, true), &mut counters[3], false, None));
        }
        ts
    }

    #[test]
    fn tables_streams_and_packets_come_out_whole() {
        let ts = stream(5, &mut [0; 4]);
        assert_eq!(ts.len() % PACKET, 0);
        let mut demuxer = Demuxer::new();
        let mut out = Vec::new();
        demuxer.push(&ts, &mut out);
        assert_eq!(demuxer.program(), Some(1));
        let kinds: Vec<(u16, Kind)> = demuxer.streams().iter().map(|s| (s.pid, s.kind)).collect();
        assert_eq!(kinds, [(VIDEO, Kind::H264), (AUDIO, Kind::AacAdts)]);
        assert_eq!(demuxer.streams()[1].descriptors, [0x0a, 4, b'p', b'o', b'l', 0]);
        assert_eq!(demuxer.generation(), 1);
        // Sound packets declare their length and come at once; the last picture waits for flush.
        assert_eq!(out.iter().filter(|p| p.kind == Kind::AacAdts).count(), 5);
        assert_eq!(out.iter().filter(|p| p.kind == Kind::H264).count(), 4);
        demuxer.flush(&mut out);
        let video: Vec<&Pes> = out.iter().filter(|p| p.kind == Kind::H264).collect();
        assert_eq!(video.len(), 5);
        for (n, picture) in video.iter().enumerate() {
            let n = n as u64;
            assert_eq!((picture.pts, picture.dts), (Some(900_000 + n * 3600 + 7200), Some(900_000 + n * 3600)));
            assert_eq!(picture.data.len(), 10 + 400 + n as usize * 50);
            assert_eq!(picture.data[..6], [0, 0, 0, 1, 9, 0xf0]);
            assert_eq!((picture.random_access, picture.discontinuity, picture.stream_id), (n == 0, false, 0xe0));
        }
        let sound: Vec<&Pes> = out.iter().filter(|p| p.kind == Kind::AacAdts).collect();
        assert_eq!((sound[2].pts, sound[2].dts, sound[2].data.len()), (Some(900_000 + 7200), None, 300));
        assert_eq!(sound[2].data[1], 7 + 2);
        assert_eq!(demuxer.pcr(), Some(900_000 + 4 * 3600));
        assert_eq!(demuxer.stats, Stats { packets: ts.len() as u64 / 188, ..Stats::default() });
    }

    #[test]
    fn input_can_be_cut_anywhere() {
        let ts = stream(6, &mut [0; 4]);
        let mut whole = Vec::new();
        let mut demuxer = Demuxer::new();
        demuxer.push(&ts, &mut whole);
        demuxer.flush(&mut whole);
        for size in [1, 7, 187, 188, 189, 1000] {
            let mut pieces = Vec::new();
            let mut demuxer = Demuxer::new();
            for chunk in ts.chunks(size) {
                demuxer.push(chunk, &mut pieces);
            }
            demuxer.flush(&mut pieces);
            assert_eq!(pieces, whole, "chunks of {size}");
        }
    }

    #[test]
    fn garbage_is_skipped_and_marks_a_discontinuity() {
        let mut counters = [0; 4];
        let mut ts = vec![0x12, 0x47, 0x00, 0x99]; // noise, with a false sync byte in it
        ts.extend(stream(2, &mut counters));
        let cut = ts.len();
        ts.extend_from_slice(&[0xaa; 100]);
        ts.extend(stream(2, &mut counters));
        let mut demuxer = Demuxer::new();
        let mut out = Vec::new();
        demuxer.push(&ts, &mut out);
        demuxer.flush(&mut out);
        assert_eq!(demuxer.stats.skipped, 104);
        let video: Vec<&Pes> = out.iter().filter(|p| p.kind == Kind::H264).collect();
        // The picture cut off by the noise is lost; the next one says so.
        assert_eq!(video.len(), 3);
        assert_eq!(video.iter().map(|p| p.discontinuity).collect::<Vec<_>>(), [false, true, false]);
        let _ = cut;
    }

    #[test]
    fn a_lost_packet_drops_the_picture_and_is_reported() {
        let ts = stream(4, &mut [0; 4]);
        // Drop the second transport packet of the second picture.
        let mut video_packets = 0;
        let mut damaged = Vec::new();
        let mut second_start = 0;
        for packet in ts.chunks(PACKET) {
            let pid = (packet[1] as u16 & 0x1f) << 8 | packet[2] as u16;
            if pid == VIDEO {
                if packet[1] & 0x40 != 0 {
                    second_start += 1;
                    video_packets = 0;
                }
                video_packets += 1;
                if second_start == 2 && video_packets == 2 {
                    continue;
                }
            }
            damaged.extend_from_slice(packet);
        }
        assert_eq!(damaged.len(), ts.len() - PACKET);
        let mut demuxer = Demuxer::new();
        let mut out = Vec::new();
        demuxer.push(&damaged, &mut out);
        demuxer.flush(&mut out);
        let video: Vec<&Pes> = out.iter().filter(|p| p.kind == Kind::H264).collect();
        assert_eq!(video.iter().map(|p| (p.pts.unwrap() - 907_200) / 3600).collect::<Vec<_>>(), [0, 2, 3]);
        assert_eq!(video.iter().map(|p| p.discontinuity).collect::<Vec<_>>(), [false, true, false]);
        assert_eq!(demuxer.stats.continuity_errors, 1);
        // Sound was not touched.
        assert_eq!(out.iter().filter(|p| p.kind == Kind::AacAdts && !p.discontinuity).count(), 4);
    }

    #[test]
    fn duplicates_are_dropped_and_signalled_discontinuities_followed() {
        let mut counter = 5;
        let mut demuxer = Demuxer::new();
        let mut out = Vec::new();
        let mut tables = [0u8; 2];
        demuxer.push(&packets(0, &pat(7, PMT), &mut tables[0], false, None), &mut out);
        demuxer.push(&packets(PMT, &pmt(7, 0, AUDIO, &[(0x0f, AUDIO, &[])]), &mut tables[1], false, None), &mut out);
        let first = packets(AUDIO, &pes(0xc0, Some(1000), None, &[1; 50], true), &mut counter, false, None);
        demuxer.push(&first, &mut out);
        demuxer.push(&first, &mut out); // the same packet again: ignored
        assert_eq!(out.len(), 1);
        // The counter jumps, but the packet says so: no error, the packet is marked.
        counter = 11;
        let mut jump = packets(AUDIO, &pes(0xc0, Some(5000), None, &[2; 50], true), &mut counter, true, None);
        jump[5] |= 0x80;
        demuxer.push(&jump, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!((out[1].discontinuity, out[1].pts, demuxer.stats.continuity_errors), (true, Some(5000), 0));
        // A new segment may restart the counter: not a loss.
        demuxer.end_of_segment(&mut out);
        counter = 3;
        demuxer.push(&packets(AUDIO, &pes(0xc0, Some(7000), None, &[4; 50], true), &mut counter, false, None), &mut out);
        assert_eq!((out.len(), out[2].discontinuity, demuxer.stats.continuity_errors), (3, false, 0));
        out.pop();
        // The caller's own discontinuity marks the next packet too.
        demuxer.discontinuity();
        demuxer.push(&packets(AUDIO, &pes(0xc0, Some(9000), None, &[3; 50], true), &mut counter, false, None), &mut out);
        assert!(out[2].discontinuity);
    }

    #[test]
    fn a_new_pmt_changes_the_streams_and_a_damaged_one_is_ignored() {
        let mut counters = [0u8; 3];
        let mut demuxer = Demuxer::new();
        let mut out = Vec::new();
        demuxer.push(&packets(0, &pat(1, PMT), &mut counters[0], false, None), &mut out);
        // A PMT long enough to span two packets (a long descriptor).
        let long = [0x05; 250];
        let mut descriptor = vec![0x80, 250];
        descriptor.extend_from_slice(&long);
        let table = pmt(1, 0, VIDEO, &[(0x1b, VIDEO, &descriptor), (0x06, AUDIO, &[0x6a, 1, 0])]);
        let wire = packets(PMT, &table, &mut counters[1], false, None);
        assert_eq!(wire.len(), 2 * PACKET);
        demuxer.push(&wire, &mut out);
        assert_eq!(demuxer.streams().iter().map(|s| s.kind).collect::<Vec<_>>(), [Kind::H264, Kind::Ac3]);
        demuxer.push(&packets(VIDEO, &pes(0xe0, Some(1), None, &[9; 20], false), &mut counters[2], false, None), &mut out);
        // A corrupted new version: the CRC fails, nothing changes.
        let mut broken = packets(PMT, &pmt(1, 1, VIDEO, &[(0x24, VIDEO, &[])]), &mut counters[1], false, None);
        let inside = broken.len() - 6; // the payload is at the end of the packet, after the stuffing
        broken[inside] ^= 0xff;
        demuxer.push(&broken, &mut out);
        assert_eq!((demuxer.generation(), demuxer.stats.bad_sections), (1, 1));
        // A real new version without the old video PID: its unfinished picture is handed out.
        demuxer.push(&packets(PMT, &pmt(1, 2, 0x200, &[(0x24, 0x200, &[]), (0x83, 0x201, &[])]), &mut counters[1], false, None), &mut out);
        assert_eq!(demuxer.streams().iter().map(|s| (s.pid, s.kind)).collect::<Vec<_>>(), [(0x200, Kind::Hevc), (0x201, Kind::Lpcm)]);
        assert_eq!(demuxer.generation(), 2);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].pid, out[0].data.len()), (VIDEO, 20));
    }

    #[test]
    fn stream_types_map_to_kinds() {
        assert_eq!(Kind::from_stream_type(0x03, &[]), Kind::MpegAudio);
        assert_eq!(Kind::from_stream_type(0x06, &[0x0a, 4, b'e', b'n', b'g', 0, 0x7a, 2, 0, 0]), Kind::Eac3);
        assert_eq!(Kind::from_stream_type(0x06, &[0x05, 4, b'A', b'C', b'-', b'3']), Kind::Ac3);
        assert_eq!(Kind::from_stream_type(0x06, &[0x56, 5, 1, 2, 3, 4, 5]), Kind::Other); // teletext
        assert_eq!(Kind::from_stream_type(0x06, &[0x05, 9, 1]), Kind::Other); // truncated descriptor
        assert!(Kind::Hevc.is_video() && Kind::Lpcm.is_audio() && !Kind::Other.is_audio());
        assert_eq!(Kind::Eac3.name(), "E-AC-3");
    }

    #[test]
    fn stamps_unwrap_across_the_33_bit_wrap() {
        let mut timeline = Timeline::default();
        let near_end = WRAP - 1000;
        assert_eq!(timeline.unwrap(near_end), near_end as i64);
        assert_eq!(timeline.unwrap(500), WRAP as i64 + 500); // wrapped forwards
        assert_eq!(timeline.unwrap(WRAP - 200), WRAP as i64 - 200); // a B-frame from before the wrap
        assert_eq!(timeline.unwrap(90_000), WRAP as i64 + 90_000);
        assert_eq!(seconds(timeline.unwrap(180_000) - timeline.unwrap(90_000)), 1.0);
        timeline.reset();
        assert_eq!(timeline.unwrap(42), 42);
        // The CRC of a section including its CRC is zero.
        let table = pat(1, 0x100);
        assert_eq!(crc32(&table[1..]), 0);
        assert_eq!(crc32(b"123456789"), 0x0376_e6e7);
    }
}
