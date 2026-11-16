//! App storage ("storage" permission).
//!
//! While an app runs, its /data is a tmpfs of exactly its quota (hard limit by the kernel,
//! noexec, owned by the app's uid), mounted in appd's namespace at /run/appd/storage/<id>
//! and bind-mounted into the sandbox. appd saves it to flash as one snapshot file
//! (/data/appdata/<id>.pack, written atomically) when the app exits or asks for a sync
//! (rate-limited), and only if the content changed. So a buggy or hostile app can neither
//! exceed its quota nor wear out the NOR flash, and a power cut never leaves a half-written
//! state: the previous snapshot stays valid until the new one is complete.
//!
//! Snapshot format: "IAPD1\n", then per entry: kind (b'd' | b'f'), u16 path length, path,
//! u16 mode, u32 size, data; then the SHA-256 of everything before it.

use crate::manifest::valid_rel_path;
use crate::util::{cstr, cvt};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const MAGIC: &[u8] = b"IAPD1\n";
const MAX_ENTRIES: usize = 2048;

/// Mount the quota tmpfs at `mnt` (owned by `uid`) and fill it from `snapshot` (if present).
pub fn mount_and_load(mnt: &Path, quota_kb: u32, uid: u32, snapshot: &Path) -> Result<(), String> {
    std::fs::create_dir_all(mnt).map_err(|e| format!("{}: {e}", mnt.display()))?;
    let opts = format!("size={quota_kb}k,nr_inodes={},mode=0700,uid={uid},gid={uid}", 64 + quota_kb.min(2048));
    let fs = crate::util::cstring("tmpfs");
    let o = crate::util::cstring(&opts);
    cvt(unsafe {
        libc::mount(fs.as_ptr(), cstr(mnt).as_ptr(), fs.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, o.as_ptr() as *const _)
    })
    .map_err(|e| format!("mount storage tmpfs: {e}"))?;
    match std::fs::read(snapshot) {
        Ok(bytes) => {
            if let Err(e) = unpack(&bytes, mnt, uid) {
                // A corrupt snapshot must not block the app: keep it aside, start empty.
                let bad = snapshot.with_extension("pack.bad");
                let _ = std::fs::rename(snapshot, &bad);
                clear_dir(mnt);
                return Err(format!("snapshot unreadable ({e}); moved to {}", bad.display()));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", snapshot.display())),
    }
}

fn clear_dir(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

pub fn unmount(mnt: &Path) {
    unsafe { libc::umount2(cstr(mnt).as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_dir(mnt);
}

/// Serialise the tree under `root`. Symlinks, devices, FIFOs and sockets are skipped (the
/// app can create symlinks and FIFOs in its tmpfs; they are not persisted).
pub fn pack(root: &Path) -> Result<(Vec<u8>, usize), String> {
    let mut out = MAGIC.to_vec();
    let mut n = 0usize;
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(&rel) };
        let mut ents: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if !valid_rel_path(&path) || path.len() > 1000 {
                continue;
            }
            let meta = std::fs::symlink_metadata(root.join(&path)).map_err(|e| e.to_string())?;
            let kind = if meta.is_dir() {
                b'd'
            } else if meta.is_file() {
                b'f'
            } else {
                continue;
            };
            n += 1;
            if n > MAX_ENTRIES {
                return Err("too many files".into());
            }
            let data = if kind == b'f' {
                // O_NOFOLLOW: the app may swap a file for a symlink while we read.
                let mut f = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(root.join(&path))
                    .map_err(|e| format!("{path}: {e}"))?;
                let mut v = Vec::new();
                std::io::Read::read_to_end(&mut f, &mut v).map_err(|e| format!("{path}: {e}"))?;
                v
            } else {
                stack.push(path.clone());
                Vec::new()
            };
            out.push(kind);
            out.extend_from_slice(&(path.len() as u16).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&((meta.mode() & 0o777) as u16).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&data);
        }
    }
    let digest = Sha256::digest(&out);
    out.extend_from_slice(&digest);
    Ok((out, n))
}

pub fn unpack(bytes: &[u8], root: &Path, uid: u32) -> Result<usize, String> {
    if bytes.len() < MAGIC.len() + 32 || !bytes.starts_with(MAGIC) {
        return Err("not a snapshot".into());
    }
    let (body, digest) = bytes.split_at(bytes.len() - 32);
    if Sha256::digest(body).as_slice() != digest {
        return Err("checksum mismatch".into());
    }
    let mut pos = MAGIC.len();
    let mut n = 0;
    let take = |pos: &mut usize, k: usize| -> Result<&[u8], String> {
        let s = body.get(*pos..*pos + k).ok_or("truncated")?;
        *pos += k;
        Ok(s)
    };
    while pos < body.len() {
        let kind = take(&mut pos, 1)?[0];
        let plen = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap()) as usize;
        let path = std::str::from_utf8(take(&mut pos, plen)?).map_err(|_| "bad path")?.to_string();
        let mode = u16::from_le_bytes(take(&mut pos, 2)?.try_into().unwrap()) as u32 & 0o777;
        let size = u32::from_le_bytes(take(&mut pos, 4)?.try_into().unwrap()) as usize;
        let data = take(&mut pos, size)?;
        if !valid_rel_path(&path) {
            return Err(format!("unsafe path {path:?}"));
        }
        let target = root.join(&path);
        match kind {
            b'd' => std::fs::create_dir(&target).map_err(|e| format!("{path}: {e}"))?,
            b'f' => {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&target)
                    .map_err(|e| format!("{path}: {e}"))?;
                std::io::Write::write_all(&mut f, data).map_err(|e| format!("{path}: {e}"))?;
            }
            _ => return Err("bad entry".into()),
        }
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode | if kind == b'd' { 0o700 } else { 0o600 }))
            .map_err(|e| e.to_string())?;
        unsafe { libc::lchown(cstr(&target).as_ptr(), uid, uid) };
        n += 1;
    }
    Ok(n)
}
