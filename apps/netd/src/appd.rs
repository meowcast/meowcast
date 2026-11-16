//! Client of appd's control socket (/run/appd/ui.sock, SEQPACKET, root peers only), speaking as
//! `appctl` does: `HELLO ctl`, then one command and its lines up to `OK [text]` or `ERR text`.
//! The pilot uses it for the system volume, the installed apps, the app on screen and keys
//! that go straight to that app. netd is linked into the appd binary but runs as its own
//! process, so it talks to the daemon like any other root tool.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub const SOCKET: &str = "/run/appd/ui.sock";
const MAX_MSG: usize = 8192;

pub struct Client {
    fd: OwnedFd,
}

/// The lines before the final reply, and the text of `OK`.
pub type Reply = Result<(Vec<String>, String), String>;

impl Client {
    pub fn connect() -> io::Result<Client> {
        Self::connect_to(&std::env::var("NETD_APPD").unwrap_or_else(|_| SOCKET.into()))
    }

    pub fn connect_to(path: &str) -> io::Result<Client> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        a.sun_family = libc::AF_UNIX as _;
        if path.len() >= a.sun_path.len() {
            return Err(io::Error::other("socket path too long"));
        }
        for (i, b) in path.bytes().enumerate() {
            a.sun_path[i] = b as _;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if unsafe { libc::connect(fd.as_raw_fd(), &a as *const _ as *const libc::sockaddr, len) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let c = Client { fd };
        c.send("HELLO\tctl")?;
        Ok(c)
    }

    fn send(&self, msg: &str) -> io::Result<()> {
        // MSG_NOSIGNAL: a vanished appd is an error, not SIGPIPE.
        let n = unsafe { libc::send(self.fd.as_raw_fd(), msg.as_ptr().cast(), msg.len(), libc::MSG_NOSIGNAL) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// One message, or None when `deadline` passes. Err: appd closed the connection.
    fn recv(&self, deadline: Instant) -> io::Result<Option<String>> {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, left.as_millis().min(60_000) as i32) } <= 0 {
            return Ok(None);
        }
        let mut buf = vec![0u8; MAX_MSG];
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) };
        }
        if n == 0 {
            return Err(io::Error::other("appd closed the connection"));
        }
        buf.truncate(n as usize);
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// Send a command and collect its reply: Ok(Err(text)) is appd's `ERR`. An io error means
    /// the connection is no longer usable (appd restarted, or did not answer in time).
    pub fn request(&self, cmd: &str, timeout: Duration) -> io::Result<Reply> {
        self.send(cmd)?;
        let deadline = Instant::now() + timeout;
        let mut lines = Vec::new();
        loop {
            let Some(m) = self.recv(deadline)? else {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "appd did not answer"));
            };
            let (head, rest) = m.split_once('\t').unwrap_or((m.as_str(), ""));
            match head {
                "WELCOME" => {}
                "OK" => return Ok(Ok((lines, rest.to_string()))),
                "ERR" => return Ok(Err(rest.to_string())),
                _ => lines.push(m),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    pub id: String,
    pub name: String,
    /// available | installed | fetching | running | error
    pub state: String,
}

/// `APP id name summary glyph tint badge0 badge1 shape state detail` (a LIST line).
pub fn parse_app(line: &str) -> Option<App> {
    let f: Vec<&str> = line.split('\t').collect();
    (f.len() >= 10 && f[0] == "APP" && !f[1].is_empty()).then(|| App { id: f[1].to_string(), name: f[2].to_string(), state: f[9].to_string() })
}

/// `RUNNING id pid=.. secs=.. mem_kb=.. peak_kb=.. pids=.. display=true|false` (a PS line):
/// the id and whether the app has the screen.
pub fn parse_running(line: &str) -> Option<(String, bool)> {
    let mut f = line.split('\t');
    if f.next() != Some("RUNNING") {
        return None;
    }
    let id = f.next().filter(|s| !s.is_empty())?.to_string();
    Some((id, f.any(|x| x == "display=true")))
}

/// The text of `OK` after `VOLUME ...`: "40", or "40 muted" from an appd with a mute switch.
/// The mute state is None when the reply does not say (which, from such an appd, means "not
/// muted": the caller knows which appd it talks to).
pub fn parse_volume(text: &str) -> Option<(u8, Option<bool>)> {
    let mut words = text.split(['\t', ' ']).filter(|w| !w.is_empty());
    let level = words.next()?.parse::<u8>().ok()?.min(100);
    let muted = match words.next() {
        Some("muted") => Some(true),
        Some("unmuted") => Some(false),
        _ => None,
    };
    Some((level, muted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_lines() {
        let a = parse_app("APP\tdev.meow.youtube\tYouTube\tCast from your phone\tplay\t#ff0000\t\t\tround\trunning\t").unwrap();
        assert_eq!(a, App { id: "dev.meow.youtube".into(), name: "YouTube".into(), state: "running".into() });
        assert!(parse_app("APP\tx\ty").is_none());
        assert!(parse_app("RUNNING\tx").is_none());
    }

    #[test]
    fn running_lines() {
        let l = "RUNNING\tdev.meow.youtube\tpid=312\tsecs=14\tmem_kb=20480\tpeak_kb=30000\tpids=9\tdisplay=true";
        assert_eq!(parse_running(l), Some(("dev.meow.youtube".into(), true)));
        assert_eq!(parse_running("RUNNING\tdev.meow.net\tpid=1\tdisplay=false"), Some(("dev.meow.net".into(), false)));
        assert_eq!(parse_running("APP\tx"), None);
    }

    #[test]
    fn volume_replies() {
        assert_eq!(parse_volume("40"), Some((40, None)));
        assert_eq!(parse_volume("40 muted"), Some((40, Some(true))));
        assert_eq!(parse_volume("0 muted"), Some((0, Some(true))));
        assert_eq!(parse_volume("40 unmuted"), Some((40, Some(false))));
        assert_eq!(parse_volume("250"), Some((100, None)));
        assert_eq!(parse_volume(""), None);
        assert_eq!(parse_volume("muted"), None);
    }

    /// A stand-in appd on a socketpair-like listener: HELLO, a multi-line reply, an error.
    #[test]
    fn request_reply() {
        let path = format!("/tmp/netd-appd-test-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&path);
        let l = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        a.sun_family = libc::AF_UNIX as _;
        for (i, b) in path.bytes().enumerate() {
            a.sun_path[i] = b as _;
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        assert_eq!(unsafe { libc::bind(l, &a as *const _ as *const libc::sockaddr, len) }, 0);
        assert_eq!(unsafe { libc::listen(l, 1) }, 0);
        let server = std::thread::spawn(move || {
            let c = unsafe { libc::accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
            let say = |s: &str| unsafe { libc::send(c, s.as_ptr().cast(), s.len(), 0) };
            let hear = || {
                let mut b = [0u8; 256];
                let n = unsafe { libc::recv(c, b.as_mut_ptr().cast(), b.len(), 0) };
                String::from_utf8_lossy(&b[..n.max(0) as usize]).into_owned()
            };
            assert_eq!(hear(), "HELLO\tctl");
            say("WELCOME\tappd test");
            assert_eq!(hear(), "PS");
            say("RUNNING\ta\tdisplay=true");
            say("OK");
            assert_eq!(hear(), "VOLUME\t+5");
            say("OK\t45");
            assert_eq!(hear(), "VOLUME\tmute");
            say("ERR\tvolume mute: expected 0..100, +N or -N");
            unsafe { libc::close(c) };
            unsafe { libc::close(l) };
        });
        let c = Client::connect_to(&path).unwrap();
        let t = Duration::from_secs(2);
        assert_eq!(c.request("PS", t).unwrap(), Ok((vec!["RUNNING\ta\tdisplay=true".to_string()], String::new())));
        assert_eq!(c.request("VOLUME\t+5", t).unwrap(), Ok((vec![], "45".to_string())));
        assert!(c.request("VOLUME\tmute", t).unwrap().is_err());
        server.join().unwrap();
        // appd went away: an io error, so the caller reconnects.
        assert!(c.request("PS", t).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
