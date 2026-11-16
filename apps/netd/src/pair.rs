//! Pilot pairing. A phone becomes a paired pilot by presenting the 6-digit code the TV shows
//! (inside the QR code's link, or typed). It gets a random token that it keeps; the dongle stores
//! only the token's SHA-256 (/data/net/pilots: "<sha256> <unix time> <name>"). Pairing is open
//! only while the TV shows the code (the UI sends pair-open / pair-close); every successful
//! pairing and every MAX_FAILS wrong guesses replace the code, and wrong guesses also lock
//! pairing for LOCKOUT_S, so guessing over the network is not practical.
//!
//! Developer enrollment is implemented separately in debug.rs using SPAKE2.

use crate::state::{self, Shared};
use crate::util;
use std::path::Path;

pub const PILOTS: &str = "/data/net/pilots";
const MAX_FAILS: u32 = 5;
/// After MAX_FAILS wrong codes: a new code, and no attempts for this long.
const LOCKOUT_S: f64 = 30.0;
/// How long a "pair-open" from the UI keeps the window open without being renewed.
pub const WINDOW_S: f64 = 900.0;
const MAX_PILOTS: usize = 16;

pub fn count() -> usize {
    std::fs::read_to_string(PILOTS).map(|s| s.lines().filter(|l| l.len() >= 64).count()).unwrap_or(0)
}

pub fn token_valid(token: &str) -> bool {
    if token.is_empty() || token.len() > 64 {
        return false;
    }
    let h = util::sha256_hex(token.as_bytes());
    std::fs::read_to_string(PILOTS)
        .map(|s| s.lines().any(|l| l.get(..64).is_some_and(|x| util::ct_eq(x.as_bytes(), h.as_bytes()))))
        .unwrap_or(false)
}

fn clean_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| c.is_alphanumeric() || " -_.()".contains(*c)).take(40).collect();
    if n.trim().is_empty() { "phone".into() } else { n.trim().replace(' ', "_") }
}

/// A pilot token given as its SHA-256 (hex): what the cloud relay forwards.
pub fn hash_valid(hash: &str) -> bool {
    if hash.len() != 64 {
        return false;
    }
    let h = hash.to_ascii_lowercase();
    std::fs::read_to_string(PILOTS)
        .map(|s| s.lines().any(|l| l.get(..64).is_some_and(|x| util::ct_eq(x.as_bytes(), h.as_bytes()))))
        .unwrap_or(false)
}

/// Exchange the TV's pairing code for a new pilot token.
pub fn pair(shared: &Shared, code: &str, name: &str) -> Result<String, &'static str> {
    {
        let mut n = shared.0.lock().unwrap();
        let now = util::uptime();
        if now >= n.pair_open_until {
            return Err("not_open");
        }
        if now < n.pair_locked_until {
            return Err("locked");
        }
        if !util::ct_eq(code.trim().as_bytes(), n.pair_code.as_bytes()) {
            n.pair_fails += 1;
            if n.pair_fails >= MAX_FAILS {
                n.pair_code = util::random_digits(6);
                n.pair_fails = 0;
                n.pair_locked_until = now + LOCKOUT_S;
                util::log("netd: too many wrong pairing codes; new code, 30 s lockout");
                drop(n);
                state::changed(shared);
            }
            return Err("bad_code");
        }
        n.pair_code = util::random_digits(6);
        n.pair_fails = 0;
    }
    let token = util::random_token();
    let mut lines: Vec<String> = std::fs::read_to_string(PILOTS).unwrap_or_default().lines().filter(|l| l.len() >= 64).map(|l| l.to_string()).collect();
    lines.push(format!("{} {} {}", util::sha256_hex(token.as_bytes()), util::now_unix(), clean_name(name)));
    if lines.len() > MAX_PILOTS {
        lines.drain(..lines.len() - MAX_PILOTS);
    }
    let body = lines.join("\n") + "\n";
    util::write_atomic(Path::new(PILOTS), body.as_bytes(), 0o600).map_err(|_| "storage")?;
    util::log(format!("netd: pilot paired ({}), {} paired", clean_name(name), lines.len()));
    shared.0.lock().unwrap().pilots = lines.len();
    state::changed(shared);
    Ok(token)
}

/// Forget every paired pilot (Settings > Wi-Fi > Unpair phones).
pub fn unpair_all(shared: &Shared) {
    let _ = std::fs::remove_file(PILOTS);
    shared.0.lock().unwrap().pilots = 0;
    state::changed(shared);
    util::log("netd: all pilots unpaired");
}
