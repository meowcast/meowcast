//! meow-net: the system network service.
//!
//! Apps do not ship TLS, certificates or HTTP code, and (with the "internet" permission) have
//! no network of their own. They ask this service, which is the one place to fix networking
//! bugs and to update the trust policy: it is published to the store like an app, signed, and
//! fetched into RAM, so a new root-CA set or a fix reaches every dongle without a firmware
//! flash. appd runs it in the ordinary app sandbox (own uid, no capabilities, Landlock,
//! seccomp) with the device's network namespace and nothing else.
//!
//!   appd --CLIENT id hosts + dial fd-->  service        (over the service's broker, fd 3)
//!   app  --OPEN n-->  service  --OK n + channel fd-->  app          (dial, SEQPACKET)
//!   app  <== requests and responses ==>  service  <== TCP/TLS ==>  internet   (channel)
//!
//! Wire formats: apps/sdk/src/net.rs. Design: apps/README.md.
//!
//! `meow-net --dial FD [--hosts LIST] [--local]` serves one client on an inherited socket
//! (tests and the build host: `MEOW_NET_HELPER`, see the SDK).

mod channel;
mod policy;

use meow_app::ipc;
use policy::Policy;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Channels one client may hold open (each is a thread and up to a few connections).
const CLIENT_CHANNELS: usize = 16;
/// All channels together; the service's cgroup allows 64 tasks.
const ALL_CHANNELS: usize = 44;
const STACK: usize = 512 << 10;

static CHANNELS: AtomicUsize = AtomicUsize::new(0);

/// Serve one client's dial socket until the client closes it.
fn serve_client(id: String, dial: OwnedFd, policy: Policy, tls: Arc<rustls::ClientConfig>) {
    let policy = Arc::new(policy);
    let mine = Arc::new(AtomicUsize::new(0));
    loop {
        let message = match ipc::recv(dial.as_raw_fd(), true) {
            Ok(Some((message, _))) => message,
            _ => break,
        };
        let mut fields = message.split('\t');
        let (verb, number) = (fields.next().unwrap_or(""), fields.next().unwrap_or("0").to_string());
        if verb != "OPEN" {
            let _ = ipc::send(dial.as_raw_fd(), &format!("ERR\t{number}\tunknown request"), None);
            continue;
        }
        if mine.load(Ordering::Relaxed) >= CLIENT_CHANNELS || CHANNELS.load(Ordering::Relaxed) >= ALL_CHANNELS {
            let _ = ipc::send(dial.as_raw_fd(), &format!("ERR\t{number}\ttoo many open channels"), None);
            continue;
        }
        let Ok((app, service)) = UnixStream::pair() else {
            let _ = ipc::send(dial.as_raw_fd(), &format!("ERR\t{number}\tout of sockets"), None);
            continue;
        };
        let (policy, tls, counted) = (policy.clone(), tls.clone(), mine.clone());
        mine.fetch_add(1, Ordering::Relaxed);
        CHANNELS.fetch_add(1, Ordering::Relaxed);
        let started = std::thread::Builder::new().name(format!("ch-{id}")).stack_size(STACK).spawn(move || {
            channel::Channel::new(service, policy, tls).run();
            counted.fetch_sub(1, Ordering::Relaxed);
            CHANNELS.fetch_sub(1, Ordering::Relaxed);
        });
        if started.is_err() {
            mine.fetch_sub(1, Ordering::Relaxed);
            CHANNELS.fetch_sub(1, Ordering::Relaxed);
            let _ = ipc::send(dial.as_raw_fd(), &format!("ERR\t{number}\tout of threads"), None);
            continue;
        }
        if ipc::send(dial.as_raw_fd(), &format!("OK\t{number}"), Some(app.as_raw_fd())).is_err() {
            break;
        }
    }
    eprintln!("net: client {id} gone");
}

fn run() -> Result<(), String> {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let tls = channel::tls_config()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if let Some(fd) = option("--dial") {
        let fd: RawFd = fd.parse().map_err(|_| "--dial FD")?;
        let policy = Policy::new(&option("--hosts").unwrap_or_else(|| "*".into()), args.iter().any(|a| a == "--local"));
        serve_client("dial".into(), unsafe { OwnedFd::from_raw_fd(fd) }, policy, tls);
        return Ok(());
    }
    let broker: RawFd = std::env::var("MEOW_BROKER_FD").ok().and_then(|v| v.parse().ok()).ok_or("not started by appd (or use --dial FD)")?;
    eprintln!("net: ready, {} root certificates", webpki_roots::TLS_SERVER_ROOTS.len());
    loop {
        let (message, fd) = match ipc::recv(broker, true) {
            Ok(Some(message)) => message,
            _ => return Ok(()), // appd is gone
        };
        let fields: Vec<&str> = message.split('\t').collect();
        match (fields[0], fd) {
            // A client: an app with the "internet" permission and the hosts of its manifest.
            ("CLIENT", Some(dial)) => {
                let id = fields.get(1).unwrap_or(&"?").to_string();
                let hosts = fields.get(2).unwrap_or(&"").to_string();
                eprintln!("net: client {id} may reach {hosts}");
                let (tls, policy) = (tls.clone(), Policy::new(&hosts, false));
                let name = id.clone();
                if let Err(e) = std::thread::Builder::new().name(format!("dial-{name}")).stack_size(STACK).spawn(move || serve_client(id, dial, policy, tls)) {
                    eprintln!("net: client {name}: {e}");
                }
            }
            ("QUIT", _) => return Ok(()),
            _ => {}
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("meow-net: {error}");
        std::process::exit(1);
    }
}
