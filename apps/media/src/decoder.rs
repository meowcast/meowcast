//! Drives the hardware H.264 decoder (rkvdec) through V4L2's stateless interface: one request
//! per picture carrying the parameter controls and the slice data; decoded NV12 pictures stay
//! in the driver's buffers and are handed on as dmabufs, never copied.
//!
//! NOT YET RUN ON HARDWARE (14 Nov 2026): written against the 7.3.4 driver source and UAPI
//! headers. `youtube decode` is the first test.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use crate::h264::{NAL_IDR, NAL_SLICE, Picture, Pps, Sps};
use crate::v4l2::*;

/// Room for one coded picture. 1080p60 at YouTube's bitrates peaks well under 1 MiB per frame.
const INPUT_BUFFER: u32 = 2 << 20;
/// Decoded pictures beyond the references: one being decoded, two at the display (on screen
/// and queued for the next vertical blank) and a reorder queue.
const EXTRA_PICTURES: u32 = 7;

fn ioctl<T>(fd: RawFd, request: u32, argument: *mut T, what: &str) -> Result<(), String> {
    loop {
        // SAFETY: `argument` points to the struct this request number was built from.
        if unsafe { libc::ioctl(fd, request as _, argument) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("{what}: {error}"));
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string()
}

fn open(path: &str) -> Option<File> {
    OpenOptions::new().read(true).write(true).open(path).ok()
}

/// The decoder's video node and the media node that owns its requests.
fn find_devices() -> Result<(File, File, String), String> {
    let video = (0..16).find_map(|n| {
        let file = open(&format!("/dev/video{n}"))?;
        // SAFETY: all-zero is a valid value for these plain-data kernel structs.
        let mut capability: Capability = unsafe { std::mem::zeroed() };
        ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP, &mut capability, "QUERYCAP").ok()?;
        let wanted = CAP_VIDEO_M2M_MPLANE | CAP_STREAMING;
        (text(&capability.driver) == "rkvdec" && capability.device_caps & wanted == wanted).then(|| (file, text(&capability.card)))
    });
    let (video, card) = video.ok_or("no rkvdec video device (is this a MEDIA=1 kernel?)")?;
    let media = (0..8).find_map(|n| {
        let file = open(&format!("/dev/media{n}"))?;
        // SAFETY: as above.
        let mut info: MediaDeviceInfo = unsafe { std::mem::zeroed() };
        ioctl(file.as_raw_fd(), MEDIA_IOC_DEVICE_INFO, &mut info, "DEVICE_INFO").ok()?;
        (text(&info.driver) == "rkvdec").then_some(file)
    });
    Ok((video, media.ok_or("no rkvdec media device")?, card))
}

struct Input {
    data: *mut u8,
    length: usize,
    request: OwnedFd,
}

/// A decoded picture's buffer.
pub struct Output {
    /// The NV12 picture as a dmabuf, for a KMS plane.
    pub dmabuf: OwnedFd,
    /// The picture it holds while the application has it (None: queued in the driver).
    held: Option<u64>,
}

// SAFETY: the input mappings are owned by the decoder alone; nothing else aliases them.
unsafe impl Send for Decoder {}

pub struct Decoder {
    video: File,
    media: File,
    pub card: String,
    inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    /// Layout of every decoded picture: coded size, bytes per line, bytes in total.
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub picture_bytes: u32,
    scratch: Vec<u8>,
}

/// One decoded picture: `Decoder::outputs[index]` holds it until `release(index)`.
#[derive(Debug, Clone, Copy)]
pub struct Decoded {
    pub index: usize,
    /// The hardware reported an error; the picture may be damaged.
    pub damaged: bool,
    pub took: Duration,
}

impl Decoder {
    pub fn new(sps: &Sps) -> Result<Decoder, String> {
        let (video, media, card) = find_devices()?;
        let fd = video.as_raw_fd();
        let (width, height) = sps.coded_size();

        // SAFETY: all-zero is a valid value for these plain-data kernel structs (here and below).
        let mut format: Format = unsafe { std::mem::zeroed() };
        format.kind = BUF_TYPE_OUTPUT_MPLANE;
        format.pix_mp.width = width;
        format.pix_mp.height = height;
        format.pix_mp.pixelformat = PIX_FMT_H264_SLICE;
        format.pix_mp.field = FIELD_NONE;
        format.pix_mp.num_planes = 1;
        let mut plane = format.pix_mp.plane_fmt;
        plane[0].sizeimage = INPUT_BUFFER;
        format.pix_mp.plane_fmt = plane;
        ioctl(fd, VIDIOC_S_FMT, &mut format, "set the coded format")?;
        let input_size = { format.pix_mp.plane_fmt }[0].sizeimage;

        // The driver derives the decoded format (size, bit depth) from the SPS.
        let mut sequence = sps_control(sps);
        set_controls(fd, None, &mut [control(CID_H264_SPS, &mut *sequence)])?;

        let mut format: Format = unsafe { std::mem::zeroed() };
        format.kind = BUF_TYPE_CAPTURE_MPLANE;
        ioctl(fd, VIDIOC_G_FMT, &mut format, "get the decoded format")?;
        format.pix_mp.pixelformat = PIX_FMT_NV12;
        ioctl(fd, VIDIOC_S_FMT, &mut format, "set the decoded format")?;
        if { format.pix_mp.pixelformat } != PIX_FMT_NV12 {
            return Err("the decoder does not offer NV12 for this stream".to_string());
        }
        let plane = { format.pix_mp.plane_fmt }[0];
        let (width, height) = (format.pix_mp.width, format.pix_mp.height);

        let input_count = request_buffers(fd, BUF_TYPE_OUTPUT_MPLANE, 1)?;
        let mut inputs = Vec::new();
        for index in 0..input_count {
            let (buffer, plane) = query(fd, BUF_TYPE_OUTPUT_MPLANE, index)?;
            let _ = buffer;
            // SAFETY: mapping a buffer the driver just described; unmapped in Drop.
            let data = unsafe { libc::mmap(std::ptr::null_mut(), plane.length as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, plane.m as libc::off_t) };
            if data == libc::MAP_FAILED {
                return Err(format!("map the input buffer: {}", io::Error::last_os_error()));
            }
            let mut request: i32 = -1;
            ioctl(media.as_raw_fd(), MEDIA_IOC_REQUEST_ALLOC, &mut request, "allocate a request")?;
            // SAFETY: the kernel returned a new descriptor that nothing else owns.
            inputs.push(Input { data: data.cast(), length: plane.length.min(input_size) as usize, request: unsafe { OwnedFd::from_raw_fd(request) } });
        }

        let output_count = request_buffers(fd, BUF_TYPE_CAPTURE_MPLANE, (sps.max_num_ref_frames as u32).max(1) + EXTRA_PICTURES)?;
        let mut outputs = Vec::new();
        for index in 0..output_count {
            let mut export: ExportBuffer = unsafe { std::mem::zeroed() };
            export.kind = BUF_TYPE_CAPTURE_MPLANE;
            export.index = index;
            export.flags = (libc::O_CLOEXEC | libc::O_RDONLY) as u32;
            ioctl(fd, VIDIOC_EXPBUF, &mut export, "export a picture buffer")?;
            // SAFETY: as above.
            outputs.push(Output { dmabuf: unsafe { OwnedFd::from_raw_fd(export.fd) }, held: None });
        }
        let mut decoder = Decoder { video, media, card, inputs, outputs, width, height, stride: plane.bytesperline, picture_bytes: plane.sizeimage, scratch: Vec::new() };
        for index in 0..decoder.outputs.len() {
            decoder.queue_output(index)?;
        }
        for kind in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
            let mut kind = kind;
            ioctl(fd, VIDIOC_STREAMON, &mut kind, "start streaming")?;
        }
        Ok(decoder)
    }

    fn queue_output(&mut self, index: usize) -> Result<(), String> {
        let mut plane: Plane = unsafe { std::mem::zeroed() };
        let mut buffer: Buffer = unsafe { std::mem::zeroed() };
        buffer.index = index as u32;
        buffer.kind = BUF_TYPE_CAPTURE_MPLANE;
        buffer.memory = MEMORY_MMAP;
        buffer.m = &mut plane as *mut Plane as usize;
        buffer.length = 1;
        ioctl(self.video.as_raw_fd(), VIDIOC_QBUF, &mut buffer, "queue a picture buffer")?;
        self.outputs[index].held = None;
        Ok(())
    }

    /// Pictures the driver can decode into right now.
    pub fn free_outputs(&self) -> usize {
        self.outputs.iter().filter(|o| o.held.is_none()).count()
    }

    /// Give a picture's buffer back to the driver, once it is neither shown nor a reference.
    pub fn release(&mut self, index: usize) -> Result<(), String> {
        if self.outputs[index].held.is_some() { self.queue_output(index) } else { Ok(()) }
    }

    /// The picture each held buffer contains.
    pub fn held(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
        self.outputs.iter().enumerate().filter_map(|(index, o)| Some((index, o.held?)))
    }

    /// Decode one picture and wait for it. `sample` is the MP4 sample (length-prefixed NAL
    /// units); `picture` comes from `h264::Tracker::begin` with a key that is a multiple of
    /// 1000 (the kernel keeps buffer timestamps in microseconds).
    pub fn decode(&mut self, sample: &[u8], nal_length_size: usize, sps: &Sps, pps: &Pps, picture: &Picture, key: u64) -> Result<Decoded, String> {
        if self.free_outputs() == 0 {
            return Err("no free picture buffer (release some first)".to_string());
        }
        let started = Instant::now();
        // The slices of the picture, each behind an Annex B start code.
        self.scratch.clear();
        for nal in crate::h264::nal_units(sample, nal_length_size) {
            let nal = nal?;
            if matches!(nal[0] & 0x1f, NAL_SLICE | NAL_IDR) {
                self.scratch.extend_from_slice(&[0, 0, 1]);
                self.scratch.extend_from_slice(nal);
            }
        }
        let input = &self.inputs[0];
        if self.scratch.len() > input.length {
            return Err(format!("coded picture of {} bytes exceeds the input buffer", self.scratch.len()));
        }
        // SAFETY: the mapping is `input.length` bytes and nothing else references it.
        unsafe { std::ptr::copy_nonoverlapping(self.scratch.as_ptr(), input.data, self.scratch.len()) };
        let (fd, request) = (self.video.as_raw_fd(), input.request.as_raw_fd());

        let mut sequence = sps_control(sps);
        let (mut parameters, mut matrix) = pps_controls(pps, sps);
        let mut decode = decode_params(picture);
        set_controls(fd, Some(request), &mut [
            control(CID_H264_SPS, &mut *sequence),
            control(CID_H264_PPS, &mut parameters),
            control(CID_H264_SCALING_MATRIX, &mut matrix),
            control(CID_H264_DECODE_PARAMS, &mut decode),
        ])?;

        let mut plane: Plane = unsafe { std::mem::zeroed() };
        plane.bytesused = self.scratch.len() as u32;
        let mut buffer: Buffer = unsafe { std::mem::zeroed() };
        buffer.index = 0;
        buffer.kind = BUF_TYPE_OUTPUT_MPLANE;
        buffer.memory = MEMORY_MMAP;
        buffer.m = &mut plane as *mut Plane as usize;
        buffer.length = 1;
        buffer.flags = BUF_FLAG_REQUEST_FD;
        buffer.request_fd = request;
        buffer.tv_sec = (key / 1_000_000_000) as i64;
        buffer.tv_usec = (key % 1_000_000_000 / 1000) as i64;
        ioctl(fd, VIDIOC_QBUF, &mut buffer, "queue the coded picture")?;
        ioctl(request, MEDIA_REQUEST_IOC_QUEUE, std::ptr::null_mut::<u8>(), "queue the request")?;

        let mut poll = libc::pollfd { fd: request, events: libc::POLLPRI, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut poll, 1, 2000) };
        if ready <= 0 {
            return Err("the decoder did not finish a picture within 2 s".to_string());
        }
        let (_, _) = self.dequeue(BUF_TYPE_OUTPUT_MPLANE)?;
        let (done, _) = self.dequeue(BUF_TYPE_CAPTURE_MPLANE)?;
        ioctl(request, MEDIA_REQUEST_IOC_REINIT, std::ptr::null_mut::<u8>(), "reuse the request")?;
        let index = done.index as usize;
        let got = done.tv_sec as u64 * 1_000_000_000 + done.tv_usec as u64 * 1000;
        if got != key {
            return Err(format!("decoder returned picture {got}, expected {key}"));
        }
        self.outputs[index].held = Some(key);
        Ok(Decoded { index, damaged: done.flags & BUF_FLAG_ERROR != 0, took: started.elapsed() })
    }

    fn dequeue(&mut self, kind: u32) -> Result<(Buffer, Plane), String> {
        let mut plane: Plane = unsafe { std::mem::zeroed() };
        let mut buffer: Buffer = unsafe { std::mem::zeroed() };
        buffer.kind = kind;
        buffer.memory = MEMORY_MMAP;
        buffer.m = &mut plane as *mut Plane as usize;
        buffer.length = 1;
        ioctl(self.video.as_raw_fd(), VIDIOC_DQBUF, &mut buffer, "dequeue a buffer")?;
        Ok((buffer, plane))
    }

    /// Copy a held picture out (NV12, `picture_bytes` long): for tests, never for playback.
    pub fn read_picture(&self, index: usize) -> Result<Vec<u8>, String> {
        let length = self.picture_bytes as usize;
        // SAFETY: a read-only mapping of the exported buffer, unmapped before returning.
        unsafe {
            let data = libc::mmap(std::ptr::null_mut(), length, libc::PROT_READ, libc::MAP_SHARED, self.outputs[index].dmabuf.as_raw_fd(), 0);
            if data == libc::MAP_FAILED {
                return Err(format!("map picture {index}: {}", io::Error::last_os_error()));
            }
            let copy = std::slice::from_raw_parts(data.cast::<u8>(), length).to_vec();
            libc::munmap(data, length);
            Ok(copy)
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        for kind in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
            let mut kind = kind;
            let _ = ioctl(self.video.as_raw_fd(), VIDIOC_STREAMOFF, &mut kind, "stop streaming");
        }
        for input in &self.inputs {
            // SAFETY: the mapping made in `new`.
            unsafe { libc::munmap(input.data.cast(), input.length) };
        }
        let _ = &self.media;
    }
}

fn control<T>(id: u32, value: &mut T) -> ExtControl {
    ExtControl { id, size: size_of::<T>() as u32, reserved2: 0, ptr: value as *mut T as usize as u64 }
}

fn set_controls(fd: RawFd, request: Option<RawFd>, controls: &mut [ExtControl]) -> Result<(), String> {
    let mut set = ExtControls {
        which: if request.is_some() { CTRL_WHICH_REQUEST_VAL } else { 0 },
        count: controls.len() as u32,
        error_idx: 0,
        request_fd: request.unwrap_or(0),
        reserved: 0,
        controls: controls.as_mut_ptr(),
    };
    ioctl(fd, VIDIOC_S_EXT_CTRLS, &mut set, "set the H.264 controls").map_err(|e| format!("{e} (control {})", set.error_idx))
}

fn request_buffers(fd: RawFd, kind: u32, count: u32) -> Result<u32, String> {
    let mut request = RequestBuffers { count, kind, memory: MEMORY_MMAP, capabilities: 0, flags: 0, reserved: [0; 3] };
    ioctl(fd, VIDIOC_REQBUFS, &mut request, "allocate buffers")?;
    if request.count == 0 {
        return Err("the decoder allocated no buffers".to_string());
    }
    Ok(request.count)
}

fn query(fd: RawFd, kind: u32, index: u32) -> Result<(Buffer, Plane), String> {
    // SAFETY: all-zero is a valid value for these plain-data kernel structs.
    let mut plane: Plane = unsafe { std::mem::zeroed() };
    let mut buffer: Buffer = unsafe { std::mem::zeroed() };
    buffer.index = index;
    buffer.kind = kind;
    buffer.memory = MEMORY_MMAP;
    buffer.m = &mut plane as *mut Plane as usize;
    buffer.length = 1;
    ioctl(fd, VIDIOC_QUERYBUF, &mut buffer, "query a buffer")?;
    Ok((buffer, plane))
}
