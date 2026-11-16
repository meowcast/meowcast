//! One channel: an app's requests, one after another, relayed as HTTP/1.1 over TCP or TLS.
//!
//! The wire format is in apps/sdk/src/net.rs. Each channel has its own kept-alive
//! connections (a TLS handshake costs far more than a request on the dongle's Cortex-A7, and
//! media playback is a long series of range requests to one host); nothing is shared between
//! channels, so one app can never ride on another app's connection.
//!
//! The app ends a request by closing (or shutting down) its channel: every wait on the
//! server also watches the channel, so a silent long poll is dropped at once.

use crate::policy::{Policy, Url};
use meow_app::net::{FRAME_DATA, FRAME_END, FRAME_ERROR, FRAME_HEAD, MAX_REQUEST_BODY, MAX_REQUEST_HEAD, write_frame};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// No byte from the server for this long: the request fails.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest silence a request may ask for (`silence=`, long polls).
const MAX_SILENCE: Duration = Duration::from_secs(900);
/// A kept connection unused for this long is not tried again (servers close them).
const KEEP_FOR: Duration = Duration::from_secs(50);
const MAX_REDIRECTS: usize = 3;
const MAX_HEAD: usize = 64 << 10;
const ABORTED: &str = "the app closed the request";

/// The TLS setup every channel shares: Mozilla's root CAs, TLS 1.2 and 1.3.
pub fn tls_config() -> Result<Arc<ClientConfig>, String> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls setup: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The server's socket. Reads wait for the server with a deadline and give up when the
/// app's channel closes.
struct Sock {
    tcp: TcpStream,
    watch: RawFd,
    timeout: Duration,
}

impl Read for Sock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let deadline = Instant::now() + self.timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the server sent nothing in time"));
            }
            let mut fds = [
                libc::pollfd { fd: self.tcp.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.watch, events: libc::POLLIN | libc::POLLRDHUP, revents: 0 },
            ];
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, left.as_millis().min(60_000) as i32 + 1) };
            if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
            if n > 0 && fds[1].revents != 0 {
                // During a request the app sends nothing: readable means closed.
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, ABORTED));
            }
            if n > 0 && fds[0].revents != 0 {
                return self.tcp.read(buf);
            }
        }
    }
}

impl Write for Sock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tcp.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

enum Stream {
    Plain(Sock),
    Tls(Box<StreamOwned<ClientConnection, Sock>>),
}

impl Stream {
    fn sock(&mut self) -> &mut Sock {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &mut s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

struct Conn {
    stream: Stream,
    /// Bytes received and not consumed yet.
    buffer: Vec<u8>,
    used: Instant,
}

impl Conn {
    /// More bytes into the buffer; Ok(0) = the server closed.
    fn fill(&mut self) -> io::Result<usize> {
        let old = self.buffer.len();
        self.buffer.resize(old + 16384, 0);
        let result = self.stream.read(&mut self.buffer[old..]);
        self.buffer.truncate(old + *result.as_ref().unwrap_or(&0));
        result
    }

    /// The buffer up to (not including) `mark`, which is consumed too.
    fn until(&mut self, mark: &[u8], limit: usize) -> Result<Vec<u8>, String> {
        loop {
            if let Some(i) = self.buffer.windows(mark.len()).position(|w| w == mark) {
                let line = self.buffer[..i].to_vec();
                self.buffer.drain(..i + mark.len());
                return Ok(line);
            }
            if self.buffer.len() > limit {
                return Err("response head too large".into());
            }
            match self.fill() {
                Ok(0) => return Err("connection closed".into()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

/// Why an exchange stopped.
enum Fail {
    /// Told to the app as an 'X' frame; the channel goes on.
    Request(String),
    /// The app's side is gone.
    Channel,
}

impl From<String> for Fail {
    fn from(text: String) -> Fail {
        if text.contains(ABORTED) { Fail::Channel } else { Fail::Request(text) }
    }
}

struct Request {
    method: String,
    url: String,
    silence: Option<Duration>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_block(channel: &mut UnixStream, limit: usize) -> io::Result<Vec<u8>> {
    let mut length = [0u8; 4];
    channel.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::other("request too large"));
    }
    let mut block = vec![0u8; length];
    channel.read_exact(&mut block)?;
    Ok(block)
}

fn token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn parse_request(head: &[u8], body: Vec<u8>) -> Result<Request, String> {
    let head = std::str::from_utf8(head).map_err(|_| "request head is not text".to_string())?;
    let mut lines = head.split('\n');
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, url) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if !token(method) || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("bad method".into());
    }
    let mut silence = None;
    for option in first {
        match option.split_once('=') {
            Some(("silence", seconds)) => silence = Some(Duration::from_secs(seconds.parse().map_err(|_| "bad silence".to_string())?).min(MAX_SILENCE)),
            _ => return Err(format!("unknown request option {option:?}")),
        }
    }
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("bad header line")?;
        let value = value.trim();
        if !token(name) || value.bytes().any(|b| b < b' ' && b != b'\t' || b == 0x7f) {
            return Err(format!("bad header {name:?}"));
        }
        // The service owns the connection: its framing and its target are not the app's to set.
        let lower = name.to_ascii_lowercase();
        if matches!(lower.as_str(), "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding" | "upgrade" | "te" | "trailer" | "proxy-authorization" | "keep-alive") {
            continue;
        }
        headers.push((name.to_string(), value.to_string()));
    }
    Ok(Request { method: method.to_string(), url: url.to_string(), silence, headers, body })
}

pub struct Channel {
    app: UnixStream,
    policy: Arc<Policy>,
    tls: Arc<ClientConfig>,
    conns: HashMap<String, Conn>,
}

impl Channel {
    pub fn new(app: UnixStream, policy: Arc<Policy>, tls: Arc<ClientConfig>) -> Channel {
        Channel { app, policy, tls, conns: HashMap::new() }
    }

    /// Serve requests until the app closes the channel.
    pub fn run(mut self) {
        loop {
            let Ok(head) = read_block(&mut self.app, MAX_REQUEST_HEAD) else { return };
            let Ok(body) = read_block(&mut self.app, MAX_REQUEST_BODY) else { return };
            let outcome = parse_request(&head, body).map_err(Fail::Request).and_then(|request| self.exchange(&request));
            match outcome {
                Ok(()) => {}
                Err(Fail::Channel) => return,
                Err(Fail::Request(text)) => {
                    if write_frame(&mut self.app, FRAME_ERROR, text.as_bytes()).is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn connect(&mut self, url: &Url, timeout: Duration) -> Result<Conn, String> {
        let address = self.policy.resolve(url)?;
        let tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(|e| format!("{}: {e}", url.host))?;
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_write_timeout(Some(IDLE_TIMEOUT));
        let sock = Sock { tcp, watch: self.app.as_raw_fd(), timeout };
        let stream = if url.tls {
            let name = ServerName::try_from(url.host.clone()).map_err(|e| format!("{}: {e}", url.host))?;
            let session = ClientConnection::new(self.tls.clone(), name).map_err(|e| format!("{}: tls: {e}", url.host))?;
            Stream::Tls(Box::new(StreamOwned::new(session, sock)))
        } else {
            Stream::Plain(sock)
        };
        Ok(Conn { stream, buffer: Vec::new(), used: Instant::now() })
    }

    /// Send the request to `url` and read the response head: (status, headers).
    fn send(&mut self, request: &Request, url: &Url) -> Result<(u16, Vec<(String, String)>), String> {
        let origin = url.origin();
        let timeout = request.silence.unwrap_or(IDLE_TIMEOUT);
        let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n", request.method, url.path, url.authority());
        for (name, value) in &request.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !request.body.is_empty() || matches!(request.method.as_str(), "POST" | "PUT" | "PATCH") {
            head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
        }
        head.push_str("\r\n");

        if self.conns.get(&origin).is_some_and(|c| c.used.elapsed() > KEEP_FOR) {
            self.conns.remove(&origin);
        }
        for attempt in 0..2 {
            let reused = self.conns.contains_key(&origin);
            if !reused {
                let conn = self.connect(url, timeout)?;
                self.conns.insert(origin.clone(), conn);
            }
            let conn = self.conns.get_mut(&origin).expect("just inserted");
            conn.stream.sock().timeout = timeout;
            conn.buffer.clear();
            let sent = conn.stream.write_all(head.as_bytes()).and_then(|_| conn.stream.write_all(&request.body)).and_then(|_| conn.stream.flush());
            // Nothing of a response yet: a failure here on a kept connection (the server
            // closed it while idle) is safe to try once more on a new one.
            let first = sent.map_err(|e| format!("send: {e}")).and_then(|_| match conn.fill() {
                Ok(0) => Err("connection closed".to_string()),
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            });
            if let Err(text) = first {
                self.conns.remove(&origin);
                if reused && attempt == 0 && !text.contains(ABORTED) {
                    continue;
                }
                return Err(format!("{}: {text}", url.host));
            }
            let parsed = conn.until(b"\r\n\r\n", MAX_HEAD).and_then(|head| {
                let head = String::from_utf8_lossy(&head).into_owned();
                let mut lines = head.split("\r\n");
                let status: u16 = lines.next().and_then(|l| l.split(' ').nth(1)).and_then(|s| s.parse().ok()).ok_or("bad status line")?;
                let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
                Ok((status, headers))
            });
            if parsed.is_err() {
                self.conns.remove(&origin);
            }
            return parsed.map_err(|text| format!("{}: {text}", url.host));
        }
        unreachable!("the second attempt always returns")
    }

    /// Read the response body from the connection of `origin`; pieces go to `sink`.
    /// Ok(whether the connection can serve another request).
    fn body(&mut self, origin: &str, request: &Request, status: u16, headers: &[(String, String)], sink: &mut dyn FnMut(&[u8]) -> Result<(), Fail>) -> Result<bool, Fail> {
        let conn = self.conns.get_mut(origin).expect("the connection of this exchange");
        let value = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let mut keep = !value("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let more = |conn: &mut Conn, what: &str| -> Result<(), Fail> {
            match conn.fill() {
                Ok(0) => Err(Fail::Request(format!("connection closed inside {what}"))),
                Ok(_) => Ok(()),
                Err(e) => Err(Fail::from(e.to_string())),
            }
        };
        if request.method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status) {
            // No body.
        } else if value("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
            loop {
                let line = conn.until(b"\r\n", 1024).map_err(Fail::from)?;
                let size = usize::from_str_radix(String::from_utf8_lossy(&line).split(';').next().unwrap_or("").trim(), 16).map_err(|_| Fail::Request("bad chunk size".into()))?;
                if size == 0 {
                    // Trailers, then the empty line.
                    while !conn.until(b"\r\n", MAX_HEAD).map_err(Fail::from)?.is_empty() {}
                    break;
                }
                let mut left = size;
                while left > 0 {
                    if conn.buffer.is_empty() {
                        more(conn, "a chunk")?;
                    }
                    let take = left.min(conn.buffer.len());
                    sink(&conn.buffer[..take])?;
                    conn.buffer.drain(..take);
                    left -= take;
                }
                if !conn.until(b"\r\n", 2).map_err(Fail::from)?.is_empty() {
                    return Err(Fail::Request("bad chunk end".into()));
                }
            }
        } else if let Some(length) = value("content-length") {
            let mut left: u64 = length.parse().map_err(|_| Fail::Request("bad content-length".into()))?;
            while left > 0 {
                if conn.buffer.is_empty() {
                    more(conn, "the body")?;
                }
                let take = (left.min(conn.buffer.len() as u64)) as usize;
                sink(&conn.buffer[..take])?;
                conn.buffer.drain(..take);
                left -= take as u64;
            }
        } else {
            // The body ends when the server closes (a TLS close without notify included).
            keep = false;
            loop {
                if !conn.buffer.is_empty() {
                    sink(&conn.buffer)?;
                    conn.buffer.clear();
                }
                match conn.fill() {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if e.to_string().contains(ABORTED) => return Err(Fail::Channel),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => return Err(Fail::Request(e.to_string())),
                    Err(_) => break,
                }
            }
        }
        conn.used = Instant::now();
        Ok(keep)
    }

    fn exchange(&mut self, request: &Request) -> Result<(), Fail> {
        let mut url = Url::parse(&request.url).map_err(Fail::Request)?;
        for hop in 0.. {
            let (status, headers) = self.send(request, &url)?;
            let origin = url.origin();
            let location = headers.iter().find(|(k, _)| k == "location").map(|(_, v)| v.clone());
            // A long poll is answered as it is; anything else follows redirects here, each
            // hop checked against the app's hosts like the first.
            if let (true, Some(location), None) = (matches!(status, 301 | 302 | 303 | 307 | 308), location, request.silence) {
                if hop >= MAX_REDIRECTS {
                    self.conns.remove(&origin);
                    return Err(Fail::Request("too many redirects".into()));
                }
                // Read the redirect's body away to keep the connection, if it is small.
                let mut seen = 0usize;
                let drained = self.body(&origin, request, status, &headers, &mut |piece| {
                    seen += piece.len();
                    if seen > 64 << 10 { Err(Fail::Request(String::new())) } else { Ok(()) }
                });
                match drained {
                    Ok(true) => {}
                    Err(Fail::Channel) => return Err(Fail::Channel),
                    _ => drop(self.conns.remove(&origin)),
                }
                url = if location.starts_with('/') { Url::parse(&format!("{origin}{location}")) } else { Url::parse(&location) }.map_err(|e| Fail::Request(format!("redirect: {e}")))?;
                continue;
            }
            let mut head = status.to_string();
            for (name, value) in &headers {
                head.push_str(&format!("\n{name}: {value}"));
            }
            write_frame(&mut self.app, FRAME_HEAD, head.as_bytes()).map_err(|_| Fail::Channel)?;
            let mut app = self.app.try_clone().map_err(|_| Fail::Channel)?;
            let relayed = self.body(&origin, request, status, &headers, &mut |piece| write_frame(&mut app, FRAME_DATA, piece).map_err(|_| Fail::Channel));
            return match relayed {
                Ok(keep) => {
                    if !keep {
                        self.conns.remove(&origin);
                    }
                    write_frame(&mut self.app, FRAME_END, b"").map_err(|_| Fail::Channel)
                }
                Err(fail) => {
                    self.conns.remove(&origin);
                    Err(fail)
                }
            };
        }
        unreachable!("the loop returns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meow_app::net::read_frame;
    use std::net::TcpListener;

    /// A server answering `responses` in order on ONE connection, then closing. Returns its
    /// URL and a handle giving the request heads it saw.
    fn serve(responses: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            for response in responses {
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        return seen;
                    }
                    request.push(byte[0]);
                }
                seen.push(String::from_utf8_lossy(&request).into_owned());
                socket.write_all(&response).unwrap();
            }
            seen
        });
        (url, handle)
    }

    fn channel(hosts: &str) -> UnixStream {
        let (app, service) = UnixStream::pair().unwrap();
        let (policy, tls) = (Arc::new(Policy::new(hosts, true)), tls_config().unwrap());
        std::thread::spawn(move || Channel::new(service, policy, tls).run());
        app
    }

    fn ask(app: &mut UnixStream, head: &str, body: &[u8]) -> Result<(String, Vec<u8>), String> {
        let mut message = Vec::new();
        message.extend_from_slice(&(head.len() as u32).to_le_bytes());
        message.extend_from_slice(head.as_bytes());
        message.extend_from_slice(&(body.len() as u32).to_le_bytes());
        message.extend_from_slice(body);
        app.write_all(&message).unwrap();
        let (mut head, mut data) = (String::new(), Vec::new());
        loop {
            match read_frame(app).unwrap() {
                (FRAME_HEAD, payload) => head = String::from_utf8(payload).unwrap(),
                (FRAME_DATA, payload) => data.extend_from_slice(&payload),
                (FRAME_END, _) => return Ok((head, data)),
                (FRAME_ERROR, payload) => return Err(String::from_utf8(payload).unwrap()),
                other => panic!("frame {other:?}"),
            }
        }
    }

    #[test]
    fn keep_alive_length_chunked_and_headers() {
        let (url, server) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Thing: a b\r\n\r\nhello".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;x=y\r\nabc\r\n2\r\nde\r\n0\r\nT: 1\r\n\r\n".to_vec(),
            b"HTTP/1.1 206 Partial\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
        ]);
        let mut app = channel("*");
        let (head, body) = ask(&mut app, &format!("GET {url}/a\nUser-Agent: test\nHost: evil.example\nConnection: close\n"), b"").unwrap();
        assert_eq!((head.as_str(), body.as_slice()), ("200\ncontent-length: 5\nx-thing: a b", &b"hello"[..]));
        assert_eq!(ask(&mut app, &format!("POST {url}/b\nContent-Type: text/plain\n"), b"body").unwrap().1, b"abcde");
        let (head, body) = ask(&mut app, &format!("GET {url}/c\nRange: bytes=0-1\n"), b"").unwrap();
        assert!(head.starts_with("206\n") && body == b"ok");
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 3, "one connection served all three");
        // The app cannot set the target or the framing.
        assert!(seen[0].starts_with("GET /a HTTP/1.1\r\nHost: 127.0.0.1:") && seen[0].contains("User-Agent: test\r\n"));
        assert!(!seen[0].contains("evil.example") && !seen[0].contains("Connection: close"));
        assert!(seen[1].contains("Content-Length: 4\r\n") && seen[1].contains("Content-Type: text/plain\r\n"));
        assert!(seen[2].contains("Range: bytes=0-1\r\n"));
    }

    #[test]
    fn errors_keep_the_channel() {
        let (url, _server) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nk".to_vec()]);
        let mut app = channel("*");
        assert!(ask(&mut app, "GET ftp://x/\n", b"").unwrap_err().contains("not an http"));
        assert!(ask(&mut app, "get http://x/\n", b"").unwrap_err().contains("bad method"));
        assert!(ask(&mut app, &format!("GET {url}/\nBad Header: x\n"), b"").unwrap_err().contains("bad header"));
        assert!(ask(&mut app, &format!("GET {url}/\nX: a\rb\n"), b"").unwrap_err().contains("bad header"));
        assert!(ask(&mut app, "GET http://127.0.0.1:1/\n", b"").is_err(), "nothing listens there");
        assert_eq!(ask(&mut app, &format!("GET {url}/\n"), b"").unwrap().1, b"k");
    }

    #[test]
    fn hosts_are_enforced_on_requests_and_redirects() {
        let mut app = channel("allowed.example");
        assert!(ask(&mut app, "GET http://127.0.0.1:9/\n", b"").unwrap_err().contains("may reach"));
        // A redirect to a host outside the list is refused too (localhost resolves locally).
        let (url, _server) = serve(vec![b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/x\r\nContent-Length: 0\r\n\r\n".to_vec()]);
        let mut app = channel("localhost");
        let via_name = url.replace("127.0.0.1", "localhost");
        assert!(ask(&mut app, &format!("GET {via_name}/\n"), b"").unwrap_err().contains("may reach"));
    }

    #[test]
    fn follows_redirects_on_the_same_connection() {
        let (url, server) = serve(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 4\r\n\r\ngone".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nhere".to_vec(),
        ]);
        let mut app = channel("*");
        assert_eq!(ask(&mut app, &format!("GET {url}/first\n"), b"").unwrap().1, b"here");
        let seen = server.join().unwrap();
        assert!(seen[1].starts_with("GET /next HTTP/1.1"));
    }

    #[test]
    fn reconnects_when_a_kept_connection_was_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/x", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 512];
                let _ = socket.read(&mut buffer);
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nk").unwrap();
                // then close without "Connection: close" (an idle timeout on the server)
            }
        });
        let mut app = channel("*");
        assert_eq!(ask(&mut app, &format!("GET {url}\n"), b"").unwrap().1, b"k");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(ask(&mut app, &format!("GET {url}\n"), b"").unwrap().1, b"k");
    }

    #[test]
    fn a_long_poll_streams_and_ends_when_the_app_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/poll", listener.local_addr().unwrap().port());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 512];
            let _ = socket.read(&mut buffer);
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n").unwrap();
            // Silent from here on: the read returns when the service drops the connection.
            let started = Instant::now();
            let closed = socket.read(&mut buffer).unwrap_or(0) == 0;
            (closed, started.elapsed())
        });
        let mut app = channel("*");
        let head = format!("GET {url} silence=120\n");
        let mut message = Vec::new();
        message.extend_from_slice(&(head.len() as u32).to_le_bytes());
        message.extend_from_slice(head.as_bytes());
        message.extend_from_slice(&0u32.to_le_bytes());
        app.write_all(&message).unwrap();
        assert_eq!(read_frame(&mut app).unwrap().0, FRAME_HEAD);
        assert_eq!(read_frame(&mut app).unwrap(), (FRAME_DATA, b"first".to_vec()));
        app.shutdown(std::net::Shutdown::Both).unwrap();
        let (closed, took) = server.join().unwrap();
        assert!(closed && took < Duration::from_secs(5), "the service dropped the server connection at once ({took:?})");
    }
}
