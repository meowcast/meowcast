//! meow-image: the system image service.
//!
//! Decodes the picture formats the device does not decode itself (WebP, JPEG) for every app.
//! The decoders are in no app and not in the firmware: this is a store package like the
//! network service, signed, fetched into RAM when an app first asks for a picture, and run by
//! appd in the ordinary app sandbox with no permission at all: its own uid, an empty network
//! namespace, no storage, no screen, a read-only root, Landlock, seccomp, 16 MiB of memory.
//! What it reads comes from the internet unchecked, so it is parsed here and not in an app,
//! by decoders without `unsafe` code, behind size limits that are checked first.
//!
//!   appd --CLIENT id + a socket-->  service              (over the service's broker, fd 3)
//!   app  == file ==>  service  == pixels ==>  app        (that socket: sdk/src/image.rs)
//!
//! It leaves when no app has been connected for a while; appd starts it again when asked.
//!
//! `meow-image --serve FD` serves one connection on an inherited socket (tests and the build
//! host: `MEOW_IMAGE_HELPER`, see the SDK); `meow-image FILE... [--fit W H] [--times N]`
//! decodes files and prints what they are and how long it took.

mod decode;

use meow_app::image::{MAGIC, MAX_INPUT, REQUEST_HEAD, response_head};
use meow_app::ipc;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Connections served at once (each is a thread); more are closed, and the app carries on
/// without the picture.
const CLIENTS: usize = 8;
/// With no client for this long the service exits: its memory goes back to the device.
const IDLE: Duration = Duration::from_secs(60);
/// A request must arrive whole within this time, and an answer must be taken.
const TRANSFER: Duration = Duration::from_secs(10);
const STACK: usize = 256 << 10;

static CONNECTED: AtomicUsize = AtomicUsize::new(0);
/// When the last client left, in milliseconds since the start.
static LAST_LEFT: AtomicU64 = AtomicU64::new(0);
/// One picture at a time: the memory limit is sized for one, and pictures are small.
static ONE: Mutex<()> = Mutex::new(());

/// Serve one connection until the client closes it or sends something that is not a request.
fn serve(id: &str, mut stream: UnixStream) {
    let _ = stream.set_write_timeout(Some(TRANSFER));
    let (mut pictures, mut refused) = (0u32, 0u32);
    loop {
        // Waiting for the next request may take as long as the client likes.
        let _ = stream.set_read_timeout(None);
        let mut head = [0u8; REQUEST_HEAD];
        if stream.read_exact(&mut head).is_err() {
            break;
        }
        let length = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let fit = (u16::from_le_bytes([head[8], head[9]]) as usize, u16::from_le_bytes([head[10], head[11]]) as usize);
        if head[..4] != MAGIC || length > MAX_INPUT {
            eprintln!("image: client {id}: not a request, closing");
            break;
        }
        let _ = stream.set_read_timeout(Some(TRANSFER));
        let mut data = vec![0u8; length];
        if stream.read_exact(&mut data).is_err() {
            break;
        }
        let result = {
            let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
            decode::decode(&data, fit)
        };
        drop(data);
        let sent = match result {
            Ok(picture) => {
                pictures += 1;
                stream.write_all(&response_head(0, picture.width as u32, picture.height as u32, picture.rgba.len() as u32)).and_then(|()| stream.write_all(&picture.rgba))
            }
            Err(why) => {
                refused += 1;
                let why: String = why.chars().filter(|c| !c.is_control()).take(200).collect();
                stream.write_all(&response_head(1, 0, 0, why.len() as u32)).and_then(|()| stream.write_all(why.as_bytes()))
            }
        };
        if sent.is_err() {
            break;
        }
    }
    eprintln!("image: client {id} gone after {pictures} pictures, {refused} refused");
}

fn tool(args: &[String]) -> Result<(), String> {
    let number = |name: &str, offset: usize| args.iter().position(|a| a == name).and_then(|i| args.get(i + offset)).and_then(|v| v.parse::<usize>().ok());
    let fit = (number("--fit", 1).unwrap_or(0), number("--fit", 2).unwrap_or(0));
    let times = number("--times", 1).unwrap_or(1).max(1);
    let mut skip = 0;
    for argument in args {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        match argument.as_str() {
            "--fit" => skip = 2,
            "--times" => skip = 1,
            file => {
                let data = std::fs::read(file).map_err(|e| format!("{file}: {e}"))?;
                let mut best = Duration::MAX;
                let mut result = Err(String::new());
                for _ in 0..times {
                    let started = Instant::now();
                    result = decode::decode(&data, fit);
                    best = best.min(started.elapsed());
                }
                match result {
                    Ok(picture) => {
                        let sum: u64 = picture.rgba.iter().map(|b| *b as u64).sum();
                        let clear = picture.rgba.chunks_exact(4).filter(|p| p[3] < 255).count();
                        println!("{file}: {:?} {} bytes -> {}x{}, {clear} pixels not opaque, sum {sum}, {} us", decode::sniff(&data), data.len(), picture.width, picture.height, best.as_micros());
                    }
                    Err(why) => println!("{file}: {} bytes: {why} ({} us)", data.len(), best.as_micros()),
                }
            }
        }
    }
    Ok(())
}

fn run() -> Result<(), String> {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(fd) = args.iter().position(|a| a == "--serve").map(|i| args.get(i + 1)) {
        let fd: RawFd = fd.and_then(|fd| fd.parse().ok()).ok_or("--serve FD")?;
        serve("helper", UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) }));
        return Ok(());
    }
    if !args.is_empty() {
        return tool(&args);
    }
    let broker: RawFd = std::env::var("MEOW_BROKER_FD").ok().and_then(|v| v.parse().ok()).ok_or("not started by appd (use --serve FD, or name files to decode)")?;
    let started = Instant::now();
    eprintln!("image: ready (WebP, JPEG)");
    loop {
        let mut poll = libc::pollfd { fd: broker, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut poll, 1, 5000) } == 0 {
            let idle = started.elapsed().saturating_sub(Duration::from_millis(LAST_LEFT.load(Ordering::Relaxed)));
            if CONNECTED.load(Ordering::Relaxed) == 0 && idle >= IDLE {
                eprintln!("image: nobody connected for {} s, leaving", idle.as_secs());
                return Ok(());
            }
            continue;
        }
        let (message, fd) = match ipc::recv(broker, false) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(_) => return Ok(()), // appd is gone
        };
        let fields: Vec<&str> = message.split('\t').collect();
        match (fields[0], fd) {
            // An app's connection (any app may have pictures decoded; it gets nothing else).
            ("CLIENT", Some(socket)) => {
                let id = fields.get(1).unwrap_or(&"?").to_string();
                if CONNECTED.load(Ordering::Relaxed) >= CLIENTS {
                    eprintln!("image: client {id} refused: too many connections");
                    continue; // (the socket closes: the app sees the end of the stream)
                }
                CONNECTED.fetch_add(1, Ordering::Relaxed);
                let name = id.clone();
                let thread = std::thread::Builder::new().name(format!("c-{id}")).stack_size(STACK).spawn(move || {
                    serve(&id, UnixStream::from(socket));
                    LAST_LEFT.store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    CONNECTED.fetch_sub(1, Ordering::Relaxed);
                });
                if let Err(e) = thread {
                    CONNECTED.fetch_sub(1, Ordering::Relaxed);
                    eprintln!("image: client {name}: {e}");
                }
            }
            ("QUIT", _) => return Ok(()),
            _ => {}
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("meow-image: {error}");
        std::process::exit(1);
    }
}
