//! appd — the meow app manager. One binary, several roles:
//!   appd daemon            the app manager (started by rootfs/appd-session)
//!   appd sandbox-exec      internal: builds one app's sandbox (spawned by the daemon)
//!   appctl ... / appd ctl  command-line control (symlink appctl -> appd)
//!   meow-netd              network daemon (symlink meow-netd -> appd, crate ../netd)
//!   appd keygen|pack|index|verify|assets|repo   store publishing tools (build host)
//!   appd store-check --store URL --keys DIR  fetch a live store like a device does (any host)
//! Architecture and permission model: ../README.md.

mod assets;
mod bundle;
mod ctl;
mod daemon;
mod handover;
mod http;
mod ipc;
mod landlock;
mod manifest;
mod miracast;
mod perms;
mod repos;
mod sandbox;
mod seccomp;
mod service;
mod sign;
mod state;
mod storage;
mod store;
mod tools;
mod util;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().and_then(|a| a.rsplit('/').next()).unwrap_or("appd").to_string();
    if prog == "appctl" {
        std::process::exit(ctl::main(&args[1..]));
    }
    if prog == "meow-netd" {
        meow_netd::main(&args);
        return;
    }
    let rest = if args.len() > 2 { &args[2..] } else { &[] };
    let r = match args.get(1).map(|s| s.as_str()) {
        Some("daemon") => daemon::run(),
        Some("sandbox-exec") => sandbox::exec_main(),
        Some("ctl") => std::process::exit(ctl::main(rest)),
        Some("keygen") => tools::keygen(rest),
        Some("pack") => tools::pack(rest),
        Some("index") => tools::index(rest),
        Some("verify") => tools::verify(rest),
        Some("assets") => tools::assets(rest),
        Some("repo") => tools::repo(rest),
        Some("store-check") => tools::store_check(rest),
        Some("seccomp-list") => {
            for (n, nr) in seccomp::denied() {
                println!("{nr:4} {n}");
            }
            Ok(())
        }
        _ => {
            eprintln!("usage: appd daemon | ctl ... | keygen KEY | pack --manifest M --payload DIR --key KEY --store WWW | index --store WWW | verify --store WWW --keys DIR | assets --store WWW --key KEY | repo --store WWW --key KEY --name NAME | store-check --store URL --keys DIR");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("appd: {e}");
        std::process::exit(1);
    }
}
