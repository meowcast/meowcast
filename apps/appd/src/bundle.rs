//! App payloads: plain POSIX ustar archives with regular files and directories only.
//!
//! The extractor treats the archive as hostile even though its hash is signed: no links,
//! devices or FIFOs, no absolute paths or "..", bounded entry count and size, header
//! checksums verified, files created with O_EXCL|O_NOFOLLOW and owned by root (the app runs
//! as another user and sees the payload read-only).

use crate::manifest::valid_rel_path;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const BLOCK: usize = 512;
pub const MAX_ENTRIES: usize = 4096;

fn octal(field: &[u8]) -> Option<u64> {
    let s: Vec<u8> = field.iter().copied().take_while(|&b| b != 0).filter(|&b| b != b' ').collect();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(std::str::from_utf8(&s).ok()?, 8).ok()
}

fn cstr_field(field: &[u8]) -> Result<String, String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|_| "tar: non-UTF-8 name".to_string())
}

fn checksum(h: &[u8; BLOCK]) -> u64 {
    h.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { 32 } else { b as u64 }).sum()
}

/// Extract `tar` into `dest` (which must not exist yet). Returns (files, bytes).
pub fn extract(tar: &[u8], dest: &Path, max_bytes: u64) -> Result<(usize, u64), String> {
    std::fs::create_dir(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let (mut pos, mut entries, mut total) = (0usize, 0usize, 0u64);
    loop {
        let Some(h) = tar.get(pos..pos + BLOCK) else { return Err("tar: truncated header".into()) };
        let h: &[u8; BLOCK] = h.try_into().unwrap();
        if h.iter().all(|&b| b == 0) {
            return Ok((entries, total)); // end-of-archive marker
        }
        pos += BLOCK;
        if octal(&h[148..156]) != Some(checksum(h)) {
            return Err("tar: bad header checksum".into());
        }
        if &h[257..262] != b"ustar" {
            return Err("tar: not a ustar archive".into());
        }
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err("tar: too many entries".into());
        }
        let name = cstr_field(&h[0..100])?;
        let prefix = cstr_field(&h[345..500])?;
        let mut path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        while let Some(p) = path.strip_prefix("./") {
            path = p.to_string();
        }
        let path = path.trim_end_matches('/').to_string();
        let size = octal(&h[124..136]).ok_or("tar: bad size")?;
        let mode = octal(&h[100..108]).ok_or("tar: bad mode")?;
        let kind = h[156];
        if path.is_empty() && kind == b'5' {
            continue; // "./"
        }
        if !valid_rel_path(&path) {
            return Err(format!("tar: unsafe path {path:?}"));
        }
        let target = dest.join(&path);
        match kind {
            b'5' => {
                if size != 0 {
                    return Err("tar: directory with data".into());
                }
                std::fs::create_dir(&target).map_err(|e| format!("{path}: {e}"))?;
                std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                    .map_err(|e| format!("{path}: {e}"))?;
            }
            b'0' | 0 => {
                total += size;
                if total > max_bytes {
                    return Err("tar: contents larger than allowed".into());
                }
                let end = pos.checked_add(size as usize).ok_or("tar: size overflow")?;
                let data = tar.get(pos..end).ok_or("tar: truncated file")?;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 })
                    .open(&target)
                    .map_err(|e| format!("{path}: {e}"))?;
                f.write_all(data).map_err(|e| format!("{path}: {e}"))?;
                pos = end.div_ceil(BLOCK) * BLOCK;
            }
            other => return Err(format!("tar: entry type {:?} not allowed ({path})", other as char)),
        }
    }
}

/// Write a ustar archive of `root` (regular files and directories, sorted, mtime 0, uid 0).
pub fn create(root: &Path) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    let mut items: Vec<(String, bool)> = Vec::new();
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let mut names: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let ft = e.file_type().map_err(|e| e.to_string())?;
            if ft.is_dir() {
                items.push((path.clone(), true));
                stack.push(path);
            } else if ft.is_file() {
                items.push((path, false));
            } else {
                return Err(format!("{path}: only files and directories may go into a payload"));
            }
        }
    }
    items.sort();
    for (path, is_dir) in items {
        if !valid_rel_path(&path) || path.len() > 99 {
            return Err(format!("{path}: path not allowed (max 99 bytes, plain components)"));
        }
        let full = root.join(&path);
        let (data, mode) = if is_dir {
            (Vec::new(), 0o755)
        } else {
            let meta = std::fs::metadata(&full).map_err(|e| e.to_string())?;
            let exec = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
            (std::fs::read(&full).map_err(|e| e.to_string())?, if exec { 0o755 } else { 0o644 })
        };
        let mut h = [0u8; BLOCK];
        h[..path.len()].copy_from_slice(path.as_bytes());
        let put = |h: &mut [u8; BLOCK], at: usize, len: usize, v: u64| {
            let s = format!("{:0w$o}\0", v, w = len - 1);
            h[at..at + len].copy_from_slice(s.as_bytes());
        };
        put(&mut h, 100, 8, mode);
        put(&mut h, 108, 8, 0);
        put(&mut h, 116, 8, 0);
        put(&mut h, 124, 12, data.len() as u64);
        put(&mut h, 136, 12, 0);
        h[156] = if is_dir { b'5' } else { b'0' };
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum = checksum(&h);
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(&data);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    out.extend_from_slice(&[0u8; BLOCK * 2]);
    Ok(out)
}
