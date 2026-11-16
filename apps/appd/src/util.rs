//! Small helpers shared by the daemon, the sandbox helper and the host tools.

use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Write a file atomically: temp file in the same directory, fsync, rename, fsync the dir.
/// On JFFS2 the rename is atomic, so a power cut leaves either the old or the new file.
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

pub fn cstr(p: impl AsRef<Path>) -> CString {
    CString::new(p.as_ref().as_os_str().as_bytes()).expect("path with NUL")
}

pub fn cstring(s: &str) -> CString {
    CString::new(s).expect("string with NUL")
}

/// libc return value -> io::Result.
pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub fn cvt_ctx(r: libc::c_int, what: &str) -> Result<libc::c_int, String> {
    cvt(r).map_err(|e| format!("{what}: {e}"))
}

/// One protocol field: no tabs/newlines/control characters.
pub fn field(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Total bytes of free space on the filesystem holding `path`.
pub fn fs_free(path: &Path) -> io::Result<u64> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::statvfs(cstr(path).as_ptr(), &mut st) })?;
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

pub fn fs_size(path: &Path) -> io::Result<u64> {
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    cvt(unsafe { libc::statvfs(cstr(path).as_ptr(), &mut st) })?;
    Ok(st.f_blocks as u64 * st.f_frsize as u64)
}
