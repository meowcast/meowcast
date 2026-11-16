//! Seccomp filter for apps: a deny list of kernel interfaces no app needs (they either
//! change system state or are large attack surface), returning EPERM; any foreign syscall
//! ABI kills the process. A deny list (not an allow list) keeps Mesa, glibc and Rust std
//! working without per-release syscall audits; the other layers (separate uid, no
//! capabilities, namespaces, Landlock) do not depend on it.
//!
//! One syscall is filtered by its arguments: `socket`. Apps get Unix sockets, IPv4/IPv6
//! sockets, and the routing netlink socket (how libc lists interfaces and addresses).
//! Every other family is refused: packet sockets, the other netlink protocols (socket
//! diagnostics would list every connection on the device to an app that shares the network
//! namespace; kernel object events), kernel crypto, Bluetooth, CAN and the rest are attack
//! surface no app here needs. Raw IP and packet sockets already need a capability no app
//! has; this is a second lock on those.

#[cfg(target_arch = "arm")]
const AUDIT_ARCH: u32 = 0x4000_0028; // AUDIT_ARCH_ARM (EM_ARM | __AUDIT_ARCH_LE)
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const RET_K: u16 = 0x06;
/// seccomp_data: nr at 0, arch at 4, the low words of the arguments at 16 + 8 * n.
const ARG0: u32 = 16;
const ARG2: u32 = 32;
const NETLINK_ROUTE: u32 = 0;

#[cfg(target_arch = "arm")]
pub fn denied() -> Vec<(&'static str, libc::c_long)> {
    use libc::*;
    vec![
        ("ptrace", SYS_ptrace),
        ("process_vm_readv", SYS_process_vm_readv),
        ("process_vm_writev", SYS_process_vm_writev),
        ("kcmp", SYS_kcmp),
        ("pidfd_getfd", SYS_pidfd_getfd),
        ("mount", SYS_mount),
        ("umount2", SYS_umount2),
        ("pivot_root", SYS_pivot_root),
        ("chroot", SYS_chroot),
        ("move_mount", SYS_move_mount),
        ("open_tree", SYS_open_tree),
        ("fsopen", SYS_fsopen),
        ("fsconfig", SYS_fsconfig),
        ("fsmount", SYS_fsmount),
        ("fspick", SYS_fspick),
        ("mount_setattr", SYS_mount_setattr),
        ("swapon", SYS_swapon),
        ("swapoff", SYS_swapoff),
        ("reboot", SYS_reboot),
        ("kexec_load", SYS_kexec_load),
        ("init_module", SYS_init_module),
        ("finit_module", SYS_finit_module),
        ("delete_module", SYS_delete_module),
        ("bpf", SYS_bpf),
        ("perf_event_open", SYS_perf_event_open),
        ("keyctl", SYS_keyctl),
        ("add_key", SYS_add_key),
        ("request_key", SYS_request_key),
        ("unshare", SYS_unshare),
        ("setns", SYS_setns),
        ("acct", SYS_acct),
        ("settimeofday", SYS_settimeofday),
        ("clock_settime", SYS_clock_settime),
        ("clock_adjtime", SYS_clock_adjtime),
        ("adjtimex", SYS_adjtimex),
        ("open_by_handle_at", SYS_open_by_handle_at),
        ("name_to_handle_at", SYS_name_to_handle_at),
        ("userfaultfd", SYS_userfaultfd),
        ("io_uring_setup", SYS_io_uring_setup),
        ("io_uring_enter", SYS_io_uring_enter),
        ("io_uring_register", SYS_io_uring_register),
        ("syslog", SYS_syslog),
        ("quotactl", SYS_quotactl),
        ("sethostname", SYS_sethostname),
        ("setdomainname", SYS_setdomainname),
        ("mknod", SYS_mknod),
        ("mknodat", SYS_mknodat),
        ("uselib", SYS_uselib),
        ("vhangup", SYS_vhangup),
        ("fanotify_init", SYS_fanotify_init),
        // The multiplexed socket call still exists on ARM EABI; libc does not use it, and
        // it would get around the filter on `socket`.
        ("socketcall", 102),
        // 64-bit-time variants (ARM EABI, numbers from the kernel's syscall.tbl).
        ("clock_settime64", 404),
        ("clock_adjtime64", 405),
    ]
}

/// Host builds are for the publishing tools; a sandbox runs there only in tests
/// (test-store.sh). The list is the part of the dongle's that has the same names on every
/// architecture: enough for a real filter, not a policy anyone relies on.
#[cfg(not(target_arch = "arm"))]
pub fn denied() -> Vec<(&'static str, libc::c_long)> {
    use libc::*;
    vec![
        ("ptrace", SYS_ptrace),
        ("mount", SYS_mount),
        ("umount2", SYS_umount2),
        ("pivot_root", SYS_pivot_root),
        ("chroot", SYS_chroot),
        ("swapon", SYS_swapon),
        ("swapoff", SYS_swapoff),
        ("reboot", SYS_reboot),
        ("kexec_load", SYS_kexec_load),
        ("init_module", SYS_init_module),
        ("finit_module", SYS_finit_module),
        ("delete_module", SYS_delete_module),
        ("bpf", SYS_bpf),
        ("unshare", SYS_unshare),
        ("setns", SYS_setns),
    ]
}

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// The filter: foreign ABIs are killed, `deny` gets EPERM, `socket` (syscall number
/// `socket`) is allowed for the families in the module comment and gets EAFNOSUPPORT, as
/// from a kernel built without the family, otherwise.
fn program(deny: &[libc::c_long], socket: libc::c_long) -> Vec<SockFilter> {
    let ins = |code, jt, jf, k| SockFilter { code, jt, jf, k };
    let mut f = vec![
        ins(LD_W_ABS, 0, 0, 4), // seccomp_data.arch
        ins(JEQ_K, 1, 0, AUDIT_ARCH),
        ins(RET_K, 0, 0, RET_KILL_PROCESS),
        ins(LD_W_ABS, 0, 0, 0), // seccomp_data.nr
    ];
    for nr in deny {
        f.push(ins(JEQ_K, 0, 1, *nr as u32));
        f.push(ins(RET_K, 0, 0, RET_ERRNO | libc::EPERM as u32));
    }
    f.extend([
        ins(JEQ_K, 0, 8, socket as u32), // not socket: to the final allow
        ins(LD_W_ABS, 0, 0, ARG0),       // the family
        ins(JEQ_K, 6, 0, libc::AF_UNIX as u32),
        ins(JEQ_K, 5, 0, libc::AF_INET as u32),
        ins(JEQ_K, 4, 0, libc::AF_INET6 as u32),
        ins(JEQ_K, 0, 2, libc::AF_NETLINK as u32),
        ins(LD_W_ABS, 0, 0, ARG2), // netlink: the protocol
        ins(JEQ_K, 1, 0, NETLINK_ROUTE),
        ins(RET_K, 0, 0, RET_ERRNO | libc::EAFNOSUPPORT as u32),
        ins(RET_K, 0, 0, RET_ALLOW),
    ]);
    f
}

/// Install the filter (requires no_new_privs, set by the caller).
pub fn install() -> Result<usize, String> {
    let deny = denied();
    if deny.is_empty() {
        return Err("no seccomp policy for this architecture".into());
    }
    let f = program(&deny.iter().map(|d| d.1).collect::<Vec<_>>(), libc::SYS_socket);
    let prog = SockFprog { len: f.len() as u16, filter: f.as_ptr() };
    let r = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER as libc::c_ulong, &prog as *const SockFprog as libc::c_ulong, 0, 0) };
    if r != 0 {
        return Err(format!("seccomp: {}", std::io::Error::last_os_error()));
    }
    Ok(deny.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOCKET: libc::c_long = 281;

    /// Run the filter as the kernel would (the three instructions it uses), on a syscall
    /// with these arguments.
    fn run(f: &[SockFilter], arch: u32, nr: u32, args: [u32; 3]) -> u32 {
        let load = |offset: u32| match offset {
            0 => nr,
            4 => arch,
            16 | 24 | 32 => args[(offset as usize - 16) / 8],
            _ => panic!("load of offset {offset}"),
        };
        let (mut pc, mut a) = (0usize, 0u32);
        loop {
            let i = &f[pc];
            pc += 1;
            match i.code {
                LD_W_ABS => a = load(i.k),
                JEQ_K => pc += if a == i.k { i.jt } else { i.jf } as usize,
                RET_K => return i.k,
                other => panic!("instruction {other:#x}"),
            }
        }
    }

    #[test]
    fn sockets_are_filtered_by_family() {
        let f = program(&[26, 21], SOCKET);
        let socket = |family: i32, kind: i32, protocol: u32| run(&f, AUDIT_ARCH, SOCKET as u32, [family as u32, kind as u32, protocol]);
        let refused = RET_ERRNO | libc::EAFNOSUPPORT as u32;
        assert_eq!(socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0), RET_ALLOW);
        assert_eq!(socket(libc::AF_INET, libc::SOCK_STREAM, 0), RET_ALLOW);
        assert_eq!(socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP as u32), RET_ALLOW);
        assert_eq!(socket(libc::AF_INET6, libc::SOCK_DGRAM, 0), RET_ALLOW);
        assert_eq!(socket(libc::AF_NETLINK, libc::SOCK_RAW, NETLINK_ROUTE), RET_ALLOW);
        for protocol in [libc::NETLINK_SOCK_DIAG, libc::NETLINK_KOBJECT_UEVENT, libc::NETLINK_GENERIC, libc::NETLINK_AUDIT, libc::NETLINK_NETFILTER] {
            assert_eq!(socket(libc::AF_NETLINK, libc::SOCK_RAW, protocol as u32), refused, "netlink protocol {protocol}");
        }
        for family in [libc::AF_PACKET, libc::AF_ALG, libc::AF_BLUETOOTH, libc::AF_CAN, libc::AF_KEY, libc::AF_VSOCK, libc::AF_XDP, 0, 45, 1000] {
            assert_eq!(socket(family, libc::SOCK_RAW, 0), refused, "family {family}");
        }
        // The deny list and everything else are as before.
        assert_eq!(run(&f, AUDIT_ARCH, 26, [0; 3]), RET_ERRNO | libc::EPERM as u32);
        assert_eq!(run(&f, AUDIT_ARCH, 21, [libc::AF_PACKET as u32, 0, 0]), RET_ERRNO | libc::EPERM as u32);
        assert_eq!(run(&f, AUDIT_ARCH, 3, [libc::AF_PACKET as u32, 0, 0]), RET_ALLOW, "another syscall with the same first argument");
        assert_eq!(run(&f, AUDIT_ARCH ^ 1, 3, [0; 3]), RET_KILL_PROCESS);
        // Every jump lands inside the program.
        for (at, i) in f.iter().enumerate().filter(|(_, i)| i.code == JEQ_K) {
            assert!(at + 1 + (i.jt.max(i.jf) as usize) < f.len());
        }
        assert_eq!(program(&[], SOCKET).len(), 14);
    }

    /// The same program in the build host's kernel, in a child process.
    #[test]
    fn the_kernel_takes_the_filter() {
        let child = unsafe { libc::fork() };
        if child == 0 {
            let code = unsafe {
                let f = program(&[libc::SYS_swapon], libc::SYS_socket);
                let prog = SockFprog { len: f.len() as u16, filter: f.as_ptr() };
                let open = |family: i32, kind: i32, protocol: i32| {
                    let fd = libc::socket(family, kind, protocol);
                    if fd < 0 { Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)) } else { Ok(()) }
                };
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER as libc::c_ulong, &prog as *const SockFprog as libc::c_ulong, 0, 0) != 0 {
                    20
                } else if open(libc::AF_UNIX, libc::SOCK_STREAM, 0).is_err() || open(libc::AF_INET, libc::SOCK_DGRAM, 0).is_err() {
                    21
                } else if open(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE).is_err() {
                    22
                } else if open(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_SOCK_DIAG) != Err(libc::EAFNOSUPPORT) || open(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_KOBJECT_UEVENT) != Err(libc::EAFNOSUPPORT) {
                    23
                } else if open(libc::AF_PACKET, libc::SOCK_RAW, 0) != Err(libc::EAFNOSUPPORT) || open(libc::AF_ALG, libc::SOCK_SEQPACKET, 0) != Err(libc::EAFNOSUPPORT) {
                    24
                } else if libc::swapon(c"/nonexistent".as_ptr(), 0) != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                    25
                } else {
                    0
                }
            };
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        unsafe { libc::waitpid(child, &mut status, 0) };
        assert_eq!((libc::WIFEXITED(status), libc::WEXITSTATUS(status)), (true, 0));
    }
}
