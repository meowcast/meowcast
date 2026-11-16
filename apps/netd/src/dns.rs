//! Captive DNS for the setup network: every A query is answered with the dongle's setup
//! address, so a phone's connectivity check reaches our HTTP server, which redirects it to the
//! pilot setup page (the phone then shows "Sign in to network" / the captive portal sheet).
//! AAAA and other types get an empty answer; nothing is forwarded anywhere.

use crate::util::log;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub fn spawn(addr: Ipv4Addr, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        // The address appears on the interface a moment after the access point starts.
        let sock = loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match UdpSocket::bind((addr, 53)) {
                Ok(s) => break s,
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
        log(format!("netd: captive DNS on {addr}:53"));
        let mut buf = [0u8; 512];
        while !stop.load(Ordering::SeqCst) {
            let Ok((n, peer)) = sock.recv_from(&mut buf) else { continue };
            if let Some(r) = answer(&buf[..n], addr) {
                let _ = sock.send_to(&r, peer);
            }
        }
        log("netd: captive DNS stopped");
    });
}

/// The response to one query, or None for anything that is not a single-question query.
pub fn answer(q: &[u8], addr: Ipv4Addr) -> Option<Vec<u8>> {
    if q.len() < 12 || q[2] & 0x80 != 0 || u16::from_be_bytes([q[4], q[5]]) != 1 {
        return None;
    }
    // The question: labels up to the zero byte, then type and class.
    let mut i = 12;
    while i < q.len() && q[i] != 0 {
        if q[i] & 0xc0 != 0 {
            return None;
        }
        i += q[i] as usize + 1;
    }
    let end = i + 5;
    if end > q.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([q[i + 1], q[i + 2]]);
    let qclass = u16::from_be_bytes([q[i + 3], q[i + 4]]);
    let a = qtype == 1 && qclass == 1;
    let mut r = Vec::with_capacity(end + 16);
    r.extend_from_slice(&q[..2]); // id
    r.push(0x84 | (q[2] & 0x01)); // response, authoritative, copy RD
    r.push(0x00); // no error
    r.extend_from_slice(&[0, 1, 0, a as u8, 0, 0, 0, 0]);
    r.extend_from_slice(&q[12..end]);
    if a {
        r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 10, 0, 4]); // name ptr, A, IN, TTL 10 s
        r.extend_from_slice(&addr.octets());
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name.split('.') {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q
    }

    #[test]
    fn a_record() {
        let ip = Ipv4Addr::new(192, 168, 73, 1);
        let r = answer(&query("connectivitycheck.gstatic.com", 1), ip).unwrap();
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1);
        assert_eq!(&r[r.len() - 4..], &[192, 168, 73, 1]);
    }

    #[test]
    fn aaaa_empty_and_garbage() {
        let ip = Ipv4Addr::new(192, 168, 73, 1);
        let r = answer(&query("captive.apple.com", 28), ip).unwrap();
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0);
        assert!(answer(&[0; 5], ip).is_none());
        let mut resp = query("x.y", 1);
        resp[2] |= 0x80;
        assert!(answer(&resp, ip).is_none());
    }
}
