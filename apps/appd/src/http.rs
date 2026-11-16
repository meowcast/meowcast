//! Minimal HTTP/1.1 GET client (std only) for the store: http:// (development: `wrangler dev`)
//! and https:// through `meow-tls HOST [PORT]` (BearSSL, pinned root CAs, host name checked;
//! cloud/tls), which relays the TLS connection to its stdin/stdout.
//!
//! Integrity never depends on the transport: every manifest and the asset index are
//! signature-checked and every file is hash-checked. TLS adds confidentiality and keeps
//! networks from tampering with the (unsigned) app index.
//!
//! A `Client` keeps one connection open (keep-alive) to the origin it last used: a store
//! refresh is ~2 + 2 x apps small requests, and a TLS handshake costs far more than a request
//! on the dongle's Cortex-A7.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// No byte for this long: the request fails (and the connection is dropped).
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Connecting and the TLS handshake (meow-tls) together.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSED: &str = "connection closed";

#[derive(Debug, PartialEq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    fn origin(&self) -> String {
        format!("{}://{}:{}", if self.tls { "https" } else { "http" }, self.host, self.port)
    }
}

pub fn parse(url: &str) -> Result<Url, String> {
    let (tls, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return Err(format!("{url}: only http:// and https:// are supported")),
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| format!("{url}: bad port"))?),
        None => (hostport, if tls { 443 } else { 80 }),
    };
    let host_ok = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if !host_ok || path.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(format!("{url}: bad URL"));
    }
    Ok(Url { tls, host: host.to_string(), port, path: path.to_string() })
}

/// Resolve `rel` against the directory of `base` ("https://h/a/index.json" + "x/y" -> ".../a/x/y").
pub fn join(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let dir = match base.rfind('/') {
        Some(i) if i >= scheme_end => &base[..=i],
        _ => return format!("{base}/{}", rel.trim_start_matches('/')),
    };
    format!("{}{}", dir, rel.trim_start_matches('/'))
}

/// Why `url` cannot be fetched right now without trying (cheap: no thread, no socket), so the
/// daemon can poll for the network every second. None = go ahead.
pub fn not_ready(url: &str) -> Option<&'static str> {
    let tls = url.starts_with("https://");
    let route = std::fs::read_to_string("/proc/net/route")
        .map(|s| s.lines().skip(1).any(|l| l.split_whitespace().nth(1) == Some("00000000")))
        .unwrap_or(true);
    if !route {
        return Some("offline: no network");
    }
    // Certificates have validity dates: TLS needs NTP to have set the clock (no RTC).
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if tls && now < 1_767_225_600 {
        return Some("offline: clock not set yet (NTP)");
    }
    None
}

/// The meow-tls binary (Wi-Fi builds install it in /usr/sbin; APPD_TLS overrides, for tests).
fn tls_helper() -> String {
    std::env::var("APPD_TLS").unwrap_or_else(|_| if std::path::Path::new("/usr/sbin/meow-tls").exists() { "/usr/sbin/meow-tls".into() } else { "meow-tls".into() })
}

/// Read side with a deadline per read (poll), for both a socket and the meow-tls pipe.
struct Conn {
    origin: String,
    rd: Box<dyn Read + Send>,
    rd_fd: RawFd,
    wr: Box<dyn Write + Send>,
    child: Option<Child>,
    buf: Vec<u8>,
    pos: usize,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Conn {
    fn open(u: &Url) -> Result<Conn, String> {
        if u.tls {
            let mut c = Command::new(tls_helper())
                .arg(&u.host)
                .arg(u.port.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| format!("meow-tls: {e}"))?;
            let (r, w) = (c.stdout.take().unwrap(), c.stdin.take().unwrap());
            let rd_fd = r.as_raw_fd();
            Ok(Conn { origin: u.origin(), rd: Box::new(r), rd_fd, wr: Box::new(w), child: Some(c), buf: Vec::new(), pos: 0 })
        } else {
            let addr = (u.host.as_str(), u.port)
                .to_socket_addrs()
                .map_err(|e| format!("{}: {e}", u.host))?
                .next()
                .ok_or_else(|| format!("{}: no address", u.host))?;
            let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("{}: {e}", u.host))?;
            s.set_write_timeout(Some(Duration::from_secs(5))).ok();
            let r = s.try_clone().map_err(|e| e.to_string())?;
            let rd_fd = r.as_raw_fd();
            Ok(Conn { origin: u.origin(), rd: Box::new(r), rd_fd, wr: Box::new(s), child: None, buf: Vec::new(), pos: 0 })
        }
    }

    /// Refill the buffer: at least one byte or an error (EOF is an error here).
    fn fill(&mut self, timeout: Duration) -> Result<(), String> {
        if self.pos < self.buf.len() {
            return Ok(());
        }
        let mut p = libc::pollfd { fd: self.rd_fd, events: libc::POLLIN, revents: 0 };
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out".into());
            }
            let n = unsafe { libc::poll(&mut p, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if n > 0 {
                break;
            }
            if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        self.buf.resize(64 * 1024, 0);
        let n = self.rd.read(&mut self.buf).map_err(|e| e.to_string())?;
        self.buf.truncate(n);
        self.pos = 0;
        if n == 0 {
            return Err(CLOSED.into());
        }
        Ok(())
    }

    fn line(&mut self, timeout: Duration, cap: usize) -> Result<String, String> {
        let mut out = Vec::new();
        loop {
            self.fill(timeout)?;
            let avail = &self.buf[self.pos..];
            match avail.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    out.extend_from_slice(&avail[..i]);
                    self.pos += i + 1;
                    break;
                }
                None => {
                    out.extend_from_slice(avail);
                    self.pos = self.buf.len();
                }
            }
            if out.len() > cap {
                return Err("header line too long".into());
            }
        }
        if out.last() == Some(&b'\r') {
            out.pop();
        }
        String::from_utf8(out).map_err(|_| "header not text".into())
    }

    /// Exactly `n` bytes into `out` (n = None: until the peer closes).
    fn body(&mut self, n: Option<u64>, timeout: Duration, out: &mut Vec<u8>, cap: u64, progress: &mut dyn FnMut(u64)) -> Result<(), String> {
        let mut left = n;
        loop {
            if left == Some(0) {
                return Ok(());
            }
            match self.fill(timeout) {
                Ok(()) => {}
                Err(e) if n.is_none() && e == CLOSED => return Ok(()), // closed: the end
                Err(e) => return Err(e),
            }
            let avail = &self.buf[self.pos..];
            let take = left.map_or(avail.len(), |l| (l as usize).min(avail.len()));
            if (out.len() + take) as u64 > cap {
                return Err(format!("body larger than allowed ({cap})"));
            }
            out.extend_from_slice(&avail[..take]);
            self.pos += take;
            left = left.map(|l| l - take as u64);
            progress(out.len() as u64);
        }
    }
}

/// One keep-alive connection, reused while the origin stays the same.
#[derive(Default)]
pub struct Client {
    conn: Option<Conn>,
    /// Requests sent and connections opened (log lines, tests).
    pub requests: u32,
    pub connects: u32,
}

impl Client {
    pub fn new() -> Client {
        Client::default()
    }

    /// GET `url`, at most `cap` bytes of body. `progress(done, total)` is called while reading.
    pub fn get(&mut self, url: &str, cap: u64, mut progress: impl FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
        let u = parse(url)?;
        // A kept connection may have been closed by the server while idle: one retry on a
        // fresh connection if nothing of the response arrived.
        let reused = self.conn.as_ref().is_some_and(|c| c.origin == u.origin());
        match self.get_once(&u, cap, &mut progress) {
            Err((e, false)) if reused => {
                self.conn = None;
                self.get_once(&u, cap, &mut progress).map_err(|(e2, _)| format!("{url}: {e2} (after {e})"))
            }
            r => r.map_err(|(e, _)| format!("{url}: {e}")),
        }
    }

    /// Err((why, whether any response byte arrived)).
    fn get_once(&mut self, u: &Url, cap: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, (String, bool)> {
        if self.conn.as_ref().is_some_and(|c| c.origin != u.origin()) {
            self.conn = None;
        }
        let fresh = self.conn.is_none();
        if fresh {
            self.conn = Some(Conn::open(u).map_err(|e| (e, true))?);
            self.connects += 1;
        }
        self.requests += 1;
        let c = self.conn.as_mut().unwrap();
        let host = if u.port == if u.tls { 443 } else { 80 } { u.host.clone() } else { format!("{}:{}", u.host, u.port) };
        let req = format!("GET {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: meow-appd/{}\r\nAccept-Encoding: identity\r\nConnection: keep-alive\r\n\r\n", u.path, env!("CARGO_PKG_VERSION"));
        if let Err(e) = c.wr.write_all(req.as_bytes()).and_then(|_| c.wr.flush()) {
            self.conn = None;
            return Err((e.to_string(), false));
        }
        let first_timeout = if fresh { CONNECT_TIMEOUT + IDLE_TIMEOUT } else { IDLE_TIMEOUT };
        let r = read_response(c, first_timeout, cap, progress);
        match r {
            Ok((body, keep)) => {
                if !keep {
                    self.conn = None;
                }
                Ok(body)
            }
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }
}

/// Status line, headers and body. Ok((body, connection reusable)).
fn read_response(c: &mut Conn, first_timeout: Duration, cap: u64, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(Vec<u8>, bool), (String, bool)> {
    let status_line = c.line(first_timeout, 8192).map_err(|e| (e, false))?;
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| (format!("bad status line {status_line:?}"), true))?;
    let (mut length, mut chunked, mut close) = (None::<u64>, false, version == "HTTP/1.0");
    let mut header_bytes = 0;
    loop {
        let l = c.line(IDLE_TIMEOUT, 8192).map_err(|e| (e, true))?;
        header_bytes += l.len() + 2;
        if header_bytes > 16 * 1024 {
            return Err(("headers too large".into(), true));
        }
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "content-length" => length = Some(v.parse().map_err(|_| ("bad content-length".to_string(), true))?),
                "transfer-encoding" => chunked = v.to_ascii_lowercase().split(',').any(|t| t.trim() == "chunked"),
                "connection" => close |= v.eq_ignore_ascii_case("close"),
                _ => {}
            }
        }
    }
    if status != 200 {
        // Not worth draining for reuse: errors are rare.
        return Err((format!("HTTP {status}"), true));
    }
    if length.is_some_and(|l| l > cap) {
        return Err((format!("{} bytes is more than allowed ({cap})", length.unwrap()), true));
    }
    let mut body = Vec::with_capacity(length.unwrap_or(0).min(cap) as usize);
    let total = length;
    let mut report = |n: u64| progress(n, total);
    if chunked {
        loop {
            let size_line = c.line(IDLE_TIMEOUT, 1024).map_err(|e| (e, true))?;
            let hex = size_line.split(';').next().unwrap_or("").trim();
            let n = u64::from_str_radix(hex, 16).map_err(|_| (format!("bad chunk size {hex:?}"), true))?;
            if body.len() as u64 + n > cap {
                return Err((format!("body larger than allowed ({cap})"), true));
            }
            if n == 0 {
                // Trailers until the empty line.
                while !c.line(IDLE_TIMEOUT, 8192).map_err(|e| (e, true))?.is_empty() {}
                break;
            }
            c.body(Some(n), IDLE_TIMEOUT, &mut body, cap, &mut report).map_err(|e| (e, true))?;
            if !c.line(IDLE_TIMEOUT, 16).map_err(|e| (e, true))?.is_empty() {
                return Err(("bad chunk end".into(), true));
            }
        }
    } else {
        if length.is_none() {
            close = true; // body ends when the server closes
        }
        c.body(length, IDLE_TIMEOUT, &mut body, cap, &mut report).map_err(|e| (e, true))?;
    }
    Ok((body, !close))
}

/// One-shot GET (a fresh connection).
pub fn get(url: &str, cap: u64, progress: impl FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
    Client::new().get(url, cap, progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn urls() {
        let u = parse("https://store.example.org/index.json").unwrap();
        assert_eq!((u.tls, u.port, u.path.as_str()), (true, 443, "/index.json"));
        let u = parse("http://192.168.2.1:8787").unwrap();
        assert_eq!((u.tls, u.port, u.path.as_str()), (false, 8787, "/"));
        assert!(parse("ftp://x/").is_err());
        assert!(parse("https://ho st/").is_err());
        assert!(parse("https://h/a b").is_err());
        assert_eq!(join("https://h/a/index.json", "apps/x"), "https://h/a/apps/x");
        assert_eq!(join("https://h/", "/assets.json"), "https://h/assets.json");
        assert_eq!(join("https://h", "assets.json"), "https://h/assets.json");
        assert_eq!(join("https://h/a/", "https://o/x"), "https://o/x");
    }

    /// A server answering `responses` in order on ONE connection (keep-alive), then closing.
    fn serve(responses: Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<usize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut served = 0;
            for r in responses {
                let mut req = Vec::new();
                let mut b = [0u8; 1];
                while !req.ends_with(b"\r\n\r\n") {
                    if s.read(&mut b).unwrap() == 0 {
                        return served;
                    }
                    req.push(b[0]);
                }
                s.write_all(&r).unwrap();
                served += 1;
            }
            served
        });
        (url, h)
    }

    #[test]
    fn keep_alive_length_and_chunked() {
        let (url, h) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec(),
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;x=y\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec(),
        ]);
        let mut c = Client::new();
        assert_eq!(c.get(&join(&url, "a"), 100, |_, _| {}).unwrap(), b"hello");
        assert_eq!(c.get(&join(&url, "b"), 100, |_, _| {}).unwrap(), b"abcde");
        assert_eq!(c.get(&join(&url, "c"), 100, |_, _| {}).unwrap(), b"ok");
        assert_eq!((c.requests, c.connects), (3, 1));
        assert!(c.conn.is_none(), "Connection: close drops it");
        assert_eq!(h.join().unwrap(), 3);
    }

    #[test]
    fn limits_and_errors() {
        let (url, _h) = serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\n".to_vec()]);
        assert!(Client::new().get(&url, 10, |_, _| {}).unwrap_err().contains("more than allowed"));
        let (url, _h) = serve(vec![b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()]);
        assert!(Client::new().get(&url, 10, |_, _| {}).unwrap_err().contains("HTTP 404"));
        let (url, _h) = serve(vec![b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n20\r\n0123456789".to_vec()]);
        assert!(Client::new().get(&url, 10, |_, _| {}).unwrap_err().contains("larger than allowed"));
        // No length, no chunking: the body ends at close.
        let (url, _h) = serve(vec![b"HTTP/1.1 200 OK\r\n\r\nuntil-close".to_vec()]);
        assert_eq!(Client::new().get(&url, 100, |_, _| {}).unwrap(), b"until-close");
    }

    #[test]
    fn reconnects_when_a_kept_connection_was_closed() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/x", l.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = l.accept().unwrap();
                let mut b = [0u8; 512];
                let _ = s.read(&mut b);
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nk").unwrap();
                // then close without "Connection: close" (idle timeout on the server)
            }
        });
        let mut c = Client::new();
        assert_eq!(c.get(&url, 10, |_, _| {}).unwrap(), b"k");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(c.get(&url, 10, |_, _| {}).unwrap(), b"k");
        assert_eq!(c.connects, 2);
    }
}
