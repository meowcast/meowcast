//! Client of the app manager (appd, apps): the launcher shows appd's apps, asks the
//! user for consent when appd says so, and hands the screen to an app and takes it back.
//! Protocol: SEQPACKET messages on /run/appd/ui.sock, TAB-separated (appd/src/daemon.rs).
//! appd is optional: without it the launcher keeps its built-in placeholder cards and
//! retries the connection every few seconds.

use crate::art::{Badge, Glyph};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

const SOCKET: &str = "/run/appd/ui.sock";

#[derive(Clone, Debug, PartialEq)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub glyph: Glyph,
    pub tint: u32,
    pub badge: (u32, u32),
    pub shape: Badge,
    /// available | installed | fetching | running | error
    pub state: String,
    pub detail: String,
}

impl AppInfo {
    /// The card's second line.
    pub fn subtitle(&self) -> String {
        match self.state.as_str() {
            "available" => format!("Get  ·  {}", self.detail),
            "fetching" => format!("Downloading  {}", self.detail),
            "running" => "Running".into(),
            "error" => self.detail.clone(),
            _ if !self.detail.is_empty() => self.detail.clone(),
            _ => self.summary.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Perm {
    pub name: String,
    pub sensitive: bool,
    pub required: bool,
    pub label: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct ConsentReq {
    pub id: String,
    pub name: String,
    pub perms: Vec<Perm>,
    /// The app is installed: an update asks for these (optional) permissions.
    pub update: bool,
}

/// Whether an app runs a background service (appd's APPINFO `bg` field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// The app has no background service.
    None,
    Off,
    On,
}

/// One permission of an installed app, as Settings > Apps shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct PermInfo {
    pub name: String,
    pub sensitive: bool,
    pub required: bool,
    /// granted | denied | undecided
    pub state: String,
    pub label: String,
    pub reason: String,
    /// The hosts of "internet", the quota of "storage", the memory of "background".
    pub extra: String,
}

/// Details of an installed app (appd's APPINFO).
#[derive(Clone, Debug, PartialEq)]
pub struct AppDetail {
    pub id: String,
    pub version: String,
    pub background: Background,
    /// running | starting | failing | stopped
    pub service: String,
    pub perms: Vec<PermInfo>,
}

impl AppDetail {
    /// The line under the "Run in background" switch.
    pub fn service_label(&self) -> &'static str {
        match (self.background, self.service.as_str()) {
            (Background::None, _) => "This app does not run in the background",
            (Background::Off, "running") => "Stopping…",
            (Background::Off, _) => "Off: it works only while it is open",
            (_, "running") => "Its service is running",
            (_, "failing") => "Its service keeps stopping; trying again",
            _ => "Its service is starting…",
        }
    }
}

pub enum Event {
    /// Reply to a command this client sent (UNINSTALL): Ok(text) or Err(why).
    Reply(Result<String, String>),
    App(AppInfo),
    Remove(String),
    ListEnd,
    Consent(ConsentReq),
    ReleaseDisplay(String),
    ReclaimDisplay(String),
    Foreground(Option<String>),
    Notice(String),
    /// The system volume: level 0-100, muted, and whether something just set it (the volume
    /// pop-up shows; false for the value sent when the launcher connects).
    Volume(u32, bool, bool),
    /// The apps opened last (ids, most recent first).
    Recents(Vec<String>),
    /// Details of an installed app (Settings > Apps).
    Info(AppDetail),
    /// A signed asset from appd: the open file, or why not.
    Asset(String, Result<OwnedFd, String>),
    /// The asset is not here yet (offline, downloading): its tiny preview (JPEG bytes) from
    /// the signed index, to show until the file arrives as `Asset`.
    AssetPreview(String, Vec<u8>),
    /// A new signed index changed this asset: ask again.
    AssetUpdated(String),
    Connected,
    Disconnected,
}

pub struct Appd {
    fd: Option<OwnedFd>,
    retry_at: Instant,
}

fn glyph(name: &str) -> Glyph {
    match name {
        "search" => Glyph::Search,
        "apps" => Glyph::Apps,
        "edit" => Glyph::Edit,
        "sliders" => Glyph::Sliders,
        "volume" => Glyph::Volume,
        "wifi" => Glyph::Wifi,
        "device" => Glyph::Device,
        "power" => Glyph::Power,
        "globe" => Glyph::Globe,
        "play" => Glyph::Play,
        "music" => Glyph::Music,
        "terminal" => Glyph::Terminal,
        "cast" => Glyph::Cast,
        "photo" => Glyph::Photo,
        "folder" => Glyph::Folder,
        "archive" => Glyph::Archive,
        "headphones" => Glyph::Headphones,
        "sun" => Glyph::Sun,
        "gamepad" => Glyph::Gamepad,
        "monitor" => Glyph::Monitor,
        "clock" => Glyph::Clock,
        "info" => Glyph::Info,
        "network" | "lan" => Glyph::Lan,
        "manage-receivers" => Glyph::Cast,
        _ => Glyph::Gear,
    }
}

fn colour(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0x2a2e32)
}

fn sockaddr() -> (libc::sockaddr_un, libc::socklen_t) {
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as _;
    for (i, b) in SOCKET.bytes().enumerate() {
        a.sun_path[i] = b as _;
    }
    (a, std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t)
}

impl Appd {
    pub fn new() -> Appd {
        Appd { fd: None, retry_at: Instant::now() }
    }

    pub fn fd(&self) -> RawFd {
        self.fd.as_ref().map_or(-1, |f| f.as_raw_fd())
    }

    pub fn connected(&self) -> bool {
        self.fd.is_some()
    }

    /// When to try connecting again (for the poll timeout).
    pub fn retry_at(&self) -> Option<Instant> {
        self.fd.is_none().then_some(self.retry_at)
    }

    pub fn send(&mut self, msg: &str) {
        let Some(fd) = &self.fd else { return };
        let n = unsafe { libc::send(fd.as_raw_fd(), msg.as_ptr() as *const _, msg.len(), libc::MSG_NOSIGNAL) };
        if n < 0 {
            self.fd = None;
        }
    }

    fn connect(&mut self, out: &mut Vec<Event>) {
        // appd starts right after the launcher at boot: retry often (a connect() is cheap).
        self.retry_at = Instant::now() + Duration::from_secs(1);
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
        if fd < 0 {
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let (a, len) = sockaddr();
        if unsafe { libc::connect(fd.as_raw_fd(), &a as *const _ as *const libc::sockaddr, len) } < 0 {
            return;
        }
        self.fd = Some(fd);
        self.send("HELLO\tui");
        if self.fd.is_some() {
            eprintln!("UI_APPD connected");
            out.push(Event::Connected);
        }
    }

    /// Reconnect if due, then read every pending message.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<Event>) {
        if self.fd.is_none() {
            if now >= self.retry_at {
                self.connect(out);
            }
            if self.fd.is_none() {
                return;
            }
        }
        let mut buf = vec![0u8; 8192];
        loop {
            let fd = self.fd.as_ref().unwrap().as_raw_fd();
            // recvmsg: ASSET replies carry an open file (SCM_RIGHTS).
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
            let mut cbuf = [0u64; 8];
            let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
            hdr.msg_iov = &mut iov;
            hdr.msg_iovlen = 1;
            hdr.msg_control = cbuf.as_mut_ptr() as *mut _;
            hdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;
            let n = unsafe { libc::recvmsg(fd, &mut hdr, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                return;
            }
            let mut passed: Option<OwnedFd> = None;
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&hdr);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                        for i in 0..count {
                            let f = std::ptr::read_unaligned((libc::CMSG_DATA(c) as *const i32).add(i));
                            if passed.is_none() { passed = Some(OwnedFd::from_raw_fd(f)) } else { libc::close(f); }
                        }
                    }
                    c = libc::CMSG_NXTHDR(&hdr, c);
                }
            }
            if n <= 0 {
                eprintln!("UI_APPD disconnected");
                self.fd = None;
                self.retry_at = now + Duration::from_secs(1);
                out.push(Event::Disconnected);
                return;
            }
            let msg = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
            if let Some(ev) = parse(&msg, passed) {
                out.push(ev);
            }
        }
    }
}

fn parse(msg: &str, fd: Option<OwnedFd>) -> Option<Event> {
    let f: Vec<&str> = msg.split('\t').collect();
    let a = |i: usize| f.get(i).copied().unwrap_or("").to_string();
    Some(match f[0] {
        "APP" if f.len() >= 11 => Event::App(AppInfo {
            id: a(1),
            name: a(2),
            summary: a(3),
            glyph: glyph(f[4]),
            tint: colour(f[5]),
            badge: (colour(f[6]), colour(f[7])),
            shape: if f[8] == "square" { Badge::Square } else { Badge::Circle },
            state: a(9),
            detail: a(10),
        }),
        "APPS_END" => Event::ListEnd,
        "REMOVE" => Event::Remove(a(1)),
        "CONSENT" => Event::Consent(ConsentReq {
            id: a(1),
            name: a(2),
            update: f[3..].contains(&"mode=update"),
            perms: f[3..]
                .iter()
                .filter_map(|p| {
                    let q: Vec<&str> = p.split('|').collect();
                    (q.len() >= 5).then(|| Perm {
                        name: q[0].into(),
                        sensitive: q[1] == "sensitive",
                        required: q[2] == "1",
                        label: q[3].into(),
                        reason: q[4].into(),
                    })
                })
                .collect(),
        }),
        "RELEASE_DISPLAY" => Event::ReleaseDisplay(a(1)),
        "RECLAIM_DISPLAY" => Event::ReclaimDisplay(a(1)),
        "FOREGROUND" => Event::Foreground(Some(a(1)).filter(|s| s != "-" && !s.is_empty())),
        "NOTICE" => Event::Notice(a(1)),
        "VOLUME" => Event::Volume(a(1).parse::<u32>().ok()?.min(100), a(2) == "1", a(3) == "1"),
        "RECENTS" => Event::Recents(f[1..].iter().filter(|id| !id.is_empty()).map(|id| id.to_string()).collect()),
        "APPINFO" if f.len() >= 5 => Event::Info(AppDetail {
            id: a(1),
            version: a(2),
            background: match f[3] {
                "on" => Background::On,
                "off" => Background::Off,
                _ => Background::None,
            },
            service: a(4),
            perms: f[5..]
                .iter()
                .filter_map(|p| {
                    let q: Vec<&str> = p.split('|').collect();
                    (q.len() >= 7).then(|| PermInfo {
                        name: q[0].into(),
                        sensitive: q[1] == "sensitive",
                        required: q[2] == "1",
                        state: q[3].into(),
                        label: q[4].into(),
                        reason: q[5].into(),
                        extra: q[6].into(),
                    })
                })
                .collect(),
        }),
        "ASSET" if f.get(2) == Some(&"preview") => match base64_decode(f.get(3).copied().unwrap_or("")) {
            Some(b) => Event::AssetPreview(a(1), b),
            None => return None,
        },
        "ASSET" => Event::Asset(a(1), match (f.get(2).copied(), fd) {
            (Some("ok"), Some(fd)) => Ok(fd),
            _ => Err(f.get(3).copied().unwrap_or("no file").to_string()),
        }),
        "ASSET_UPDATED" => Event::AssetUpdated(a(1)),
        "WELCOME" => return None,
        "OK" => Event::Reply(Ok(a(1))),
        "ERR" => Event::Reply(Err(a(1))),
        _ => {
            eprintln!("UI_APPD unexpected {msg:?}");
            return None;
        }
    })
}

/// Standard base64 (with or without padding); None on any other character.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().take_while(|&c| c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appd_messages() {
        match parse("VOLUME\t35\t1\t1", None) {
            Some(Event::Volume(35, true, true)) => {}
            _ => panic!("volume with mute"),
        }
        match parse("VOLUME\t250", None) {
            Some(Event::Volume(100, false, false)) => {}
            _ => panic!("an older appd's volume"),
        }
        match parse("RECENTS\tdev.meow.youtube\tdev.meow.inspector", None) {
            Some(Event::Recents(ids)) => assert_eq!(ids, ["dev.meow.youtube", "dev.meow.inspector"]),
            _ => panic!("recents"),
        }
        match parse("RECENTS", None) {
            Some(Event::Recents(ids)) => assert!(ids.is_empty()),
            _ => panic!("no recents"),
        }
        let line = "APPINFO\tdev.meow.youtube\t0.3.0\ton\trunning\tdisplay|normal|1|granted|Show pictures||\tinternet|sensitive|1|granted|Connect to youtube.com|Streams video|*.youtube.com, *.googlevideo.com\tbackground|sensitive|0|undecided|Keep running|Is ready for a cast|up to 16 MiB of memory";
        match parse(line, None) {
            Some(Event::Info(d)) => {
                assert_eq!((d.id.as_str(), d.version.as_str(), d.background, d.service.as_str()), ("dev.meow.youtube", "0.3.0", Background::On, "running"));
                assert_eq!(d.perms.len(), 3);
                assert!(d.perms[1].sensitive && d.perms[1].required && d.perms[1].extra.contains("googlevideo"));
                assert_eq!((d.perms[2].state.as_str(), d.perms[2].required), ("undecided", false));
                assert_eq!(d.service_label(), "Its service is running");
            }
            _ => panic!("app details"),
        }
        match parse("CONSENT\tdev.meow.youtube\tYouTube\tmode=update\tbackground|sensitive|0|Keep running|Is ready", None) {
            Some(Event::Consent(c)) => assert!(c.update && c.perms.len() == 1 && c.perms[0].name == "background"),
            _ => panic!("consent for an update"),
        }
    }

    #[test]
    fn base64() {
        for (o, i) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::base64_decode(i).unwrap(), o.as_bytes());
        }
        assert!(super::base64_decode("Zm9v!").is_none());
    }
}
