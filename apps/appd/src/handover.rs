//! What a background service hands to its app when it asks for the foreground: sockets that
//! are in use (a listening port, a sender's connection that is waiting for an answer) and a
//! small block of bytes (the session state that goes with them).
//!
//! The service sends `ATTACH socket` or `ATTACH data`, each with one fd, before
//! `FOREGROUND payload`. appd keeps the fds (it never reads from or writes to them), and the
//! app it then launches inherits them on fixed numbers from `sandbox::LAUNCH_FD` on, named
//! in MEOW_LAUNCH_FDS and MEOW_LAUNCH_DATA. If the launch is refused, fails or times out,
//! the fds are closed here; the service still has its own copies.
//!
//! What is accepted is checked on the fd itself, not on what the service says:
//! - a socket must be TCP or UDP over IPv4/IPv6, and the service must hold "lan" or
//!   "network". That is exactly the kind of socket the service can make itself, and the app
//!   runs under the same uid with the same grants (plus screen, keys and decoder), so the
//!   app gains nothing it could not open. Unix sockets (the broker, the network service's
//!   dial), netlink, packet sockets, files, devices and pipes are refused;
//! - the data must be a sealed memfd of at most `DATA_MAX` bytes: its size cannot change
//!   after the check, and its pages are bounded.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

/// Sockets plus the data block.
pub const MAX_FDS: usize = 8;
pub const DATA_MAX: i64 = 64 * 1024;

const F_SEAL_SHRINK: libc::c_int = 0x0002;
const F_SEAL_GROW: libc::c_int = 0x0004;
const F_SEAL_WRITE: libc::c_int = 0x0008;

#[derive(Default)]
pub struct Handover {
    sockets: Vec<OwnedFd>,
    data: Option<OwnedFd>,
    /// Why the whole set is refused (the first attachment that failed).
    refused: Option<String>,
}

fn sockopt(fd: RawFd, name: libc::c_int) -> Option<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, name, &mut value as *mut _ as *mut libc::c_void, &mut len) };
    (r == 0).then_some(value)
}

/// A TCP or UDP socket over IP: what "lan" lets a sandbox create.
pub fn check_socket(fd: RawFd) -> Result<(), String> {
    let (Some(domain), Some(kind), Some(protocol)) = (sockopt(fd, libc::SO_DOMAIN), sockopt(fd, libc::SO_TYPE), sockopt(fd, libc::SO_PROTOCOL)) else {
        return Err("not a socket".into());
    };
    let ip = domain == libc::AF_INET || domain == libc::AF_INET6;
    let tcp = kind == libc::SOCK_STREAM && protocol == libc::IPPROTO_TCP;
    let udp = kind == libc::SOCK_DGRAM && protocol == libc::IPPROTO_UDP;
    if ip && (tcp || udp) { Ok(()) } else { Err("only TCP and UDP sockets can be handed over".into()) }
}

/// A memfd that can no longer change, of at most DATA_MAX bytes.
pub fn check_data(fd: RawFd) -> Result<(), String> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err("the data is not a memfd".into());
    }
    // Only a memfd made with MFD_ALLOW_SEALING can carry these seals (other files answer
    // EINVAL, or F_SEAL_SEAL alone).
    let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
    let wanted = F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;
    if seals < 0 || seals & wanted != wanted {
        return Err("the data is not a sealed memfd".into());
    }
    #[allow(clippy::unnecessary_cast)] // off_t is 32 bits on the dongle
    if st.st_size as i64 > DATA_MAX {
        return Err(format!("the data is larger than {} KiB", DATA_MAX / 1024));
    }
    Ok(())
}

impl Handover {
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty() && self.data.is_none() && self.refused.is_none()
    }

    pub fn count(&self) -> usize {
        self.sockets.len() + self.data.is_some() as usize
    }

    /// One `ATTACH kind` message. `sockets_allowed`: the service holds "lan" or "network".
    /// A failure refuses the whole set (`take` reports it), so a service never ends up
    /// handing over half of a session.
    pub fn attach(&mut self, kind: &str, fd: Option<OwnedFd>, sockets_allowed: bool) {
        let result = match (kind, fd) {
            (_, None) => Err("ATTACH without a file descriptor".to_string()),
            _ if self.count() >= MAX_FDS => Err(format!("more than {MAX_FDS} file descriptors")),
            ("socket", Some(_)) if !sockets_allowed => Err("sockets need the lan or network permission".into()),
            ("socket", Some(fd)) => check_socket(fd.as_raw_fd()).map(|_| self.sockets.push(fd)),
            ("data", Some(_)) if self.data.is_some() => Err("more than one data block".into()),
            ("data", Some(fd)) => check_data(fd.as_raw_fd()).map(|_| self.data = Some(fd)),
            _ => Err("unknown kind of attachment".into()),
        };
        if let Err(e) = result {
            self.refused.get_or_insert(e);
        }
    }

    /// The set for a `FOREGROUND` request, or why it is refused. Either way `self` is empty
    /// afterwards (refused fds are closed).
    pub fn take(&mut self) -> Result<Handover, String> {
        let taken = std::mem::take(self);
        match taken.refused {
            Some(why) => Err(why),
            None => Ok(taken),
        }
    }

    /// The fds in the order they get their numbers in the app: sockets as attached, then
    /// the data.
    pub fn raw(&self) -> Vec<RawFd> {
        self.sockets.iter().chain(&self.data).map(|fd| fd.as_raw_fd()).collect()
    }

    /// The app's environment, when the fds sit on `first`, `first + 1`, ...
    pub fn env(&self, first: i32) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if !self.sockets.is_empty() {
            let numbers: Vec<String> = (0..self.sockets.len() as i32).map(|i| (first + i).to_string()).collect();
            env.push(("MEOW_LAUNCH_FDS".to_string(), numbers.join(",")));
        }
        if self.data.is_some() {
            env.push(("MEOW_LAUNCH_DATA".to_string(), (first + self.sockets.len() as i32).to_string()));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    fn socket(domain: libc::c_int, kind: libc::c_int) -> OwnedFd {
        let fd = unsafe { libc::socket(domain, kind | libc::SOCK_CLOEXEC, 0) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn memfd(size: usize, seals: libc::c_int) -> OwnedFd {
        let fd = unsafe { libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        assert!(fd >= 0);
        let bytes = vec![7u8; size];
        assert_eq!(unsafe { libc::write(fd, bytes.as_ptr() as *const _, size) }, size as isize);
        if seals != 0 {
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) }, 0);
        }
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    const SEALED: libc::c_int = F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;

    #[test]
    fn only_ip_sockets_and_sealed_memfds_pass() {
        assert!(check_socket(socket(libc::AF_INET, libc::SOCK_STREAM).as_raw_fd()).is_ok());
        assert!(check_socket(socket(libc::AF_INET, libc::SOCK_DGRAM).as_raw_fd()).is_ok());
        assert!(check_socket(socket(libc::AF_UNIX, libc::SOCK_STREAM).as_raw_fd()).is_err());
        assert!(check_socket(socket(libc::AF_UNIX, libc::SOCK_SEQPACKET).as_raw_fd()).is_err());
        assert!(check_socket(socket(libc::AF_NETLINK, libc::SOCK_RAW).as_raw_fd()).is_err());
        let file = std::fs::File::open("/proc/self/exe").unwrap();
        assert_eq!(check_socket(file.as_raw_fd()).unwrap_err(), "not a socket");

        assert!(check_data(memfd(100, SEALED).as_raw_fd()).is_ok());
        assert!(check_data(memfd(DATA_MAX as usize, SEALED).as_raw_fd()).is_ok());
        assert!(check_data(memfd(DATA_MAX as usize + 1, SEALED).as_raw_fd()).unwrap_err().contains("larger"));
        assert!(check_data(memfd(100, 0).as_raw_fd()).unwrap_err().contains("sealed"));
        assert!(check_data(memfd(100, F_SEAL_GROW).as_raw_fd()).is_err(), "it could still be written");
        assert!(check_data(file.as_raw_fd()).is_err());
        assert!(check_data(socket(libc::AF_INET, libc::SOCK_STREAM).as_raw_fd()).is_err());
    }

    #[test]
    fn a_set_is_taken_whole_or_refused_whole() {
        let mut set = Handover::default();
        assert!(set.is_empty() && set.take().is_ok_and(|s| s.raw().is_empty()));
        set.attach("socket", Some(socket(libc::AF_INET, libc::SOCK_STREAM)), true);
        set.attach("data", Some(memfd(10, SEALED)), true);
        set.attach("socket", Some(socket(libc::AF_INET, libc::SOCK_DGRAM)), true);
        let taken = set.take().unwrap();
        assert!(set.is_empty());
        assert_eq!(taken.count(), 3);
        // Sockets first, in order, then the data.
        assert_eq!(taken.env(5), [("MEOW_LAUNCH_FDS".to_string(), "5,6".to_string()), ("MEOW_LAUNCH_DATA".to_string(), "7".to_string())]);
        assert_eq!(taken.raw().len(), 3);
        assert!(check_data(taken.raw()[2]).is_ok());

        // One bad attachment refuses everything, and the set is empty again afterwards.
        set.attach("socket", Some(socket(libc::AF_INET, libc::SOCK_STREAM)), true);
        set.attach("socket", Some(socket(libc::AF_UNIX, libc::SOCK_STREAM)), true);
        set.attach("data", Some(memfd(10, SEALED)), true);
        assert_eq!(set.take().err().unwrap(), "only TCP and UDP sockets can be handed over");
        assert!(set.is_empty());

        set.attach("socket", Some(socket(libc::AF_INET, libc::SOCK_STREAM)), false);
        assert!(set.take().err().unwrap().contains("lan"));
        set.attach("data", Some(memfd(10, SEALED)), false);
        assert_eq!(set.take().unwrap().env(5), [("MEOW_LAUNCH_DATA".to_string(), "5".to_string())]);
        set.attach("data", Some(memfd(10, SEALED)), true);
        set.attach("data", Some(memfd(10, SEALED)), true);
        assert!(set.take().is_err());
        set.attach("socket", None, true);
        assert!(set.take().is_err());
        set.attach("pipe", Some(memfd(10, SEALED)), true);
        assert!(set.take().is_err());
        for _ in 0..MAX_FDS + 1 {
            set.attach("socket", Some(socket(libc::AF_INET, libc::SOCK_DGRAM)), true);
        }
        assert!(set.take().err().unwrap().contains("more than 8"));
    }

    /// The fd arrives over a broker-like socket, as in the daemon.
    #[test]
    fn attachments_travel_over_the_broker_socket() {
        let (service, appd) = crate::ipc::socketpair().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        crate::ipc::send(service.as_raw_fd(), "ATTACH\tsocket", Some(listener.as_raw_fd())).unwrap();
        drop(listener); // the service exits; appd's copy keeps the port open
        let mut set = Handover::default();
        let crate::ipc::Recv::Msg(message, fd) = crate::ipc::recv(appd.as_raw_fd()).unwrap() else { panic!() };
        set.attach(message.split('\t').nth(1).unwrap(), fd, true);
        let taken = set.take().unwrap();
        // A sender that connects meanwhile waits in the backlog instead of being refused.
        let client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let inherited = unsafe { libc::dup(taken.raw()[0]) };
        let inherited = unsafe { std::net::TcpListener::from_raw_fd(inherited) };
        drop(taken);
        let (_, peer) = inherited.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
    }
}
