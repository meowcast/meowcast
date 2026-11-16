//! meow-app: the SDK for apps on the meow TV dongle.
//!
//! Licence: MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE), so an app that links it may
//! carry any licence. The rest of the system is GPL-3.0-or-later.
//!
//! An app runs in a sandbox (see apps/README.md) and talks to the system only
//! through its broker socket, fd 3 (MEOW_BROKER_FD). Everything privileged arrives over it:
//!
//! ```no_run
//! let mut app = meow_app::App::connect().expect("not started by appd");
//! # #[cfg(feature = "display")] {
//! let mut screen = meow_app::display::Display::open(&mut app).expect("display");
//! # }
//! loop {
//!     for ev in app.wait(Some(std::time::Duration::from_millis(16))) {
//!         match ev {
//!             meow_app::Event::Key(meow_app::Key::Back) | meow_app::Event::Quit => return,
//!             _ => {}
//!         }
//!     }
//! }
//! ```
//!
//! Broker protocol (SEQPACKET, TAB-separated text):
//!   app -> appd: HELLO | DISPLAY | SYNC | LOG text | VOLUME n | MUTE 0|1 | EXIT
//!                | FOREGROUND payload (a background service: open my app)
//!                | MIRACAST on pbc|pin name | MIRACAST disconnect | MIRACAST off ("wifi-direct")
//!                | ATTACH socket|data (+ an fd: goes to the app with the next FOREGROUND)
//!                | BACKGROUND get|set id [0|1] (another app's background switch)
//!                | IMAGE (a connection to the image service: see `image`)
//!                | STORE request ... (the Store app, "manage-apps": see `System::store`)
//!   appd -> app: WELCOME id perms | DISPLAY ok (+ DRM fd) | DISPLAY denied why
//!                | SYNC ok|unchanged|deferred|error why | KEY name | QUIT
//!                | MUTE 0|1, then VOLUME n (the system volume was set: the pop-up shows)
//!                | NET (+ a new dial fd, see net.rs) | IMAGE ok (+ a socket, see image.rs) | IMAGE unavailable why
//!                | FOREGROUND ok | FOREGROUND denied why
//!                | MIRACAST state [detail] (the Miracast receiver: see `Event::Miracast`)
//!                | BACKGROUND id absent|on|off|denied
//!                | STORE repo|app|gone|repo-gone|end|result ... (see `Event::Store`)
//!
//! Internet access goes through the system network service: see [`net`].
//!
//! # Background services
//!
//! An app with the "background" permission (and a "background" section in its manifest) has
//! a second instance of the same program that the system keeps running while the app is
//! closed: small (the manifest's limits), without screen, keys, decoder or sound. It tells
//! itself apart with [`App::is_service`], and when something should be on the TV it calls
//! [`System::request_foreground`] with a payload. The system then closes whatever is in
//! front, stops the service (`Event::Quit`) and opens the app the normal way; the app reads
//! the payload with [`App::launch_payload`]. The two never run at the same time, so they can
//! share `/data` and hand over a session with a server:
//!
//! ```no_run
//! let mut app = meow_app::App::connect().unwrap();
//! if meow_app::App::is_service() {
//!     // ... stay registered with the server; when a cast starts:
//!     app.system().request_foreground("{\"video\":\"dQw4w9WgXcQ\"}");
//!     // keep serving until Event::Quit arrives, then exit.
//! } else if let Some(payload) = meow_app::App::launch_payload() {
//!     // opened by the service: start playing what the payload says.
//! }
//! ```

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod handover;
pub mod image;
pub mod ipc;
pub mod net;
pub mod osd;

#[cfg(feature = "display")]
pub mod display;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Back,
    Other(String),
}

impl Key {
    fn parse(s: &str) -> Key {
        match s {
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "enter" => Key::Enter,
            "back" => Key::Back,
            o => Key::Other(o.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A remote/keyboard key (needs the "input" permission and the screen).
    Key(Key),
    /// The system closes the app (Home pressed, uninstall, shutdown). Save and exit soon:
    /// after 3 s the whole app is killed.
    Quit,
    /// The system volume was set (0-100): the user moved it, a phone did, or this app did.
    /// Apps with "media" scale their sound by it (`App::gain`); `App::muted` says whether the
    /// sound is off (the level is kept). The value may be the one before: every change request
    /// arrives, because each shows the volume pop-up ([`osd`]).
    Volume(u8),
    /// The system refused `request_foreground` (asked too often, or the app is already open).
    ForegroundDenied(String),
    /// The Miracast receiver's state (`System::miracast`; needs "wifi-direct"): `off`,
    /// `starting`, `ready`, `pin<TAB>12345670`, `connecting<TAB>who`, `peer<TAB>ip:port` (a
    /// source is connected: its RTSP server) or `unavailable<TAB>why`. Sent as the answer to
    /// every request and whenever it changes.
    Miracast(String),
    /// A line for the device's app store (`System::store`; needs "manage-apps"): the fields
    /// after `STORE`, TAB-separated. `repo ...` and `app ...` describe a repository or an app
    /// (again whenever one changes), `gone id` and `repo-gone id` take one away, `end` closes
    /// the first full list, `result what target ok|error text` answers a request. The field
    /// lists are at the top of appd's daemon.rs.
    Store(String),
}

/// Another app's "Run in background" switch (`App::background_of`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// The app is not installed, or has no background service.
    Absent,
    On,
    Off,
}

/// How this instance was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The app, opened by the user (or by its own background service).
    App,
    /// The app's background service: no screen, no keys, no decoder, no sound.
    Service,
}

pub struct App {
    fd: Arc<OwnedFd>,
    volume: u8,
    muted: bool,
    pub id: String,
    pub permissions: Vec<String>,
    queue: VecDeque<Event>,
    replies: VecDeque<(String, Option<OwnedFd>)>,
}

#[derive(Debug)]
pub enum Error {
    NotSandboxed,
    Io(io::Error),
    Denied(String),
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotSandboxed => write!(f, "not started by appd (no MEOW_BROKER_FD)"),
            Error::Io(e) => write!(f, "broker: {e}"),
            Error::Denied(w) => write!(f, "denied: {w}"),
            Error::Protocol(w) => write!(f, "broker protocol: {w}"),
        }
    }
}
impl std::error::Error for Error {}

fn send(fd: RawFd, msg: &str) -> io::Result<()> {
    ipc::send(fd, msg, None)
}

fn recv(fd: RawFd, block: bool) -> io::Result<Option<(String, Option<OwnedFd>)>> {
    ipc::recv(fd, block)
}

/// The system volume as a gain for 16-bit samples, in 1/65536: `sample * gain >> 16`.
/// The curve is a square, so the middle of the scale sounds about half as loud.
pub fn volume_gain(volume: u8) -> i32 {
    let volume = volume.min(100) as i32;
    volume * volume * 65536 / 10000
}

/// Requests to the system that any thread may make (`App::system`).
#[derive(Clone)]
pub struct System {
    fd: Arc<OwnedFd>,
}

impl System {
    /// Set the system volume (0-100; needs "media"). It is one setting for the whole device:
    /// every app follows it, and it is remembered across restarts.
    pub fn set_volume(&self, volume: u8) {
        let _ = send(self.fd.as_raw_fd(), &format!("VOLUME\t{}", volume.min(100)));
    }

    /// Switch the system's sound off or on (needs "media"). The level is kept.
    pub fn set_muted(&self, muted: bool) {
        let _ = send(self.fd.as_raw_fd(), &format!("MUTE\t{}", muted as u8));
    }
    /// A background service asks for its app to be opened now (a cast started). Whatever is
    /// on the TV is closed, this service gets `Event::Quit`, and the app starts with
    /// `payload` as its `App::launch_payload`: one line of printable text, at most
    /// [`PAYLOAD_MAX`] bytes (false if it is not; nothing is sent then). A refusal arrives
    /// as `Event::ForegroundDenied`.
    pub fn request_foreground(&self, payload: &str) -> bool {
        if payload.len() > PAYLOAD_MAX || payload.chars().any(|c| c.is_control()) {
            return false;
        }
        send(self.fd.as_raw_fd(), &format!("FOREGROUND\t{payload}")).is_ok()
    }
    /// `request_foreground` that also hands sockets to the app (it reads them with
    /// `App::launch_fds`, in this order) and up to 64 KiB of `data` (`App::launch_data`):
    /// see [`handover`]. This instance keeps its own copies; it should leave them alone
    /// until `Event::Quit` or `Event::ForegroundDenied` says how it went.
    pub fn request_foreground_with(&self, payload: &str, fds: &[std::os::fd::BorrowedFd<'_>], data: &[u8]) -> bool {
        if payload.len() > PAYLOAD_MAX || payload.chars().any(|c| c.is_control()) {
            return false;
        }
        // One request's messages stay together when several threads ask.
        static ORDER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _order = ORDER.lock().unwrap_or_else(|e| e.into_inner());
        handover::request(self.fd.as_raw_fd(), payload, fds, data).is_ok()
    }
    /// A line in the app's log.
    pub fn log(&self, msg: &str) {
        let clean: String = msg.chars().map(|c| if c.is_control() { ' ' } else { c }).take(1000).collect();
        let _ = send(self.fd.as_raw_fd(), &format!("LOG\t{clean}"));
    }
    /// Ask the system to be a Miracast sink (needs "wifi-direct"): `["on", "pbc" | "pin",
    /// name]` makes the device visible to sources under `name` (push button: anyone nearby
    /// may connect; pin: the source must type a PIN that arrives as `pin` state),
    /// `["disconnect"]` ends the session with the connected source, `["off"]` withdraws the
    /// receiver. The answer and every later change arrive as `Event::Miracast`. A receiver
    /// asked for by the background service stays up while the app opens and asks again.
    pub fn miracast(&self, request: &[&str]) {
        let clean: Vec<String> = request.iter().map(|f| f.chars().filter(|c| !c.is_control()).collect()).collect();
        let _ = send(self.fd.as_raw_fd(), &format!("MIRACAST\t{}", clean.join("\t")));
    }
    /// A request of the device's app store (needs "manage-apps", which only an app of the
    /// built-in repository can hold): `["subscribe"]` asks for every repository and app now
    /// and every change later, `["refresh"]` for a new look at the repositories;
    /// `["install", id, "allow" | "deny"]` (the user's answer to the app's permissions),
    /// `["uninstall", id]`, `["open", id]`; `["probe", url]` looks a repository address up,
    /// `["add", url, fingerprint]` adds what was found, `["remove", repo]` and
    /// `["enable", repo, "0" | "1"]` change the list. Answers arrive as `Event::Store`.
    pub fn store(&self, request: &[&str]) {
        let clean: Vec<String> = request.iter().map(|f| f.chars().filter(|c| !c.is_control()).collect()).collect();
        let _ = send(self.fd.as_raw_fd(), &format!("STORE\t{}", clean.join("\t")));
    }
}

/// Longest payload of `request_foreground`.
pub const PAYLOAD_MAX: usize = 4096;

impl App {
    /// Connect to appd through the inherited broker socket.
    pub fn connect() -> Result<App, Error> {
        let fd: RawFd = std::env::var("MEOW_BROKER_FD").ok().and_then(|v| v.parse().ok()).ok_or(Error::NotSandboxed)?;
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(Error::NotSandboxed);
        }
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        let volume = std::env::var("MEOW_VOLUME").ok().and_then(|v| v.parse::<u8>().ok()).map_or(100, |v| v.min(100));
        let muted = std::env::var("MEOW_MUTED").is_ok_and(|v| v == "1");
        let mut app = App { fd: Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }), volume, muted, id: String::new(), permissions: Vec::new(), queue: VecDeque::new(), replies: VecDeque::new() };
        // appd sends WELCOME first; HELLO asks again in case it was consumed elsewhere.
        send(fd, "HELLO").map_err(Error::Io)?;
        let w = app.request_reply("WELCOME", Duration::from_secs(3))?;
        let f: Vec<&str> = w.0.split('\t').collect();
        app.id = f.get(1).unwrap_or(&"").to_string();
        app.permissions = f.get(2).unwrap_or(&"").split(',').filter(|s| !s.is_empty()).map(String::from).collect();
        Ok(app)
    }

    /// App or background service (`MEOW_MODE`).
    pub fn mode() -> Mode {
        if std::env::var("MEOW_MODE").is_ok_and(|m| m == "service") { Mode::Service } else { Mode::App }
    }
    /// This instance is the app's background service.
    pub fn is_service() -> bool {
        App::mode() == Mode::Service
    }
    /// What the app's background service passed to `request_foreground` when it had this
    /// app opened. None when the user opened it.
    pub fn launch_payload() -> Option<String> {
        std::env::var("MEOW_LAUNCH_PAYLOAD").ok()
    }
    /// The sockets the app's background service handed over with its foreground request
    /// (`System::request_foreground_with`), in its order. The first call gets them; they
    /// are closed when dropped. Empty when the user opened the app.
    pub fn launch_fds() -> Vec<OwnedFd> {
        handover::launch_fds()
    }
    /// The bytes that came with `launch_fds` (first call only; empty when there are none).
    pub fn launch_data() -> Vec<u8> {
        handover::launch_data()
    }
    /// Whether the user granted `perm` ("display", "input", "media", "storage", "internet",
    /// "network", "lan", "background", "manage-receivers", "manage-apps"). A background service never has
    /// "display", "input" or "media".
    pub fn has(&self, perm: &str) -> bool {
        self.permissions.iter().any(|p| p == perm)
    }

    /// The broker fd, for an app's own poll loop (readable = call `poll_events`).
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn handle(&mut self, msg: String, fd: Option<OwnedFd>) {
        let (head, rest) = msg.split_once('\t').unwrap_or((msg.as_str(), ""));
        match head {
            "KEY" => self.queue.push_back(Event::Key(Key::parse(rest))),
            "QUIT" => self.queue.push_back(Event::Quit),
            "MUTE" => self.muted = rest == "1",
            "VOLUME" => {
                // Also the echo of this app's own `set_volume`: the system's value is the truth.
                if let Ok(v) = rest.parse::<u8>().map(|v| v.min(100)) {
                    self.volume = v;
                    osd::notify(v, self.muted);
                    self.queue.push_back(Event::Volume(v));
                }
            }
            "FOREGROUND" => {
                if let Some(why) = rest.strip_prefix("denied") {
                    self.queue.push_back(Event::ForegroundDenied(why.trim_start_matches('\t').to_string()));
                }
            }
            "MIRACAST" => self.queue.push_back(Event::Miracast(rest.to_string())),
            "STORE" => self.queue.push_back(Event::Store(rest.to_string())),
            // The network service was restarted: its new dial socket.
            "NET" => {
                if let Some(fd) = fd {
                    net::replace_dial(fd);
                }
            }
            // A connection to the image service, for the thread that asked (image.rs).
            "IMAGE" => image::answer(rest, fd),
            // A firmware from before the image service answers the request like this.
            "ERR" if image::asking() => image::answer("unavailable\tthis firmware has no image service", None),
            _ => self.replies.push_back((msg, fd)),
        }
    }

    fn request_reply(&mut self, head: &str, timeout: Duration) -> Result<(String, Option<OwnedFd>), Error> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(i) = self.replies.iter().position(|(m, _)| m == head || m.starts_with(&format!("{head}\t"))) {
                let r = self.replies.remove(i).unwrap();
                self.replies.clear(); // stale replies to earlier, timed-out requests
                return Ok(r);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Protocol(format!("no {head} reply")));
            }
            let mut p = libc::pollfd { fd: self.fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, left.as_millis() as i32 + 1) };
            while let Some((m, f)) = recv(self.fd(), false).map_err(Error::Io)? {
                self.handle(m, f);
            }
        }
    }

    /// Events received so far (never blocks).
    pub fn poll_events(&mut self) -> Vec<Event> {
        loop {
            match recv(self.fd(), false) {
                Ok(Some((m, f))) => self.handle(m, f),
                Ok(None) => break,
                Err(_) => {
                    // appd is gone: nothing will ever arrive again; treat as Quit.
                    self.queue.push_back(Event::Quit);
                    break;
                }
            }
        }
        self.queue.drain(..).collect()
    }

    /// Wait up to `timeout` (None = forever) for events.
    pub fn wait(&mut self, timeout: Option<Duration>) -> Vec<Event> {
        if self.queue.is_empty() {
            let mut p = libc::pollfd { fd: self.fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, timeout.map_or(-1, |t| t.as_millis() as i32)) };
        }
        self.poll_events()
    }

    /// The system volume, 0-100 (see `Event::Volume`, `volume_gain`).
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// The system's sound is switched off (`volume` keeps the level).
    pub fn muted(&self) -> bool {
        self.muted
    }
    /// The gain for 16-bit samples now: `volume_gain(volume)`, or 0 while muted.
    pub fn gain(&self) -> i32 {
        if self.muted { 0 } else { volume_gain(self.volume) }
    }
    /// Set the system volume (needs "media").
    pub fn set_volume(&mut self, volume: u8) {
        self.volume = volume.min(100);
        self.system().set_volume(self.volume);
    }
    /// A background service asks for its app to be opened: see `System::request_foreground`.
    pub fn request_foreground(&self, payload: &str) -> bool {
        self.system().request_foreground(payload)
    }

    /// `request_foreground` with sockets and data for the app: see
    /// `System::request_foreground_with`.
    pub fn request_foreground_with(&self, payload: &str, fds: &[std::os::fd::BorrowedFd<'_>], data: &[u8]) -> bool {
        self.system().request_foreground_with(payload, fds, data)
    }

    /// A handle for other threads: volume, foreground and log requests.
    pub fn system(&self) -> System {
        System { fd: self.fd.clone() }
    }

    /// The KMS display (DRM master) fd. Only while the app is in the foreground with the
    /// "display" permission; the system can revoke it at any time (KMS calls then fail).
    pub fn display_fd(&mut self) -> Result<OwnedFd, Error> {
        send(self.fd(), "DISPLAY").map_err(Error::Io)?;
        let (m, fd) = self.request_reply("DISPLAY", Duration::from_secs(3))?;
        match (m.split('\t').nth(1), fd) {
            (Some("ok"), Some(fd)) => Ok(fd),
            _ => Err(Error::Denied(m.split('\t').skip(2).collect::<Vec<_>>().join(" "))),
        }
    }

    fn background_request(&mut self, request: String) -> Result<Background, Error> {
        send(self.fd(), &request).map_err(Error::Io)?;
        let (reply, _) = self.request_reply("BACKGROUND", Duration::from_secs(3))?;
        match reply.split('\t').nth(2) {
            Some("absent") => Ok(Background::Absent),
            Some("on") => Ok(Background::On),
            Some("off") => Ok(Background::Off),
            Some("denied") => Err(Error::Denied("not an app this one manages".into())),
            _ => Err(Error::Protocol(reply)),
        }
    }

    /// The "Run in background" switch of another app. Only for an app with the
    /// "manage-receivers" permission, and only for the ids its manifest lists under
    /// "manages" (anything else is `Error::Denied`).
    pub fn background_of(&mut self, id: &str) -> Result<Background, Error> {
        self.background_request(format!("BACKGROUND\tget\t{id}"))
    }

    /// Set that switch, as the user does in the launcher's app details: on starts the
    /// other app's background service, off stops it. Returns the switch as it is then.
    pub fn set_background_of(&mut self, id: &str, on: bool) -> Result<Background, Error> {
        self.background_request(format!("BACKGROUND\tset\t{id}\t{}", on as u8))
    }

    /// Ask appd to save /data to flash now ("storage"). Rate-limited by the system: the
    /// reply may be "deferred" (saved a few seconds later). /data is always saved at exit.
    pub fn sync_storage(&mut self) -> Result<String, Error> {
        send(self.fd(), "SYNC").map_err(Error::Io)?;
        let (m, _) = self.request_reply("SYNC", Duration::from_secs(10))?;
        let r = m.split_once('\t').map_or("", |x| x.1).to_string();
        if r.starts_with("error") { Err(Error::Denied(r)) } else { Ok(r) }
    }

    /// A line in the app's log (`appctl logs <id>`); stdout/stderr go there too.
    pub fn log(&self, msg: &str) {
        let clean: String = msg.chars().map(|c| if c.is_control() { ' ' } else { c }).take(1000).collect();
        let _ = send(self.fd(), &format!("LOG\t{clean}"));
    }

    /// The launcher's splash colour (the app's tint, RGB 0..1). The screen shows a plain
    /// panel of this colour when the app gets it, so fading in from it gives a seamless
    /// launch (see examples/inspector).
    pub fn splash_color() -> Option<[f32; 3]> {
        let s = std::env::var("MEOW_SPLASH").ok()?;
        let v = u32::from_str_radix(s.trim_start_matches('#'), 16).ok()?;
        Some([((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0])
    }

    /// Facts about this sandbox, as appd set them up (MEOW_SANDBOX).
    pub fn sandbox_info() -> String {
        std::env::var("MEOW_SANDBOX").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (App, OwnedFd) {
        let (ours, theirs) = ipc::seqpacket_pair().unwrap();
        (App { fd: Arc::new(ours), volume: 100, muted: false, id: String::new(), permissions: Vec::new(), queue: VecDeque::new(), replies: VecDeque::new() }, theirs)
    }

    #[test]
    fn volume_and_mute_arrive_as_one_event() {
        let (mut app, system) = app();
        for msg in ["MUTE\t1", "VOLUME\t40", "MUTE\t0", "VOLUME\t40"] {
            ipc::send(system.as_raw_fd(), msg, None).unwrap();
        }
        assert_eq!(app.poll_events(), [Event::Volume(40), Event::Volume(40)]);
        assert_eq!((app.volume(), app.muted(), app.gain()), (40, false, volume_gain(40)));
        ipc::send(system.as_raw_fd(), "MUTE\t1", None).unwrap();
        ipc::send(system.as_raw_fd(), "VOLUME\t40", None).unwrap();
        app.poll_events();
        assert_eq!((app.muted(), app.gain()), (true, 0));
    }

    #[test]
    fn another_apps_background_switch() {
        let (mut app, system) = app();
        // The system's side answers from a thread, as appd does from its loop.
        let answers = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in ["BACKGROUND\tdev.meow.youtube\ton", "BACKGROUND\tdev.meow.youtube\toff", "BACKGROUND\tdev.meow.gone\tabsent", "BACKGROUND\tdev.meow.other\tdenied"] {
                let deadline = Instant::now() + Duration::from_secs(5);
                seen.push(ipc::recv_until(system.as_raw_fd(), deadline).unwrap().unwrap().0);
                // Something else arrives in between: it must not be taken for the answer.
                ipc::send(system.as_raw_fd(), "KEY\tup", None).unwrap();
                ipc::send(system.as_raw_fd(), reply, None).unwrap();
            }
            (seen, system) // the socket stays open until the app has read everything
        });
        assert_eq!(app.background_of("dev.meow.youtube").unwrap(), Background::On);
        assert_eq!(app.set_background_of("dev.meow.youtube", false).unwrap(), Background::Off);
        assert_eq!(app.background_of("dev.meow.gone").unwrap(), Background::Absent);
        assert!(matches!(app.set_background_of("dev.meow.other", true), Err(Error::Denied(_))));
        let (seen, _system) = answers.join().unwrap();
        assert_eq!(seen, ["BACKGROUND\tget\tdev.meow.youtube", "BACKGROUND\tset\tdev.meow.youtube\t0", "BACKGROUND\tget\tdev.meow.gone", "BACKGROUND\tset\tdev.meow.other\t1"]);
        assert_eq!(app.poll_events().len(), 4, "the keys are still delivered");
    }

    #[test]
    fn a_service_asks_for_the_foreground() {
        let (mut app, system) = app();
        assert!(app.request_foreground("{\"v\":\"abc\"}"));
        assert_eq!(ipc::recv(system.as_raw_fd(), false).unwrap().unwrap().0, "FOREGROUND\t{\"v\":\"abc\"}");
        assert!(!app.request_foreground("two\nlines"), "refused here, nothing sent");
        assert!(!app.request_foreground(&"x".repeat(PAYLOAD_MAX + 1)));
        assert!(ipc::recv(system.as_raw_fd(), false).unwrap().is_none());
        ipc::send(system.as_raw_fd(), "FOREGROUND\tok", None).unwrap();
        ipc::send(system.as_raw_fd(), "FOREGROUND\tdenied\tasked a moment ago", None).unwrap();
        assert_eq!(app.poll_events(), [Event::ForegroundDenied("asked a moment ago".into())]);
    }
}
