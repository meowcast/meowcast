//! Pictures in formats the device does not decode itself (WebP, JPEG), through the system
//! image service (`meow-image`, apps/imagesvc).
//!
//! The decoders are not in the firmware and not in any app: they are one store package that
//! the system fetches into RAM the first time an app asks for a picture to be decoded, and
//! runs in a sandbox of its own with nothing in it (no network, no storage, no screen). An
//! app hands it the bytes of a file and gets pixels back, so a picture from the internet is
//! never parsed inside the app.
//!
//! ```no_run
//! let mut decoder = meow_app::image::Decoder::new();
//! # let bytes: &[u8] = &[];
//! let picture = decoder.decode(bytes, (96, 96))?;      // at most 96x96, shape kept
//! println!("{}x{}", picture.width, picture.height);     // picture.rgba: straight RGBA
//! # Ok::<(), String>(())
//! ```
//!
//! A `Decoder` is one connection: requests on it run one after another, without a process
//! or a connection per picture. Nothing happens until the first `decode`. It fails softly:
//! on a firmware or a store without the service, or with no network to fetch it, `decode`
//! returns an error at once (after the first try) and the app shows what it can.
//!
//! In an app the connection is asked of appd over the broker, and the answer is taken by
//! whichever thread runs `App::wait`/`poll_events`: call `decode` from another thread than
//! that one (pictures do not belong on the UI thread anyway).
//!
//! Where the connection comes from:
//! 1. `MEOW_IMAGE_HELPER=/path/to/meow-image`: runs a private service (tests, the build host).
//! 2. The broker (apps): `IMAGE` -> `IMAGE ok` + a socket, or `IMAGE unavailable why`.
//! 3. appd's control socket (root shells on the device), the same request.
//!
//! Wire format on the socket (a stream; little endian; the service side is imagesvc):
//!   request   "MIMG", u32 length, u16 fit width, u16 fit height, u32 zero; the file
//!   response  u32 status (0 = a picture), u32 width, u32 height, u32 length; then the
//!             pixels (RGBA, 8 bits, not premultiplied, width*height*4 bytes) or, for any
//!             other status, a line of text saying why not
//! The service answers in the order asked. A request it cannot frame (bad magic, too long)
//! ends the connection.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MAGIC: [u8; 4] = *b"MIMG";
pub const REQUEST_HEAD: usize = 16;
pub const RESPONSE_HEAD: usize = 16;
/// The largest file the service takes.
pub const MAX_INPUT: usize = 2 << 20;
/// The largest picture it decodes, in pixels and along one side (1280x720 fits; a decoded
/// picture is at most 4 MiB). An app's UI has no use for more, and the memory is the device's.
pub const MAX_PIXELS: usize = 1 << 20;
pub const MAX_SIDE: usize = 4096;

/// appd answers the request for a connection at once; the service may still be on its way.
const ASK_TIMEOUT: Duration = Duration::from_secs(3);
/// The first answer may wait for the service to be downloaded and started.
const FIRST_ANSWER: Duration = Duration::from_secs(20);
const ANSWER: Duration = Duration::from_secs(8);
/// After a failed try the service is left alone for this long: every `decode` until then
/// fails at once, so an app without the service does not wait per picture.
const RETRY_AFTER: Duration = Duration::from_secs(30);
const CONTROL_SOCKET: &str = "/run/appd/ui.sock";

/// A decoded picture: 8-bit RGBA, not premultiplied, rows top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct Pixels {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// The header of a request for `length` bytes of file, to fit in `fit` (0 = no limit).
pub fn request_head(length: usize, fit: (u16, u16)) -> [u8; REQUEST_HEAD] {
    let mut head = [0u8; REQUEST_HEAD];
    head[..4].copy_from_slice(&MAGIC);
    head[4..8].copy_from_slice(&(length as u32).to_le_bytes());
    head[8..10].copy_from_slice(&fit.0.to_le_bytes());
    head[10..12].copy_from_slice(&fit.1.to_le_bytes());
    head
}

pub fn response_head(status: u32, width: u32, height: u32, length: u32) -> [u8; RESPONSE_HEAD] {
    let mut head = [0u8; RESPONSE_HEAD];
    for (slot, value) in head.chunks_exact_mut(4).zip([status, width, height, length]) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    head
}

// ---------------------------------------------------------------- the broker's answer

/// The answer to the one `IMAGE` request that is waiting (requests are made one at a time).
enum Answer {
    None,
    Asked,
    Socket(OwnedFd),
    Unavailable(String),
}

static ANSWER_SLOT: Mutex<Answer> = Mutex::new(Answer::None);
static ANSWERED: Condvar = Condvar::new();
/// One thread asks at a time, so an answer belongs to the request before it.
static ASKING: Mutex<()> = Mutex::new(());

/// A thread is waiting for appd's answer (`App` asks before it takes an `ERR` as one).
pub(crate) fn asking() -> bool {
    matches!(*ANSWER_SLOT.lock().unwrap_or_else(|e| e.into_inner()), Answer::Asked)
}

/// appd's answer arrived on the broker: `ok` + a socket, or `unavailable why`.
pub(crate) fn answer(rest: &str, fd: Option<OwnedFd>) {
    let mut slot = ANSWER_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if !matches!(*slot, Answer::Asked) {
        return; // nobody waits any more: the socket closes here
    }
    *slot = match (rest.split('\t').next(), fd) {
        (Some("ok"), Some(fd)) => Answer::Socket(fd),
        _ => Answer::Unavailable(rest.strip_prefix("unavailable\t").unwrap_or(rest).to_string()),
    };
    ANSWERED.notify_all();
}

fn ask_broker(broker: RawFd) -> Result<OwnedFd, String> {
    let _one = ASKING.lock().unwrap_or_else(|e| e.into_inner());
    *ANSWER_SLOT.lock().unwrap_or_else(|e| e.into_inner()) = Answer::Asked;
    let sent = crate::ipc::send(broker, "IMAGE", None);
    let deadline = Instant::now() + ASK_TIMEOUT;
    let mut slot = ANSWER_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = sent {
        *slot = Answer::None;
        return Err(format!("the system did not take the request: {e}"));
    }
    loop {
        match std::mem::replace(&mut *slot, Answer::Asked) {
            Answer::Socket(fd) => {
                *slot = Answer::None;
                return Ok(fd);
            }
            Answer::Unavailable(why) => {
                *slot = Answer::None;
                return Err(why);
            }
            Answer::None | Answer::Asked => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            *slot = Answer::None;
            return Err("the system did not answer (is another thread running App::wait?)".to_string());
        }
        slot = ANSWERED.wait_timeout(slot, left).unwrap_or_else(|e| e.into_inner()).0;
    }
}

fn ask_control() -> Result<OwnedFd, String> {
    let control = crate::ipc::connect_seqpacket(CONTROL_SOCKET).map_err(|e| format!("{CONTROL_SOCKET}: {e} (or set MEOW_IMAGE_HELPER)"))?;
    let ask = |message: &str, want: &str| -> Result<Option<OwnedFd>, String> {
        crate::ipc::send(control.as_raw_fd(), message, None).map_err(|e| format!("appd: {e}"))?;
        let deadline = Instant::now() + ASK_TIMEOUT;
        loop {
            let (reply, fd) = crate::ipc::recv_until(control.as_raw_fd(), deadline).map_err(|e| format!("appd: {e}"))?.ok_or("appd did not answer")?;
            if reply.starts_with(want) {
                return Ok(fd);
            }
            if let Some(why) = reply.strip_prefix("ERR\t") {
                return Err(why.to_string());
            }
        }
    };
    ask("HELLO\tctl", "WELCOME")?;
    ask("IMAGE", "IMAGE\tok")?.ok_or_else(|| "appd sent no socket".to_string())
}

fn helper(path: &str) -> Result<(UnixStream, std::process::Child), String> {
    use std::os::unix::process::CommandExt;
    let (mine, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
    let raw = theirs.as_raw_fd();
    let mut command = std::process::Command::new(path);
    command.args(["--serve", "3"]).stdin(std::process::Stdio::null());
    unsafe {
        command.pre_exec(move || {
            // Async-signal-safe only: the service's end becomes fd 3, inheritable.
            if raw == 3 {
                libc::fcntl(3, libc::F_SETFD, 0);
            } else if libc::dup2(raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|e| format!("{path}: {e}"))?;
    Ok((mine, child))
}

// ---------------------------------------------------------------- the connection

/// A connection to the image service, made at the first `decode`.
#[derive(Default)]
pub struct Decoder {
    stream: Option<UnixStream>,
    /// Nothing was answered on this connection yet (the service may still be starting).
    fresh: bool,
    /// The service could not be reached: not before this time again.
    retry_at: Option<Instant>,
    why: String,
    /// A private service (MEOW_IMAGE_HELPER), ended with the decoder.
    helper: Option<std::process::Child>,
}

/// What went wrong with a request.
enum Failure {
    /// The service answered: this file is not a picture it can decode.
    Refused(String),
    /// The connection broke (the service stopped, or was never there); true: by waiting
    /// too long for an answer.
    Broken(String, bool),
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// False after a failed try, until the next one is due: the caller may then prefer
    /// what needs no decoder (a PNG address before a WebP one).
    pub fn available(&self) -> bool {
        self.retry_at.is_none_or(|at| Instant::now() >= at)
    }

    fn connect(&mut self) -> Result<(), String> {
        self.end_helper();
        let stream = if let Ok(path) = std::env::var("MEOW_IMAGE_HELPER") {
            let (stream, child) = helper(&path)?;
            self.helper = Some(child);
            stream
        } else if let Some(broker) = std::env::var("MEOW_BROKER_FD").ok().and_then(|v| v.parse::<RawFd>().ok()) {
            UnixStream::from(ask_broker(broker)?)
        } else {
            UnixStream::from(ask_control()?)
        };
        let _ = stream.set_write_timeout(Some(ANSWER));
        self.stream = Some(stream);
        self.fresh = true;
        Ok(())
    }

    fn end_helper(&mut self) {
        self.stream = None;
        if let Some(mut child) = self.helper.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn exchange(&mut self, data: &[u8], fit: (u16, u16)) -> Result<Pixels, Failure> {
        let fresh = self.fresh;
        let stream = self.stream.as_mut().ok_or_else(|| Failure::Broken("not connected".to_string(), false))?;
        let broken = |e: std::io::Error| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Failure::Broken("the image service stopped".to_string(), false),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Failure::Broken("the image service did not answer in time".to_string(), true),
            _ => Failure::Broken(format!("image service: {e}"), false),
        };
        let _ = stream.set_read_timeout(Some(if fresh { FIRST_ANSWER } else { ANSWER }));
        stream.write_all(&request_head(data.len(), fit)).and_then(|()| stream.write_all(data)).map_err(broken)?;
        let mut head = [0u8; RESPONSE_HEAD];
        stream.read_exact(&mut head).map_err(broken)?;
        self.fresh = false;
        let word = |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]) as usize;
        let (status, width, height, length) = (word(0), word(4), word(8), word(12));
        if status != 0 {
            let mut why = vec![0u8; length.min(1024)];
            stream.read_exact(&mut why).map_err(broken)?;
            if length > why.len() {
                return Err(Failure::Broken("image service: an answer out of bounds".to_string(), true));
            }
            return Err(Failure::Refused(String::from_utf8_lossy(&why).into_owned()));
        }
        // What was asked for bounds what is accepted: the service is not trusted to keep
        // to the limits either.
        let limit = |side: u16| if side == 0 { MAX_SIDE } else { side as usize };
        if width == 0 || height == 0 || width > limit(fit.0) || height > limit(fit.1) || width * height > MAX_PIXELS || length != width * height * 4 {
            return Err(Failure::Broken("image service: an answer out of bounds".to_string(), true));
        }
        let mut rgba = vec![0u8; length];
        stream.read_exact(&mut rgba).map_err(broken)?;
        Ok(Pixels { width, height, rgba })
    }

    /// Decode a WebP or JPEG file. `fit`: the picture is made smaller to fit in this many
    /// pixels (its shape kept; 0 = no limit on that side), so a logo shown at 96 pixels
    /// costs 37 KB here whatever size the file has.
    pub fn decode(&mut self, data: &[u8], fit: (u16, u16)) -> Result<Pixels, String> {
        if data.len() > MAX_INPUT {
            return Err(format!("{} bytes is too large (at most {MAX_INPUT})", data.len()));
        }
        if !self.available() {
            return Err(format!("no image service: {}", self.why));
        }
        self.retry_at = None;
        // A second try on a new connection when the first one broke: the service leaves
        // when nobody has used it for a while, and comes back when asked.
        for attempt in 0..2 {
            if self.stream.is_none() {
                if let Err(why) = self.connect() {
                    self.end_helper();
                    self.retry_at = Some(Instant::now() + RETRY_AFTER);
                    self.why = why;
                    return Err(format!("no image service: {}", self.why));
                }
            }
            match self.exchange(data, fit) {
                Ok(pixels) => return Ok(pixels),
                Err(Failure::Refused(why)) => return Err(why),
                Err(Failure::Broken(why, final_)) => {
                    self.end_helper();
                    // It stopped (nobody had used it for a while, or it was replaced): ask
                    // again once. No answer in time, a wrong one, or stopped twice: the
                    // service is left alone for a while.
                    if final_ || attempt == 1 {
                        self.retry_at = Some(Instant::now() + RETRY_AFTER);
                        self.why = why.clone();
                        return Err(why);
                    }
                }
            }
        }
        Err("image service: no answer".to_string())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.end_helper();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in service on the other end of a socket pair: answers each request with
    /// what `reply` makes of it, `None` = close the connection.
    fn served(reply: impl Fn(usize, &[u8], (u16, u16)) -> Option<Vec<u8>> + Send + 'static) -> Decoder {
        let (mine, theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut theirs = theirs;
            for number in 0.. {
                let mut head = [0u8; REQUEST_HEAD];
                if theirs.read_exact(&mut head).is_err() {
                    return;
                }
                assert_eq!(head[..4], MAGIC);
                let mut data = vec![0u8; u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize];
                theirs.read_exact(&mut data).unwrap();
                let fit = (u16::from_le_bytes([head[8], head[9]]), u16::from_le_bytes([head[10], head[11]]));
                match reply(number, &data, fit) {
                    Some(bytes) => theirs.write_all(&bytes).unwrap(),
                    None => return,
                }
            }
        });
        let mut decoder = Decoder::new();
        decoder.stream = Some(mine);
        decoder
    }

    fn picture(width: u32, height: u32, fill: u8) -> Vec<u8> {
        let mut bytes = response_head(0, width, height, width * height * 4).to_vec();
        bytes.extend(std::iter::repeat_n(fill, (width * height * 4) as usize));
        bytes
    }

    #[test]
    fn pictures_and_refusals_on_one_connection() {
        let mut decoder = served(|number, data, fit| {
            assert_eq!(fit, (96, 96));
            Some(match number {
                0 => picture(2, 3, data[0]),
                1 => {
                    let why = b"webp: no picture data";
                    let mut bytes = response_head(1, 0, 0, why.len() as u32).to_vec();
                    bytes.extend_from_slice(why);
                    bytes
                }
                _ => picture(96, 96, 7),
            })
        });
        let first = decoder.decode(&[9, 9, 9], (96, 96)).unwrap();
        assert_eq!((first.width, first.height, first.rgba), (2, 3, vec![9; 24]));
        assert_eq!(decoder.decode(b"x", (96, 96)).unwrap_err(), "webp: no picture data");
        // A refusal is about that file: the connection goes on, and nothing backs off.
        assert!(decoder.available());
        assert_eq!(decoder.decode(b"y", (96, 96)).unwrap().rgba.len(), 96 * 96 * 4);
    }

    #[test]
    fn answers_out_of_bounds_are_not_believed() {
        // Larger than asked for; a length that does not match; more pixels than allowed.
        for bad in [picture(97, 96, 0), response_head(0, 4, 4, 65).to_vec(), response_head(0, 0, 4, 0).to_vec()] {
            let mut decoder = served(move |_, _, _| Some(bad.clone()));
            assert!(decoder.decode(b"x", (96, 96)).unwrap_err().contains("out of bounds"));
        }
        let mut decoder = served(|_, _, _| Some(response_head(0, 2048, 2048, 2048 * 2048 * 4).to_vec()));
        assert!(decoder.decode(b"x", (0, 0)).unwrap_err().contains("out of bounds"));
        // Too large a file is refused before anything is sent.
        let mut decoder = Decoder::new();
        assert!(decoder.decode(&vec![0; MAX_INPUT + 1], (0, 0)).unwrap_err().contains("too large"));
        assert!(decoder.stream.is_none() && decoder.available());
    }

    #[test]
    fn without_a_service_it_fails_once_and_then_at_once() {
        // (Run alone in this process's environment: no helper, no broker, no appd.)
        if std::env::var("MEOW_IMAGE_HELPER").is_ok() || std::env::var("MEOW_BROKER_FD").is_ok() || std::path::Path::new(CONTROL_SOCKET).exists() {
            return;
        }
        let mut decoder = Decoder::new();
        assert!(decoder.available());
        assert!(decoder.decode(b"RIFF", (96, 96)).unwrap_err().starts_with("no image service"));
        assert!(!decoder.available());
        let started = Instant::now();
        assert!(decoder.decode(b"RIFF", (96, 96)).unwrap_err().starts_with("no image service"));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_service_that_stopped_is_asked_for_again_once() {
        // The connection worked, then the service left: the next request finds it closed
        // and asks for a new one (there is nowhere to get one in a test: it fails at once).
        let mut decoder = served(|number, _, _| if number == 0 { Some(picture(1, 1, 5)) } else { None });
        assert_eq!(decoder.decode(b"x", (0, 0)).unwrap().rgba, [5; 4]);
        if std::env::var("MEOW_IMAGE_HELPER").is_ok() || std::env::var("MEOW_BROKER_FD").is_ok() || std::path::Path::new(CONTROL_SOCKET).exists() {
            return;
        }
        assert!(decoder.decode(b"x", (0, 0)).is_err());
        assert!(!decoder.available());
    }

    #[test]
    fn the_brokers_answer_reaches_the_asking_thread() {
        // A socket pair stands in for the broker: "appd" reads IMAGE and answers through
        // `answer`, as `App::handle` does.
        let (broker, appd) = crate::ipc::seqpacket_pair().unwrap();
        let (mine, theirs) = UnixStream::pair().unwrap();
        let pump = std::thread::spawn(move || {
            let (message, _) = crate::ipc::recv(appd.as_raw_fd(), true).unwrap().unwrap();
            assert_eq!(message, "IMAGE");
            assert!(asking());
            answer("ok", Some(OwnedFd::from(theirs)));
            let (message, _) = crate::ipc::recv(appd.as_raw_fd(), true).unwrap().unwrap();
            assert_eq!(message, "IMAGE");
            answer("unavailable\tthe store has not been reached", None);
        });
        let socket = ask_broker(broker.as_raw_fd()).unwrap();
        drop(mine);
        let mut gone = [0u8; 1];
        assert_eq!(UnixStream::from(socket).read(&mut gone).unwrap(), 0);
        assert_eq!(ask_broker(broker.as_raw_fd()).unwrap_err(), "the store has not been reached");
        pump.join().unwrap();
        // Nobody is asking: a late answer is dropped.
        assert!(!asking());
        answer("ok", None);
        assert!(!asking());
    }
}
