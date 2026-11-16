//! Cloud link: the dongle keeps one WebSocket open to the meowcast relay (Cloudflare Worker +
//! Durable Object, cloud/relay), so a phone can pair with it and use it as a remote
//! from any network: routers that isolate Wi-Fi clients, or a phone on mobile data. The pilot
//! app then runs from Cloudflare Pages and talks to the relay.
//!
//! - TLS: `meow-tls HOST` (BearSSL, pinned root CAs) as a child process; HTTP/WebSocket over
//!   its stdin/stdout. Needs the clock set (NTP), since certificates have validity dates.
//! - Identity: /data/net/cloud/id (128-bit, in the TV's QR link) and /data/net/cloud/key (256-bit,
//!   sent as X-Device-Key; the relay keeps its SHA-256 from the first connection).
//! - Messages from the relay: hello {pid, hash} -> auth {pid, ok} (hash = SHA-256 of a pilot
//!   token, checked against /data/net/pilots); key {k} -> the UI's key FIFO; pair {rid, code,
//!   name} -> pair-result (the same code/window/lockout rules as the LAN pairing).
//! - To the relay: status {dongle} (the launcher is up) on change and every 30 s.
//! - Pilot protocol 2 (pilot.rs: volume, apps, text, heartbeat), carried by the relay in two
//!   envelopes so that it needs no knowledge of the contents: msg {pid, m} is a message from an
//!   authorised phone; pilot {pid?, m} goes to that phone, or to every authorised phone. bye
//!   {pid}: the phone's connection to the relay closed. Older relays and pilots send neither;
//!   the plain key path keeps working with them.
//!
//! Config: /etc/meow/cloud.conf, overridden by /data/net/cloud.conf (relay=HOST, app=URL;
//! relay= empty switches the link off; plain=HOST:PORT talks unencrypted to a `wrangler dev`).

use crate::state::{self, Shared};
use crate::util::{self, log};
use crate::{http, pair, pilot};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const DIR: &str = "/data/net/cloud";
/// Internal message kinds on the session channel (outside the WebSocket opcode range).
const OUT_BIN: u8 = 0x40;
const OUT_TEXT: u8 = 0x41;
const READER_END: u8 = 0xff;

/// A developer's SSH connection through the relay (meowdbg --cloud): only while network
/// debugging is on; Dropbear on 127.0.0.1:22 authenticates the key end to end.
fn open_tunnel(shared: &Shared, cid: u32, hash: &str, out: mpsc::SyncSender<(u8, Vec<u8>)>) -> Result<Tunnel, &'static str> {
    let generation = {
        let n = shared.0.lock().unwrap();
        if !n.debug || !n.debug_running || !n.debug_cloud || util::uptime() >= n.debug_until {
            return Err("network debugging is off");
        }
        if !n.developers.iter().any(|d| util::ct_eq(hash.as_bytes(), d.token_hash.as_bytes())) { return Err("developer not enrolled"); }
        n.debug_generation
    };
    let s = TcpStream::connect(("127.0.0.1", 22)).map_err(|_| "ssh server not reachable")?;
    let _ = s.set_nodelay(true);
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
    let shared = shared.clone();
    let mut r = s.try_clone().map_err(|_| "socket")?;
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16384];
        loop {
            { let n = shared.0.lock().unwrap();
              if !n.debug || !n.debug_cloud || util::uptime() >= n.debug_until || n.debug_generation != generation { break; } }
            match r.read(&mut buf) {
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut p = Vec::with_capacity(n + 4);
                    p.extend_from_slice(&cid.to_be_bytes());
                    p.extend_from_slice(&buf[..n]);
                    if out.try_send((OUT_BIN, p)).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = r.shutdown(std::net::Shutdown::Both);
        // Control cleanup must survive a full data queue; send() waits for bounded
        // backpressure or returns when the cloud session's receiver is dropped.
        let _ = out.send((OUT_TEXT, json!({"t": "tunnel-close", "cid": cid}).to_string().into_bytes()));
    });
    Ok(Tunnel { stream: s })
}

/// A phone that is connected to the relay and whose token this dongle accepted.
struct CloudPilot {
    hash: String,
    last: Instant,
    /// It sends pings (pilot protocol 2): silence means it has gone, even if the relay still
    /// holds its socket (a phone that lost its network closes nothing).
    heartbeat: bool,
    live: bool,
}

/// The phones on the relay, by the relay's connection id.
#[derive(Default)]
struct CloudPilots(HashMap<String, CloudPilot>);

/// Three missed pings (the pilot sends one every 5 s).
const PILOT_SILENCE: Duration = Duration::from_secs(16);
const MAX_CLOUD_PILOTS: usize = 32;

impl CloudPilots {
    fn hello(&mut self, pid: &str, hash: &str, now: Instant) {
        if self.0.len() >= MAX_CLOUD_PILOTS && !self.0.contains_key(pid) {
            // A relay that never says bye: forget the one that has been silent the longest.
            if let Some(oldest) = self.0.iter().min_by_key(|(_, p)| p.last).map(|(k, _)| k.clone()) {
                self.0.remove(&oldest);
            }
        }
        self.0.insert(pid.to_string(), CloudPilot { hash: hash.to_string(), last: now, heartbeat: false, live: true });
    }

    /// Something arrived from `pid`. False: not a phone this dongle authorised.
    fn seen(&mut self, pid: &str, ping: bool, now: Instant) -> bool {
        match self.0.get_mut(pid) {
            Some(p) => {
                p.last = now;
                p.live = true; // a phone back from the background, on the same socket
                p.heartbeat |= ping;
                true
            }
            None => false,
        }
    }

    fn bye(&mut self, pid: &str) {
        self.0.remove(pid);
    }

    /// Phones with a heartbeat that went silent are no longer "connected" (they stay
    /// authorised: the same connection may wake up again).
    fn expire(&mut self, now: Instant) {
        for p in self.0.values_mut() {
            if p.heartbeat && now.duration_since(p.last) > PILOT_SILENCE {
                p.live = false;
            }
        }
    }

    fn live(&self) -> usize {
        self.0.values().filter(|p| p.live).count()
    }

    /// After "Unpair phones": the connection ids whose token is no longer paired.
    fn unpaired(&mut self, valid: impl Fn(&str) -> bool) -> Vec<String> {
        let gone: Vec<String> = self.0.iter().filter(|(_, p)| !valid(&p.hash)).map(|(k, _)| k.clone()).collect();
        for pid in &gone {
            self.0.remove(pid);
        }
        gone
    }
}

fn set_cloud_pilots(shared: &Shared, count: usize) {
    let changed = {
        let mut n = shared.0.lock().unwrap();
        let c = n.cloud_pilots != count;
        n.cloud_pilots = count;
        c
    };
    if changed {
        state::changed(shared);
    }
}

struct Tunnel { stream: TcpStream }
impl Drop for Tunnel {
    fn drop(&mut self) { let _ = self.stream.shutdown(std::net::Shutdown::Both); }
}

pub struct Config {
    pub relay: String,
    pub app: String,
    pub plain: String,
}

pub fn config() -> Config {
    let mut c = Config { relay: String::new(), app: String::new(), plain: String::new() };
    for f in ["/etc/meow/cloud.conf", "/data/net/cloud.conf"] {
        for l in std::fs::read_to_string(f).unwrap_or_default().lines() {
            match l.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
                Some(("relay", v)) => c.relay = v.to_string(),
                Some(("app", v)) => c.app = v.trim_end_matches('/').to_string(),
                Some(("plain", v)) => c.plain = v.to_string(),
                _ => {}
            }
        }
    }
    c
}

/// The device id and key, created on first use.
pub fn identity() -> Option<(String, String)> {
    util::mkdir_private(DIR);
    let get = |name: &str, bytes: usize| -> Option<String> {
        let p = format!("{DIR}/{name}");
        let v = util::read(&p);
        if v.len() == bytes * 2 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(v);
        }
        let mut b = vec![0u8; bytes];
        util::random_bytes(&mut b);
        let v = util::hex(&b);
        util::write_atomic(std::path::Path::new(&p), format!("{v}\n").as_bytes(), 0o600).ok()?;
        Some(v)
    };
    Some((get("id", 16)?, get("key", 32)?))
}

fn default_route() -> bool {
    std::fs::read_to_string("/proc/net/route")
        .map(|s| s.lines().skip(1).any(|l| l.split_whitespace().nth(1) == Some("00000000")))
        .unwrap_or(false)
}

fn clock_set() -> bool {
    crate::timesync::clock_set()
}

fn set_state(shared: &Shared, s: &str) {
    let changed = {
        let mut n = shared.0.lock().unwrap();
        let c = n.cloud != s;
        n.cloud = s.to_string();
        c
    };
    if changed {
        state::changed(shared);
    }
}

/// A client frame (masked, as RFC 6455 requires from clients).
fn frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | op];
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    }
    let mut mask = [0u8; 4];
    util::random_bytes(&mut mask);
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

/// One server frame (unmasked): (opcode, payload).
fn read_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h)?;
    let mut len = (h[1] & 0x7f) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    if len > 65536 {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut mask = [0u8; 4];
    if h[1] & 0x80 != 0 {
        r.read_exact(&mut mask)?;
    }
    let mut p = vec![0u8; len as usize];
    r.read_exact(&mut p)?;
    if h[1] & 0x80 != 0 {
        for (i, b) in p.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((h[0] & 0x0f, p))
}

pub fn run(shared: Shared) {
    let cfg = config();
    if cfg.relay.is_empty() && cfg.plain.is_empty() {
        log("netd: cloud link off (no relay configured)");
        set_state(&shared, "off");
        return;
    }
    let Some((id, key)) = identity() else {
        log("netd: cloud link: cannot store the device identity");
        set_state(&shared, "off");
        return;
    };
    {
        let mut n = shared.0.lock().unwrap();
        n.cloud_id = id.clone();
        n.cloud_app = cfg.app.clone();
    }
    set_state(&shared, "waiting");
    let mut backoff = 2u64;
    loop {
        if !default_route() || (cfg.plain.is_empty() && !clock_set()) {
            set_state(&shared, "waiting");
            // Short: the clock is set within a second of the route appearing (timesync.rs).
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        set_state(&shared, "connecting");
        let t0 = Instant::now();
        match session(&shared, &cfg, &id, &key) {
            Ok(()) => log("netd: cloud link closed"),
            Err(e) => log(format!("netd: cloud link: {e}")),
        }
        set_state(&shared, "waiting");
        if t0.elapsed() > Duration::from_secs(60) {
            backoff = 2;
        }
        std::thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(60);
    }
}

fn session(shared: &Shared, cfg: &Config, id: &str, key: &str) -> Result<(), String> {
    // Transport: meow-tls (verified TLS), or plain TCP to a development relay.
    let (host, mut rd, mut wr, mut child): (String, Box<dyn Read + Send>, Box<dyn Write + Send>, Option<std::process::Child>) = if cfg.plain.is_empty() {
        let mut c = Command::new("meow-tls")
            .arg(&cfg.relay)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("meow-tls: {e}"))?;
        let r = c.stdout.take().unwrap();
        let w = c.stdin.take().unwrap();
        (cfg.relay.clone(), Box::new(r), Box::new(w), Some(c))
    } else {
        let s = std::net::TcpStream::connect(&cfg.plain).map_err(|e| format!("{}: {e}", cfg.plain))?;
        let r = s.try_clone().map_err(|e| e.to_string())?;
        (cfg.plain.clone(), Box::new(r), Box::new(s), None)
    };
    let result = (|| -> Result<(), String> {
        let mut nonce = [0u8; 16];
        util::random_bytes(&mut nonce);
        let req = format!(
            "GET /device/{id} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\nX-Device-Key: {key}\r\nUser-Agent: meow-netd/{}\r\n\r\n",
            util::base64(&nonce, false),
            env!("CARGO_PKG_VERSION")
        );
        wr.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        wr.flush().map_err(|e| e.to_string())?;
        // Response head, byte by byte (frames may follow right behind it).
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            rd.read_exact(&mut b).map_err(|e| format!("handshake: {e}"))?;
            head.push(b[0]);
            if head.len() > 8192 {
                return Err("handshake: response too large".into());
            }
        }
        let status = BufReader::new(&head[..]).lines().next().and_then(|l| l.ok()).unwrap_or_default();
        if !status.contains(" 101 ") {
            return Err(format!("relay refused: {status}"));
        }
        log(format!("netd: cloud link up ({host}, device {id})"));
        set_state(shared, "online");

        // Reader thread -> channel; this thread sends status, pings and replies.
        let (tx, rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(64);
        let out_tx = tx.clone();
        let mut tunnels: HashMap<u32, Tunnel> = HashMap::new();
        std::thread::spawn(move || {
            while let Ok(f) = read_frame(&mut rd) {
                if tx.send(f).is_err() {
                    return;
                }
            }
            // The session also holds senders (tunnels): say explicitly that the relay went away.
            let _ = tx.send((READER_END, Vec::new()));
        });
        let send = |wr: &mut Box<dyn Write + Send>, v: Value| -> Result<(), String> {
            wr.write_all(&frame(1, v.to_string().as_bytes())).and_then(|_| wr.flush()).map_err(|e| e.to_string())
        };
        let mut up = http::ui_up();
        send(&mut wr, json!({"t": "status", "dongle": up}))?;
        let (mut last_status, mut last_ping) = (Instant::now(), Instant::now());
        // Application-level heartbeat: the relay's code answers {t:ping} with {t:pong}.
        let mut pong_due: Option<Instant> = None;
        let mut last_policy = Instant::now() - Duration::from_secs(10);
        let mut policy_generation = u64::MAX;
        let mut policy_enabled = false;
        let mut pilots = CloudPilots::default();
        let mut seen = pilot::seq();
        let mut paired = shared.0.lock().unwrap().pilots;
        loop {
            let (generation, enabled, hashes) = {
                let n = shared.0.lock().unwrap();
                (n.debug_generation, n.debug && n.debug_running && n.debug_cloud && util::uptime() < n.debug_until,
                 n.developers.iter().map(|d| d.token_hash.clone()).collect::<Vec<_>>())
            };
            if generation != policy_generation || !enabled { tunnels.clear(); }
            if generation != policy_generation || enabled != policy_enabled || (enabled && last_policy.elapsed() >= Duration::from_secs(5)) {
                send(&mut wr, json!({"t":"debug-policy", "enabled":enabled, "hashes":hashes, "generation":generation, "lease_ms":15_000}))?;
                policy_generation = generation;
                policy_enabled = enabled;
                last_policy = Instant::now();
            }
            // With a phone on the line the loop also carries the TV's state to it: look more often.
            match rx.recv_timeout(Duration::from_millis(if pilots.0.is_empty() { 1000 } else { 250 })) {
                Ok((1, p)) => {
                    let m: Value = serde_json::from_slice(&p).unwrap_or(Value::Null);
                    match m["t"].as_str() {
                        Some("pong") => pong_due = None,
                        Some("hello") => {
                            let ok = pair::hash_valid(m["hash"].as_str().unwrap_or(""));
                            if !ok {
                                log("netd: cloud pilot with an unknown token refused");
                            }
                            send(&mut wr, json!({"t": "auth", "pid": m["pid"], "ok": ok}))?;
                            if let (true, Some(pid)) = (ok, m["pid"].as_str()) {
                                pilots.hello(pid, m["hash"].as_str().unwrap_or(""), Instant::now());
                                // What the TV shows now (an older relay drops these).
                                send(&mut wr, json!({"t": "pilot", "pid": pid, "m": pilot::state_msg()}))?;
                                send(&mut wr, json!({"t": "pilot", "pid": pid, "m": pilot::apps_msg()}))?;
                                pilot::kick();
                            }
                        }
                        Some("key") => {
                            if let Some(pid) = m["pid"].as_str() {
                                pilots.seen(pid, false, Instant::now());
                            }
                            if let Some(k) = m["k"].as_str() {
                                pilot::key(k);
                            }
                        }
                        // A pilot-protocol message from an authorised phone.
                        Some("msg") => {
                            let inner = &m["m"];
                            if let Some(pid) = m["pid"].as_str().filter(|pid| pilots.seen(pid, inner["t"] == "ping", Instant::now())) {
                                if let Some(reply) = pilot::handle(inner) {
                                    send(&mut wr, json!({"t": "pilot", "pid": pid, "m": reply}))?;
                                }
                            }
                        }
                        Some("bye") => {
                            if let Some(pid) = m["pid"].as_str() {
                                pilots.bye(pid);
                            }
                        }
                        Some("tunnel-open") => {
                            let cid = m["cid"].as_u64().unwrap_or(0) as u32;
                            let hash = m["hash"].as_str().unwrap_or("");
                            let result = if tunnels.len() >= 2 || tunnels.contains_key(&cid) { Err("too many tunnels") }
                                else { open_tunnel(shared, cid, hash, out_tx.clone()) };
                            match result {
                                Ok(stream) => {
                                    log(format!("netd: debug tunnel {cid:08x} opened through the relay"));
                                    tunnels.insert(cid, stream);
                                    send(&mut wr, json!({"t":"tunnel-ready", "cid":cid}))?;
                                }
                                Err(why) => {
                                    log(format!("netd: debug tunnel refused: {why}"));
                                    send(&mut wr, json!({"t": "tunnel-close", "cid": cid, "reason": why}))?;
                                }
                            }
                        }
                        Some("tunnel-close") => {
                            if let Some(t) = tunnels.remove(&(m["cid"].as_u64().unwrap_or(0) as u32)) {
                                let _ = t.stream.shutdown(std::net::Shutdown::Both);
                            }
                        }
                        Some("pair") => {
                            let r = pair::pair(shared, m["code"].as_str().unwrap_or(""), m["name"].as_str().unwrap_or("phone"));
                            let v = match r {
                                Ok(token) => json!({"t": "pair-result", "rid": m["rid"], "ok": true, "token": token}),
                                Err(e) => {
                                    log(format!("netd: cloud pairing refused: {e}"));
                                    json!({"t": "pair-result", "rid": m["rid"], "ok": false, "error": e})
                                }
                            };
                            send(&mut wr, v)?;
                        }
                        _ => {}
                    }
                }
                // Tunnel bytes from the relay: 4-byte channel id, then data for that SSH connection.
                Ok((2, p)) if p.len() >= 4 => {
                    let cid = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
                    let allowed = { let n = shared.0.lock().unwrap(); n.debug && n.debug_cloud && n.debug_generation == policy_generation && util::uptime() < n.debug_until };
                    let failed = !allowed || p.len() > 16388 || tunnels.get_mut(&cid).is_none_or(|t| t.stream.write_all(&p[4..]).is_err());
                    if failed && tunnels.remove(&cid).is_some() {
                        send(&mut wr, json!({"t": "tunnel-close", "cid": cid}))?;
                    }
                }
                // From tunnel threads: OUT_BIN = binary frame to the relay, OUT_TEXT = text frame.
                Ok((OUT_BIN, p)) => wr.write_all(&frame(2, &p)).and_then(|_| wr.flush()).map_err(|e| e.to_string())?,
                Ok((OUT_TEXT, p)) => {
                    let m: Value = serde_json::from_slice(&p).unwrap_or(Value::Null);
                    if let Some(cid) = m["cid"].as_u64() {
                        tunnels.remove(&(cid as u32));
                    }
                    wr.write_all(&frame(1, &p)).and_then(|_| wr.flush()).map_err(|e| e.to_string())?;
                }
                Ok((8, _)) => return Ok(()),
                Ok((READER_END, _)) => return Err("relay connection lost".into()),
                Ok((9, p)) => wr.write_all(&frame(10, &p)).map_err(|e| e.to_string())?,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err("relay connection lost".into()),
            }
            // The TV's state for every phone, when it changed.
            let now_seq = pilot::seq();
            if !pilots.0.is_empty() {
                if now_seq.0 != seen.0 {
                    send(&mut wr, json!({"t": "pilot", "m": pilot::state_msg()}))?;
                }
                if now_seq.1 != seen.1 {
                    send(&mut wr, json!({"t": "pilot", "m": pilot::apps_msg()}))?;
                }
            }
            seen = now_seq;
            // "Unpair phones" on the TV: the relay closes those phones (4401, "pair again").
            let now_paired = shared.0.lock().unwrap().pilots;
            if now_paired != paired {
                paired = now_paired;
                for pid in pilots.unpaired(pair::hash_valid) {
                    send(&mut wr, json!({"t": "auth", "pid": pid, "ok": false}))?;
                }
            }
            pilots.expire(Instant::now());
            set_cloud_pilots(shared, pilots.live());
            let now_up = http::ui_up();
            if now_up != up || last_status.elapsed() > Duration::from_secs(30) {
                up = now_up;
                last_status = Instant::now();
                send(&mut wr, json!({"t": "status", "dongle": up}))?;
            }
            // Keep NAT mappings alive; a dead link shows up as a write error.
            if last_ping.elapsed() > Duration::from_secs(25) {
                last_ping = Instant::now();
                wr.write_all(&frame(9, b"")).and_then(|_| wr.flush()).map_err(|e| e.to_string())?;
                send(&mut wr, json!({"t": "ping"}))?;
                pong_due.get_or_insert(Instant::now() + Duration::from_secs(20));
            }
            if pong_due.is_some_and(|d| Instant::now() > d) {
                return Err("relay stopped answering; reconnecting".into());
            }
        }
    })();
    // No link, no phones through it.
    set_cloud_pilots(shared, 0);
    if let Some(c) = child.as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let f = frame(1, b"hello");
        assert_eq!(f[0], 0x81);
        assert_eq!(f[1], 0x80 | 5);
        // A masked frame decodes with read_frame too.
        let (op, p) = read_frame(&mut &f[..]).unwrap();
        assert_eq!((op, p.as_slice()), (1, &b"hello"[..]));
        let server = [0x81u8, 2, b'h', b'i'];
        assert_eq!(read_frame(&mut &server[..]).unwrap(), (1, b"hi".to_vec()));
        let big = vec![7u8; 300];
        let (_, p) = read_frame(&mut &frame(2, &big)[..]).unwrap();
        assert_eq!(p, big);
    }
    #[test]
    fn cloud_pilot_presence() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut p = CloudPilots::default();
        assert!(!p.seen("a", false, t0)); // never authorised: its messages are dropped
        p.hello("a", "hash-a", t0);
        p.hello("b", "hash-b", t0);
        assert_eq!(p.live(), 2);
        // "a" is a protocol-2 pilot (pings); "b" is an older one that only sends keys.
        assert!(p.seen("a", true, t0 + s(5)));
        p.expire(t0 + s(60));
        assert_eq!(p.live(), 1, "a went silent; b has no heartbeat and counts until bye");
        // The same socket wakes up again (phone back from the background).
        assert!(p.seen("a", true, t0 + s(61)));
        assert_eq!(p.live(), 2);
        p.expire(t0 + s(70));
        assert_eq!(p.live(), 2);
        p.bye("b");
        assert_eq!(p.live(), 1);
        assert_eq!(p.unpaired(|h| h == "hash-b"), vec!["a".to_string()]);
        assert_eq!(p.live(), 0);
    }

    #[test]
    fn cloud_pilots_are_bounded() {
        let t0 = Instant::now();
        let mut p = CloudPilots::default();
        for i in 0..100u64 {
            p.hello(&format!("p{i}"), "h", t0 + Duration::from_secs(i));
        }
        assert_eq!(p.0.len(), MAX_CLOUD_PILOTS);
        assert!(p.0.contains_key("p99") && !p.0.contains_key("p0"));
    }

    #[test]
    fn dropped_tunnel_shuts_down_cloned_socket() {
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let _reader_clone = stream.try_clone().unwrap();
        drop(Tunnel { stream });
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
    }
    #[test]
    fn oversized_frame_is_rejected_before_allocating_body() {
        let mut frame = vec![0x82, 127];
        frame.extend_from_slice(&65537u64.to_be_bytes());
        assert!(read_frame(&mut &frame[..]).is_err());
    }
}
