//! Handing live sockets from an app's background service to the app.
//!
//! A service that asks for the foreground stops before its app starts, and its sockets
//! would close with it: a listening port would refuse connections for a moment, and a
//! sender waiting for an answer would see its connection reset. So the service can attach
//! them to the request ([`crate::System::request_foreground_with`]): the system keeps them
//! open across the switch and the app finds them with [`crate::App::launch_fds`], in the
//! same order, plus a block of bytes ([`crate::App::launch_data`]) for the state that goes
//! with them.
//!
//! ```no_run
//! # use std::os::fd::AsFd;
//! # let (listener, connection) = (std::net::TcpListener::bind("0.0.0.0:7000").unwrap(), std::net::TcpStream::connect("10.0.0.2:1").unwrap());
//! # let app = meow_app::App::connect().unwrap();
//! // In the service: a sender connected and asked for a session.
//! app.request_foreground_with("cast", &[listener.as_fd(), connection.as_fd()], b"what was read so far");
//! // Keep the sockets, touch them no more, and exit on Event::Quit (or carry on after
//! // Event::ForegroundDenied: the copies were closed, these still work).
//!
//! // In the app:
//! let mut fds = meow_app::App::launch_fds().into_iter();
//! if let (Some(listener), Some(connection)) = (fds.next(), fds.next()) {
//!     let listener = std::net::TcpListener::from(listener);
//!     let connection = std::net::TcpStream::from(connection);
//!     let state = meow_app::App::launch_data();
//! }
//! ```
//!
//! Only TCP and UDP sockets pass (the service needs "lan" or "network" to have any), at most
//! [`FDS_MAX`] of them, and at most [`DATA_MAX`] bytes. Anything else refuses the whole
//! request (`Event::ForegroundDenied`).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

/// Sockets per request.
pub const FDS_MAX: usize = 7;
/// Bytes of data per request.
pub const DATA_MAX: usize = 64 * 1024;

const F_SEAL_SEAL: libc::c_int = 0x0001;
const F_SEAL_SHRINK: libc::c_int = 0x0002;
const F_SEAL_GROW: libc::c_int = 0x0004;
const F_SEAL_WRITE: libc::c_int = 0x0008;

/// `data` in a memfd that can no longer change (the system checks its size once).
fn sealed(data: &[u8]) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::memfd_create(c"launch-data".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut written = 0;
    while written < data.len() {
        let n = unsafe { libc::write(fd.as_raw_fd(), data[written..].as_ptr() as *const libc::c_void, data.len() - written) };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        written += n as usize;
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_SEAL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Send the attachments of one foreground request, then the request. The caller holds the
/// lock that keeps another thread's request from mixing in.
pub(crate) fn request(broker: RawFd, payload: &str, fds: &[BorrowedFd<'_>], data: &[u8]) -> io::Result<()> {
    if fds.len() > FDS_MAX || data.len() > DATA_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "too much to hand over"));
    }
    let block = if data.is_empty() { None } else { Some(sealed(data)?) };
    for fd in fds {
        crate::ipc::send(broker, "ATTACH\tsocket", Some(fd.as_raw_fd()))?;
    }
    if let Some(block) = &block {
        crate::ipc::send(broker, "ATTACH\tdata", Some(block.as_raw_fd()))?;
    }
    crate::ipc::send(broker, &format!("FOREGROUND\t{payload}"), None)
}

/// The fds a list like "5,6,7" names, if they are open. They become close-on-exec.
fn take(list: &str) -> Vec<OwnedFd> {
    let mut out = Vec::new();
    for number in list.split(',').filter_map(|n| n.trim().parse::<RawFd>().ok()).filter(|n| *n > 2) {
        if out.iter().any(|fd: &OwnedFd| fd.as_raw_fd() == number) || unsafe { libc::fcntl(number, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            continue;
        }
        out.push(unsafe { OwnedFd::from_raw_fd(number) });
    }
    out
}

fn read_all(fd: &OwnedFd) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    while out.len() <= DATA_MAX {
        // pread: the writer's file position came along with the fd.
        let n = unsafe { libc::pread(fd.as_raw_fd(), chunk.as_mut_ptr() as *mut libc::c_void, chunk.len(), out.len() as libc::off_t) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n as usize]);
    }
    out.truncate(DATA_MAX);
    out
}

static FDS_TAKEN: AtomicBool = AtomicBool::new(false);
static DATA_TAKEN: AtomicBool = AtomicBool::new(false);

/// See `App::launch_fds`: the first call gets them, later calls get nothing (an fd has one
/// owner).
pub(crate) fn launch_fds() -> Vec<OwnedFd> {
    if FDS_TAKEN.swap(true, Ordering::SeqCst) {
        return Vec::new();
    }
    std::env::var("MEOW_LAUNCH_FDS").map(|list| take(&list)).unwrap_or_default()
}

pub(crate) fn launch_data() -> Vec<u8> {
    if DATA_TAKEN.swap(true, Ordering::SeqCst) {
        return Vec::new();
    }
    std::env::var("MEOW_LAUNCH_DATA").ok().and_then(|n| take(&n).pop()).map(|fd| read_all(&fd)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsFd;

    /// The system's side, as appd does it: receive, keep, give the app its numbers.
    fn relay(system: RawFd) -> (Vec<OwnedFd>, Option<OwnedFd>, String) {
        let (mut sockets, mut data) = (Vec::new(), None);
        loop {
            let (message, fd) = crate::ipc::recv(system, false).unwrap().expect("a message");
            match (message.as_str(), fd) {
                ("ATTACH\tsocket", Some(fd)) => sockets.push(fd),
                ("ATTACH\tdata", Some(fd)) => data = Some(fd),
                (request, None) if request.starts_with("FOREGROUND\t") => return (sockets, data, request[11..].to_string()),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_live_connection_and_its_state_reach_the_app() {
        let (service, system) = crate::ipc::seqpacket_pair().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut sender = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (connection, _) = listener.accept().unwrap();
        sender.write_all(b"SETUP").unwrap();
        let state: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        request(service.as_raw_fd(), "cast", &[listener.as_fd(), connection.as_fd()], &state).unwrap();
        // The service is gone before the app starts.
        drop((listener, connection, service));

        let (sockets, data, payload) = relay(system.as_raw_fd());
        assert_eq!((sockets.len(), payload.as_str()), (2, "cast"));
        let data = data.unwrap();
        // The block is sealed: nobody can grow it after the system looked at its size.
        assert_eq!(unsafe { libc::ftruncate(data.as_raw_fd(), 1 << 20) }, -1);
        assert_eq!(unsafe { libc::write(data.as_raw_fd(), b"x".as_ptr() as *const _, 1) }, -1);

        // The app: fds named by number in the environment.
        let list = sockets.iter().map(|fd| fd.as_raw_fd().to_string()).collect::<Vec<_>>().join(",");
        let numbers: Vec<RawFd> = sockets.into_iter().map(std::os::fd::IntoRawFd::into_raw_fd).collect();
        let mut taken = take(&format!("{list},{},999999,x", numbers[0])).into_iter();
        let (listener, mut connection) = (TcpListener::from(taken.next().unwrap()), TcpStream::from(taken.next().unwrap()));
        assert!(taken.next().is_none(), "a repeated, a closed and a malformed number are skipped");
        assert_eq!(unsafe { libc::fcntl(numbers[1], libc::F_GETFD) } & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        assert_eq!(read_all(&data), state);

        // The sender never noticed: its request is answered on the same connection, and the
        // port takes new connections.
        let mut request = [0u8; 5];
        connection.read_exact(&mut request).unwrap();
        connection.write_all(b"200 OK").unwrap();
        let mut answer = [0u8; 6];
        sender.read_exact(&mut answer).unwrap();
        assert_eq!((&request, &answer), (b"SETUP", b"200 OK"));
        assert_eq!(listener.local_addr().unwrap().port(), port);
        let _second = TcpStream::connect(("127.0.0.1", port)).unwrap();
        assert!(listener.accept().is_ok());
    }

    #[test]
    fn limits_are_checked_before_anything_is_sent() {
        let (service, system) = crate::ipc::seqpacket_pair().unwrap();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let many = vec![socket.as_fd(); FDS_MAX + 1];
        assert!(request(service.as_raw_fd(), "p", &many, b"").is_err());
        assert!(request(service.as_raw_fd(), "p", &[], &vec![0; DATA_MAX + 1]).is_err());
        assert!(crate::ipc::recv(system.as_raw_fd(), false).unwrap().is_none());
        // Without data no block is made; the limits themselves pass.
        request(service.as_raw_fd(), "p", &many[..FDS_MAX], &vec![1; DATA_MAX]).unwrap();
        let (sockets, data, _) = relay(system.as_raw_fd());
        assert_eq!((sockets.len(), read_all(&data.unwrap()).len()), (FDS_MAX, DATA_MAX));
        request(service.as_raw_fd(), "p", &[], b"").unwrap();
        let (sockets, data, payload) = relay(system.as_raw_fd());
        assert!(sockets.is_empty() && data.is_none() && payload == "p");
    }
}
