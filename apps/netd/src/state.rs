//! Shared daemon state and its export to the TV UI (/run/netd/state, key=value lines).

use crate::util;
use qrcodegen::{QrCode, QrCodeEcc};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

pub const STATE_FILE: &str = "/run/netd/state";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wifi {
    /// No wireless interface (no adapter, or the USB port is in device mode).
    None,
    /// Adapter present, wpa_supplicant starting.
    Starting,
    /// Running, no network saved.
    Idle,
    Connecting,
    /// Associated, waiting for a DHCP lease.
    Dhcp,
    Connected,
    /// Saved network not reachable or rejected (see `error`).
    Failed,
}

impl Wifi {
    pub fn name(self) -> &'static str {
        match self {
            Wifi::None => "none",
            Wifi::Starting => "starting",
            Wifi::Idle => "idle",
            Wifi::Connecting => "connecting",
            Wifi::Dhcp => "dhcp",
            Wifi::Connected => "connected",
            Wifi::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanItem {
    pub ssid: String,
    pub signal: i32,
    pub secure: bool,
    pub freq: u32,
}

#[derive(Clone, Debug)]
pub struct Setup {
    #[allow(dead_code)]
    pub iface: String,
    pub ssid: String,
    pub psk: String,
    pub ip: String,
    pub clients: u32,
}

pub struct Net {
    pub usb_role: String,
    pub adapter: Option<String>,
    pub wifi: Wifi,
    pub ssid: String,
    pub ip: String,
    pub signal: i32,
    pub freq: u32,
    pub error: String,
    pub saved: Vec<String>,
    pub setup: Option<Setup>,
    pub scan: Vec<ScanItem>,
    pub scan_at: f64,
    /// Paired pilots (/data/net/pilots).
    pub pilots: usize,
    /// Pilots connected right now: on the LAN WebSocket, and through the cloud relay.
    pub online: usize,
    pub cloud_pilots: usize,
    pub pair_code: String,
    pub pair_fails: u32,
    /// Pairing is accepted only while the TV shows the code (the UI opens the window), and
    /// not during a lockout after repeated wrong codes.
    pub pair_open_until: f64,
    pub pair_locked_until: f64,
    pub debug_locked_until: f64,
    pub debug: bool,
    pub debug_running: bool,
    pub debug_code: String,
    pub debug_fails: u32,
    pub debug_lan: bool,
    pub debug_cloud: bool,
    pub debug_until: f64,
    pub debug_pair_until: f64,
    pub debug_generation: u64,
    pub debug_pending: Option<crate::debug::Pending>,
    pub developers: Vec<crate::debug::Developer>,
    /// The UI's key FIFO has a reader (the launcher is up).
    pub ui: bool,
    /// Cloud link (cloud.rs): off | waiting | connecting | online; device id; pilot app URL.
    pub cloud: String,
    pub cloud_id: String,
    pub cloud_app: String,
    pub seq: u64,
}

pub type Shared = Arc<(Mutex<Net>, Condvar)>;

pub fn new() -> Shared {
    Arc::new((
        Mutex::new(Net {
            usb_role: String::new(),
            adapter: None,
            wifi: Wifi::None,
            ssid: String::new(),
            ip: String::new(),
            signal: 0,
            freq: 0,
            error: String::new(),
            saved: Vec::new(),
            setup: None,
            scan: Vec::new(),
            scan_at: 0.0,
            pilots: 0,
            online: 0,
            cloud_pilots: 0,
            pair_code: util::random_digits(6),
            pair_fails: 0,
            pair_open_until: 0.0,
            pair_locked_until: 0.0,
            debug_locked_until: 0.0,
            debug: false,
            debug_running: false,
            debug_code: String::new(),
            debug_fails: 0,
            debug_lan: false,
            debug_cloud: false,
            debug_until: 0.0,
            debug_pair_until: 0.0,
            debug_generation: 0,
            debug_pending: None,
            developers: Vec::new(),
            ui: false,
            cloud: String::new(),
            cloud_id: String::new(),
            cloud_app: String::new(),
            seq: 0,
        }),
        Condvar::new(),
    ))
}

impl Net {
    /// The address phones on the home network (or the Mac on USB) use to reach the dongle.
    pub fn lan_ip(&self) -> String {
        if self.wifi == Wifi::Connected && !self.ip.is_empty() {
            return self.ip.clone();
        }
        util::ipv4_of("usb0").map(|a| a.to_string()).unwrap_or_default()
    }

    /// Where the TV's pairing QR code points: the pilot app on Cloudflare Pages while the cloud
    /// link is up (works from any network), else the dongle's own address.
    pub fn pair_url(&self) -> String {
        if self.cloud == "online" && !self.cloud_app.is_empty() {
            return format!("{}/?d={}&pair={}", self.cloud_app, self.cloud_id, self.pair_code);
        }
        let ip = self.lan_ip();
        if ip.is_empty() { String::new() } else { format!("http://{ip}/?pair={}", self.pair_code) }
    }

    /// The text of the "join the setup network" QR code (the standard Wi-Fi QR format that
    /// iOS and Android cameras understand).
    pub fn join_qr_text(&self) -> Option<String> {
        let s = self.setup.as_ref()?;
        Some(format!("WIFI:T:WPA;S:{};P:{};;", qr_escape(&s.ssid), qr_escape(&s.psk)))
    }

    /// The pilot QR: pair over the home network when connected, else the setup page. Without
    /// a Wi-Fi adapter but with the cloud link up (USB network on a developer's desk), the
    /// pairing link through the relay.
    pub fn pilot_qr_text(&self) -> Option<String> {
        if (self.wifi == Wifi::Connected && !self.ip.is_empty()) || (self.adapter.is_none() && self.cloud == "online") {
            return Some(self.pair_url());
        }
        self.setup.as_ref().map(|s| format!("http://{}/", s.ip))
    }

    pub fn export(&self) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "seq={}", self.seq);
        let _ = writeln!(o, "usb={}", self.usb_role);
        let _ = writeln!(o, "adapter={}", self.adapter.as_deref().unwrap_or(""));
        let _ = writeln!(o, "wifi={}", self.wifi.name());
        let _ = writeln!(o, "ssid={}", one_line(&self.ssid));
        let _ = writeln!(o, "ip={}", self.ip);
        let _ = writeln!(o, "signal={}", self.signal);
        let _ = writeln!(o, "error={}", one_line(&self.error));
        let _ = writeln!(o, "saved={}", self.saved.len());
        match &self.setup {
            Some(s) => {
                let _ = writeln!(o, "setup=1\nsetup_ssid={}\nsetup_psk={}\nsetup_ip={}\nsetup_clients={}", one_line(&s.ssid), s.psk, s.ip, s.clients);
            }
            None => o.push_str("setup=0\n"),
        }
        let connected = self.online + self.cloud_pilots;
        let _ = writeln!(o, "pilots={}\nonline={connected}", self.pilots);
        // For the top bar's phone icon: a pilot is paired / one is connected right now.
        let _ = writeln!(o, "pilot_paired={}\npilot_connected={}", (self.pilots > 0) as u8, (connected > 0) as u8);
        let _ = writeln!(o, "pilot_lan={}\npilot_cloud={}", self.online, self.cloud_pilots);
        let _ = writeln!(o, "pair_code={}\npair_url={}", self.pair_code, self.pair_url());
        let _ = writeln!(o, "pair_open={}", (util::uptime() < self.pair_open_until) as u8);
        let _ = writeln!(o, "lan_ip={}", self.lan_ip());
        let _ = writeln!(o, "cloud={}\ncloud_app={}\ncloud_id={}", self.cloud, self.cloud_app, self.cloud_id);
        if let Some(t) = self.join_qr_text() {
            let _ = writeln!(o, "qr_join={}", qr_matrix(&t));
        }
        if let Some(t) = self.pilot_qr_text() {
            let _ = writeln!(o, "qr_pilot={}\npilot_url={}", qr_matrix(&t), t);
        }
        let _ = writeln!(o, "debug={}\ndebug_running={}\ndebug_code={}", self.debug as u8, self.debug_running as u8, if self.debug { &self.debug_code } else { "" });
        let _ = writeln!(o, "debug_lan={}\ndebug_cloud={}\ndebug_minutes={}", self.debug_lan as u8, self.debug_cloud as u8, ((self.debug_until - util::uptime()).max(0.0) / 60.0).ceil() as u32);
        let _ = writeln!(o, "debug_usb={}", util::ipv4_of("usb0").map(|a| a.to_string()).unwrap_or_default());
        for (i, d) in self.developers.iter().enumerate() {
            let _ = writeln!(o, "developer_{i}={}:{}", d.id, d.key.split_whitespace().nth(2).unwrap_or("computer"));
        }
        o
    }
}

fn one_line(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Special characters of the WIFI: QR format are backslash-escaped.
fn qr_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if matches!(c, '\\' | ';' | ',' | ':' | '"') {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// "<size>:<hex>" — the modules row by row, 1 = dark, packed MSB first into bytes.
pub fn qr_matrix(text: &str) -> String {
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else { return String::new() };
    let n = qr.size();
    let mut bits = Vec::with_capacity((n * n) as usize / 8 + 1);
    let (mut cur, mut k) = (0u8, 0);
    for y in 0..n {
        for x in 0..n {
            cur = cur << 1 | qr.get_module(x, y) as u8;
            k += 1;
            if k == 8 {
                bits.push(cur);
                cur = 0;
                k = 0;
            }
        }
    }
    if k > 0 {
        bits.push(cur << (8 - k));
    }
    format!("{n}:{}", util::hex(&bits))
}

/// Bump the sequence, wake waiters, and rewrite the state file when its content changed.
pub fn changed(shared: &Shared) {
    let (m, cv) = &**shared;
    let mut n = m.lock().unwrap();
    n.seq += 1;
    publish(&n);
    cv.notify_all();
}

pub fn publish(n: &Net) {
    static LAST: Mutex<String> = Mutex::new(String::new());
    let mut text = n.export();
    // The sequence alone does not make the UI redraw.
    let body = text.split_once('\n').map(|(_, b)| b.to_string()).unwrap_or_default();
    let mut last = LAST.lock().unwrap();
    if *last == body {
        return;
    }
    *last = body;
    text.push_str("end=1\n");
    if let Err(e) = util::write_atomic(Path::new(STATE_FILE), text.as_bytes(), 0o644) {
        util::log(format!("netd: {STATE_FILE}: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_roundtrip_size() {
        let m = qr_matrix("WIFI:T:WPA;S:MEOW-3F2A;P:12345678;;");
        let (n, hex) = m.split_once(':').unwrap();
        let n: usize = n.parse().unwrap();
        assert!(n >= 21 && (n - 17) % 4 == 0);
        assert_eq!(hex.len(), (n * n).div_ceil(8) * 2);
    }

    #[test]
    fn pilot_keys() {
        let shared = new();
        let mut n = shared.0.lock().unwrap();
        let get = |n: &Net, k: &str| n.export().lines().find_map(|l| l.strip_prefix(&format!("{k}=")).map(|v| v.to_string())).unwrap();
        assert_eq!((get(&n, "pilot_paired"), get(&n, "pilot_connected")), ("0".into(), "0".into()));
        n.pilots = 2;
        n.cloud_pilots = 1;
        assert_eq!((get(&n, "pilot_paired"), get(&n, "pilot_connected")), ("1".into(), "1".into()));
        assert_eq!((get(&n, "online"), get(&n, "pilot_lan"), get(&n, "pilot_cloud")), ("1".into(), "0".into(), "1".into()));
        n.cloud_pilots = 0;
        n.online = 1;
        assert_eq!((get(&n, "pilot_connected"), get(&n, "pilot_lan")), ("1".into(), "1".into()));
    }

    #[test]
    fn escape() {
        assert_eq!(qr_escape("a;b:c"), "a\\;b\\:c");
    }
}
