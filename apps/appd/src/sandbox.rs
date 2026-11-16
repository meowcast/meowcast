//! The app sandbox. appd never forks into app setup from its own multi-threaded process:
//! it spawns itself as `appd sandbox-exec` (spec in APPD_SPEC, broker socket on fd 3,
//! stdout/stderr to a log pipe). That helper is single-threaded and does, in order:
//!
//! 1. join the app's cgroup (memory/pids/cpu limits; appd kills the app with cgroup.kill);
//! 2. unshare mount, UTS, PID and (without "network") network namespaces; bring up lo;
//! 3. build the app's root on a small tmpfs: /app (payload, read-only), /lib and /usr/lib
//!    (system libraries, read-only), fonts, /etc (generated), /dev (fresh device nodes:
//!    null, zero, full, random, urandom; plus the GPU render node with "display"), /tmp
//!    (tmpfs), /data (the storage quota tmpfs, with "storage"), /sys (read-only, with
//!    "display": libdrm/Mesa enumerate the GPU there);
//! 4. fork the namespace's init (pid 1), which mounts /proc, pivot_roots into the new root
//!    and makes it read-only, drops every capability and the bounding set, switches to the
//!    app's own uid/gid (no supplementary groups), sets no_new_privs, applies Landlock and
//!    seccomp, then forks and execs the app (pid 2) and reaps until it exits.
//!
//! /tmp and /data are noexec: the only code an app can run is its signed payload.

use crate::util::{cstr, cstring, cvt_ctx};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Spec {
    pub id: String,
    pub uid: u32,
    /// Where the app's root is assembled (appd's view): /run/appd/sandbox/<id>.
    pub root: String,
    pub payload: String,
    pub exec: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cgroup: String,
    pub network: bool,
    pub display: bool,
    /// The hardware video decoder and the sound card's playback stream ("media").
    #[serde(default)]
    pub media: bool,
    /// Host path of the storage tmpfs, with "storage".
    pub storage: Option<String>,
    pub tmp_mb: u32,
    /// The dial socket to the network service is inherited on NET_FD ("internet").
    #[serde(default)]
    pub net_fd: bool,
    /// This many fds from the app's background service are inherited on LAUNCH_FD and up
    /// (daemon handover.rs).
    #[serde(default)]
    pub launch_fds: u32,
    /// "lan" without "network": the fixed TCP and UDP ports the app may bind (Landlock).
    #[serde(default)]
    pub lan_ports: Option<(Vec<u16>, Vec<u16>)>,
}

pub const BROKER_FD: i32 = 3;
/// The app's dial socket to the network service (MEOW_NET_FD), when it has "internet".
pub const NET_FD: i32 = 4;
/// The first of the fds an app inherits from its background service (MEOW_LAUNCH_FDS,
/// MEOW_LAUNCH_DATA).
pub const LAUNCH_FD: i32 = 5;

/// Close the helper's copies of the fds that are for the app only. A connection that was
/// handed over must close when the app closes it, not when the sandbox ends.
unsafe fn close_app_fds(s: &Spec) {
    unsafe {
        libc::close(BROKER_FD);
        if s.net_fd {
            libc::close(NET_FD);
        }
        for i in 0..s.launch_fds as i32 {
            libc::close(LAUNCH_FD + i);
        }
    }
}

fn mount(src: &str, target: &Path, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<(), String> {
    let (s, t, f, d) = (cstring(src), cstr(target), cstring(fstype), cstring(data));
    let r = unsafe {
        libc::mount(
            if src.is_empty() { std::ptr::null() } else { s.as_ptr() },
            t.as_ptr(),
            if fstype.is_empty() { std::ptr::null() } else { f.as_ptr() },
            flags,
            if data.is_empty() { std::ptr::null() } else { d.as_ptr() as *const _ },
        )
    };
    cvt_ctx(r, &format!("mount {src} -> {}", target.display())).map(|_| ())
}

/// Read-only bind mount (non-recursive: submounts such as debugfs under /sys stay hidden).
fn bind_ro(src: &str, target: &Path, extra: libc::c_ulong) -> Result<(), String> {
    mount(src, target, "", libc::MS_BIND, "")?;
    mount("", target, "", libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | extra, "")
}

fn mkdirs(root: &Path, dirs: &[&str]) -> Result<(), String> {
    for d in dirs {
        std::fs::create_dir_all(root.join(d)).map_err(|e| format!("mkdir {d}: {e}"))?;
    }
    Ok(())
}

fn mknod(path: &Path, major: u32, minor: u32, mode: u32, uid: u32) -> Result<(), String> {
    let dev = libc::makedev(major, minor);
    cvt_ctx(unsafe { libc::mknod(cstr(path).as_ptr(), libc::S_IFCHR | mode, dev) }, &format!("mknod {}", path.display()))?;
    unsafe {
        libc::chmod(cstr(path).as_ptr(), mode);
        libc::chown(cstr(path).as_ptr(), uid, uid);
    }
    Ok(())
}

fn write(path: impl AsRef<Path>, s: &str) -> Result<(), String> {
    std::fs::write(path.as_ref(), s).map_err(|e| format!("{}: {e}", path.as_ref().display()))
}

fn loopback_up() -> Result<(), String> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    cvt_ctx(s, "socket")?;
    let mut ifr = [0u8; 40];
    ifr[..2].copy_from_slice(b"lo");
    let r = unsafe { libc::ioctl(s, libc::SIOCGIFFLAGS as _, ifr.as_mut_ptr()) };
    if r == 0 {
        let flags = i16::from_ne_bytes([ifr[16], ifr[17]]) | libc::IFF_UP as i16;
        ifr[16..18].copy_from_slice(&flags.to_ne_bytes());
        unsafe { libc::ioctl(s, libc::SIOCSIFFLAGS as _, ifr.as_mut_ptr()) };
    }
    unsafe { libc::close(s) };
    Ok(())
}

fn die(msg: &str) -> ! {
    eprintln!("SANDBOX_ERROR {msg}");
    unsafe { libc::_exit(70) }
}

/// Entry point of `appd sandbox-exec`.
pub fn exec_main() -> ! {
    let spec: Spec = match std::env::var("APPD_SPEC").ok().and_then(|s| serde_json::from_str(&s).ok()) {
        Some(s) => s,
        None => die("no spec"),
    };
    match setup(&spec) {
        Ok(never) => match never {},
        Err(e) => die(&e),
    }
}

fn setup(s: &Spec) -> Result<std::convert::Infallible, String> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("sandbox-exec must run as root (from appd)".into());
    }
    // 1. cgroup (before any fork, so every descendant is accounted and killable).
    write(format!("{}/cgroup.procs", s.cgroup), "0")?;
    // 2. namespaces.
    let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWPID;
    if !s.network {
        flags |= libc::CLONE_NEWNET;
    }
    cvt_ctx(unsafe { libc::unshare(flags) }, "unshare")?;
    if !s.network {
        loopback_up()?;
    }
    let host = cstring(&s.id);
    unsafe { libc::sethostname(host.as_ptr(), s.id.len()) };
    // 3. root filesystem.
    mount("", Path::new("/"), "", libc::MS_REC | libc::MS_PRIVATE, "")?;
    let root = Path::new(&s.root);
    std::fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
    mount("tmpfs", root, "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, "size=1m,mode=0755")?;
    mkdirs(root, &["app", "lib", "usr/lib", "usr/share/fonts", "dev", "proc", "tmp", "data", "etc", "sys", ".oldroot"])?;
    bind_ro(&s.payload, &root.join("app"), 0)?;
    bind_ro("/lib", &root.join("lib"), 0)?;
    bind_ro("/usr/lib", &root.join("usr/lib"), 0)?;
    if Path::new("/usr/share/fonts").is_dir() {
        bind_ro("/usr/share/fonts", &root.join("usr/share/fonts"), libc::MS_NOEXEC)?;
    }
    // /dev: own tmpfs with fresh nodes (not nodev, so the nodes work; the app cannot mknod).
    let dev = root.join("dev");
    mount("tmpfs", &dev, "tmpfs", libc::MS_NOSUID | libc::MS_NOEXEC, "size=64k,mode=0755")?;
    for (name, minor) in [("null", 3), ("zero", 5), ("full", 7), ("random", 8), ("urandom", 9)] {
        mknod(&dev.join(name), 1, minor, 0o666, 0)?;
    }
    for (link, to) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        let _ = std::os::unix::fs::symlink(to, dev.join(link));
    }
    if s.display {
        std::fs::create_dir(dev.join("dri")).map_err(|e| e.to_string())?;
        // The GPU render node, owned by the app's uid. The KMS (display) fd itself is never
        // a node in the sandbox: appd passes it over the broker while the app is in front.
        mknod(&dev.join("dri/renderD128"), 226, 128, 0o600, s.uid)?;
        // libdrm stats the KMS node of the brokered fd; root-only, so it cannot be opened.
        mknod(&dev.join("dri/card0"), 226, 0, 0o600, 0)?;
        bind_ro("/sys", &root.join("sys"), libc::MS_NOEXEC)?;
    }
    if s.media {
        // The decoder is a memory-to-memory device (no camera exists on this board); the
        // sound nodes are playback streams only (no capture, no mixer).
        let host = |pattern: &str| -> Vec<String> {
            let (directory, prefix) = pattern.rsplit_once('/').unwrap_or(("/dev", pattern));
            let mut names: Vec<String> = std::fs::read_dir(directory)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(prefix))
                .map(|n| format!("{directory}/{n}"))
                .collect();
            names.sort();
            names
        };
        let nodes = host("/dev/video").into_iter().chain(host("/dev/media")).chain(host("/dev/snd/pcmC").into_iter().filter(|n| n.ends_with('p')));
        for path in nodes {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if !meta.file_type().is_char_device() {
                continue;
            }
            let target = dev.join(path.trim_start_matches("/dev/"));
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let rdev = meta.rdev();
            mknod(&target, libc::major(rdev) as u32, libc::minor(rdev) as u32, 0o600, s.uid)?;
        }
    }
    mount("tmpfs", &root.join("tmp"), "tmpfs", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, &format!("size={}m,mode=0700,uid={},gid={}", s.tmp_mb, s.uid, s.uid))?;
    if let Some(st) = &s.storage {
        mount(st, &root.join("data"), "", libc::MS_BIND, "")?;
        mount("", &root.join("data"), "", libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "")?;
    }
    let etc = root.join("etc");
    write(etc.join("passwd"), &format!("root:x:0:0::/:/bin/false\napp:x:{u}:{u}::/data:/bin/false\n", u = s.uid))?;
    write(etc.join("group"), &format!("root:x:0:\napp:x:{u}:\n", u = s.uid))?;
    write(etc.join("hostname"), &format!("{}\n", s.id))?;
    write(etc.join("hosts"), &format!("127.0.0.1 localhost\n127.0.1.1 {}\n", s.id))?;
    write(etc.join("nsswitch.conf"), "hosts: files dns\n")?;
    if s.network {
        if let Ok(r) = std::fs::read_to_string("/etc/resolv.conf") {
            write(etc.join("resolv.conf"), &r)?;
        }
    }
    // 4. pid 1 of the new PID namespace.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        if let Err(e) = init(s) {
            die(&e);
        }
        unreachable!();
    }
    // The helper only waits: its exit status is the app's.
    unsafe { close_app_fds(s) };
    let mut st = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        if r == pid {
            break;
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            die("waitpid");
        }
    }
    let code = if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) };
    unsafe { libc::_exit(code) }
}

static mut APP_PID: libc::pid_t = 0;
extern "C" fn forward(sig: libc::c_int) {
    unsafe {
        if APP_PID > 0 {
            libc::kill(APP_PID, sig);
        }
    }
}

fn init(s: &Spec) -> Result<(), String> {
    let root = Path::new(&s.root);
    mount("proc", &root.join("proc"), "proc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, "")?;
    cvt_ctx(unsafe { libc::chdir(cstr(root).as_ptr()) }, "chdir root")?;
    let (dot, old) = (cstring("."), cstring(".oldroot"));
    cvt_ctx(unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), old.as_ptr()) } as i32, "pivot_root")?;
    cvt_ctx(unsafe { libc::chdir(cstring("/").as_ptr()) }, "chdir /")?;
    cvt_ctx(unsafe { libc::umount2(cstring("/.oldroot").as_ptr(), libc::MNT_DETACH) }, "umount old root")?;
    let _ = std::fs::remove_dir("/.oldroot");
    mount("", Path::new("/"), "", libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV, "")?;

    // Resource limits on top of the cgroup.
    let lim = |res: u32, v: u64| {
        let r = libc::rlimit { rlim_cur: v as libc::rlim_t, rlim_max: v as libc::rlim_t };
        unsafe { libc::setrlimit(res as _, &r) };
    };
    lim(libc::RLIMIT_NOFILE as u32, 512);
    lim(libc::RLIMIT_CORE as u32, 0);
    lim(libc::RLIMIT_MEMLOCK as u32, 64 * 1024);
    lim(libc::RLIMIT_RTPRIO as u32, 0);
    lim(libc::RLIMIT_NICE as u32, 0);

    // Privileges: empty bounding set, own uid/gid, no groups, no_new_privs.
    for cap in 0..64 {
        unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
    }
    unsafe { libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) };
    cvt_ctx(unsafe { libc::setgroups(0, std::ptr::null()) }, "setgroups")?;
    cvt_ctx(unsafe { libc::setresgid(s.uid, s.uid, s.uid) }, "setresgid")?;
    cvt_ctx(unsafe { libc::setresuid(s.uid, s.uid, s.uid) }, "setresuid")?;
    if unsafe { libc::setuid(0) } == 0 || unsafe { libc::geteuid() } != s.uid {
        return Err("privilege drop failed".into());
    }
    cvt_ctx(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }, "no_new_privs")?;
    let net = match (&s.lan_ports, s.network) {
        (Some((tcp, udp)), _) => crate::landlock::Net::Ports { tcp, udp },
        (None, true) => crate::landlock::Net::Open,
        (None, false) => crate::landlock::Net::Closed,
    };
    let landlock = match crate::landlock::restrict(net, s.display, s.storage.is_some()) {
        Ok(abi) => format!("abi{abi}"),
        Err(e) => {
            eprintln!("SANDBOX_WARN {e}");
            "off".into()
        }
    };
    let n = crate::seccomp::install()?;

    let mut env: Vec<(String, String)> = s.env.clone();
    env.push(("MEOW_SANDBOX".into(), format!("uid={} landlock={landlock} seccomp_denied={n} netns={}{}", s.uid, if s.network { "host" } else { "private" }, if s.lan_ports.is_some() { " bind=listed-ports" } else { "" })));
    let exec = format!("/app/{}", s.exec);
    let argv: Vec<CString> = std::iter::once(exec.clone()).chain(s.args.iter().cloned()).map(|a| cstring(&a)).collect();
    let envp: Vec<CString> = env.iter().map(|(k, v)| cstring(&format!("{k}={v}"))).collect();
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp_p: Vec<*const libc::c_char> = envp.iter().map(|a| a.as_ptr()).collect();
    envp_p.push(std::ptr::null());
    let exec_c = cstring(&exec);
    let home = cstring(if s.storage.is_some() { "/data" } else { "/tmp" });

    let app = unsafe { libc::fork() };
    if app < 0 {
        return Err(format!("fork app: {}", std::io::Error::last_os_error()));
    }
    if app == 0 {
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            libc::chdir(home.as_ptr());
            // Only stdio, the broker socket, (with "internet") the dial socket and what the
            // app's background service handed over survive into the app.
            let keep = if s.launch_fds > 0 {
                if !s.net_fd {
                    libc::close(NET_FD);
                }
                LAUNCH_FD + s.launch_fds as i32 - 1
            } else if s.net_fd {
                NET_FD
            } else {
                BROKER_FD
            };
            libc::syscall(libc::SYS_close_range, (keep + 1) as u32, u32::MAX, 0u32);
            libc::execve(exec_c.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
        }
        die(&format!("exec {exec}: {}", std::io::Error::last_os_error()));
    }
    unsafe {
        APP_PID = app;
        close_app_fds(s);
        libc::signal(libc::SIGTERM, forward as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, forward as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, forward as *const () as libc::sighandler_t);
    }
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(-1, &mut st, 0) };
        if r == app {
            let code = if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) };
            unsafe { libc::_exit(code) }; // pid 1 exits: the kernel kills the rest of the namespace
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            unsafe { libc::_exit(1) };
        }
    }
}
