//! The pilot server (HTTP/1.1, port 80 on every interface): the pilot web app, its API, the
//! WebSocket key channel into the UI, and captive-portal redirects on the setup network.
//!
//! Authorisation: a paired pilot token (X-Pilot-Token header or ?k=), or, for Wi-Fi setup only,
//! being on the setup network (whose password only the TV shows). Pairing needs the TV's code.

use crate::state::{Shared, Wifi};
use crate::util::{self, log};
use crate::wifi::{self, Cmd};
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_CONNS: usize = 24;
const MAX_HEAD: usize = 8192;
const MAX_BODY: usize = 8192;
pub const UI_KEYS: &str = "/run/ui-keys";
const LAN_PILOT_TIMEOUT: Duration = Duration::from_secs(12);

pub struct Ctx {
    pub shared: Shared,
    pub wifi: Mutex<Sender<Cmd>>,
    pub web: String,
    conns: AtomicUsize,
}

impl Ctx {
    pub fn new(shared: Shared, wifi: Sender<Cmd>, web: String) -> Arc<Ctx> {
        Arc::new(Ctx { shared, wifi: Mutex::new(wifi), web, conns: AtomicUsize::new(0) })
    }
}

pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: IpAddr,
}

impl Req {
    pub fn header(&self, name: &str) -> &str {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    }
    pub fn param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then(|| pct_decode(v))
        })
    }
    fn token(&self) -> String {
        let h = self.header("x-pilot-token");
        if h.is_empty() { self.param("k").unwrap_or_default() } else { h.to_string() }
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub fn pct_decode(s: &str) -> String {
    let hexval = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 3 <= b.len() => match (hexval(b[i + 1]), hexval(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn serve(ctx: Arc<Ctx>, port: u16) {
    let listener = loop {
        match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => break l,
            Err(e) => {
                log(format!("netd: http :{port}: {e}; retrying"));
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    log(format!("netd: pilot server on :{port} (web root {})", ctx.web));
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        if ctx.conns.load(Ordering::SeqCst) >= MAX_CONNS {
            continue; // dropped: closes the socket
        }
        ctx.conns.fetch_add(1, Ordering::SeqCst);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = connection(&ctx, s);
            ctx.conns.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn connection(ctx: &Ctx, s: TcpStream) -> io::Result<()> {
    let peer = s.peer_addr()?.ip();
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    s.set_nodelay(true)?;
    let mut w = s.try_clone()?;
    let mut r = BufReader::new(s);
    loop {
        let Some(req) = read_request(&mut r, peer)? else { return Ok(()) };
        let close = req.header("connection").eq_ignore_ascii_case("close");
        if req.path == "/ws" && req.header("upgrade").eq_ignore_ascii_case("websocket") {
            return websocket(ctx, &req, r, w);
        }
        route(ctx, &req, &mut w)?;
        if close {
            return Ok(());
        }
    }
}

fn read_request(r: &mut BufReader<TcpStream>, peer: IpAddr) -> io::Result<Option<Req>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = r.by_ref().take(MAX_HEAD as u64).read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        if line == b"\r\n" || line == b"\n" {
            if head.is_empty() {
                continue;
            }
            break;
        }
        head.push(String::from_utf8_lossy(&line).trim_end().to_string());
        if head.iter().map(|l| l.len()).sum::<usize>() > MAX_HEAD {
            return Err(io::Error::other("header too large"));
        }
    }
    let mut first = head[0].split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, String)> = head[1..]
        .iter()
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())))
        .collect();
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err(io::Error::other("body too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(Req { method, path: pct_decode(path), query: query.to_string(), headers, body, peer }))
}

fn send(w: &mut TcpStream, status: &str, ctype: &str, extra: &str, body: &[u8], head_only: bool) -> io::Result<()> {
    let hdr = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n{extra}X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\r\n",
        body.len()
    );
    w.write_all(hdr.as_bytes())?;
    if !head_only {
        w.write_all(body)?;
    }
    w.flush()
}

fn send_json(w: &mut TcpStream, status: &str, v: &Value) -> io::Result<()> {
    send(w, status, "application/json", "Cache-Control: no-store\r\n", v.to_string().as_bytes(), false)
}

fn route(ctx: &Ctx, req: &Req, w: &mut TcpStream) -> io::Result<()> {
    let setup_peer = wifi::is_setup_peer(req.peer);
    let host = req.header("host").rsplit_once(':').map(|(h, _)| h).unwrap_or(req.header("host")).to_string();
    // Captive portal: on the setup network every other host name lands on the setup page. Our
    // own home-network address stays reachable (a phone still on the setup network can already
    // open the pairing link the TV shows once it is connected).
    let own = host == wifi::SETUP_IP || host == ctx.shared.0.lock().unwrap().lan_ip();
    if setup_peer && !host.is_empty() && !own {
        let loc = format!("http://{}/", wifi::SETUP_IP);
        return send(w, "302 Found", "text/html", &format!("Location: {loc}\r\nCache-Control: no-store\r\n"), format!("<a href=\"{loc}\">MEOW setup</a>").as_bytes(), false);
    }
    let token_ok = || crate::pair::token_valid(&req.token());
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/info") => {
            let paired = token_ok();
            let n = ctx.shared.0.lock().unwrap();
            let mut v = json!({
                "name": n.setup.as_ref().map(|s| s.ssid.clone()).unwrap_or_else(|| "MEOW".into()),
                "version": env!("CARGO_PKG_VERSION"),
                "via": if setup_peer { "setup" } else { "lan" },
                "paired": paired,
                "pilots": n.pilots,
                "ui": n.ui,
                "adapter": n.adapter.is_some(),
                "wifi": {"state": n.wifi.name(), "ssid": n.ssid, "ip": n.ip, "signal": n.signal, "error": n.error, "saved": n.saved},
                "setup": {"active": n.setup.is_some(), "ssid": n.setup.as_ref().map(|s| s.ssid.clone())},
                "cloud": {"state": n.cloud, "id": n.cloud_id, "app": n.cloud_app},
            });
            if (setup_peer || paired) && n.wifi == Wifi::Connected {
                v["pair_url"] = json!(n.pair_url());
            }
            drop(n);
            send_json(w, "200 OK", &v)
        }
        ("GET", "/api/wifi/scan") => {
            if !setup_peer && !token_ok() {
                return send_json(w, "401 Unauthorized", &json!({"ok": false, "error": "unauthorized"}));
            }
            let fresh = req.param("fresh").is_some();
            let (m, cv) = &*ctx.shared;
            let mut n = m.lock().unwrap();
            if fresh || util::uptime() - n.scan_at > 8.0 {
                let before = n.scan_at;
                let _ = ctx.wifi.lock().unwrap().send(Cmd::Scan);
                let t0 = Instant::now();
                while n.scan_at == before && t0.elapsed() < Duration::from_secs(6) {
                    n = cv.wait_timeout(n, Duration::from_millis(500)).unwrap().0;
                }
            }
            let nets: Vec<Value> = n.scan.iter().map(|s| json!({"ssid": s.ssid, "signal": s.signal, "secure": s.secure})).collect();
            let v = json!({"ok": true, "networks": nets, "current": n.ssid, "saved": n.saved});
            drop(n);
            send_json(w, "200 OK", &v)
        }
        ("POST", "/api/wifi/join") => {
            if !setup_peer && !token_ok() {
                return send_json(w, "401 Unauthorized", &json!({"ok": false, "error": "unauthorized"}));
            }
            let b = req.json();
            let ssid = b["ssid"].as_str().unwrap_or("").to_string();
            let psk = b["psk"].as_str().unwrap_or("").to_string();
            let psk_ok = psk.is_empty() || (8..=63).contains(&psk.len()) || (psk.len() == 64 && psk.bytes().all(|c| c.is_ascii_hexdigit()));
            if ssid.is_empty() || ssid.len() > 32 || !psk_ok {
                return send_json(w, "400 Bad Request", &json!({"ok": false, "error": if psk_ok { "bad_ssid" } else { "bad_password" }}));
            }
            let (tx, rx) = mpsc::channel();
            let _ = ctx.wifi.lock().unwrap().send(Cmd::Join { ssid: ssid.clone(), psk, reply: tx });
            let v = match rx.recv_timeout(Duration::from_secs(75)) {
                Ok(Ok(ip)) => {
                    let n = ctx.shared.0.lock().unwrap();
                    json!({"ok": true, "ssid": ssid, "ip": ip, "pair_url": n.pair_url()})
                }
                Ok(Err(e)) => json!({"ok": false, "error": e}),
                Err(_) => json!({"ok": false, "error": "timeout"}),
            };
            send_json(w, "200 OK", &v)
        }
        ("POST", "/api/wifi/forget") => {
            if !token_ok() {
                return send_json(w, "401 Unauthorized", &json!({"ok": false, "error": "unauthorized"}));
            }
            let _ = ctx.wifi.lock().unwrap().send(Cmd::Forget);
            send_json(w, "200 OK", &json!({"ok": true}))
        }
        ("POST", "/api/pair") => {
            let b = req.json();
            match crate::pair::pair(&ctx.shared, b["code"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or("")) {
                Ok(t) => send_json(w, "200 OK", &json!({"ok": true, "token": t})),
                Err(e) => {
                    log(format!("netd: pairing from {} refused: {e}", req.peer));
                    send_json(w, "403 Forbidden", &json!({"ok": false, "error": e}))
                }
            }
        }
        ("GET", "/api/pair/check") => {
            if token_ok() { send_json(w, "200 OK", &json!({"ok": true})) } else { send_json(w, "401 Unauthorized", &json!({"ok": false})) }
        }
        ("POST", "/api/debug/pair/start") | ("POST", "/api/debug/pair/finish") => {
            let b = req.json();
            let result = if req.path.ends_with("/start") { crate::debug::pair_start(&ctx.shared, &b) } else { crate::debug::pair_finish(&ctx.shared, &b) };
            match result {
                Ok(r) => send_json(w, "200 OK", &r),
                Err(e) => {
                    log(format!("netd: debug key from {} refused: {e}", req.peer));
                    send_json(w, "403 Forbidden", &json!({"ok": false, "error": e}))
                }
            }
        }
        ("GET", p) | ("HEAD", p) if !p.starts_with("/api/") => static_file(ctx, req, w),
        _ => send_json(w, "404 Not Found", &json!({"ok": false, "error": "not_found"})),
    }
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn static_file(ctx: &Ctx, req: &Req, w: &mut TcpStream) -> io::Result<()> {
    let head = req.method == "HEAD";
    let clean = req.path.trim_start_matches('/');
    let bad = clean.split('/').any(|c| c == ".." || c.starts_with('.')) || clean.contains('\0');
    let mut rel = if bad || clean.is_empty() { "index.html".to_string() } else { clean.to_string() };
    if !std::path::Path::new(&format!("{}/{rel}", ctx.web)).is_file() && !std::path::Path::new(&format!("{}/{rel}.gz", ctx.web)).is_file() {
        rel = "index.html".into(); // single-page app
    }
    let gz = req.header("accept-encoding").contains("gzip") && std::path::Path::new(&format!("{}/{rel}.gz", ctx.web)).is_file();
    let file = if gz { format!("{}/{rel}.gz", ctx.web) } else { format!("{}/{rel}", ctx.web) };
    let Ok(body) = std::fs::read(&file) else {
        return send(w, "404 Not Found", "text/plain", "", b"pilot app not installed", head);
    };
    let cache = if rel.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let extra = format!("Cache-Control: {cache}\r\n{}Vary: Accept-Encoding\r\n", if gz { "Content-Encoding: gzip\r\n" } else { "" });
    send(w, "200 OK", mime(&rel), &extra, &body, head)
}

// ---- WebSocket (RFC 6455): keys from the pilot into the UI ------------------------------

pub fn key_byte(k: &str) -> Option<u8> {
    Some(match k {
        "up" => b'w',
        "down" => b's',
        "left" => b'a',
        "right" => b'd',
        "ok" => b'e',
        "back" => b'b',
        "home" => b'm',
        _ => return None,
    })
}

/// The launcher's key FIFO (NETD_UI_KEYS: another path, for tests off the dongle).
fn ui_keys() -> String {
    std::env::var("NETD_UI_KEYS").unwrap_or_else(|_| UI_KEYS.into())
}

/// One byte into the UI's key FIFO; false when no UI is reading it.
pub fn press(b: u8) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(ui_keys()) {
        Ok(mut f) => f.write_all(&[b]).is_ok(),
        Err(_) => false,
    }
}

pub fn ui_up() -> bool {
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    // Opening a FIFO for writing without blocking fails (ENXIO) unless a reader has it open.
    std::fs::metadata(ui_keys()).is_ok_and(|m| m.file_type().is_fifo())
        && std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(ui_keys()).is_ok()
}

fn ws_frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | op];
    match payload.len() {
        n @ 0..=125 => f.push(n as u8),
        n @ 126..=65535 => {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(payload);
    f
}

/// Parse one client frame from `buf`: (fin, opcode, payload, bytes used).
pub fn ws_parse(buf: &[u8]) -> Result<Option<(bool, u8, Vec<u8>, usize)>, &'static str> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    let op = buf[0] & 0x0f;
    if buf[1] & 0x80 == 0 {
        return Err("unmasked client frame");
    }
    let (len, mut i) = match buf[1] & 0x7f {
        126 if buf.len() >= 4 => (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4),
        127 => return Err("frame too large"),
        126 => return Ok(None),
        n => (n as usize, 2),
    };
    if len > 4096 {
        return Err("frame too large");
    }
    if buf.len() < i + 4 + len {
        return Ok(None);
    }
    let mask = [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]];
    i += 4;
    let payload: Vec<u8> = buf[i..i + len].iter().enumerate().map(|(j, b)| b ^ mask[j % 4]).collect();
    Ok(Some((fin, op, payload, i + len)))
}

struct Online<'a>(&'a Shared);

impl Drop for Online<'_> {
    fn drop(&mut self) {
        {
            let mut n = self.0.0.lock().unwrap();
            n.online = n.online.saturating_sub(1);
        }
        crate::state::changed(self.0);
    }
}

fn websocket(ctx: &Ctx, req: &Req, r: BufReader<TcpStream>, mut w: TcpStream) -> io::Result<()> {
    let key = req.header("sec-websocket-key");
    if key.is_empty() {
        return send(&mut w, "400 Bad Request", "text/plain", "", b"", false);
    }
    let accept = util::base64(&util::sha1(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()), false);
    w.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes())?;
    if !crate::pair::token_valid(&req.token()) {
        // Close code 4401: the pilot shows "pair again".
        let mut p = 4401u16.to_be_bytes().to_vec();
        p.extend_from_slice(b"bad token");
        let _ = w.write_all(&ws_frame(8, &p));
        return Ok(());
    }
    {
        let mut n = ctx.shared.0.lock().unwrap();
        n.online += 1;
    }
    crate::state::changed(&ctx.shared);
    let _online = Online(&ctx.shared);
    log(format!("netd: pilot connected from {}", req.peer));
    crate::pilot::kick();
    let token = req.token();
    let mut paired = ctx.shared.0.lock().unwrap().pilots;
    let mut s = r.into_inner();
    // Anything already buffered by the HTTP reader belongs to the frame stream; BufReader
    // returned it with into_inner only if it was empty, so read fresh from here on.
    // A short timeout: the loop also forwards changes of the TV's state (volume, apps).
    s.set_read_timeout(Some(Duration::from_millis(250)))?;
    let text = |v: Value| ws_frame(1, v.to_string().as_bytes());
    let status = |up: bool| ws_frame(1, json!({"t": "status", "dongle": up}).to_string().as_bytes());
    let mut up = ui_up();
    w.write_all(&status(up))?;
    // What the TV shows now; changes follow when the sequence numbers move.
    let mut seen = crate::pilot::seq();
    w.write_all(&text(crate::pilot::state_msg()))?;
    w.write_all(&text(crate::pilot::apps_msg()))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut last_rx = Instant::now();
    let mut last_status = Instant::now();
    let mut chunk = [0u8; 2048];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                last_rx = Instant::now();
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
        loop {
            let (_fin, op, payload, used) = match ws_parse(&buf) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    log(format!("netd: websocket {}: {e}", req.peer));
                    return Ok(());
                }
            };
            buf.drain(..used);
            match op {
                1 => {
                    let m: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                    if m["t"] == "key" {
                        if let Some(ok) = m["k"].as_str().and_then(crate::pilot::key) {
                            // A navigation key nobody read: the launcher is not running.
                            let now_up = ui_up();
                            if !ok && now_up != up {
                                up = now_up;
                                w.write_all(&status(up))?;
                            }
                        }
                    } else if let Some(reply) = crate::pilot::handle(&m) {
                        w.write_all(&text(reply))?;
                    }
                }
                8 => {
                    let _ = w.write_all(&ws_frame(8, &payload[..payload.len().min(2)]));
                    return Ok(());
                }
                9 => w.write_all(&ws_frame(10, &payload))?,
                _ => {}
            }
        }
        let now = crate::pilot::seq();
        if now.0 != seen.0 {
            w.write_all(&text(crate::pilot::state_msg()))?;
        }
        if now.1 != seen.1 {
            w.write_all(&text(crate::pilot::apps_msg()))?;
        }
        seen = now;
        if last_status.elapsed() > Duration::from_secs(3) {
            last_status = Instant::now();
            let now_up = ui_up();
            if now_up != up {
                up = now_up;
                w.write_all(&status(up))?;
            }
            // "Unpair phones" on the TV: this phone has to pair again.
            let now_paired = ctx.shared.0.lock().unwrap().pilots;
            if now_paired != paired {
                paired = now_paired;
                if !crate::pair::token_valid(&token) {
                    let mut p = 4401u16.to_be_bytes().to_vec();
                    p.extend_from_slice(b"unpaired");
                    let _ = w.write_all(&ws_frame(8, &p));
                    return Ok(());
                }
            }
            // Keep NAT and the phone's radio aware of the connection; drop dead peers.
            w.write_all(&ws_frame(9, b""))?;
        }
        // A browser answers every ping (3 s apart) with a pong: a phone that says nothing for
        // this long has gone (out of range, asleep), and the TV stops showing it as connected.
        if last_rx.elapsed() > LAN_PILOT_TIMEOUT {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode() {
        assert_eq!(pct_decode("a%20b+c%2F"), "a b c/");
        assert_eq!(pct_decode("100%"), "100%");
        assert_eq!(pct_decode("%zz"), "%zz");
    }

    #[test]
    fn frames() {
        // "Hello" masked, from RFC 6455 section 5.7
        let f = [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58];
        let (fin, op, p, used) = ws_parse(&f).unwrap().unwrap();
        assert!(fin);
        assert_eq!(op, 1);
        assert_eq!(p, b"Hello");
        assert_eq!(used, f.len());
        assert!(ws_parse(&f[..5]).unwrap().is_none());
        assert!(ws_parse(&[0x81, 0x05, 1, 2, 3, 4, 5]).is_err());
        assert_eq!(ws_frame(1, b"Hi"), vec![0x81, 2, b'H', b'i']);
    }

    #[test]
    fn accept_key() {
        // RFC 6455 section 1.3
        let a = util::base64(&util::sha1(b"dGhlIHNhbXBsZSBub25jZQ==258EAFA5-E914-47DA-95CA-C5AB0DC85B11"), false);
        assert_eq!(a, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
