//! Sets the wall clock the moment the dongle has a route to the internet.
//!
//! The board has no RTC, so every boot starts in 1970 and nothing that uses TLS (the cloud
//! link, the store) can start before the clock is right. BusyBox `ntpd` (started by the DHCP
//! script) gets there, but late: it wants two answers from a server before it believes it,
//! waits 16 s for an answer that got lost (the first packets after a Wi-Fi association often
//! are) and tries a host name again only seconds after a failed lookup.
//!
//! This thread does the first step itself: as soon as a default route exists it asks several
//! servers at once (by fixed address, so a slow resolver does not matter, plus the gateway
//! and the usual names), repeats quickly, and steps the clock on the first sane answer.
//! `ntpd` keeps running and disciplines the clock from there.

use crate::util::log;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Anycast addresses of public NTP services (time.cloudflare.com, time.google.com): asked
/// without a name lookup.
const FIXED: [Ipv4Addr; 4] = [Ipv4Addr::new(162, 159, 200, 1), Ipv4Addr::new(216, 239, 35, 0), Ipv4Addr::new(162, 159, 200, 123), Ipv4Addr::new(216, 239, 35, 4)];
const NAMES: [&str; 3] = ["time.apple.com", "pool.ntp.org", "time.cloudflare.com"];
/// Seconds between 1900 (NTP) and 1970 (Unix).
const NTP_UNIX: u64 = 2_208_988_800;
/// 2026-01-01: a clock before this has not been set; an answer before this is not believed.
pub const SANE: u64 = 1_767_225_600;

pub fn clock_set() -> bool {
    crate::util::now_unix() > SANE
}

/// The gateway of the default route (/proc/net/route), if there is one.
pub fn gateway() -> Option<Ipv4Addr> {
    parse_gateway(&std::fs::read_to_string("/proc/net/route").ok()?)
}

fn parse_gateway(route: &str) -> Option<Ipv4Addr> {
    route.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 || f[1] != "00000000" {
            return None;
        }
        // Little-endian hex of the address in network order.
        u32::from_str_radix(f[2], 16).ok().map(|g| Ipv4Addr::from(g.to_le_bytes()))
    })
}

/// A client request (mode 3, version 4) whose transmit timestamp is `cookie`: the server
/// echoes it as the originate timestamp, which ties an answer to this request.
fn request(cookie: [u8; 8]) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = 0x23;
    p[40..48].copy_from_slice(&cookie);
    p
}

fn stamp(b: &[u8]) -> f64 {
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
    let frac = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as f64 / 4_294_967_296.0;
    // Era 1 starts in 2036: small values are after the wrap.
    let secs = if secs < 0x8000_0000 { secs + (1 << 32) } else { secs };
    secs.saturating_sub(NTP_UNIX) as f64 + frac
}

/// The server's (receive, transmit) times as Unix seconds, if `p` is a usable answer to the
/// request with `cookie`.
fn parse(p: &[u8], cookie: [u8; 8]) -> Option<(f64, f64)> {
    if p.len() < 48 {
        return None;
    }
    let (leap, mode, stratum) = (p[0] >> 6, p[0] & 7, p[1]);
    // Server answers only; unsynchronised servers (leap 3, stratum 0 "kiss of death" or 16) not.
    if mode != 4 || leap == 3 || stratum == 0 || stratum > 15 || p[24..32] != cookie {
        return None;
    }
    let (rx, tx) = (stamp(&p[32..40]), stamp(&p[40..48]));
    (tx > SANE as f64 && rx > SANE as f64 && (tx - rx).abs() < 10.0).then_some((rx, tx))
}

/// One round: ask `fixed` at once and the addresses of `names` as they resolve, again every
/// `every`, until an answer arrives or `budget` runs out. Returns the Unix time of the moment
/// of return (from the answer, corrected for half the round trip) and who gave it.
fn ask(fixed: &[SocketAddr], names: &[String], every: Duration, budget: Duration) -> Option<(f64, SocketAddr)> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(50)));
    // Name lookups block (and can take the resolver's whole timeout): one thread each.
    let (tx, rx) = mpsc::channel::<SocketAddr>();
    for n in names {
        let (tx, n) = (tx.clone(), n.clone());
        std::thread::spawn(move || {
            if let Ok(addrs) = n.as_str().to_socket_addrs() {
                for a in addrs.filter(|a| a.is_ipv4()).take(2) {
                    let _ = tx.send(a);
                }
            }
        });
    }
    drop(tx);
    let mut targets: Vec<SocketAddr> = fixed.to_vec();
    // (cookie, when it was sent, to whom)
    let mut sent: Vec<([u8; 8], Instant, SocketAddr)> = Vec::new();
    let start = Instant::now();
    let mut next = start;
    let mut buf = [0u8; 128];
    while start.elapsed() < budget {
        while let Ok(a) = rx.try_recv() {
            if !targets.contains(&a) {
                targets.push(a);
                next = Instant::now(); // ask the new one now
            }
        }
        if Instant::now() >= next {
            for t in &targets {
                let mut cookie = [0u8; 8];
                crate::util::random_bytes(&mut cookie);
                if sock.send_to(&request(cookie), t).is_ok() {
                    sent.push((cookie, Instant::now(), *t));
                }
            }
            if sent.len() > 256 {
                sent.drain(..sent.len() - 256);
            }
            next = Instant::now() + every;
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let got = Instant::now();
        let Some((_, at, _)) = sent.iter().find(|(c, _, to)| *to == from && buf[..n].len() >= 32 && buf[24..32] == *c) else { continue };
        let cookie: [u8; 8] = buf[24..32].try_into().unwrap();
        let Some((srx, stx)) = parse(&buf[..n], cookie) else { continue };
        // Round trip without the server's own processing time; the answer left the server
        // half of it ago.
        let trip = (got.duration_since(*at).as_secs_f64() - (stx - srx)).max(0.0);
        if trip > 3.0 {
            continue; // a stale answer says little
        }
        return Some((stx + trip / 2.0 + got.elapsed().as_secs_f64(), from));
    }
    None
}

fn set_clock(unix: f64) -> std::io::Result<()> {
    let ts = libc::timespec { tv_sec: unix.floor() as libc::time_t, tv_nsec: ((unix - unix.floor()) * 1e9) as _ };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn now_f64() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// The daemon's thread: wait for a default route, set the clock, then watch for the next
/// network (a new route after none) and check again.
pub fn run() {
    let mut had_route = false;
    let mut tries = 0u32;
    loop {
        let gw = gateway();
        let route = gw.is_some();
        let appeared = route && !had_route;
        had_route = route;
        if !route {
            tries = 0;
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if !appeared && clock_set() {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let t0 = Instant::now();
        let mut fixed: Vec<SocketAddr> = FIXED.iter().map(|a| SocketAddr::new(IpAddr::V4(*a), 123)).collect();
        // Home routers often answer NTP themselves, and the gateway is the nearest server.
        fixed.extend(gw.filter(|g| !g.is_unspecified()).map(|g| SocketAddr::new(IpAddr::V4(g), 123)));
        let names: Vec<String> = NAMES.iter().map(|n| format!("{n}:123")).collect();
        // Quick repeats at first (the first packets on a new link get lost), slower when the
        // network evidently has no way out yet.
        let (every, budget) = if tries < 4 { (Duration::from_millis(300), Duration::from_secs(5)) } else { (Duration::from_secs(2), Duration::from_secs(10)) };
        tries += 1;
        match ask(&fixed, &names, every, budget) {
            Some((unix, from)) => {
                let off = unix - now_f64();
                // ntpd slews small errors itself; stepping under it would only disturb it.
                if off.abs() < 0.5 {
                    log(format!("netd: clock is right (offset {off:+.3} s, {} after {:.2} s)", from.ip(), t0.elapsed().as_secs_f64()));
                } else {
                    match set_clock(unix) {
                        Ok(()) => log(format!("netd: clock set from {} {:.2} s after the route came up (step {off:+.1} s)", from.ip(), t0.elapsed().as_secs_f64())),
                        Err(e) => log(format!("netd: clock: cannot set it: {e}")),
                    }
                }
                tries = 0;
            }
            None if tries == 1 || tries % 8 == 0 => log("netd: clock: no time server answered yet"),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntp(unix: f64) -> [u8; 8] {
        let secs = (unix.floor() as u64 + NTP_UNIX) as u32;
        let frac = ((unix - unix.floor()) * 4_294_967_296.0) as u32;
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&secs.to_be_bytes());
        b[4..].copy_from_slice(&frac.to_be_bytes());
        b
    }

    fn reply(req: &[u8], unix: f64, first: u8, stratum: u8) -> [u8; 48] {
        let mut r = [0u8; 48];
        r[0] = first;
        r[1] = stratum;
        r[24..32].copy_from_slice(&req[40..48]);
        r[32..40].copy_from_slice(&ntp(unix));
        r[40..48].copy_from_slice(&ntp(unix));
        r
    }

    /// A server on loopback that says it is `unix`, ignoring the first `drop` requests.
    fn server(unix: f64, drop: usize, first: u8, stratum: u8) -> SocketAddr {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = s.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 128];
            let mut seen = 0;
            while let Ok((n, peer)) = s.recv_from(&mut buf) {
                seen += 1;
                if n >= 48 && seen > drop {
                    let _ = s.send_to(&reply(&buf[..n], unix, first, stratum), peer);
                }
            }
        });
        addr
    }

    #[test]
    fn gateway_from_route_table() {
        let t = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                 wlan0\t0002A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
                 wlan0\t00000000\t0102A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(parse_gateway(t), Some(Ipv4Addr::new(192, 168, 2, 1)));
        assert_eq!(parse_gateway(t.lines().take(2).collect::<Vec<_>>().join("\n").as_str()), None);
    }

    #[test]
    fn timestamps() {
        // 2026-11-15 20:00:00 UTC.
        let t = 1_794_772_800.25;
        assert!((stamp(&ntp(t)) - t).abs() < 1e-6);
        // After the 2036 wrap of the 32-bit seconds.
        let late = 2_100_000_000.0;
        assert!((stamp(&ntp(late)) - late).abs() < 1e-6);
    }

    #[test]
    fn answers_are_checked() {
        let cookie = [7u8; 8];
        let req = request(cookie);
        let t = 1_794_772_800.0;
        assert!(parse(&reply(&req, t, 0x24, 2), cookie).is_some());
        assert!(parse(&reply(&req, t, 0x24, 2), [8u8; 8]).is_none(), "another request's answer");
        assert!(parse(&reply(&req, t, 0xe4, 2), cookie).is_none(), "unsynchronised server");
        assert!(parse(&reply(&req, t, 0x24, 0), cookie).is_none(), "kiss of death");
        assert!(parse(&reply(&req, t, 0x23, 2), cookie).is_none(), "not a server answer");
        assert!(parse(&reply(&req, 1_000_000.0, 0x24, 2), cookie).is_none(), "a server in 1970");
        assert!(parse(&reply(&req, t, 0x24, 2)[..40], cookie).is_none());
    }

    #[test]
    fn first_answer_wins_and_lost_packets_are_repeated() {
        let t = 1_794_772_800.0;
        // One server never answers usefully, one drops the first two requests.
        let dead = server(t, usize::MAX, 0x24, 2);
        let unsync = server(t + 500.0, 0, 0xe4, 16);
        let slow = server(t, 2, 0x24, 2);
        let t0 = Instant::now();
        let (unix, from) = ask(&[dead, unsync, slow], &[], Duration::from_millis(100), Duration::from_secs(5)).expect("an answer");
        assert_eq!(from, slow);
        assert!((unix - t).abs() < 0.5, "{unix}");
        assert!(t0.elapsed() < Duration::from_secs(2), "took {:?}", t0.elapsed());
    }

    #[test]
    fn names_are_asked_too_and_silence_ends() {
        let t = 1_794_772_800.0;
        let s = server(t, 0, 0x24, 1);
        let (unix, from) = ask(&[], &[format!("localhost:{}", s.port())], Duration::from_millis(100), Duration::from_secs(5)).expect("an answer");
        assert_eq!(from.port(), s.port());
        assert!((unix - t).abs() < 0.5);
        let dead = server(t, usize::MAX, 0x24, 2);
        assert!(ask(&[dead], &[], Duration::from_millis(50), Duration::from_millis(300)).is_none());
    }
}
