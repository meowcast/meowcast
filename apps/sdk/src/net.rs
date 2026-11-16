//! Internet access for apps, through the system network service (`meow-net`).
//!
//! An app with the "internet" permission has no network of its own: its sandbox keeps an
//! empty network namespace. It gets a *dial* socket to the network service instead, and this
//! module speaks to it. TLS, certificates, DNS and HTTP live in the service, in one place for
//! every app; the service only connects to the hosts the app's signed manifest lists.
//!
//! ```no_run
//! let mut http = meow_app::net::Client::new()?;
//! let page = http.get("https://example.com/", &[("User-Agent", "demo")])?;
//! println!("{} {}", page.status, page.text());
//! # Ok::<(), String>(())
//! ```
//!
//! A `Client` is one *channel*: requests on it run one after another and share the service's
//! kept-alive connections. Threads that fetch in parallel each make their own `Client`.
//!
//! Where the dial socket comes from:
//! 1. `MEOW_NET_FD`: inherited from appd (apps). appd sends a new one over the broker when
//!    the service restarts; `App::wait`/`poll_events` installs it.
//! 2. `MEOW_NET_HELPER=/path/to/meow-net`: runs a private service (tests, the build host).
//! 3. appd's control socket (root shells on the device: `youtube resolve ID`).
//!
//! Wire format (the service side is apps/netsvc):
//!   dial (SEQPACKET):  app `OPEN n`  ->  service `OK n` + a channel fd, or `ERR n why`
//!   channel (stream):  request  = u32 length + head, u32 length + body  (little endian)
//!                      head     = `METHOD URL [silence=SECONDS]`, then `Name: value` lines
//!                      response = frames of u8 kind + u32 length + payload:
//!                        'H' status line + lower-case `name: value` lines   (once)
//!                        'D' a piece of the body                            (any number)
//!                        'E' end of the body: the channel takes the next request
//!                        'X' the request failed: text (the channel stays usable)

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub const FRAME_HEAD: u8 = b'H';
pub const FRAME_DATA: u8 = b'D';
pub const FRAME_END: u8 = b'E';
pub const FRAME_ERROR: u8 = b'X';
/// Largest request head and body the service accepts.
pub const MAX_REQUEST_HEAD: usize = 32 << 10;
pub const MAX_REQUEST_BODY: usize = 4 << 20;
/// Largest body `Client::request` buffers (media is fetched in ranges; streams are unbounded).
const MAX_BODY: usize = 24 << 20;
/// A first channel may wait for the service to be downloaded and started.
const OPEN_TIMEOUT: Duration = Duration::from_secs(25);
const CONTROL_SOCKET: &str = "/run/appd/ui.sock";

struct Dial {
    fd: OwnedFd,
    /// A private service (MEOW_NET_HELPER), ended with the process.
    _helper: Option<std::process::Child>,
}

static DIAL: Mutex<Option<Dial>> = Mutex::new(None);
static SEQUENCE: AtomicU32 = AtomicU32::new(1);

/// Install a dial socket (appd sent a new one: the service was restarted).
pub fn replace_dial(fd: OwnedFd) {
    *DIAL.lock().unwrap() = Some(Dial { fd, _helper: None });
}

fn find_dial() -> Result<Dial, String> {
    if let Some(fd) = std::env::var("MEOW_NET_FD").ok().and_then(|v| v.parse::<RawFd>().ok()) {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            return Ok(Dial { fd: unsafe { OwnedFd::from_raw_fd(fd) }, _helper: None });
        }
        return Err("the network service is not available (it stopped; waiting for the system to restart it)".into());
    }
    if std::env::var("MEOW_BROKER_FD").is_ok() {
        return Err("no internet access: the app does not have the \"internet\" permission".into());
    }
    let (mine, theirs) = crate::ipc::seqpacket_pair().map_err(|e| format!("socketpair: {e}"))?;
    if let Ok(helper) = std::env::var("MEOW_NET_HELPER") {
        use std::os::unix::process::CommandExt;
        let raw = theirs.as_raw_fd();
        let mut command = std::process::Command::new(&helper);
        command.args(["--dial", "3"]).stdin(std::process::Stdio::null());
        if let Ok(hosts) = std::env::var("MEOW_NET_HOSTS") {
            command.args(["--hosts", &hosts]);
        }
        unsafe {
            command.pre_exec(move || {
                // Async-signal-safe only: the service end becomes fd 3, inheritable.
                if raw == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0);
                } else if libc::dup2(raw, 3) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("{helper}: {e}"))?;
        return Ok(Dial { fd: mine, _helper: Some(child) });
    }
    // A root shell on the device: appd hands out a dial like an app's (any host).
    drop((mine, theirs));
    let control = crate::ipc::connect_seqpacket(CONTROL_SOCKET).map_err(|e| format!("no network service: {CONTROL_SOCKET}: {e} (or set MEOW_NET_HELPER)"))?;
    let ask = |message: &str, want: &str| -> Result<Option<OwnedFd>, String> {
        crate::ipc::send(control.as_raw_fd(), message, None).map_err(|e| format!("appd: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (reply, fd) = crate::ipc::recv_until(control.as_raw_fd(), deadline).map_err(|e| format!("appd: {e}"))?.ok_or("appd did not answer")?;
            if reply.starts_with(want) {
                return Ok(fd);
            }
            if let Some(why) = reply.strip_prefix("ERR\t") {
                return Err(format!("appd: {why}"));
            }
        }
    };
    ask("HELLO\tctl", "WELCOME")?;
    let fd = ask("NET", "NET\tok")?.ok_or("appd sent no dial socket")?;
    Ok(Dial { fd, _helper: None })
}

/// A new channel to the network service.
fn open_channel() -> Result<UnixStream, String> {
    let mut dial = DIAL.lock().unwrap();
    if dial.is_none() {
        *dial = Some(find_dial()?);
    }
    let fd = dial.as_ref().unwrap().fd.as_raw_fd();
    let number = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let deadline = Instant::now() + OPEN_TIMEOUT;
    let gone = |dial: &mut Option<Dial>, why: String| {
        // In an app the system sends a new dial; elsewhere the next call looks for one again.
        *dial = None;
        format!("network service: {why}")
    };
    if let Err(e) = crate::ipc::send(fd, &format!("OPEN\t{number}"), None) {
        return Err(gone(&mut dial, e.to_string()));
    }
    loop {
        let (reply, passed) = match crate::ipc::recv_until(fd, deadline) {
            Ok(Some(message)) => message,
            Ok(None) => return Err("network service: not ready yet (still starting, or the store is unreachable)".into()),
            Err(e) => return Err(gone(&mut dial, e.to_string())),
        };
        let mut fields = reply.split('\t');
        let (kind, of) = (fields.next().unwrap_or(""), fields.next().and_then(|n| n.parse::<u32>().ok()));
        if of != Some(number) {
            continue; // the answer to an OPEN that timed out earlier (its fd closes here)
        }
        return match (kind, passed) {
            ("OK", Some(channel)) => Ok(UnixStream::from(channel)),
            _ => Err(format!("network service: {}", fields.next().unwrap_or("refused"))),
        };
    }
}

pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// Header value by lower-case name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A URL for a log line: no query string (stream URLs carry signatures).
pub fn short(url: &str) -> &str {
    let end = url.find('?').unwrap_or(url.len()).min(120);
    &url[..end]
}

/// One channel to the network service: requests run one at a time.
pub struct Client {
    channel: Option<UnixStream>,
    /// Channels opened so far (diagnostics).
    pub connects: u32,
}

/// Why a request failed, and whether nothing of an answer had arrived (so a new channel may
/// try again: the service was restarted under an idle channel).
struct Failure {
    text: String,
    channel_dead: bool,
}

fn write_request(channel: &mut UnixStream, method: &str, url: &str, headers: &[(&str, &str)], body: &[u8], silence: Option<Duration>) -> io::Result<()> {
    let mut head = format!("{method} {url}");
    if let Some(silence) = silence {
        head.push_str(&format!(" silence={}", silence.as_secs().max(1)));
    }
    head.push('\n');
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\n"));
    }
    let mut message = Vec::with_capacity(8 + head.len() + body.len());
    message.extend_from_slice(&(head.len() as u32).to_le_bytes());
    message.extend_from_slice(head.as_bytes());
    message.extend_from_slice(&(body.len() as u32).to_le_bytes());
    message.extend_from_slice(body);
    channel.write_all(&message)
}

/// One frame: (kind, payload).
pub fn read_frame(channel: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    channel.read_exact(&mut header)?;
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > 1 << 20 {
        return Err(io::Error::other("oversized frame"));
    }
    let mut payload = vec![0u8; length];
    channel.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

pub fn write_frame(channel: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    channel.write_all(&frame)
}

fn parse_head(payload: &[u8]) -> (u16, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(payload);
    let mut lines = text.split('\n');
    let status = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    (status, headers)
}

impl Client {
    /// Fails when the app has no way to the network service (no "internet" permission).
    pub fn new() -> Result<Client, String> {
        let mut dial = DIAL.lock().unwrap();
        if dial.is_none() {
            *dial = Some(find_dial()?);
        }
        Ok(Client { channel: None, connects: 0 })
    }

    pub fn get(&mut self, url: &str, headers: &[(&str, &str)]) -> Result<Response, String> {
        self.request("GET", url, headers, None)
    }

    /// The client holds a channel to the network service (and with it the service's
    /// kept-alive connections): its next request need not connect again. False before the
    /// first request and after one that was given up half-way.
    pub fn connected(&self) -> bool {
        self.channel.is_some()
    }

    /// GET bytes `start..=end` of a resource. Ok only for a 206 (or a 200 of exactly that size).
    pub fn get_range(&mut self, url: &str, headers: &[(&str, &str)], start: u64, end: u64) -> Result<Vec<u8>, String> {
        let range = format!("bytes={start}-{end}");
        let mut all: Vec<(&str, &str)> = headers.to_vec();
        all.push(("Range", &range));
        let response = self.request("GET", url, &all, None)?;
        let want = (end - start + 1) as usize;
        match response.status {
            206 => Ok(response.body),
            200 if response.body.len() == want => Ok(response.body),
            status => Err(format!("{}: HTTP {status} for range {start}-{end}", short(url))),
        }
    }

    /// One request; the service follows redirects (within the app's hosts) and keeps
    /// connections alive between requests.
    pub fn request(&mut self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Response, String> {
        self.request_unless(method, url, headers, body, None)
    }

    /// A GET that is given up soon after `stop` is set (an error): the rest of the answer is
    /// not read, nor kept, and the service drops the transfer. For large downloads that may
    /// stop mattering half-way, like a segment of a channel the viewer has just left.
    pub fn get_unless(&mut self, url: &str, headers: &[(&str, &str)], stop: &AtomicBool) -> Result<Response, String> {
        self.request_unless("GET", url, headers, None, Some(stop))
    }

    /// A GET whose body is handed to `take` piece by piece as it arrives, and not kept: a
    /// large download that is of use from its first bytes on (a media segment that is played
    /// while it comes in). `take` gets the HTTP status with every piece. Returns the status
    /// once the body is complete. Given up (an error) soon after `stop` is set, as
    /// `get_unless`.
    pub fn get_pieces(&mut self, url: &str, headers: &[(&str, &str)], stop: &AtomicBool, take: &mut dyn FnMut(u16, &[u8])) -> Result<u16, String> {
        for attempt in 0..2 {
            let fresh = self.channel.is_none();
            match self.once("GET", url, headers, None, Some(stop), Some(&mut *take)) {
                Ok(response) => return Ok(response.status),
                Err(failure) => {
                    if failure.channel_dead {
                        self.channel = None;
                        // (Dead before anything of an answer came: nothing was handed over.)
                        if !fresh && attempt == 0 {
                            continue;
                        }
                    }
                    return Err(format!("{}: {}", short(url), failure.text));
                }
            }
        }
        unreachable!("the second attempt always returns")
    }

    fn request_unless(&mut self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>, stop: Option<&AtomicBool>) -> Result<Response, String> {
        for attempt in 0..2 {
            let fresh = self.channel.is_none();
            match self.once(method, url, headers, body, stop, None) {
                Ok(response) => return Ok(response),
                Err(failure) => {
                    if failure.channel_dead {
                        self.channel = None;
                        if !fresh && attempt == 0 {
                            continue;
                        }
                    }
                    return Err(format!("{}: {}", short(url), failure.text));
                }
            }
        }
        unreachable!("the second attempt always returns")
    }

    /// `pieces`: where the body goes instead of into the response.
    fn once(&mut self, method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>, stop: Option<&AtomicBool>, mut pieces: Option<&mut dyn FnMut(u16, &[u8])>) -> Result<Response, Failure> {
        if self.channel.is_none() {
            self.channel = Some(open_channel().map_err(|text| Failure { text, channel_dead: true })?);
            self.connects += 1;
        }
        let channel = self.channel.as_mut().unwrap();
        let dead = |e: io::Error| Failure { text: format!("network service: {e}"), channel_dead: true };
        write_request(channel, method, url, headers, body.unwrap_or(&[]), None).map_err(dead)?;
        let (mut status, mut response_headers, mut data) = (0, Vec::new(), Vec::new());
        let mut answered = false;
        loop {
            let (kind, payload) = read_frame(channel).map_err(|e| Failure { text: format!("network service: {e}"), channel_dead: !answered })?;
            answered = true;
            if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
                // The channel goes with what is still on its way.
                self.channel = None;
                return Err(Failure { text: "stopped".into(), channel_dead: false });
            }
            match kind {
                FRAME_HEAD => {
                    (status, response_headers) = parse_head(&payload);
                    // The size is usually known: one allocation of that size, not a doubling one.
                    let length = response_headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-length")).and_then(|(_, value)| value.trim().parse::<usize>().ok());
                    if pieces.is_none() {
                        data.reserve_exact(length.unwrap_or(0).min(MAX_BODY));
                    }
                }
                FRAME_DATA if pieces.is_some() => {
                    if let Some(take) = pieces.as_mut() {
                        take(status, &payload);
                    }
                }
                FRAME_DATA if data.len() + payload.len() > MAX_BODY => {
                    // Too much to buffer: give the channel up rather than read it all.
                    self.channel = None;
                    return Err(Failure { text: "response too large".into(), channel_dead: false });
                }
                FRAME_DATA => data.extend_from_slice(&payload),
                FRAME_END => return Ok(Response { status, headers: response_headers, body: data }),
                FRAME_ERROR => return Err(Failure { text: String::from_utf8_lossy(&payload).into_owned(), channel_dead: false }),
                _ => return Err(Failure { text: "network service: unknown frame".into(), channel_dead: true }),
            }
        }
    }

    /// GET a response that arrives over minutes (a long poll), on a channel of its own.
    /// `silence` is how long the server may send nothing before the read fails.
    pub fn open_stream(&mut self, url: &str, headers: &[(&str, &str)], silence: Duration) -> Result<Streaming, String> {
        let mut channel = open_channel()?;
        self.connects += 1;
        write_request(&mut channel, "GET", url, headers, &[], Some(silence)).map_err(|e| format!("network service: {e}"))?;
        match read_frame(&mut channel).map_err(|e| format!("{}: network service: {e}", short(url)))? {
            (FRAME_HEAD, payload) => Ok(Streaming { status: parse_head(&payload).0, channel, done: false }),
            (FRAME_ERROR, payload) => Err(format!("{}: {}", short(url), String::from_utf8_lossy(&payload))),
            _ => Err(format!("{}: network service: unexpected frame", short(url))),
        }
    }
}

/// A response body read piece by piece as it arrives.
pub struct Streaming {
    pub status: u16,
    channel: UnixStream,
    done: bool,
}

impl Streaming {
    /// A handle that ends a blocked `read` from another thread (`shutdown` on it); the service
    /// then drops the connection to the server.
    pub fn socket(&self) -> io::Result<UnixStream> {
        self.channel.try_clone()
    }

    /// The next piece of the body; None at its end. Blocks until something arrives.
    pub fn read(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.done {
            return Ok(None);
        }
        match read_frame(&mut self.channel) {
            Ok((FRAME_DATA, payload)) => Ok(Some(payload)),
            Ok((FRAME_END, _)) => {
                self.done = true;
                Ok(None)
            }
            Ok((FRAME_ERROR, payload)) => {
                self.done = true;
                Err(String::from_utf8_lossy(&payload).into_owned())
            }
            Ok(_) => Err("network service: unexpected frame".into()),
            Err(e) => {
                self.done = true;
                Err(format!("read: {e}"))
            }
        }
    }
}

/// `a=1&b=x%20y`: percent-encoded pairs for a query string or a form body.
pub fn encode_pairs(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (name, value) in pairs {
        if !out.is_empty() {
            out.push('&');
        }
        for (text, last) in [(name, false), (value, true)] {
            for byte in text.bytes() {
                match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
                    _ => out.push_str(&format!("%{byte:02X}")),
                }
            }
            if !last {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_are_percent_encoded() {
        assert_eq!(encode_pairs(&[("name", "meow TV"), ("t", "a/b+c=d&e"), ("ż", "")]), "name=meow%20TV&t=a%2Fb%2Bc%3Dd%26e&%C5%BC=");
    }

    #[test]
    fn log_urls_carry_no_query() {
        assert_eq!(short("https://h/videoplayback?sig=secret"), "https://h/videoplayback");
    }

    #[test]
    fn a_download_is_given_up_when_it_stops_mattering() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut client = Client { channel: Some(ours), connects: 1 };
        let stop = AtomicBool::new(false);
        let error = std::thread::scope(|scope| {
            let stop = &stop;
            scope.spawn(move || {
                // The service: the head with the size, a first piece, and (once the app has
                // lost interest) more pieces until the app hangs up.
                let mut request = [0u8; 4];
                theirs.read_exact(&mut request).unwrap();
                write_frame(&mut theirs, FRAME_HEAD, b"200\nContent-Length: 4000000\n").unwrap();
                write_frame(&mut theirs, FRAME_DATA, &[7; 65536]).unwrap();
                stop.store(true, Ordering::Relaxed);
                while write_frame(&mut theirs, FRAME_DATA, &[7; 65536]).is_ok() {}
            });
            client.get_unless("https://h/segment.ts", &[], stop).map(|_| ()).unwrap_err()
        });
        assert!(error.ends_with("stopped"), "{error}");
        assert!(client.channel.is_none());
        // Without the flag set the same call reads to the end.
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut client = Client { channel: Some(ours), connects: 1 };
        write_frame(&mut theirs, FRAME_HEAD, b"200\ncontent-length: 3\n").unwrap();
        write_frame(&mut theirs, FRAME_DATA, b"abc").unwrap();
        write_frame(&mut theirs, FRAME_END, b"").unwrap();
        let response = client.get_unless("https://h/x", &[], &AtomicBool::new(false)).unwrap();
        assert_eq!((response.status, response.body.as_slice(), response.body.capacity()), (200, &b"abc"[..], 3));
    }

    #[test]
    fn frames_round_trip() {
        let mut wire = Vec::new();
        write_frame(&mut wire, FRAME_HEAD, b"206\ncontent-range: bytes 0-1/9\n").unwrap();
        write_frame(&mut wire, FRAME_END, b"").unwrap();
        let mut reader = &wire[..];
        let (kind, payload) = read_frame(&mut reader).unwrap();
        assert_eq!(kind, FRAME_HEAD);
        let (status, headers) = parse_head(&payload);
        assert_eq!((status, headers), (206, vec![("content-range".to_string(), "bytes 0-1/9".to_string())]));
        assert_eq!(read_frame(&mut reader).unwrap(), (FRAME_END, Vec::new()));
    }
}
