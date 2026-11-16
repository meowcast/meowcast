//! Assets other than apps (wallpapers, full fonts, later icons, ...) for the launcher.
//!
//! The asset server publishes assets.json: {"schema":1,"assets":{"<name>":{"sha256","size"
//! [,"preview"]}}}, signed like app manifests (assets.sig). `preview` is an optional tiny
//! base64 JPEG (a wallpaper's colours, ~1 KB) that the launcher shows, blurred, until the file
//! itself arrives: assets come from the network, and the dongle may be offline.
//!
//! What stays on the device: only the last verified index (/data/appd/assets.json + .sig,
//! a few KB, rewritten only when it changes), so previews work offline at boot. The files
//! themselves are kept in RAM (/apps/.assets, content-addressed, RAM_MAX with the least
//! recently used evicted) until reboot, never on flash. Clients get an open read-only fd:
//!   ASSET name  ->  ASSET name preview BASE64  (first, when the file is not here yet)
//!                   ASSET name ok (+ fd)  |  ASSET name error why
//! and ASSET_UPDATED name when a new signed index changes a file they were given.

use crate::sign::Trusted;
use crate::util::write_atomic;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const RAM_DIR: &str = "/apps/.assets";
/// The flash cache of earlier firmware (removed at start: assets are no longer stored on flash).
pub const OLD_FLASH_CACHE: &str = "/data/assets";
pub const RAM_MAX: u64 = 12 * 1024 * 1024;
pub const MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_INDEX_BYTES: u64 = 256 * 1024;
/// Base64 of the preview JPEG (~1 KB typical).
pub const MAX_PREVIEW_CHARS: usize = 6 * 1024; // fits one 8 KiB control message (ipc::MAX_MSG)

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Entry {
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub preview: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Index {
    pub schema: u32,
    pub assets: BTreeMap<String, Entry>,
}

impl Index {
    pub fn parse(bytes: &[u8]) -> Result<Index, String> {
        if bytes.len() as u64 > MAX_INDEX_BYTES {
            return Err("assets.json too large".into());
        }
        let idx: Index = serde_json::from_slice(bytes).map_err(|e| format!("assets.json: {e}"))?;
        if idx.schema != 1 {
            return Err(format!("assets.json schema {}", idx.schema));
        }
        for (name, e) in &idx.assets {
            let hex = e.sha256.len() == 64 && e.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
            let preview_ok = e.preview.as_ref().is_none_or(|p| {
                !p.is_empty() && p.len() <= MAX_PREVIEW_CHARS && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
            });
            if !crate::manifest::valid_rel_path(name) || !hex || e.size > MAX_ASSET_BYTES || !preview_ok {
                return Err(format!("assets.json: bad entry {name:?}"));
            }
        }
        Ok(idx)
    }
}

/// Verify a signed index (bytes + hex signature) against the trusted store keys.
pub fn verify(trusted: &[Trusted], bytes: &[u8], sig: &str) -> Result<Index, String> {
    crate::sign::verify(trusted, bytes, sig)?;
    Index::parse(bytes)
}

/// The index saved on flash by the last successful refresh (re-verified).
pub fn load_saved(state_dir: &Path, trusted: &[Trusted]) -> Option<(Index, Vec<u8>)> {
    let bytes = std::fs::read(state_dir.join("assets.json")).ok()?;
    let sig = std::fs::read_to_string(state_dir.join("assets.sig")).ok()?;
    verify(trusted, &bytes, sig.trim()).ok().map(|i| (i, bytes))
}

pub fn save(state_dir: &Path, bytes: &[u8], sig: &str) -> Result<(), String> {
    write_atomic(&state_dir.join("assets.sig"), sig.as_bytes(), 0o600).map_err(|e| e.to_string())?;
    write_atomic(&state_dir.join("assets.json"), bytes, 0o600).map_err(|e| e.to_string())
}

/// Free the flash used by the asset cache of earlier firmware (once; nothing writes it now).
pub fn drop_flash_cache() -> Option<u64> {
    let p = Path::new(OLD_FLASH_CACHE);
    if !p.is_dir() {
        return None;
    }
    let bytes = std::fs::read_dir(p).ok()?.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
    std::fs::remove_dir_all(p).ok()?;
    Some(bytes)
}

/// Where a verified copy of `e` is, if any (RAM; checked when stored, and tmpfs is root-only).
pub fn cached(e: &Entry) -> Option<PathBuf> {
    let p = Path::new(RAM_DIR).join(&e.sha256);
    let m = std::fs::metadata(&p).ok()?;
    if m.len() != e.size {
        let _ = std::fs::remove_file(&p);
        return None;
    }
    // LRU: mark as used.
    let _ = std::fs::File::options().append(true).open(&p).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    Some(p)
}

/// Store verified `data` in RAM, evicting the least recently used files beyond RAM_MAX.
pub fn store(e: &Entry, data: &[u8]) -> Result<PathBuf, String> {
    let dir = Path::new(RAM_DIR);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let size = data.len() as u64;
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|f| {
            let m = f.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), f.path()))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (_, len, path) in files {
        if total + size <= RAM_MAX {
            break;
        }
        // A file still open by a client stays readable through its fd.
        let _ = std::fs::remove_file(&path);
        total -= len;
    }
    let p = dir.join(&e.sha256);
    // tmpfs: no fsync needed, but keep the write atomic for readers.
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, &p)).map_err(|e| e.to_string())?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_is_optional_and_checked() {
        let h = "a".repeat(64);
        let ok = format!(r#"{{"schema":1,"assets":{{"w/a.jpg":{{"sha256":"{h}","size":3,"preview":"/9j/4AAQ=="}},"f.ttf":{{"sha256":"{h}","size":3}}}}}}"#);
        let idx = Index::parse(ok.as_bytes()).unwrap();
        assert_eq!(idx.assets["w/a.jpg"].preview.as_deref(), Some("/9j/4AAQ=="));
        assert_eq!(idx.assets["f.ttf"].preview, None);
        let bad = format!(r#"{{"schema":1,"assets":{{"a":{{"sha256":"{h}","size":3,"preview":"no\ttabs"}}}}}}"#);
        assert!(Index::parse(bad.as_bytes()).is_err());
        let big = format!(r#"{{"schema":1,"assets":{{"a":{{"sha256":"{h}","size":3,"preview":"{}"}}}}}}"#, "A".repeat(MAX_PREVIEW_CHARS + 4));
        assert!(Index::parse(big.as_bytes()).is_err());
    }
}
