//! Sound: AAC decoding in software (symphonia, pure Rust) and playback on the HDMI sound card
//! through the kernel's PCM interface directly: the firmware has no alsa-lib, and one fixed
//! format (16-bit stereo at the stream's rate) needs none of it.
//!
//! The PCM structs mirror `include/uapi/sound/asound.h`; sizes, offsets and ioctl numbers were
//! read with the cross compiler for the dongle's target and are asserted below.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem::size_of;
use std::os::fd::AsRawFd;

use symphonia_codec_aac::AacDecoder;
use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder as _, DecoderOptions};
use symphonia_core::formats::Packet;

/// Decodes AAC frames (one MP4 sample each) to interleaved 16-bit stereo.
pub struct Aac {
    decoder: AacDecoder,
    buffer: Option<SampleBuffer<i16>>,
    stereo: Vec<i16>,
    pub sample_rate: u32,
}

impl Aac {
    /// `config` is the AudioSpecificConfig from the track's `esds` box.
    pub fn new(config: &[u8], sample_rate: u32) -> Result<Aac, String> {
        let mut parameters = CodecParameters::new();
        parameters.for_codec(CODEC_TYPE_AAC).with_extra_data(config.to_vec().into_boxed_slice());
        let decoder = AacDecoder::try_new(&parameters, &DecoderOptions::default()).map_err(|e| format!("aac: {e}"))?;
        // The config is authoritative for the rate (the sample entry's can be stale).
        let sample_rate = decoder.codec_params().sample_rate.unwrap_or(sample_rate);
        Ok(Aac { decoder, buffer: None, stereo: Vec::new(), sample_rate })
    }

    /// One frame in, 1024 stereo sample pairs out (2048 for HE-AAC streams).
    pub fn decode(&mut self, frame: &[u8]) -> Result<&[i16], String> {
        let packet = Packet::new_from_slice(0, 0, 0, frame);
        let decoded = self.decoder.decode(&packet).map_err(|e| format!("aac: {e}"))?;
        let channels = decoded.spec().channels.count();
        let buffer = self.buffer.get_or_insert_with(|| SampleBuffer::new(decoded.capacity() as u64, *decoded.spec()));
        buffer.copy_interleaved_ref(decoded);
        let samples = buffer.samples();
        self.stereo.clear();
        match channels {
            2 => self.stereo.extend_from_slice(samples),
            1 => self.stereo.extend(samples.iter().flat_map(|&s| [s, s])),
            // More than two: front left and right (a proper downmix can come with surround streams).
            n => self.stereo.extend(samples.chunks_exact(n).flat_map(|frame| [frame[0], frame[1]])),
        }
        Ok(&self.stereo)
    }

    /// Forget the overlap state (after a seek).
    pub fn reset(&mut self) {
        self.decoder.reset();
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Mask {
    bits: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Interval {
    min: u32,
    max: u32,
    /// openmin, openmax, integer, empty: one bit each.
    flags: u32,
}

/// `struct snd_pcm_hw_params`. `fifo_size` is an `unsigned long`.
#[repr(C)]
struct HwParams {
    flags: u32,
    masks: [Mask; 3],
    reserved_masks: [Mask; 5],
    intervals: [Interval; 12],
    reserved_intervals: [Interval; 9],
    rmask: u32,
    cmask: u32,
    info: u32,
    msbits: u32,
    rate_num: u32,
    rate_den: u32,
    fifo_size: usize,
    reserved: [u8; 64],
}

/// `struct snd_pcm_sw_params`. The frame counts are `unsigned long`.
#[repr(C)]
struct SwParams {
    tstamp_mode: i32,
    period_step: u32,
    sleep_min: u32,
    avail_min: usize,
    xfer_align: usize,
    start_threshold: usize,
    stop_threshold: usize,
    silence_threshold: usize,
    silence_size: usize,
    boundary: usize,
    proto: u32,
    tstamp_type: u32,
    reserved: [u8; 56],
}

const fn ioc(direction: u32, number: u8, size: usize) -> u32 {
    direction << 30 | (size as u32) << 16 | (b'A' as u32) << 8 | number as u32
}
const IOCTL_HW_PARAMS: u32 = ioc(3, 0x11, size_of::<HwParams>());
const IOCTL_SW_PARAMS: u32 = ioc(3, 0x13, size_of::<SwParams>());
const IOCTL_DELAY: u32 = ioc(2, 0x21, size_of::<isize>());
const IOCTL_PAUSE: u32 = ioc(1, 0x45, 4);
const IOCTL_PREPARE: u32 = ioc(0, 0x40, 0);
const IOCTL_DROP: u32 = ioc(0, 0x43, 0);
const IOCTL_DRAIN: u32 = ioc(0, 0x44, 0);

#[cfg(target_arch = "arm")]
const _: () = {
    assert!(size_of::<HwParams>() == 604);
    assert!(std::mem::offset_of!(HwParams, intervals) == 260);
    assert!(std::mem::offset_of!(HwParams, rmask) == 512);
    assert!(std::mem::offset_of!(HwParams, fifo_size) == 536);
    assert!(size_of::<SwParams>() == 104);
    assert!(std::mem::offset_of!(SwParams, avail_min) == 12);
    assert!(std::mem::offset_of!(SwParams, start_threshold) == 20);
    assert!(std::mem::offset_of!(SwParams, boundary) == 36);
    assert!(IOCTL_HW_PARAMS == 0xc25c_4111);
    assert!(IOCTL_SW_PARAMS == 0xc068_4113);
    assert!(IOCTL_DELAY == 0x8004_4121);
    assert!(IOCTL_PAUSE == 0x4004_4145);
    assert!(IOCTL_PREPARE == 0x4140 && IOCTL_DROP == 0x4143 && IOCTL_DRAIN == 0x4144);
};

// Parameter numbers: masks 0..=2, intervals 8..=19.
const MASK_ACCESS: usize = 0;
const MASK_FORMAT: usize = 1;
const MASK_SUBFORMAT: usize = 2;
const INTERVAL_CHANNELS: usize = 10 - 8;
const INTERVAL_RATE: usize = 11 - 8;
const INTERVAL_PERIOD_SIZE: usize = 13 - 8;
const INTERVAL_BUFFER_SIZE: usize = 17 - 8;
const ACCESS_RW_INTERLEAVED: u32 = 3;
const FORMAT_S16_LE: u32 = 2;
const INTERVAL_INTEGER: u32 = 4;

/// Frames per wake-up and in the whole ring: 23 ms and 186 ms at 44.1 kHz. The ring is the
/// cushion against a late writer; the picture is timed against `delay`, so its size is not lag.
const PERIOD_FRAMES: u32 = 1024;
const BUFFER_FRAMES: u32 = 8192;

/// The HDMI sound card's playback stream: 16-bit stereo.
pub struct Pcm {
    file: File,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    /// Times the card ran dry and was restarted.
    pub underruns: u32,
}

impl Pcm {
    pub fn open(sample_rate: u32) -> Result<Pcm, String> {
        let path = "/dev/snd/pcmC0D0p";
        let file = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e} (is this a MEDIA=1 kernel?)"))?;
        let any = Interval { min: 0, max: u32::MAX, flags: 0 };
        let exactly = |value: u32| Interval { min: value, max: value, flags: INTERVAL_INTEGER };
        let only = |bit: u32| Mask { bits: [1 << bit, 0, 0, 0, 0, 0, 0, 0] };
        let mut hw = HwParams {
            flags: 0,
            masks: [Mask { bits: [u32::MAX; 8] }; 3],
            reserved_masks: [Mask { bits: [u32::MAX; 8] }; 5],
            intervals: [any; 12],
            reserved_intervals: [any; 9],
            rmask: u32::MAX,
            cmask: 0,
            info: u32::MAX,
            msbits: 0,
            rate_num: 0,
            rate_den: 0,
            fifo_size: 0,
            reserved: [0; 64],
        };
        hw.masks[MASK_ACCESS] = only(ACCESS_RW_INTERLEAVED);
        hw.masks[MASK_FORMAT] = only(FORMAT_S16_LE);
        hw.masks[MASK_SUBFORMAT] = only(0);
        hw.intervals[INTERVAL_CHANNELS] = exactly(2);
        hw.intervals[INTERVAL_RATE] = exactly(sample_rate);
        hw.intervals[INTERVAL_PERIOD_SIZE] = exactly(PERIOD_FRAMES);
        hw.intervals[INTERVAL_BUFFER_SIZE] = exactly(BUFFER_FRAMES);
        ioctl(&file, IOCTL_HW_PARAMS, &mut hw).map_err(|e| format!("sound card refuses 16-bit stereo at {sample_rate} Hz: {e}"))?;
        let buffer_frames = hw.intervals[INTERVAL_BUFFER_SIZE].min;

        // SAFETY: all-zero is a valid value for this plain-data kernel struct.
        let mut sw: SwParams = unsafe { std::mem::zeroed() };
        sw.period_step = 1;
        sw.avail_min = PERIOD_FRAMES as usize;
        // Start by itself once half the ring is filled; stop (underrun) when it runs empty.
        sw.start_threshold = buffer_frames as usize / 2;
        sw.stop_threshold = buffer_frames as usize;
        sw.boundary = buffer_frames as usize;
        ioctl(&file, IOCTL_SW_PARAMS, &mut sw).map_err(|e| format!("sound card software parameters: {e}"))?;
        ioctl(&file, IOCTL_PREPARE, std::ptr::null_mut::<u8>()).map_err(|e| format!("prepare the sound card: {e}"))?;
        Ok(Pcm { file, sample_rate, buffer_frames, underruns: 0 })
    }

    /// Queue interleaved stereo samples; blocks while the ring is full.
    pub fn write(&mut self, samples: &[i16]) -> Result<(), String> {
        // SAFETY: reinterpreting i16 samples as bytes; the target is little-endian like the format.
        let mut bytes = unsafe { std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), samples.len() * 2) };
        while !bytes.is_empty() {
            match self.file.write(bytes) {
                Ok(written) => bytes = &bytes[written..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // EPIPE: the ring ran empty. Restart the stream and carry on.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {
                    self.underruns += 1;
                    ioctl(&self.file, IOCTL_PREPARE, std::ptr::null_mut::<u8>()).map_err(|e| format!("restart the sound card: {e}"))?;
                }
                Err(e) => return Err(format!("write to the sound card: {e}")),
            }
        }
        Ok(())
    }

    /// Frames queued but not yet heard: the last sample written plays this much later.
    pub fn delay(&self) -> Result<u32, String> {
        let mut frames: isize = 0;
        ioctl(&self.file, IOCTL_DELAY, &mut frames).map_err(|e| format!("sound card delay: {e}"))?;
        Ok(frames.max(0) as u32)
    }

    /// Play out what is queued, then stop.
    pub fn drain(&mut self) {
        let _ = ioctl(&self.file, IOCTL_DRAIN, std::ptr::null_mut::<u8>());
    }

    /// Hold or continue the stream where it is. The queued sound stays queued; a card that
    /// cannot pause drops it instead (the picture then catches up with the sound).
    pub fn pause(&mut self, on: bool) -> Result<(), String> {
        // SAFETY: this request takes its argument by value, not as a pointer.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), IOCTL_PAUSE as _, on as libc::c_int) } == 0 {
            return Ok(());
        }
        if on { self.stop() } else { Ok(()) }
    }

    /// Stop at once and drop what is queued.
    pub fn stop(&mut self) -> Result<(), String> {
        ioctl(&self.file, IOCTL_DROP, std::ptr::null_mut::<u8>())?;
        ioctl(&self.file, IOCTL_PREPARE, std::ptr::null_mut::<u8>())
    }
}

fn ioctl<T>(file: &File, request: u32, argument: *mut T) -> Result<(), String> {
    loop {
        // SAFETY: `argument` points to the struct this request number was built from (or is unused).
        if unsafe { libc::ioctl(file.as_raw_fd(), request as _, argument) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.to_string());
        }
    }
}

/// A 16-bit stereo WAV file, for listening to decoded audio on another machine.
pub fn wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let bytes = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    out.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_describes_the_samples() {
        let file = wav(&[1, -1, 2, -2], 44100);
        assert_eq!(file.len(), 44 + 8);
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(file[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(u32::from_le_bytes(file[24..28].try_into().unwrap()), 44100);
        assert_eq!(u32::from_le_bytes(file[28..32].try_into().unwrap()), 176400);
        assert_eq!(&file[44..48], &[1, 0, 0xff, 0xff]);
    }

    #[test]
    fn a_broken_config_is_refused() {
        assert!(Aac::new(&[], 44100).is_err() || Aac::new(&[0xff], 44100).is_err());
    }
}
