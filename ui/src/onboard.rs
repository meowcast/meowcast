//! "Set up with your phone": the full-screen prompt while the dongle has a Wi-Fi adapter but no
//! paired pilot (phone remote), or while it lost its Wi-Fi and offers the setup network again.
//! The network daemon (meow-netd, apps/netd) does the work and exports its state in
//! /run/netd/state, including the QR codes as module matrices; this module only draws them.
//!   1. Scan to join the TV's setup Wi-Fi (standard WIFI: QR, password shown below it).
//!   2. The phone opens the pilot setup page by itself (captive portal), or scan the second code.
//! Once the TV is on the home Wi-Fi and still has no pilot: one code that pairs a phone.
//! Commands go back through the /run/netd/ctl FIFO.

use crate::atlas::Atlas;
use crate::gpu::Gpu;
use crate::home::{FOCUS_COLOR, Frame, Slices, nine, nine_ex, solid};
use crate::text::{Face, Fits, Text};
use crate::util::{RectF, Tween, rgba};
use std::time::{Duration, Instant};

pub const ID_ONBOARD: u32 = 960_000;
const STATE: &str = "/run/netd/state";
const CTL: &str = "/run/netd/ctl";

/// The daemon's exported state (key=value lines). Empty when meow-netd is not running.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetState {
    kv: Vec<(String, String)>,
}

impl NetState {
    /// Reads the state file; None while it is being replaced (no end marker yet).
    pub fn load() -> Option<NetState> {
        let s = std::fs::read_to_string(STATE).unwrap_or_default();
        if s.is_empty() {
            return Some(NetState::default());
        }
        if !s.contains("\nend=1") {
            return None;
        }
        let kv = s
            .lines()
            .filter(|l| !l.starts_with("seq="))
            .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect();
        Some(NetState { kv })
    }
    pub fn get(&self, k: &str) -> &str {
        self.kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).unwrap_or("")
    }
    fn num(&self, k: &str) -> i64 {
        self.get(k).parse().unwrap_or(0)
    }
    pub fn running(&self) -> bool {
        !self.kv.is_empty()
    }
    pub fn adapter(&self) -> bool {
        !self.get("adapter").is_empty()
    }
    pub fn connected(&self) -> bool {
        self.get("wifi") == "connected"
    }
    pub fn setup(&self) -> bool {
        self.get("setup") == "1"
    }
    pub fn pilots(&self) -> i64 {
        self.num("pilots")
    }
    pub fn signal(&self) -> i64 {
        self.num("signal")
    }
    pub fn debug(&self) -> bool {
        self.get("debug") == "1"
    }
    /// The Settings > Wi-Fi status line.
    pub fn status(&self) -> String {
        let ssid = self.get("ssid");
        match self.get("wifi") {
            "connected" => format!("Connected to {ssid}"),
            "dhcp" => format!("Getting an address from {ssid}…"),
            "connecting" if !ssid.is_empty() => format!("Connecting to {ssid}…"),
            "connecting" => "Looking for the saved network…".into(),
            "starting" => "Starting…".into(),
            "failed" => "Not connected".into(),
            _ => "Not connected".into(),
        }
    }
    /// Without a Wi-Fi adapter but with a wired way out (USB network on a developer's desk):
    /// a phone can still pair through the cloud relay.
    fn cloud_only(&self) -> bool {
        !self.adapter() && self.get("cloud") == "online"
    }
    /// The prompt has something to show when asked for (Settings, the hardware button).
    pub fn available(&self) -> bool {
        self.adapter() || self.cloud_only()
    }
    /// The prompt is wanted: an adapter, and no pilot yet or no working Wi-Fi.
    fn wanted(&self) -> bool {
        self.adapter() && (self.pilots() == 0 || (!self.connected() && self.setup()))
    }
    /// What the prompt shows; a change brings a dismissed prompt back.
    fn phase(&self) -> &'static str {
        if self.connected() || self.cloud_only() {
            "pair"
        } else if self.setup() {
            "setup"
        } else {
            "wait"
        }
    }
}

/// Send one command line to meow-netd; false when it is not running.
pub fn ctl(cmd: &str) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(CTL) {
        Ok(mut f) => f.write_all(format!("{cmd}\n").as_bytes()).is_ok(),
        Err(_) => false,
    }
}

struct Qr {
    n: usize,
    bits: Vec<u8>,
}

impl Qr {
    fn parse(s: &str) -> Option<Qr> {
        let (n, hex) = s.split_once(':')?;
        let n: usize = n.parse().ok()?;
        let bits: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()).collect();
        (n >= 21 && bits.len() * 8 >= n * n).then_some(Qr { n, bits })
    }
    fn dark(&self, x: usize, y: usize) -> bool {
        let i = y * self.n + x;
        self.bits[i / 8] >> (7 - i % 8) & 1 == 1
    }
}

pub struct Onboard {
    alpha: Tween,
    /// Shown although not wanted (Settings > Wi-Fi > Set up with phone / Pair a phone, the
    /// hardware button): the number of paired phones and the phase when it was asked for.
    forced: Option<(i64, &'static str)>,
    /// Hidden with Back until the phase changes.
    dismissed: Option<&'static str>,
    fits: Fits,
}

impl Onboard {
    pub fn new() -> Onboard {
        Onboard { alpha: Tween::fixed(0.0), forced: None, dismissed: None, fits: Fits::default() }
    }

    pub fn visible(&self) -> bool {
        self.alpha.target() > 0.0
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.alpha.active(now)
    }

    /// Re-evaluate after a state change.
    pub fn update(&mut self, st: &NetState, now: Instant) {
        if self.dismissed.is_some_and(|p| p != st.phase()) {
            self.dismissed = None;
        }
        if let Some((pilots, phase)) = self.forced {
            // Asked for to pair: it stays until one more phone is paired (the phones that are
            // already remotes do not count). Asked for to set up Wi-Fi: until a phone has
            // taken over on the new network.
            if st.pilots() > pilots || (phase != "pair" && st.pilots() > 0 && st.get("online") != "0" && st.connected()) {
                self.forced = None;
            }
        }
        let show = st.available() && (self.forced.is_some() || (st.adapter() && st.wanted() && self.dismissed.is_none()));
        self.alpha.go(if show { 1.0 } else { 0.0 }, now, Duration::from_millis(if show { 320 } else { 220 }));
    }

    pub fn force(&mut self, st: &NetState, now: Instant) {
        self.forced = Some((st.pilots(), st.phase()));
        self.dismissed = None;
        self.update(st, now);
    }

    /// Back (or OK) hides it until something changes.
    pub fn dismiss(&mut self, st: &NetState, now: Instant) {
        self.forced = None;
        self.dismissed = Some(st.phase());
        self.update(st, now);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, st: &NetState, sl: &Slices, s: f32, f: &mut Frame) {
        let a = self.alpha.value(now);
        if a <= 0.0 {
            return;
        }
        let (cr, cm) = sl.card;
        let dk = s / sl.raster;
        let st_ = f.begin();
        nine(&mut f.verts, atlas, &cr, cm, RectF::new(-20.0 * s, -20.0 * s, 1960.0 * s, 1120.0 * s), cm as f32 * dk, rgba(0x05060c, 0.86 * a));
        f.end(ID_ONBOARD, st_);

        let phase = st.phase();
        let (w, h): (f32, f32) = (1440.0, if phase == "wait" { 420.0 } else { 820.0 });
        let (x, y) = ((1920.0 - w) / 2.0, ((1080.0 - h) / 2.0).round() + 18.0 * (1.0 - a));
        let panel = RectF::new(x * s, y * s, w * s, h * s);
        let (shr, shm, shpad) = sl.shadow;
        let st_ = f.begin();
        nine_ex(&mut f.verts, atlas, &shr, shm, panel.expand(shpad as f32 * dk), shm as f32 * dk, rgba(0x000000, 0.6 * a), false);
        nine(&mut f.verts, atlas, &cr, cm, panel, cm as f32 * dk, rgba(0x1d2233, a));
        let px = |v: f32| (v * s).round();
        let (title, sub) = match phase {
            "pair" if st.get("ssid").is_empty() => ("Pair your phone", "Your phone becomes the remote of this TV.".to_string()),
            "pair" => ("Pair your phone", format!("The TV is on {}. Your phone becomes its remote.", st.get("ssid"))),
            "setup" => ("Set up with your phone", "No remote needed: your phone connects the TV to Wi-Fi and becomes its remote.".to_string()),
            _ => ("Preparing Wi-Fi setup…", st.get("error").to_string()),
        };
        text.draw(gpu, atlas, &mut f.verts, Face::Light, 46.0 * s, px(x + 72.0), px(y + 108.0), rgba(0xffffff, a), title);
        // Network names and errors come from outside: every line is cut to its room.
        let sub = self.fits.line(text, Face::Regular, 26.0 * s, &sub, (w - 144.0) * s);
        text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, px(x + 72.0), px(y + 160.0), rgba(0xffffff, 0.62 * a), sub);
        f.end(ID_ONBOARD + 1, st_);

        // Setup: two columns of 600; pair: the text right of the code, up to the panel's padding.
        let room = if phase == "setup" { 600.0 } else { w - 144.0 - 440.0 - 80.0 } * s;
        let fits = &mut self.fits;
        let mut line = |f: &mut Frame, text: &mut Text, atlas: &mut Atlas, face: Face, size: f32, lx: f32, ly: f32, alpha: f32, t: &str| {
            let t = fits.line(text, face, size * s, t, room);
            text.draw(gpu, atlas, &mut f.verts, face, size * s, px(lx), px(ly), rgba(0xffffff, alpha * a), t);
        };
        match phase {
            "setup" => {
                let ssid = st.get("setup_ssid");
                let ip = st.get("setup_ip");
                let cols = [
                    ("1", "Join the TV's Wi-Fi", st.get("qr_join"), ["Scan with your phone's camera.".to_string(), format!("Or join {ssid}, password {}", st.get("setup_psk"))]),
                    ("2", "Open the pilot", st.get("qr_pilot"), ["Your phone usually opens it by itself.".to_string(), format!("Or scan this, or open http://{ip}")]),
                ];
                for (i, (num, head, qr, lines)) in cols.iter().enumerate() {
                    let cx = x + 72.0 + i as f32 * (600.0 + 96.0);
                    let st_ = f.begin();
                    nine(&mut f.verts, atlas, &cr, cm, RectF::new(px(cx), px(y + 214.0), px(48.0), px(48.0)), 24.0 * s, rgba(FOCUS_COLOR, a));
                    let nw = text.width(Face::Medium, 28.0 * s, num);
                    text.draw(gpu, atlas, &mut f.verts, Face::Medium, 28.0 * s, (px(cx + 24.0) - nw / 2.0).round(), px(y + 248.0), rgba(0xffffff, a), num);
                    line(f, text, atlas, Face::Medium, 28.0, cx + 72.0, y + 250.0, 1.0, head);
                    f.end(ID_ONBOARD + 10 + i as u32 * 10, st_);
                    draw_qr(f, atlas, sl, s, qr, cx, y + 296.0, 360.0, a, ID_ONBOARD + 11 + i as u32 * 10);
                    let st_ = f.begin();
                    for (j, l) in lines.iter().enumerate() {
                        line(f, text, atlas, Face::Regular, 26.0, cx, y + 712.0 + j as f32 * 38.0, if j == 0 { 0.92 } else { 0.62 }, l);
                    }
                    f.end(ID_ONBOARD + 12 + i as u32 * 10, st_);
                }
            }
            "pair" => {
                draw_qr(f, atlas, sl, s, st.get("qr_pilot"), x + 72.0, y + 214.0, 440.0, a, ID_ONBOARD + 20);
                let tx = x + 72.0 + 440.0 + 80.0;
                let st_ = f.begin();
                line(f, text, atlas, Face::Medium, 28.0, tx, y + 272.0, 1.0, "Scan with your phone's camera");
                if st.get("cloud") == "online" {
                    // The link goes through the meow cloud: any network works.
                    line(f, text, atlas, Face::Regular, 26.0, tx, y + 324.0, 0.62, "It opens the MEOW Pilot. Your phone can be");
                    line(f, text, atlas, Face::Regular, 26.0, tx, y + 362.0, 0.62, "on any network, even mobile data.");
                    line(f, text, atlas, Face::Regular, 26.0, tx, y + 476.0, 0.62, "If the Pilot asks for a code:");
                } else {
                    line(f, text, atlas, Face::Regular, 26.0, tx, y + 324.0, 0.62, &format!("Your phone needs to be on {} too.", st.get("ssid")));
                    line(f, text, atlas, Face::Regular, 26.0, tx, y + 430.0, 0.92, &format!("Or open http://{} in a browser", st.get("lan_ip")));
                    line(f, text, atlas, Face::Regular, 26.0, tx, y + 476.0, 0.62, "and enter this code:");
                }
                line(f, text, atlas, Face::Light, 46.0, tx, y + 584.0, 1.0, &spaced(st.get("pair_code")));
                line(f, text, atlas, Face::Regular, 21.0, tx, y + 690.0, 0.45, "Back, or the button on the dongle, hides this screen");
                f.end(ID_ONBOARD + 21, st_);
            }
            _ => {}
        }
    }
}

/// "123456" -> "123 456", "12345678" -> "1234 5678" (easier to read from across the room).
fn spaced(code: &str) -> String {
    let mid = code.len() / 2;
    if code.len() >= 6 && code.is_ascii() { format!("{} {}", &code[..mid], &code[mid..]) } else { code.to_string() }
}

/// A QR code on a white rounded card (4-module quiet zone), modules snapped to whole pixels.
/// Horizontal runs of dark modules become one quad each.
#[allow(clippy::too_many_arguments)]
fn draw_qr(f: &mut Frame, atlas: &Atlas, sl: &Slices, s: f32, spec: &str, x: f32, y: f32, size: f32, a: f32, id: u32) {
    let Some(qr) = Qr::parse(spec) else { return };
    let (cr, cm) = sl.card;
    let total = qr.n + 8;
    let m = ((size * s) / total as f32).floor().max(1.0);
    let side = m * total as f32;
    let (x0, y0) = ((x * s).round(), (y * s).round());
    let st = f.begin();
    nine(&mut f.verts, atlas, &cr, cm, RectF::new(x0, y0, side, side), (cm as f32 * s / sl.raster).min(side / 8.0), rgba(0xffffff, a));
    let ox = x0 + 4.0 * m;
    let oy = y0 + 4.0 * m;
    for yy in 0..qr.n {
        let mut xx = 0;
        while xx < qr.n {
            if !qr.dark(xx, yy) {
                xx += 1;
                continue;
            }
            let start = xx;
            while xx < qr.n && qr.dark(xx, yy) {
                xx += 1;
            }
            solid(&mut f.verts, atlas, &cr, cm, ox + start as f32 * m, oy + yy as f32 * m, ox + xx as f32 * m, oy + (yy + 1) as f32 * m, rgba(0x000000, a));
        }
    }
    f.end(id, st);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_and_qr() {
        let st = NetState { kv: vec![("adapter".into(), "wlan0".into()), ("pilots".into(), "0".into()), ("wifi".into(), "idle".into()), ("setup".into(), "1".into())] };
        assert!(st.wanted());
        assert_eq!(st.phase(), "setup");
        let q = Qr::parse(&format!("21:{}", "ff".repeat(56))).unwrap();
        assert!(q.dark(20, 20));
        assert!(Qr::parse("21:00").is_none());
        assert_eq!(spaced("123456"), "123 456");
    }

    #[test]
    fn asked_for_pairing_stays_until_one_more_phone() {
        let state = |pilots: &str| NetState {
            kv: [("adapter", "wlan0"), ("wifi", "connected"), ("pilots", pilots), ("online", "1")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        let now = Instant::now();
        let mut ob = Onboard::new();
        // One phone is paired and connected: no prompt by itself.
        ob.update(&state("1"), now);
        assert!(!ob.visible());
        // The button asks for the pairing screen: it stays although a phone is online...
        ob.force(&state("1"), now);
        ob.update(&state("1"), now);
        assert!(ob.visible());
        // ...until the second phone is paired.
        ob.update(&state("2"), now);
        assert!(!ob.visible());
        // No adapter: only with the cloud relay.
        let wired = |cloud: &str| NetState { kv: vec![("pilots".into(), "0".into()), ("cloud".into(), cloud.into())] };
        ob.force(&wired("offline"), now);
        assert!(!ob.visible());
        ob.force(&wired("online"), now);
        assert!(ob.visible());
        assert_eq!(wired("online").phase(), "pair");
    }
}
