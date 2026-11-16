//! meow-netd: the dongle's network daemon (root, started by rootfs/netd-session). Linked into
//! the appd binary (multi-call, like appctl) so both share one copy of std and serde_json.
//!
//! - Wi-Fi (wifi.rs): station on the RTL8733BU in the cable, setup access point + captive DNS
//!   while the dongle has no working network.
//! - Pilot server (http.rs): the phone remote web app, Wi-Fi setup and pairing API, keys into
//!   the UI's FIFO. What a pilot does beyond keys (volume, apps, text) goes through appd's
//!   control socket (pilot.rs, appd.rs).
//! - The clock (timesync.rs): set from time servers the moment a default route exists.
//! - Debug bridge: Dropbear SSH while Settings > Developer > Network debugging is on (keys
//!   enrolled with the TV's code using SPAKE2, debug.rs).
//! - The TV UI reads /run/netd/state and writes commands into the /run/netd/ctl FIFO:
//!   setup | forget | scan | unpair | debug on|off|pair|lan on|off|cloud on|off|revoke ID
//!   pair-code | pair-open | pair-close
//! - Miracast's Wi-Fi Direct side (p2p.rs) for the Cast app. appd writes
//!   miracast on pbc|pin NAME | miracast disconnect | miracast off
//!   into the same FIFO and reads /run/netd/miracast.

mod appd;
mod cloud;
mod debug;
mod dns;
mod http;
pub mod p2p;
mod pair;
mod pilot;
mod state;
mod timesync;
mod util;
mod wifi;
mod wpa;

use state::Shared;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;
use util::log;

const CTL: &str = "/run/netd/ctl";

/// Entry point (appd is a multi-call binary: /usr/bin/meow-netd -> appd).
pub fn main(args: &[String]) {
    if args.get(1).map(|s| s.as_str()) == Some("qr") {
        // Development: print a QR matrix for a text ("meow-netd qr TEXT").
        println!("{}", state::qr_matrix(args.get(2).map(|s| s.as_str()).unwrap_or("")));
        return;
    }
    util::mkdir_private("/run/netd");
    let _ = std::fs::create_dir_all(wpa::CTRL_DIR);
    util::mkdir_private("/data/net");
    let shared = state::new();
    {
        let mut n = shared.0.lock().unwrap();
        n.pilots = pair::count();
        n.ui = http::ui_up();
    }
    debug::init(&shared);
    state::changed(&shared);

    let (wtx, wrx) = mpsc::channel();
    {
        let s = shared.clone();
        std::thread::spawn(move || wifi::run(s, wrx));
    }
    let web = std::env::var("NETD_WEB").unwrap_or_else(|_| "/usr/share/pilot".into());
    let port: u16 = std::env::var("NETD_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(80);
    let ctx = http::Ctx::new(shared.clone(), wtx.clone(), web);
    std::thread::spawn(move || http::serve(ctx, port));
    {
        let s = shared.clone();
        let tx = wtx.clone();
        std::thread::spawn(move || control(s, tx));
    }
    std::thread::spawn(timesync::run);
    {
        let s = shared.clone();
        std::thread::spawn(move || cloud::run(s));
    }
    {
        let s = shared.clone();
        std::thread::spawn(move || pilot::run(s));
    }
    log(format!("netd: started (pid {}), {} paired pilot(s), debug bridge {}", std::process::id(), pair::count(), if shared.0.lock().unwrap().debug { "on" } else { "off" }));
    supervise(shared);
}

/// Commands from the TV UI.
fn control(shared: Shared, wifi: Sender<wifi::Cmd>) {
    let path = util::cstring(CTL);
    let _ = std::fs::remove_file(CTL);
    unsafe { libc::mkfifo(path.as_ptr(), 0o620) };
    loop {
        // O_RDWR: the FIFO never reports end-of-file when a writer closes.
        let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(CTL) else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { break };
            let cmd = line.trim();
            // (Not the Miracast request either: appd repeats it every few seconds.)
            if !cmd.starts_with("pair-") && !cmd.starts_with("miracast ") {
                log(format!("netd: ctl {cmd}"));
            }
            match cmd {
                _ if cmd.starts_with("miracast ") => match p2p::parse_ctl(cmd) {
                    Some(ctl) => {
                        let _ = wifi.send(wifi::Cmd::Miracast(ctl));
                    }
                    None => log(format!("netd: bad ctl command {cmd:?}")),
                },
                "setup" => {
                    let _ = wifi.send(wifi::Cmd::Setup);
                }
                "forget" => {
                    let _ = wifi.send(wifi::Cmd::Forget);
                }
                "scan" => {
                    let _ = wifi.send(wifi::Cmd::Scan);
                }
                "unpair" => pair::unpair_all(&shared),
                "pair-code" => {
                    shared.0.lock().unwrap().pair_code = util::random_digits(6);
                    state::changed(&shared);
                }
                // The TV shows / stops showing the pairing code.
                "pair-open" | "pair-close" => {
                    shared.0.lock().unwrap().pair_open_until = if cmd == "pair-open" { util::uptime() + pair::WINDOW_S } else { 0.0 };
                    state::changed(&shared);
                }
                _ if cmd.starts_with("debug ") => debug::control(&shared, cmd),
                _ => log(format!("netd: unknown ctl command {cmd:?}")),
            }
        }
    }
}

/// Root's ~/.ssh for Dropbear: authorized_keys lives on /data.
fn debug_home() {
    use std::os::unix::fs::PermissionsExt;
    util::mkdir_private("/data/net/debug");
    let _ = std::fs::create_dir_all("/root");
    let _ = std::fs::set_permissions("/root", std::fs::Permissions::from_mode(0o700));
    if std::fs::read_link("/root/.ssh").is_err() {
        let _ = std::fs::remove_dir_all("/root/.ssh");
        let _ = std::os::unix::fs::symlink("/data/net/debug", "/root/.ssh");
    }
}

/// Main thread: the debug bridge process and the UI liveness flag.
fn supervise(shared: Shared) {
    let mut dropbear: Option<Child> = None;
    let mut next_start = 0.0;
    let mut generation = 0;
    let mut addresses = String::new();
    let mut minutes = 0;
    // A previous daemon may have crashed. Kill the whole inherited SSH subtree.
    let _ = std::fs::write(format!("{}/cgroup.kill", debug::CGROUP), b"1");
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = util::uptime();
        debug::expire(&shared);
        let (want, current_generation, lan) = {
            let n = shared.0.lock().unwrap();
            (n.debug, n.debug_generation, n.debug_lan)
        };
        let usb = util::ipv4_of("usb0").map(|a| a.to_string()).unwrap_or_default();
        let current_addresses = format!("{lan}:{usb}");
        if !want || generation != current_generation || addresses != current_addresses {
            // cgroup.kill includes forked SSH sessions, shells and forwarded connections.
            // The group is populated before exec, so there is no enrollment race.
            let _ = std::fs::write(format!("{}/cgroup.kill", debug::CGROUP), b"1");
            if let Some(mut c) = dropbear.take() { let _ = c.kill(); let _ = c.wait(); }
            generation = current_generation;
            addresses = current_addresses;
        }
        if let Some(c) = dropbear.as_mut() {
            if !matches!(c.try_wait(), Ok(None)) {
                log("netd: dropbear exited");
                dropbear = None;
                next_start = now + 3.0;
            }
        }
        if want && dropbear.is_none() && now >= next_start {
            debug_home();
            let ready = std::fs::create_dir_all(debug::CGROUP).and_then(|_| {
                std::fs::OpenOptions::new().write(true).open(format!("{}/cgroup.kill", debug::CGROUP))?;
                Ok(())
            });
            if ready.is_err() || debug::prepare(&shared).is_err() {
                log("netd: debug bridge requires cgroup v2 kill support and a host key; refusing to start");
                next_start = now + 30.0;
                continue;
            }
            let log_file = std::fs::File::create("/run/netd/dropbear.log").ok();
            let mut cmd = Command::new("dropbear");
            // Foreground (logs to stderr: built without syslog), no passwords, host key created
            // on first use, keys from /data, port 22.
            cmd.args(["-F", "-s", "-D", debug::DIR, "-p", if lan { "22" } else { "127.0.0.1:22" }]);
            if !lan && !usb.is_empty() { cmd.args(["-p", &format!("{usb}:22")]); }
            cmd.stdin(Stdio::null()).stdout(Stdio::null());
            use std::os::unix::process::CommandExt;
            let procs = util::cstring(&format!("{}/cgroup.procs", debug::CGROUP));
            // Only async-signal-safe syscalls between fork and exec.
            unsafe { cmd.pre_exec(move || {
                let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 { return Err(std::io::Error::last_os_error()); }
                let result = libc::write(fd, b"0".as_ptr().cast(), 1);
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                if result != 1 { return Err(error); }
                Ok(())
            }); }
            match log_file {
                Some(f) => cmd.stderr(f),
                None => cmd.stderr(Stdio::null()),
            };
            match cmd.spawn() {
                Ok(c) => {
                    log(format!("netd: debug bridge up (dropbear pid {}, port 22)", c.id()));
                    dropbear = Some(c);
                }
                Err(e) => {
                    log(format!("netd: dropbear: {e}"));
                    next_start = now + 30.0;
                }
            }
        } else if !want {
            if let Some(mut c) = dropbear.take() {
                let _ = c.kill();
                let _ = c.wait();
                log("netd: debug bridge down");
            }
        }
        let running = dropbear.is_some();
        let ui = http::ui_up();
        let changed = {
            let mut n = shared.0.lock().unwrap();
            let remaining = if n.debug { ((n.debug_until - now).max(0.0) / 60.0).ceil() as u32 } else { 0 };
            let c = n.debug_running != running || n.ui != ui || remaining != minutes;
            minutes = remaining;
            n.debug_running = running;
            n.ui = ui;
            c
        };
        if changed {
            state::changed(&shared);
        }
    }
}
