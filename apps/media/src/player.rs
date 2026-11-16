//! Plays one stream of H.264 pictures and (optionally) sound, keeps them together, and does
//! not care where they come from.
//!
//! Three threads, each blocking on the one thing it waits for; the sources are the app's
//! (a downloader behind a `VideoSource`, or a network thread pushing into a `video_queue`):
//!
//! ```text
//!  VideoSource ──access units──► video decode ──pictures──► presenter (caller's thread)
//!  AudioSource ──frames──► audio decode + sound card ──clock──┘
//! ```
//!
//! With `Sync::Audio` sound is the master clock: the sound card consumes samples at its own
//! steady rate, so the time of the sample being heard is the media time. The presenter shows,
//! at each vertical blank, the picture that belongs to that time. Pictures are decoded ahead
//! (out of presentation order) into the decoder's own buffers and shown from there without a
//! copy. `Sync::WallClock` paces pictures by their timestamps against the system clock;
//! `Sync::Arrival` shows each picture as soon as it is decoded (screen mirroring).

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::audio::{Aac, Pcm};
use crate::decoder::Decoder;
use crate::h264::{self, Tracker};
use crate::video::Display;

/// Sound and picture apart by more than this: the clock jumped (start, underrun); follow it.
const CLOCK_JUMP: f64 = 0.1;
/// `Sync::WallClock`: the next picture this far from the clock means the source's timestamps
/// jumped (a discontinuity, a new programme); the clock restarts at that picture.
const WALL_JUMP: f64 = 1.0;
/// The most pictures `VideoSetup::reorder` may hold back (the decoder has buffers for these).
pub const MAX_REORDER: usize = 4;
/// The error `play` returns when the stream's picture size changes (a phone rotated while
/// mirroring): start a new `play` with the new parameter sets.
pub const FORMAT_CHANGED: &str = "video: the picture format changed";

/// One H.264 access unit (all NAL units of one picture).
pub struct VideoUnit<'a> {
    /// Presentation time in seconds.
    pub pts: f64,
    /// NAL units, each behind a big-endian length of `VideoSetup::nal_length_size` bytes.
    pub data: &'a [u8],
    /// Pictures arrive in decoding order. A source that knows what follows (a demuxed file
    /// segment) says here that no later unit is presented before this time, and pictures
    /// before it are shown without delay (`f64::INFINITY` at the end of a segment that the
    /// next one cannot precede). None: the player holds back `VideoSetup::reorder` pictures.
    pub horizon: Option<f64>,
}

/// Where access units come from, in decoding order. `next` may block (that is how a source
/// paces the player); `Ok(None)` is the end of the stream. Called on the decoding thread.
pub trait VideoSource {
    fn next(&mut self) -> Result<Option<VideoUnit<'_>>, String>;
}

/// One AAC frame (raw, no ADTS header), or any whole number of PCM sample pairs (interleaved
/// 16-bit little-endian stereo), per `AudioSetup`.
pub struct AudioUnit<'a> {
    /// Presentation time of the first sample, in seconds.
    pub pts: f64,
    pub data: &'a [u8],
}

/// Where sound comes from; as `VideoSource`. Called on the audio thread.
pub trait AudioSource {
    fn next(&mut self) -> Result<Option<AudioUnit<'_>>, String>;
}

/// What the decoder must know before the first picture.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoSetup {
    /// Bytes per NAL length prefix in `VideoUnit::data` (4 for everything this crate converts).
    pub nal_length_size: usize,
    /// SPS and PPS NAL units (from `avcC`, or `es::annexb_parameter_sets`). The first SPS
    /// sizes the decoder; sets repeated in the stream are followed.
    pub parameter_sets: Vec<Vec<u8>>,
    /// For units without a `horizon`: how many decoded pictures to hold back so that
    /// B-pictures come out in order. 0 for streams without B-pictures (mirroring, most
    /// cameras): no added delay. 2 suits broadcast streams; at most `MAX_REORDER`.
    pub reorder: usize,
}

impl VideoSetup {
    /// From the start of an Annex B stream; None until it holds an SPS and a PPS.
    pub fn from_annexb(data: &[u8]) -> Option<VideoSetup> {
        Some(VideoSetup { nal_length_size: 4, parameter_sets: crate::es::annexb_parameter_sets(data)?, reorder: 0 })
    }

    /// From an AVCDecoderConfigurationRecord (`avcC`).
    pub fn from_avcc_record(record: &[u8]) -> Result<VideoSetup, String> {
        let (nal_length_size, parameter_sets) = crate::es::avcc_record(record)?;
        Ok(VideoSetup { nal_length_size, parameter_sets, reorder: 0 })
    }

    /// From a parsed MP4 video track.
    pub fn from_track(track: &crate::mp4::Track) -> Result<VideoSetup, String> {
        let crate::mp4::Codec::Avc { sps, pps, nal_length_size, .. } = &track.codec else { return Err("not a video track".to_string()) };
        Ok(VideoSetup { nal_length_size: *nal_length_size, parameter_sets: sps.iter().chain(pps).cloned().collect(), reorder: 0 })
    }
}

/// What the sound is.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioSetup {
    /// AAC-LC frames; `config` is the AudioSpecificConfig (`esds`, or `es::Adts::config`).
    Aac { config: Vec<u8>, sample_rate: u32 },
    /// Interleaved 16-bit little-endian stereo (what an app decodes itself, e.g. ALAC).
    Pcm { sample_rate: u32 },
}

/// What picture times are compared with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sync {
    /// The sound being heard (films, video on demand, live TV). Without sound: `WallClock`.
    Audio,
    /// The system clock, started at the first picture; restarts when timestamps jump.
    WallClock,
    /// Nothing: every picture is shown at the first vertical blank after it is decoded and
    /// sound is played as it comes. The lowest latency, for sources that are already paced
    /// in real time (AirPlay mirroring, Miracast).
    Arrival,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Pictures and sound before this time are decoded but not shown or heard (a file
    /// segment starts before the position asked for). `f64::NEG_INFINITY`: play everything.
    pub start: f64,
    pub sync: Sync,
    /// Give up with an error when no picture has appeared for this long while not paused.
    /// None for live sources that may legitimately send nothing (a still screen).
    pub stall: Option<Duration>,
}

impl Options {
    /// For live sources: play everything that arrives, never give up.
    pub fn live(sync: Sync) -> Options {
        Options { start: f64::NEG_INFINITY, sync, stall: None }
    }
}

/// How the rest of the app steers a playback in progress and follows it.
pub struct Control {
    /// Set to end playback (a new video, a seek, Stop).
    pub stop: AtomicBool,
    pub paused: AtomicBool,
    /// 0-100, applied to the samples (the HDMI link has no volume of its own).
    pub volume: AtomicU32,
    /// The first picture is on screen.
    pub started: AtomicBool,
    /// When this playback ends, leave its last picture on screen for the next one (a seek).
    pub hold_picture: AtomicBool,
    position_ms: AtomicU64,
}

impl Control {
    /// `from`: where playback will start, reported as the position until it has one.
    pub fn new(volume: u32, from: f64) -> Control {
        Control {
            stop: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(volume),
            started: AtomicBool::new(false),
            hold_picture: AtomicBool::new(false),
            position_ms: AtomicU64::new((from.max(0.0) * 1000.0) as u64),
        }
    }

    /// The media time being played, in seconds.
    pub fn position(&self) -> f64 {
        self.position_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

/// What a viewer would have noticed, and what it cost.
#[derive(Debug, Default)]
pub struct Report {
    pub shown: usize,
    /// Pictures decoded but never shown, to catch up with the sound.
    pub skipped: usize,
    /// Vertical blanks that repeated a picture while a newer one was due.
    pub held: usize,
    pub underruns: u32,
    /// Picture time minus sound time when each picture appeared, in milliseconds.
    pub offset_mean_ms: f64,
    pub offset_worst_ms: f64,
    /// From the call to the first picture on screen, and to the first sound.
    pub first_picture: Duration,
    pub first_sound: Duration,
    pub played_seconds: f64,
    /// Of `first_picture`: setting up the decoder and its pictures on the display.
    pub setup: Duration,
}

/// The media time being heard, as a line through recent measurements. Single measurements
/// are coarse (the sound card reports its position once per period, 23 ms); the line is smooth.
#[derive(Default)]
struct Clock {
    anchor: Option<(Instant, f64)>,
    running: bool,
    last_measured: f64,
}

impl Clock {
    /// A measurement: at `now`, the sample at media time `heard` was playing.
    fn update(&mut self, now: Instant, heard: f64) {
        match self.anchor {
            Some((at, media)) if self.running => {
                let predicted = media + (now - at).as_secs_f64();
                let error = heard - predicted;
                // Follow slowly (the sound card's rate is steady), or at once after a jump.
                self.anchor = Some((now, if error.abs() > CLOCK_JUMP { heard } else { predicted + 0.03 * error }));
            }
            Some(_) => {
                // Queued but not started: time stands still until the position moves.
                if heard > self.last_measured + 0.001 {
                    self.running = true;
                }
                self.anchor = Some((now, heard));
            }
            None => self.anchor = Some((now, heard)),
        }
        self.last_measured = heard;
    }

    /// Hold the time where it is (pause); it runs again when the sound moves.
    fn freeze(&mut self, now: Instant) {
        if let Some(time) = self.at(now) {
            self.anchor = Some((now, time));
            self.last_measured = time;
        }
        self.running = false;
    }

    /// Without sound: at `now` the media time is `media`, and it runs with the system clock.
    fn run_from(&mut self, now: Instant, media: f64) {
        self.anchor = Some((now, media));
        self.last_measured = media;
        self.running = true;
    }

    fn at(&self, when: Instant) -> Option<f64> {
        let (at, media) = self.anchor?;
        Some(if self.running { media + when.saturating_duration_since(at).as_secs_f64() - at.saturating_duration_since(when).as_secs_f64() } else { media })
    }
}

/// Decoding order to presentation order when the source cannot say what follows: hold back
/// `depth` pictures and let the earliest go whenever there are more.
struct Reorder {
    depth: usize,
    held: Vec<(f64, usize)>,
}

impl Reorder {
    /// A decoded picture in; the pictures that may be shown now out.
    fn push(&mut self, time: f64, index: usize, out: &mut Vec<(f64, usize)>) {
        self.held.push((time, index));
        while self.held.len() > self.depth {
            let earliest = (0..self.held.len()).min_by(|a, b| self.held[*a].0.total_cmp(&self.held[*b].0)).expect("not empty");
            out.push(self.held.swap_remove(earliest));
        }
    }
}

#[derive(Default)]
struct Shared {
    /// Decoded pictures waiting to be shown: (presentation time, buffer index).
    ready: Vec<(f64, usize)>,
    /// Buffers the presenter still needs: waiting or on screen.
    busy: Vec<usize>,
    /// No picture decoded from now on is presented before this time.
    horizon: f64,
    video_finished: bool,
    audio_finished: bool,
    /// The first picture is on screen: sound may start.
    picture_up: bool,
    failed: Option<String>,
    clock: Clock,
    underruns: u32,
    first_sound: Option<Instant>,
}

struct Link<'a> {
    shared: &'a Mutex<Shared>,
    wake: &'a Condvar,
    control: &'a Control,
    stop: &'a AtomicBool,
    sync: Sync,
}

impl Link<'_> {
    fn fail(&self, error: String) {
        let mut state = self.shared.lock().unwrap();
        state.failed.get_or_insert(error);
        self.stop.store(true, Ordering::Relaxed);
        self.wake.notify_all();
    }
}

fn decode_video(link: &Link, source: &mut dyn VideoSource, setup: &VideoSetup, mut tracker: Tracker, mut decoder: Decoder, start: f64, coded: (u32, u32)) -> Result<(), String> {
    let nal_length_size = setup.nal_length_size;
    let mut reorder = Reorder { depth: setup.reorder.min(MAX_REORDER), held: Vec::new() };
    let mut number = 0u64;
    // A stream joined in the middle (live) is undecodable until a picture that needs no other.
    let mut joined = false;
    'stream: while let Some(unit) = source.next()? {
        number += 1;
        let key = number * 1000;
        {
            let mut state = link.shared.lock().unwrap();
            loop {
                if link.stop.load(Ordering::Relaxed) {
                    break 'stream;
                }
                // A buffer is free again when it is no reference and not at the display.
                let unused: Vec<usize> = decoder.held().filter(|(index, key)| !state.busy.contains(index) && !tracker.references().iter().any(|r| r.key == *key)).map(|(index, _)| index).collect();
                for index in unused {
                    decoder.release(index)?;
                }
                if decoder.free_outputs() > 0 {
                    break;
                }
                state = link.wake.wait(state).unwrap();
            }
        }
        let mut slice = None;
        for nal in h264::nal_units(unit.data, nal_length_size) {
            let nal = nal?;
            match nal[0] & 0x1f {
                h264::NAL_SPS => {
                    // The decoder and the display plane were set up for one picture size.
                    if h264::parse_sps(nal)?.coded_size() != coded {
                        return Err(FORMAT_CHANGED.to_string());
                    }
                    tracker.parameter_set(nal)?;
                }
                h264::NAL_PPS => tracker.parameter_set(nal)?,
                h264::NAL_SLICE | h264::NAL_IDR if slice.is_none() => slice = Some(nal),
                _ => {}
            }
        }
        // Units without a picture (parameter sets or filler alone) have nothing to decode.
        let Some(slice) = slice else { continue };
        // (A source that gives a horizon has demuxed a file: it starts where it can.)
        joined |= slice[0] & 0x1f == h264::NAL_IDR || unit.horizon.is_some();
        if !joined {
            continue;
        }
        let picture = tracker.begin(slice, key)?;
        let (pps, sps) = (&tracker.pps[&picture.pps_id], &tracker.sps[&picture.sps_id]);
        let done = decoder.decode(unit.data, nal_length_size, sps, pps, &picture, key)?;
        tracker.decoded();
        let mut state = link.shared.lock().unwrap();
        // Pictures before the start position are decoded (later ones refer to them) but
        // not shown: playback starts at the position asked for, not at the segment's start.
        if unit.pts + 1e-6 >= start {
            state.busy.push(done.index);
            match unit.horizon {
                Some(_) => state.ready.push((unit.pts, done.index)),
                None => reorder.push(unit.pts, done.index, &mut state.ready),
            }
        }
        state.horizon = unit.horizon.unwrap_or(f64::INFINITY);
        link.wake.notify_all();
    }
    let mut state = link.shared.lock().unwrap();
    state.ready.append(&mut reorder.held);
    state.video_finished = true;
    state.horizon = f64::INFINITY;
    link.wake.notify_all();
    // The decoder's buffers must outlive the last picture on screen.
    while !state.busy.is_empty() && state.failed.is_none() {
        state = link.wake.wait(state).unwrap();
    }
    Ok(())
}

fn play_audio(link: &Link, source: &mut dyn AudioSource, setup: &AudioSetup, start: f64) -> Result<(), String> {
    let (mut decoder, sample_rate) = match setup {
        AudioSetup::Aac { config, sample_rate } => {
            let decoder = Aac::new(config, *sample_rate)?;
            let rate = decoder.sample_rate;
            (Some(decoder), rate)
        }
        AudioSetup::Pcm { sample_rate } => (None, *sample_rate),
    };
    let mut pcm = Pcm::open(sample_rate)?;
    let rate = pcm.sample_rate as f64;
    // Sound waits for the first picture, so neither starts without the other.
    {
        let mut state = link.shared.lock().unwrap();
        while !state.picture_up && !link.stop.load(Ordering::Relaxed) {
            state = link.wake.wait(state).unwrap();
        }
    }
    let (mut scaled, mut raw) = (Vec::new(), Vec::new());
    while let Some(unit) = source.next()? {
        if link.control.paused.load(Ordering::Relaxed) && !link.stop.load(Ordering::Relaxed) {
            pcm.pause(true)?;
            if link.sync == Sync::Audio {
                link.shared.lock().unwrap().clock.freeze(Instant::now());
            }
            while link.control.paused.load(Ordering::Relaxed) && !link.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            pcm.pause(false)?;
        }
        if link.stop.load(Ordering::Relaxed) {
            break;
        }
        let time = unit.pts;
        let samples: &[i16] = match &mut decoder {
            Some(decoder) => decoder.decode(unit.data)?,
            None => {
                raw.clear();
                raw.extend(unit.data.chunks_exact(4).flat_map(|pair| [i16::from_le_bytes([pair[0], pair[1]]), i16::from_le_bytes([pair[2], pair[3]])]));
                &raw
            }
        };
        // The segment holding the start position begins earlier: drop what precedes it.
        let lead = (((start - time) * rate).round().max(0.0) as usize * 2).min(samples.len());
        let samples = &samples[lead..];
        if samples.is_empty() {
            continue;
        }
        let volume = link.control.volume.load(Ordering::Relaxed).min(100) as u8;
        if volume < 100 {
            // The system's curve (squared: the steps at the quiet end are the ones the
            // ear resolves), the same for every app.
            let gain = meow_app::volume_gain(volume);
            scaled.clear();
            scaled.extend(samples.iter().map(|&s| ((s as i32 * gain) >> 16) as i16));
            pcm.write(&scaled)?;
        } else {
            pcm.write(samples)?;
        }
        let written_end = time + (lead / 2 + samples.len() / 2) as f64 / rate;
        let heard = written_end - pcm.delay()? as f64 / rate;
        let now = Instant::now();
        let mut state = link.shared.lock().unwrap();
        if link.sync == Sync::Audio {
            state.clock.update(now, heard);
        }
        if (state.clock.running || link.sync != Sync::Audio) && state.first_sound.is_none() {
            state.first_sound = Some(now);
        }
        state.underruns = pcm.underruns;
    }
    if !link.stop.load(Ordering::Relaxed) {
        pcm.drain();
    }
    link.shared.lock().unwrap().audio_finished = true;
    link.wake.notify_all();
    Ok(())
}

/// Play what the sources deliver until both end, `control.stop` is set, or something fails.
/// Blocks; the calling thread presents. Takes over `display`'s picture plane for the duration
/// (the UI overlay stays the app's). On return `control.stop` is set, so that the app's own
/// source threads end too.
pub fn play(
    video: &mut (dyn VideoSource + Send),
    setup: &VideoSetup,
    audio: Option<(&mut (dyn AudioSource + Send), &AudioSetup)>,
    options: &Options,
    control: &Control,
    display: &Display,
) -> Result<Report, String> {
    let stop = &control.stop;
    let called = Instant::now();
    let begin = options.start;
    // Without sound there is nothing to hear the time from.
    let sync = if audio.is_none() && options.sync == Sync::Audio { Sync::WallClock } else { options.sync };

    let mut tracker = Tracker::new();
    for nal in &setup.parameter_sets {
        tracker.parameter_set(nal)?;
    }
    let first_sps = setup.parameter_sets.iter().find(|nal| nal.first().is_some_and(|b| b & 0x1f == h264::NAL_SPS)).ok_or("no SPS")?;
    let sps = h264::parse_sps(first_sps)?;
    let decoder = Decoder::new(&sps)?;
    let dmabufs: Vec<_> = decoder.outputs.iter().map(|o| o.dmabuf.as_fd()).collect();
    if let Err(error) = display.attach(&dmabufs, (decoder.width, decoder.height), decoder.stride, sps.display_size()) {
        display.detach(false);
        return Err(error);
    }
    drop(dmabufs);
    let setup_took = called.elapsed();
    let period = 1.0 / display.refresh_hz().max(1) as f64;

    let shared = Mutex::new(Shared { horizon: begin, audio_finished: audio.is_none(), ..Default::default() });
    let wake = Condvar::new();
    let link = Link { shared: &shared, wake: &wake, control, stop, sync };
    let link = &link;
    let coded = sps.coded_size();

    std::thread::scope(|scope| {
        scope.spawn(move || {
            if let Err(error) = decode_video(link, video, setup, tracker, decoder, begin, coded) {
                link.fail(if error == FORMAT_CHANGED { error } else { format!("video: {error}") });
            }
        });
        if let Some((source, setup)) = audio {
            scope.spawn(move || {
                if let Err(error) = play_audio(link, source, setup, begin) {
                    link.fail(format!("sound: {error}"));
                }
                // Without sound there is no clock: end the picture too.
                let mut state = link.shared.lock().unwrap();
                state.audio_finished = true;
                link.wake.notify_all();
            });
        }

        let result = present(link, display, period, begin, called, options.stall).map(|report| Report { setup: setup_took, ..report });
        // The display lets go of the decoder's buffers before they can go; then every thread may.
        display.detach(control.hold_picture.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
        let mut state = shared.lock().unwrap();
        state.busy.clear();
        if let Err(error) = &result {
            state.failed.get_or_insert(error.clone());
        }
        wake.notify_all();
        drop(state);
        result
    })
}

fn present(link: &Link, display: &Display, period: f64, begin: f64, called: Instant, stall: Option<Duration>) -> Result<Report, String> {
    let mut report = Report::default();
    let (mut on_screen, mut last_flip): (Option<usize>, Option<Instant>) = (None, None);
    let (mut offsets, mut offset_sum, mut last_time) = (0usize, 0.0f64, begin);
    let mut first_time = None;
    let mut progress = Instant::now();
    let mut was_paused = false;
    loop {
        // The next vertical blank, and the media time heard then.
        let now = Instant::now();
        let blank = match last_flip {
            Some(flip) => flip + Duration::from_secs_f64(((now - flip).as_secs_f64() / period).floor() * period + period),
            None => now + Duration::from_secs_f64(period),
        };
        let paused = link.control.paused.load(Ordering::Relaxed);
        if paused {
            progress = now;
        }
        let choice = {
            let mut state = link.shared.lock().unwrap();
            if let Some(error) = &state.failed {
                return Err(error.clone());
            }
            // With sound the audio thread stops and starts the clock; otherwise it is ours.
            if link.sync == Sync::WallClock && paused != was_paused && on_screen.is_some() {
                match paused {
                    true => state.clock.freeze(now),
                    false => {
                        let frozen = state.clock.at(now).unwrap_or(last_time);
                        state.clock.run_from(now, frozen);
                    }
                }
            }
            was_paused = paused;
            if on_screen.is_some() {
                let position = state.clock.at(now).unwrap_or(last_time).max(begin);
                link.control.position_ms.store((position.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
            }
            let horizon = state.horizon;
            let mut safe: Vec<(f64, usize)> = state.ready.iter().copied().filter(|(time, _)| *time < horizon).collect();
            safe.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut wanted = state.clock.at(blank).unwrap_or(begin);
            if link.sync == Sync::WallClock && state.clock.running && !paused {
                // The source's timestamps jumped: the clock has nothing else to follow.
                if let Some((time, _)) = safe.first().filter(|(time, _)| (*time - wanted).abs() > WALL_JUMP) {
                    state.clock.run_from(blank, *time);
                    wanted = *time;
                }
            }
            let due = if on_screen.is_none() {
                // The first picture goes up as soon as it exists; sound starts after it.
                (!safe.is_empty()).then_some(0)
            } else if link.sync == Sync::Arrival {
                // The newest decoded picture, now.
                if paused { None } else { safe.len().checked_sub(1) }
            } else if state.clock.running && !paused {
                match safe.first() {
                    // Early by more than a blank: the screen keeps its picture.
                    Some((time, _)) if *time - wanted > period => None,
                    // Otherwise the next picture, or a later one if sound has run ahead.
                    Some(_) => Some(safe.iter().rposition(|(time, _)| *time <= wanted + period / 2.0).unwrap_or(0)),
                    None => None,
                }
            } else {
                None
            };
            match due {
                Some(position) => {
                    for (time, index) in &safe[..position] {
                        state.ready.retain(|r| r.1 != *index || r.0 != *time);
                        state.busy.retain(|b| b != index);
                        report.skipped += 1;
                    }
                    let (time, index) = safe[position];
                    state.ready.retain(|r| r.1 != index || r.0 != time);
                    link.wake.notify_all();
                    Some((time, index, wanted))
                }
                None => {
                    let ended = state.video_finished && state.ready.is_empty();
                    if ended && !paused && (state.audio_finished || !state.clock.running) || link.stop.load(Ordering::Relaxed) {
                        report.underruns = state.underruns;
                        report.first_sound = state.first_sound.map_or(Duration::ZERO, |t| t - called);
                        break;
                    }
                    // Nothing has moved for a long time: say what everything is waiting for.
                    if stall.is_some_and(|limit| progress.elapsed() > limit) {
                        return Err(format!(
                            "playback stalled at {last_time:.2} s: {} pictures ready ({} showable before {:.2} s), {} buffers busy, video {}, sound {}, clock {} at {:?}",
                            state.ready.len(), safe.len(), horizon, state.busy.len(),
                            if state.video_finished { "finished" } else { "decoding" },
                            if state.audio_finished { "finished" } else { "playing" },
                            if state.clock.running { "running" } else { "stopped" }, state.clock.at(now),
                        ));
                    }
                    // A picture was due but none is decoded: the viewer sees a stall.
                    if state.clock.running && safe.is_empty() && !state.video_finished && on_screen.is_some() {
                        report.held += 1;
                    }
                    None
                }
            }
        };
        let Some((time, index, wanted)) = choice else {
            // No new picture at this blank; the UI may still have something (the bar sliding
            // over a paused picture). Otherwise sleep to just past the blank.
            display.hold_for_ui(blank, true);
            if display.present(None)? {
                last_flip = Some(display.wait_shown(Duration::from_secs(2))?);
            } else {
                std::thread::sleep(blank.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
            }
            continue;
        };
        display.hold_for_ui(blank, false);
        display.present(Some(index))?;
        let flipped = display.wait_shown(Duration::from_secs(2))?;
        if on_screen.is_none() {
            report.first_picture = flipped - called;
        } else if link.sync != Sync::Arrival {
            let offset = (time - wanted) * 1000.0;
            offsets += 1;
            offset_sum += offset;
            if offset.abs() > report.offset_worst_ms.abs() {
                report.offset_worst_ms = offset;
            }
        }
        last_flip = Some(flipped);
        progress = flipped;
        last_time = time;
        first_time.get_or_insert(time);
        report.shown += 1;
        link.control.started.store(true, Ordering::Relaxed);
        let mut state = link.shared.lock().unwrap();
        if link.sync == Sync::WallClock && on_screen.is_none() {
            state.clock.run_from(flipped, time);
        }
        state.picture_up = true;
        if let Some(previous) = on_screen.replace(index) {
            state.busy.retain(|b| *b != previous);
        }
        link.wake.notify_all();
    }
    report.offset_mean_ms = offset_sum / offsets.max(1) as f64;
    // A live source has no start position: count from its first picture.
    report.played_seconds = last_time - if begin.is_finite() { begin } else { first_time.unwrap_or(last_time) };
    Ok(report)
}

// ---------------------------------------------------------------------------------------
// Push sources: for streams that arrive when they arrive (RTP, a mirroring socket, a TS
// demuxer). The app's network thread pushes; the queue is the `VideoSource`/`AudioSource`.

/// What `try_push` did with a unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pushed {
    Queued,
    /// The player is behind and the queue is full: the unit was dropped.
    Full,
    /// The playback has ended.
    Closed,
}

type Timed = (f64, Vec<u8>);

fn push(queue: &SyncSender<Timed>, pts: f64, data: Vec<u8>) -> bool {
    queue.send((pts, data)).is_ok()
}

/// As `push`, but gives up (false) soon after `stop` is set. A thread that feeds both
/// queues must use this: once playback stops nobody takes from the full queue it waits
/// on, while the player waits for the other queue to end, which only that thread can do.
fn push_until(queue: &SyncSender<Timed>, pts: f64, data: Vec<u8>, stop: &AtomicBool) -> bool {
    let mut unit = (pts, data);
    loop {
        match queue.try_send(unit) {
            Ok(()) => return true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                unit = back;
                std::thread::sleep(Duration::from_millis(4));
            }
        }
    }
}

fn try_push(queue: &SyncSender<Timed>, pts: f64, data: Vec<u8>) -> Pushed {
    match queue.try_send((pts, data)) {
        Ok(()) => Pushed::Queued,
        Err(TrySendError::Full(_)) => Pushed::Full,
        Err(TrySendError::Disconnected(_)) => Pushed::Closed,
    }
}

/// The app's end of a `video_queue`. Dropping it ends the stream (pictures already queued
/// are still shown). May be cloned to another thread.
#[derive(Clone)]
pub struct VideoPush(SyncSender<Timed>);

/// The player's end of a `video_queue`.
pub struct VideoQueue {
    queue: Receiver<Timed>,
    current: Timed,
}

/// A bounded queue of access units between the app and `play`. `depth` units is the most the
/// app may be ahead of the decoder: small for live sources (latency), a second's worth for
/// files. Use with `VideoSetup { nal_length_size: 4, .. }` when pushing Annex B.
pub fn video_queue(depth: usize) -> (VideoPush, VideoQueue) {
    let (push, queue) = sync_channel(depth.max(1));
    (VideoPush(push), VideoQueue { queue, current: (0.0, Vec::new()) })
}

impl VideoPush {
    /// Queue one access unit of length-prefixed NAL units; blocks while the queue is full.
    /// False: the playback has ended.
    pub fn push(&self, pts: f64, unit: Vec<u8>) -> bool {
        push(&self.0, pts, unit)
    }

    /// As `push` for an access unit in Annex B framing (start codes).
    pub fn push_annexb(&self, pts: f64, unit: &[u8]) -> bool {
        push(&self.0, pts, annexb(unit))
    }

    /// As `push` without blocking. A dropped picture damages those that refer to it: after
    /// `Full`, drop everything up to the next IDR picture (or ask the sender for one).
    pub fn try_push(&self, pts: f64, unit: Vec<u8>) -> Pushed {
        try_push(&self.0, pts, unit)
    }

    /// As `push`, giving up (false) soon after `stop` (the playback's `Control::stop`) is
    /// set. For a thread that fills both queues: see `push_until`.
    pub fn push_until(&self, pts: f64, unit: Vec<u8>, stop: &AtomicBool) -> bool {
        push_until(&self.0, pts, unit, stop)
    }

    pub fn try_push_annexb(&self, pts: f64, unit: &[u8]) -> Pushed {
        try_push(&self.0, pts, annexb(unit))
    }
}

fn annexb(unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(unit.len() + 16);
    crate::es::annexb_to_avcc(unit, &mut out);
    out
}

impl VideoSource for VideoQueue {
    fn next(&mut self) -> Result<Option<VideoUnit<'_>>, String> {
        let Ok(unit) = self.queue.recv() else { return Ok(None) };
        self.current = unit;
        Ok(Some(VideoUnit { pts: self.current.0, data: &self.current.1, horizon: None }))
    }
}

/// The app's end of an `audio_queue`; as `VideoPush`.
#[derive(Clone)]
pub struct AudioPush(SyncSender<Timed>);

/// The player's end of an `audio_queue`.
pub struct AudioQueue {
    queue: Receiver<Timed>,
    current: Timed,
}

/// A bounded queue of sound between the app and `play` (an AAC frame is about 21-23 ms).
pub fn audio_queue(depth: usize) -> (AudioPush, AudioQueue) {
    let (push, queue) = sync_channel(depth.max(1));
    (AudioPush(push), AudioQueue { queue, current: (0.0, Vec::new()) })
}

impl AudioPush {
    /// Queue one raw AAC frame (`AudioSetup::Aac`) or PCM bytes (`AudioSetup::Pcm`); blocks
    /// while the queue is full. False: the playback has ended.
    pub fn push(&self, pts: f64, frame: Vec<u8>) -> bool {
        push(&self.0, pts, frame)
    }

    pub fn try_push(&self, pts: f64, frame: Vec<u8>) -> Pushed {
        try_push(&self.0, pts, frame)
    }

    /// As `push`, giving up (false) soon after `stop` is set (see `VideoPush::push_until`).
    pub fn push_until(&self, pts: f64, frame: Vec<u8>, stop: &AtomicBool) -> bool {
        push_until(&self.0, pts, frame, stop)
    }

    /// Queue every AAC frame of a run of ADTS data (a PES packet); `pts` is the first
    /// frame's, the others follow it by their length.
    pub fn push_adts(&self, pts: f64, data: &[u8]) -> bool {
        let mut time = pts;
        for (header, frame) in crate::es::adts_frames(data) {
            if !push(&self.0, time, frame.to_vec()) {
                return false;
            }
            time += 1024.0 / header.sample_rate as f64;
        }
        true
    }

    /// Queue interleaved 16-bit stereo samples (`AudioSetup::Pcm`).
    pub fn push_pcm(&self, pts: f64, samples: &[i16]) -> bool {
        push(&self.0, pts, samples.iter().flat_map(|s| s.to_le_bytes()).collect())
    }
}

impl AudioSource for AudioQueue {
    fn next(&mut self) -> Result<Option<AudioUnit<'_>>, String> {
        let Ok(unit) = self.queue.recv() else { return Ok(None) };
        self.current = unit;
        Ok(Some(AudioUnit { pts: self.current.0, data: &self.current.1 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_stands_still_until_sound_moves_then_runs_smoothly() {
        let mut clock = Clock::default();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(clock.at(at(0)), None);
        // Queued, not started: the heard time does not move.
        clock.update(at(0), 10.0);
        clock.update(at(20), 10.0);
        assert_eq!(clock.at(at(500)), Some(10.0));
        // Started: measurements arrive in 23 ms steps (coarse), real time is continuous.
        for step in 1..200u64 {
            let real = step * 10;
            clock.update(at(100 + real), 10.0 + (real / 23 * 23) as f64 / 1000.0);
        }
        let now = clock.at(at(100 + 2000)).unwrap();
        // The staircase lags the truth by 11.5 ms on average; the line stays within it, smoothly.
        assert!((now - 12.0).abs() < 0.025, "{now}");
        let step = clock.at(at(100 + 2010)).unwrap() - now;
        assert!((step - 0.010).abs() < 1e-9, "{step}");
        // The time at an instant before the anchor extrapolates backwards.
        assert!(clock.at(at(100 + 1990)).unwrap() < now);
        // A jump (seek, underrun) is followed at once.
        clock.update(at(3000), 50.0);
        assert_eq!(clock.at(at(3000)), Some(50.0));
    }

    #[test]
    fn wall_clock_runs_freezes_and_resumes() {
        let mut clock = Clock::default();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        clock.run_from(at(0), 5.0);
        assert!((clock.at(at(250)).unwrap() - 5.25).abs() < 1e-9);
        clock.freeze(at(1000));
        assert!((clock.at(at(4000)).unwrap() - 6.0).abs() < 1e-9);
        clock.run_from(at(4000), 6.0);
        assert!((clock.at(at(4500)).unwrap() - 6.5).abs() < 1e-9);
    }

    #[test]
    fn reorder_holds_back_its_depth_and_releases_the_earliest() {
        // Decoding order I P B B P B B (times 0 3 1 2 6 4 5), two B-pictures deep.
        let mut reorder = Reorder { depth: 2, held: Vec::new() };
        let mut out = Vec::new();
        for (index, time) in [0.0, 3.0, 1.0, 2.0, 6.0, 4.0, 5.0].into_iter().enumerate() {
            reorder.push(time, index, &mut out);
        }
        assert_eq!(out.iter().map(|o| o.0).collect::<Vec<_>>(), [0.0, 1.0, 2.0, 3.0, 4.0]);
        assert_eq!(out[1], (1.0, 2));
        assert_eq!(reorder.held.len(), 2);
        // No B-pictures, no delay.
        let mut direct = Reorder { depth: 0, held: Vec::new() };
        let mut out = Vec::new();
        direct.push(7.0, 3, &mut out);
        assert_eq!(out, [(7.0, 3)]);
    }

    #[test]
    fn queues_carry_units_in_order_and_end_when_the_pusher_goes() {
        let (push, mut queue) = video_queue(2);
        assert!(push.push(1.0, vec![0, 0, 0, 1, 0x65]));
        assert_eq!(push.try_push_annexb(2.0, &[0, 0, 0, 1, 0x41, 9]), Pushed::Queued);
        assert_eq!(push.try_push(3.0, vec![]), Pushed::Full);
        let unit = queue.next().unwrap().unwrap();
        assert_eq!((unit.pts, unit.data, unit.horizon), (1.0, &[0, 0, 0, 1, 0x65][..], None));
        drop(push);
        let unit = queue.next().unwrap().unwrap();
        assert_eq!((unit.pts, unit.data), (2.0, &[0, 0, 0, 2, 0x41, 9][..]));
        assert!(queue.next().unwrap().is_none());

        let (push, mut queue) = audio_queue(4);
        // Two ADTS frames at 48 kHz: the second starts 1024 samples after the first.
        let adts = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x1f, 0xfc, 7, 0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc, 8, 9];
        assert!(push.push_adts(10.0, &adts));
        assert!(push.push_pcm(11.0, &[1, -2]));
        let unit = queue.next().unwrap().unwrap();
        assert_eq!((unit.pts, unit.data), (10.0, &[7][..]));
        let unit = queue.next().unwrap().unwrap();
        assert!((unit.pts - (10.0 + 1024.0 / 48000.0)).abs() < 1e-9);
        assert_eq!(unit.data, [8, 9]);
        assert_eq!(queue.next().unwrap().unwrap().data, [1, 0, 0xfe, 0xff]);
        drop(queue);
        assert!(!push.push(12.0, vec![]));
        assert_eq!(push.try_push(12.0, vec![]), Pushed::Closed);
    }

    #[test]
    fn setup_from_annexb_and_avcc() {
        let stream = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3];
        let setup = VideoSetup::from_annexb(&stream).unwrap();
        assert_eq!(setup, VideoSetup { nal_length_size: 4, parameter_sets: vec![vec![0x67, 1], vec![0x68, 2]], reorder: 0 });
        assert_eq!(VideoSetup::from_annexb(&stream[..6]), None);
        let record = [1, 100, 0, 40, 0xfd, 0xe1, 0, 2, 0x67, 1, 1, 0, 2, 0x68, 2];
        assert_eq!(VideoSetup::from_avcc_record(&record).unwrap().nal_length_size, 2);
    }
}
