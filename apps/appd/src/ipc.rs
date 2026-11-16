//! Message transport: AF_UNIX SOCK_SEQPACKET (message boundaries kept, fds via SCM_RIGHTS).
//! Messages are short UTF-8 text, fields separated by TAB:
//!
//!   control socket /run/appd/ui.sock (root only; the UI and appctl)  — see daemon.rs
//!   broker socket (fd 3 in each app; one socketpair per app)         — see README.md

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub const MAX_MSG: usize = 8192;

pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub fn listen(path: &str) -> io::Result<OwnedFd> {
    let _ = std::fs::remove_file(path);
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = sockaddr(path)?;
    // Created 0600 (umask), owned by root: only root processes may connect.
    let old = unsafe { libc::umask(0o177) };
    let r = unsafe { libc::bind(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) };
    unsafe { libc::umask(old) };
    if r < 0 || unsafe { libc::listen(fd.as_raw_fd(), 8) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

pub fn connect(path: &str) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let (addr, len) = sockaddr(path)?;
    if unsafe { libc::connect(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn sockaddr(path: &str) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as _;
    if path.len() >= a.sun_path.len() {
        return Err(io::Error::other("socket path too long"));
    }
    for (i, b) in path.bytes().enumerate() {
        a.sun_path[i] = b as _;
    }
    Ok((a, std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t))
}

pub fn accept(listener: RawFd) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::accept4(listener, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// uid of the connected peer (SO_PEERCRED).
pub fn peer_uid(fd: RawFd) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut _, &mut len) };
    (r == 0).then_some(cred.uid)
}

/// Send one message, optionally with one fd. MSG_NOSIGNAL: a vanished peer is an error, not SIGPIPE.
pub fn send(fd: RawFd, msg: &str, pass: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: msg.as_ptr() as *mut _, iov_len: msg.len() };
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    let mut cbuf = [0u64; 4];
    if let Some(p) = pass {
        hdr.msg_control = cbuf.as_mut_ptr() as *mut _;
        hdr.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut i32, p);
        }
    }
    let n = unsafe { libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub enum Recv {
    Msg(String, Option<OwnedFd>),
    WouldBlock,
    Closed,
}

/// Receive one message (and at most one fd; extra fds are closed).
pub fn recv(fd: RawFd) -> io::Result<Recv> {
    let mut buf = vec![0u8; MAX_MSG];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cbuf.as_mut_ptr() as *mut _;
    hdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    let n = unsafe { libc::recvmsg(fd, &mut hdr, libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::WouldBlock { Ok(Recv::WouldBlock) } else { Err(e) };
    }
    let mut got: Option<OwnedFd> = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                for i in 0..count {
                    let f = std::ptr::read_unaligned((libc::CMSG_DATA(c) as *const i32).add(i));
                    if got.is_none() { got = Some(OwnedFd::from_raw_fd(f)) } else { libc::close(f); }
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
    }
    if n == 0 {
        return Ok(Recv::Closed);
    }
    buf.truncate(n as usize);
    Ok(Recv::Msg(String::from_utf8_lossy(&buf).into_owned(), got))
}
