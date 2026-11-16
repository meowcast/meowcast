//! Store signatures: ed25519 over the exact manifest.json bytes.
//!
//! Key files are one line of hex: 32-byte public keys (`*.pub`, trusted by the device in
//! /etc/appd/keys, baked into the read-only initramfs) and 32-byte secret seeds (`*.key`,
//! only on the publishing machine). A key id is the first 8 bytes of SHA-256(public key).

use crate::util::{hex, sha256_hex, unhex};
use ed25519_compact::{KeyPair, PublicKey, Seed, Signature};
use std::path::Path;

pub struct Trusted {
    pub id: String,
    pub key: PublicKey,
    pub file: String,
}

pub fn key_id(pk: &[u8]) -> String {
    sha256_hex(pk)[..16].to_string()
}

/// Load every `*.pub` in `dir`. A malformed key file is an error (never silently skipped).
pub fn load_trusted(dir: &Path) -> Result<Vec<Trusted>, String> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut paths: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pub")).collect();
    paths.sort();
    for p in paths {
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let raw = unhex(&text).ok_or_else(|| format!("{}: not hex", p.display()))?;
        let key = PublicKey::from_slice(&raw).map_err(|e| format!("{}: {e}", p.display()))?;
        out.push(Trusted { id: key_id(&raw), key, file: p.display().to_string() });
    }
    Ok(out)
}

/// Verify `sig_hex` (manifest.sig) over `msg` with any trusted key; returns the key id.
pub fn verify(trusted: &[Trusted], msg: &[u8], sig_hex: &str) -> Result<String, String> {
    let raw = unhex(sig_hex).ok_or("signature: not hex")?;
    let sig = Signature::from_slice(&raw).map_err(|e| format!("signature: {e}"))?;
    for t in trusted {
        if t.key.verify(msg, &sig).is_ok() {
            return Ok(t.id.clone());
        }
    }
    Err("signature does not match any trusted store key".into())
}

pub fn keygen(seed: [u8; 32]) -> (String, String) {
    let kp = KeyPair::from_seed(Seed::new(seed));
    (hex(seed.as_slice()), hex(kp.pk.as_ref()))
}

pub fn sign(seed_hex: &str, msg: &[u8]) -> Result<String, String> {
    let raw = unhex(seed_hex).ok_or("key: not hex")?;
    let seed = Seed::from_slice(&raw).map_err(|e| format!("key: {e}"))?;
    let kp = KeyPair::from_seed(seed);
    Ok(hex(kp.sk.sign(msg, None).as_ref()))
}

pub fn random32() -> Result<[u8; 32], String> {
    let mut b = [0u8; 32];
    let n = unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, 32, 0) };
    if n != 32 {
        return Err("getrandom failed".into());
    }
    Ok(b)
}
