//! appctl: command-line client of appd's control socket (root only).

use crate::ipc::{self, Recv};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: appctl list | ps | refresh | info ID | logs ID | install ID [all] | launch ID | stop ID
              | uninstall ID | grant ID PERM | revoke ID PERM | key NAME (up/down/left/right/enter/back) | asset NAME
              | volume [N | +N | -N | mute [on|off]]   the system volume, 0-100 (mute keeps the level)
              | background [ID on|off]   apps that may run in the background; the switch of one
              | recents                  the apps opened last
              | foreground ID [PAYLOAD]  open ID as its background service would ask to (tests)
              | repo [list] | repo add URL | repo remove ID | repo on|off ID
                                         the repositories apps come from (add trusts the key the
                                         URL's repo.json names, and prints its fingerprint)
  install ... all   also grants the sensitive permissions (as \"Allow\" in the TV dialog)";

pub fn main(args: &[String]) -> i32 {
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let cmd = match a.as_slice() {
        ["list"] => "LIST".to_string(),
        ["ps"] => "PS".into(),
        ["refresh"] => "REFRESH".into(),
        ["info", id] => format!("INFO\t{id}"),
        ["logs", id] => format!("LOGS\t{id}"),
        ["install", id] => format!("INSTALL\t{id}"),
        ["install", id, "all"] => format!("INSTALL\t{id}\tall"),
        ["launch", id] => format!("LAUNCH\t{id}"),
        ["stop", id] => format!("STOP\t{id}"),
        ["uninstall", id] => format!("UNINSTALL\t{id}"),
        ["grant", id, p] => format!("GRANT\t{id}\t{p}"),
        ["revoke", id, p] => format!("REVOKE\t{id}\t{p}"),
        ["key", k] => format!("KEY\t{k}"),
        ["asset", n] => format!("ASSET\t{n}"),
        ["volume"] => "VOLUME".into(),
        ["volume", v] => format!("VOLUME\t{v}"),
        ["volume", "mute", how] => format!("VOLUME\tmute\t{how}"),
        ["background"] => "BACKGROUND".into(),
        ["background", id, how] => format!("BACKGROUND\t{id}\t{how}"),
        ["recents"] => "RECENTS".into(),
        ["repo"] | ["repo", "list"] => "REPO".into(),
        ["repo", "add", url] => format!("REPO\tadd\t{url}"),
        ["repo", "remove", id] => format!("REPO\tremove\t{id}"),
        ["repo", "on", id] => format!("REPO\tenable\t{id}\t1"),
        ["repo", "off", id] => format!("REPO\tenable\t{id}\t0"),
        ["foreground", id] => format!("FOREGROUND\t{id}"),
        ["foreground", id, payload] => format!("FOREGROUND\t{id}\t{payload}"),
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    let fd = match ipc::connect(crate::daemon::SOCKET) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("appctl: {}: {e} (is appd running? appd-session status)", crate::daemon::SOCKET);
            return 1;
        }
    };
    let raw = fd.as_raw_fd();
    if ipc::send(raw, "HELLO\tctl", None).is_err() || ipc::send(raw, &cmd, None).is_err() {
        eprintln!("appctl: send failed");
        return 1;
    }
    // launch waits for the start (downloads can take a while).
    let deadline = Instant::now() + Duration::from_secs(if cmd.starts_with("LAUNCH") { 120 } else if cmd.starts_with("REPO\tadd") { 45 } else { 20 });
    while Instant::now() < deadline {
        let mut p = libc::pollfd { fd: raw, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut p, 1, 500) };
        loop {
            match ipc::recv(raw) {
                Ok(Recv::Msg(m, fd)) => {
                    let (head, rest) = m.split_once('\t').unwrap_or((m.as_str(), ""));
                    if head == "ASSET" {
                        let size = fd.and_then(|f| std::fs::File::from(f).metadata().ok()).map(|m| m.len());
                        println!("{}{}", rest.replace('\t', " "), size.map_or(String::new(), |s| format!(" ({s} bytes, fd received)")));
                        return if rest.contains("\tok") { 0 } else { 1 };
                    }
                    match head {
                        "WELCOME" => {}
                        "OK" => {
                            if !rest.is_empty() {
                                println!("{rest}");
                            }
                            return 0;
                        }
                        "ERR" => {
                            eprintln!("appctl: {rest}");
                            return 1;
                        }
                        "LOG" => print!("{rest}"),
                        "INFO" => println!("{rest}"),
                        "APP" => {
                            let f: Vec<&str> = rest.split('\t').collect();
                            println!("{:<28} {:<10} {:<22} {}", f[0], f.get(8).unwrap_or(&""), f.get(1).unwrap_or(&""), f.get(9).unwrap_or(&""));
                        }
                        _ => println!("{}", m.replace('\t', "  ")),
                    }
                }
                Ok(Recv::WouldBlock) => break,
                Ok(Recv::Closed) | Err(_) => {
                    eprintln!("appctl: appd closed the connection");
                    return 1;
                }
            }
        }
    }
    eprintln!("appctl: timed out");
    3
}
