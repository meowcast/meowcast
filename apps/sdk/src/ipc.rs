//! Message sockets: AF_UNIX SOCK_SEQPACKET, short text messages, at most one fd per message
//! (SCM_RIGHTS). The broker, appd's control socket and the network service's dial use it.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Instant;

pub fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub fn connect_seqpacket(path: &str) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    if path.len() >= address.sun_path.len() {
        return Err(io::Error::other("socket path too long"));
    }
    for (i, b) in path.bytes().enumerate() {
        address.sun_path[i] = b as _;
    }
    let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    if unsafe { libc::connect(fd.as_raw_fd(), &address as *const _ as *const libc::sockaddr, size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Send one message, optionally with one fd. A vanished peer is an error, not SIGPIPE.
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
    if unsafe { libc::sendmsg(fd, &hdr, libc::MSG_NOSIGNAL) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One message (+ at most one fd; more are closed). Ok(None) = would block. A closed peer is
/// an UnexpectedEof error.
pub fn recv(fd: RawFd, block: bool) -> io::Result<Option<(String, Option<OwnedFd>)>> {
    let mut buf = vec![0u8; 8192];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cbuf.as_mut_ptr() as *mut _;
    hdr.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    let flags = libc::MSG_CMSG_CLOEXEC | if block { 0 } else { libc::MSG_DONTWAIT };
    let n = unsafe { libc::recvmsg(fd, &mut hdr, flags) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::WouldBlock { Ok(None) } else { Err(e) };
    }
    let mut got: Option<OwnedFd> = None;
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                for i in 0..count {
                    let passed = std::ptr::read_unaligned((libc::CMSG_DATA(c) as *const i32).add(i));
                    if got.is_none() {
                        got = Some(OwnedFd::from_raw_fd(passed));
                    } else {
                        libc::close(passed);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
    }
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the other side closed the socket"));
    }
    buf.truncate(n as usize);
    Ok(Some((String::from_utf8_lossy(&buf).into_owned(), got)))
}

/// Wait for one message until `deadline`. Ok(None) = nothing arrived in time.
pub fn recv_until(fd: RawFd, deadline: Instant) -> io::Result<Option<(String, Option<OwnedFd>)>> {
    loop {
        if let Some(message) = recv(fd, false)? {
            return Ok(Some(message));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut p, 1, left.as_millis().min(60_000) as i32 + 1) };
    }
}
