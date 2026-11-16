//! Landlock: filesystem and network-port access rules applied to the app after pivot_root,
//! as a second wall behind the mount namespace. Best effort by ABI version: the kernel
//! (7.3) supports all of it; an older kernel gets what it understands.
//!
//! Network: Landlock knows ports, not addresses. Without "network" or "lan" every TCP bind
//! and connect is refused (the app is alone in an empty namespace anyway).
//! With "lan" the app may bind only the fixed ports its manifest lists and port 0 (the
//! kernel picks one); connecting and sending stay open, because the ports of the devices
//! at home (a DLNA server, a sender's timing port) are not known in advance.

use crate::util::cstr;

const SYS_CREATE: libc::c_long = 444;
const SYS_ADD_RULE: libc::c_long = 445;
const SYS_RESTRICT: libc::c_long = 446;
const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;

const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_CHAR: u64 = 1 << 6;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_BLOCK: u64 = 1 << 11;
const MAKE_SYM: u64 = 1 << 12;
const REFER: u64 = 1 << 13; // ABI 2
const TRUNCATE: u64 = 1 << 14; // ABI 3
const IOCTL_DEV: u64 = 1 << 15; // ABI 5
const NET_BIND_TCP: u64 = 1 << 0; // ABI 4
const NET_CONNECT_TCP: u64 = 1 << 1;
const NET_BIND_UDP: u64 = 1 << 2; // ABI 10
const RULE_NET_PORT: u32 = 2;

/// What the app may do with TCP and UDP ports.
#[derive(Clone, Copy)]
pub enum Net<'a> {
    /// "network": nothing is restricted.
    Open,
    /// "lan": bind only these fixed ports, or port 0.
    Ports { tcp: &'a [u16], udp: &'a [u16] },
    /// Neither: nothing at all.
    Closed,
}

impl Net<'_> {
    /// The access rights the ruleset handles (denies unless a rule allows), and the rules:
    /// (port, allowed access).
    fn policy(self, abi: i64) -> (u64, Vec<(u64, u64)>) {
        let udp = abi >= 10;
        match self {
            _ if abi < 4 => (0, Vec::new()),
            Net::Open => (0, Vec::new()),
            Net::Closed => (NET_BIND_TCP | NET_CONNECT_TCP, Vec::new()),
            Net::Ports { tcp, udp: udp_ports } => {
                let mut rules: Vec<(u64, u64)> = Vec::new();
                let lists = [(tcp, NET_BIND_TCP), (if udp { udp_ports } else { &[] }, if udp { NET_BIND_UDP } else { 0 })];
                for (ports, access) in lists {
                    if access == 0 {
                        continue;
                    }
                    for port in ports.iter().map(|p| *p as u64).chain([0]) {
                        match rules.iter_mut().find(|r| r.0 == port) {
                            Some(rule) => rule.1 |= access,
                            None => rules.push((port, access)),
                        }
                    }
                }
                (NET_BIND_TCP | if udp { NET_BIND_UDP } else { 0 }, rules)
            }
        }
    }
}

#[repr(C, packed)]
struct NetPort {
    allowed: u64,
    port: u64,
}
const SCOPE_ABSTRACT_UNIX: u64 = 1 << 0; // ABI 6
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    fs: u64,
    net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneath {
    allowed: u64,
    parent_fd: i32,
}

pub fn abi() -> i64 {
    unsafe { libc::syscall(SYS_CREATE, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) as i64 }
}

/// Restrict the calling process (and its future children). Returns the ABI used.
pub fn restrict(net: Net, sys: bool, storage: bool) -> Result<i64, String> {
    let abi = abi();
    if abi < 1 {
        return Err("landlock not available".into());
    }
    let mut fs = EXECUTE | WRITE_FILE | READ_FILE | READ_DIR | REMOVE_DIR | REMOVE_FILE | MAKE_CHAR | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_BLOCK | MAKE_SYM;
    if abi >= 2 {
        fs |= REFER;
    }
    if abi >= 3 {
        fs |= TRUNCATE;
    }
    if abi >= 5 {
        fs |= IOCTL_DEV;
    }
    let (net, ports) = net.policy(abi);
    let scoped = if abi >= 6 { SCOPE_ABSTRACT_UNIX | SCOPE_SIGNAL } else { 0 };
    let attr = RulesetAttr { fs, net, scoped };
    let size = match abi {
        1..=3 => 8,
        4 | 5 => 16,
        _ => 24,
    };
    let rs = unsafe { libc::syscall(SYS_CREATE, &attr as *const RulesetAttr, size as usize, 0u32) } as i32;
    if rs < 0 {
        return Err(format!("landlock_create_ruleset: {}", std::io::Error::last_os_error()));
    }
    let ro = READ_FILE | READ_DIR;
    let rw = ro | WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_SYM | (fs & (REFER | TRUNCATE));
    let mut rules: Vec<(&str, u64)> = vec![
        ("/", READ_DIR),
        ("/app", ro | EXECUTE),
        ("/lib", ro | EXECUTE),
        ("/usr", ro | EXECUTE),
        ("/etc", ro),
        ("/proc", ro | WRITE_FILE),
        // TRUNCATE: O_TRUNC on /dev/null ("> /dev/null") needs it.
        ("/dev", ro | WRITE_FILE | (fs & (IOCTL_DEV | TRUNCATE))),
        ("/tmp", rw),
    ];
    if storage {
        rules.push(("/data", rw));
    }
    if sys {
        rules.push(("/sys", ro));
    }
    for (path, allowed) in rules {
        let fd = unsafe { libc::open(cstr(path).as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            continue; // not mounted in this sandbox: stays inaccessible
        }
        let rule = PathBeneath { allowed: allowed & fs, parent_fd: fd };
        let r = unsafe { libc::syscall(SYS_ADD_RULE, rs, RULE_PATH_BENEATH, &rule as *const PathBeneath, 0u32) };
        unsafe { libc::close(fd) };
        if r < 0 {
            unsafe { libc::close(rs) };
            return Err(format!("landlock_add_rule {path}: {}", std::io::Error::last_os_error()));
        }
    }
    for (port, allowed) in ports {
        let rule = NetPort { allowed, port };
        if unsafe { libc::syscall(SYS_ADD_RULE, rs, RULE_NET_PORT, &rule as *const NetPort, 0u32) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(rs) };
            return Err(format!("landlock_add_rule port {port}: {e}"));
        }
    }
    let r = unsafe { libc::syscall(SYS_RESTRICT, rs, 0u32) };
    unsafe { libc::close(rs) };
    if r < 0 {
        return Err(format!("landlock_restrict_self: {}", std::io::Error::last_os_error()));
    }
    Ok(abi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_policies() {
        let lan = Net::Ports { tcp: &[7000, 49200], udp: &[5353, 7000] };
        // The dongle's kernel: TCP and UDP binds are handled, connects and sends are not.
        let (handled, rules) = lan.policy(11);
        assert_eq!(handled, NET_BIND_TCP | NET_BIND_UDP);
        assert_eq!(rules, [(7000, NET_BIND_TCP | NET_BIND_UDP), (49200, NET_BIND_TCP), (0, NET_BIND_TCP | NET_BIND_UDP), (5353, NET_BIND_UDP)]);
        // A kernel that only knows TCP ports; one that knows none.
        assert_eq!(lan.policy(6), (NET_BIND_TCP, vec![(7000, NET_BIND_TCP), (49200, NET_BIND_TCP), (0, NET_BIND_TCP)]));
        assert_eq!(lan.policy(3), (0, Vec::new()));
        // No ports listed: only ports the kernel picks.
        assert_eq!(Net::Ports { tcp: &[], udp: &[] }.policy(10).1, [(0, NET_BIND_TCP | NET_BIND_UDP)]);
        assert_eq!(Net::Closed.policy(11), (NET_BIND_TCP | NET_CONNECT_TCP, Vec::new()));
        assert_eq!(Net::Open.policy(11), (0, Vec::new()));
    }

    /// On a kernel with Landlock network rules (the build host's, usually): a child with the
    /// "lan" policy binds a listed port and a kernel-picked one, and no other.
    #[test]
    fn the_kernel_enforces_listed_ports() {
        if abi() < 4 {
            eprintln!("no Landlock network support on this kernel: not checked");
            return;
        }
        // Two free ports: one goes on the list, the other does not.
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (listed, other) = (free(), free());
        let child = unsafe { libc::fork() };
        if child == 0 {
            let bind = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).map(|l| l.local_addr().unwrap().port());
            let code = unsafe {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                match restrict(Net::Ports { tcp: &[listed], udp: &[] }, true, true) {
                    Err(_) => 10,
                    Ok(_) if bind(listed).is_err() => 11,
                    Ok(_) if bind(0).is_err() => 12,
                    Ok(_) if bind(other).is_ok() => 13,
                    // Connecting is not confined (to a port nobody listens on: refused by
                    // the peer, not by Landlock).
                    Ok(_) if std::net::TcpStream::connect(("127.0.0.1", other)).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied) => 14,
                    Ok(_) => 0,
                }
            };
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        assert_eq!((libc::WIFEXITED(status), libc::WEXITSTATUS(status)), (true, 0));
    }
}
