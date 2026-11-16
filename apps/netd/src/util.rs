//! Small helpers: randomness, hashing, atomic files, processes, interface addresses.

use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn random_bytes(buf: &mut [u8]) {
    let mut done = 0;
    while done < buf.len() {
        // getrandom(2) blocks only until the pool is initialised once after boot.
        let r = unsafe { libc::getrandom(buf[done..].as_mut_ptr().cast(), buf.len() - done, 0) };
        if r > 0 {
            done += r as usize;
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            panic!("getrandom: {}", io::Error::last_os_error());
        }
    }
}

/// `n` decimal digits, uniformly distributed (rejection sampling).
pub fn random_digits(n: usize) -> String {
    let mut s = String::with_capacity(n);
    while s.len() < n {
        let mut b = [0u8; 1];
        random_bytes(&mut b);
        if b[0] < 250 {
            s.push(char::from(b'0' + b[0] % 10));
        }
    }
    s
}

/// 128 random bits, base64url without padding (22 characters).
pub fn random_token() -> String {
    let mut b = [0u8; 16];
    random_bytes(&mut b);
    base64(&b, true)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Constant-time comparison (pairing codes, token hashes, MACs).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn base64(data: &[u8], url: bool) -> String {
    let t: &[u8; 64] = if url {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(t[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else if !url {
                s.push('=');
            }
        }
    }
    s
}

/// SHA-1, only for the WebSocket handshake (RFC 6455 fixes it; not used for security).
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Write a file atomically (temp file + fsync + rename): a power cut on JFFS2 leaves the old
/// or the new file, never a torn one.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent dir"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp~");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

pub fn cstring(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("string with NUL")
}

pub fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn mkdir_private(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::create_dir_all(path);
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

/// The first IPv4 address of an interface.
pub fn ipv4_of(iface: &str) -> Option<Ipv4Addr> {
    let mut found = None;
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy();
                if name == iface {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    found = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
                    break;
                }
            }
            p = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    found
}

/// Run a command to completion; Err with its output when it fails.
pub fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("{cmd} {}: {}{}", args.join(" "), String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string()),
        Err(e) => Err(format!("{cmd}: {e}")),
    }
}

pub fn log(msg: impl AsRef<str>) {
    let t = unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts);
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    };
    eprintln!("[{t:9.3}] {}", msg.as_ref());
}

/// Seconds since boot (CLOCK_BOOTTIME): the wall clock jumps when NTP sets it.
pub fn uptime() -> f64 {
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts);
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn base64_known() {
        assert_eq!(base64(b"foobar", false), "Zm9vYmFy");
        assert_eq!(base64(b"fooba", false), "Zm9vYmE=");
        assert_eq!(base64(b"foob", true), "Zm9vYg");
    }

    #[test]
    fn hmac_known() {
        // RFC 4231 test case 2
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn digits() {
        let d = random_digits(8);
        assert_eq!(d.len(), 8);
        assert!(d.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(random_token().len(), 22);
    }
}
