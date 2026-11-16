//! wpa_supplicant control interface client (unix datagram sockets in /run/wpa_supplicant).
//! One socket for requests, one ATTACHed socket for unsolicited events ("<3>CTRL-EVENT-...").

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::Duration;

pub const CTRL_DIR: &str = "/run/wpa_supplicant";

pub struct Ctrl {
    req: UnixDatagram,
    ev: UnixDatagram,
    paths: [PathBuf; 2],
}

impl Ctrl {
    pub fn open(iface: &str) -> io::Result<Ctrl> {
        let target = format!("{CTRL_DIR}/{iface}");
        let mk = |tag: &str| -> io::Result<(UnixDatagram, PathBuf)> {
            let p = PathBuf::from(format!("/run/netd/wpa-{iface}-{tag}"));
            let _ = std::fs::remove_file(&p);
            let s = UnixDatagram::bind(&p)?;
            s.connect(&target)?;
            s.set_read_timeout(Some(Duration::from_secs(3)))?;
            Ok((s, p))
        };
        let (req, p1) = mk("req")?;
        let (ev, p2) = mk("ev")?;
        let c = Ctrl { req, ev, paths: [p1, p2] };
        let r = c.request_on(&c.ev, "ATTACH")?;
        if r.trim() != "OK" {
            return Err(io::Error::other(format!("ATTACH: {r}")));
        }
        c.ev.set_nonblocking(true)?;
        Ok(c)
    }

    fn request_on(&self, s: &UnixDatagram, cmd: &str) -> io::Result<String> {
        s.send(cmd.as_bytes())?;
        let mut buf = vec![0u8; 16384];
        loop {
            let n = s.recv(&mut buf)?;
            let r = String::from_utf8_lossy(&buf[..n]).to_string();
            // Events can arrive on an attached socket between request and reply.
            if std::ptr::eq(s, &self.ev) && r.starts_with('<') {
                continue;
            }
            return Ok(r);
        }
    }

    pub fn request(&self, cmd: &str) -> io::Result<String> {
        self.request_on(&self.req, cmd)
    }

    /// A request that must answer "OK".
    pub fn ok(&self, cmd: &str) -> Result<(), String> {
        match self.request(cmd) {
            Ok(r) if r.trim() == "OK" => Ok(()),
            Ok(r) => Err(format!("{}: {}", cmd.split(' ').next().unwrap_or(cmd), r.trim())),
            Err(e) => Err(format!("{cmd}: {e}")),
        }
    }

    /// Pending events, without the "<level>" prefix.
    pub fn events(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = self.ev.recv(&mut buf) {
            let s = String::from_utf8_lossy(&buf[..n]).to_string();
            let s = match s.find('>') {
                Some(i) if s.starts_with('<') => s[i + 1..].to_string(),
                _ => s,
            };
            out.push(s);
        }
        out
    }
}

impl Drop for Ctrl {
    fn drop(&mut self) {
        let _ = self.ev.send(b"DETACH");
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// key=value reply (STATUS, SIGNAL_POLL).
pub fn kv(reply: &str, key: &str) -> Option<String> {
    reply.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')).map(|v| v.to_string()))
}

/// wpa_supplicant's quoted-string config syntax, or hex for anything unusual.
pub fn quote(s: &str) -> String {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\') {
        format!("\"{s}\"")
    } else {
        crate::util::hex(s.as_bytes())
    }
}

/// SSIDs in SCAN_RESULTS use printf_encode escapes (\xNN, \\, \").
pub fn unescape_ssid(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'x' if i + 3 < b.len() => {
                    if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                        out.push(v);
                        i += 4;
                        continue;
                    }
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                c => {
                    out.push(c);
                    i += 2;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// SCAN_RESULTS: "bssid / frequency / signal level / flags / ssid", tab separated. Hidden
/// networks dropped; one entry per SSID (strongest).
pub fn parse_scan(reply: &str) -> Vec<crate::state::ScanItem> {
    let mut v: Vec<crate::state::ScanItem> = Vec::new();
    for l in reply.lines().skip(1) {
        let f: Vec<&str> = l.splitn(5, '\t').collect();
        if f.len() < 5 {
            continue;
        }
        let ssid = unescape_ssid(f[4]);
        if ssid.is_empty() || ssid.chars().all(|c| c == '\0') {
            continue;
        }
        let signal = f[2].parse().unwrap_or(-100);
        let freq = f[1].parse().unwrap_or(0);
        let secure = f[3].contains("WPA") || f[3].contains("RSN") || f[3].contains("WEP") || f[3].contains("SAE");
        match v.iter_mut().find(|i| i.ssid == ssid) {
            Some(i) if i.signal < signal => {
                i.signal = signal;
                i.freq = freq;
            }
            Some(_) => {}
            None => v.push(crate::state::ScanItem { ssid, signal, secure, freq }),
        }
    }
    v.sort_by(|a, b| b.signal.cmp(&a.signal));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_parse() {
        let r = "bssid / frequency / signal level / flags / ssid\n\
                 aa:bb:cc:dd:ee:01\t2412\t-40\t[WPA2-PSK-CCMP][ESS]\tHome\n\
                 aa:bb:cc:dd:ee:02\t2437\t-70\t[WPA2-PSK-CCMP][ESS]\tHome\n\
                 aa:bb:cc:dd:ee:03\t2462\t-60\t[ESS]\tCaf\\xc3\\xa9\n\
                 aa:bb:cc:dd:ee:04\t2462\t-60\t[ESS]\t\n";
        let v = parse_scan(r);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ssid, "Home");
        assert_eq!(v[0].signal, -40);
        assert!(v[0].secure);
        assert_eq!(v[1].ssid, "Café");
        assert!(!v[1].secure);
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("abc"), "\"abc\"");
        assert_eq!(quote("a\"b"), "612262");
        assert_eq!(kv("wpa_state=COMPLETED\nip_address=1.2.3.4\n", "ip_address").as_deref(), Some("1.2.3.4"));
    }
}
