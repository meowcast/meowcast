//! Store client. The store is a static file tree (cloud/store/www), hosted on Cloudflare
//! (https://store.example.org, cloud/store/wrangler.jsonc):
//!
//!   index.json                         {"schema":1,"apps":[{"id","manifest","signature","payload"}]}
//!   apps/<id>/<version_code>/manifest.json   signed description (see manifest.rs)
//!   apps/<id>/<version_code>/manifest.sig    hex ed25519 signature of manifest.json
//!   apps/<id>/<version_code>/payload.tar     code + assets, pinned by manifest.payload
//!
//! Other repositories the user adds are the same tree plus repo.json (repos.rs).
//!
//! The index is only discovery: nothing in it is trusted. Runs on worker threads; a refresh
//! shares one keep-alive connection (http::Client).

use crate::bundle;
use crate::http;
use crate::manifest::{MAX_MANIFEST_BYTES, Manifest};
use crate::sign::{self, Trusted};
use crate::util::sha256_hex;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct Index {
    schema: u32,
    apps: Vec<IndexEntry>,
}

#[derive(Deserialize)]
struct IndexEntry {
    id: String,
    manifest: String,
    signature: String,
    payload: String,
}

/// A verified store listing.
#[derive(Clone)]
pub struct Listing {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub signature: String,
    pub key_id: String,
    pub payload_url: String,
}

/// How many apps an index lists (a repository being looked at before it is added).
pub fn index_len(bytes: &[u8]) -> Result<usize, String> {
    let index: Index = serde_json::from_slice(bytes).map_err(|e| format!("index.json: {e}"))?;
    if index.schema != 1 {
        return Err(format!("index.json: schema {}", index.schema));
    }
    Ok(index.apps.len())
}

/// Fetch the index and every manifest; return verified listings plus per-app errors.
pub fn refresh(c: &mut http::Client, store: &str, trusted: &[Trusted]) -> Result<(Vec<Listing>, Vec<String>), String> {
    let index_url = http::join(store, "index.json");
    let body = c.get(&index_url, 256 * 1024, |_, _| {})?;
    let index: Index = serde_json::from_slice(&body).map_err(|e| format!("index.json: {e}"))?;
    if index.schema != 1 {
        return Err(format!("index.json: schema {}", index.schema));
    }
    let (mut out, mut errors) = (Vec::new(), Vec::new());
    for e in index.apps.iter().take(256) {
        match fetch_listing(c, &index_url, e, trusted) {
            Ok(l) => out.push(l),
            Err(err) => errors.push(format!("{}: {err}", e.id)),
        }
    }
    Ok((out, errors))
}

fn fetch_listing(c: &mut http::Client, index_url: &str, e: &IndexEntry, trusted: &[Trusted]) -> Result<Listing, String> {
    let manifest_bytes = c.get(&http::join(index_url, &e.manifest), MAX_MANIFEST_BYTES as u64, |_, _| {})?;
    let sig = c.get(&http::join(index_url, &e.signature), 1024, |_, _| {})?;
    let signature = String::from_utf8(sig).map_err(|_| "signature not text")?.trim().to_string();
    let key_id = sign::verify(trusted, &manifest_bytes, &signature)?;
    let manifest = Manifest::parse(&manifest_bytes)?;
    if manifest.id != e.id {
        return Err(format!("index says {:?}, manifest says {:?}", e.id, manifest.id));
    }
    Ok(Listing { manifest, manifest_bytes, signature, key_id, payload_url: http::join(index_url, &e.payload) })
}

/// Download the payload pinned by `m`, check size and SHA-256, extract into `dest` (which
/// is replaced atomically: extract to dest.new, then rename).
pub fn fetch_payload(m: &Manifest, url: &str, dest: &Path, mut progress: impl FnMut(u32)) -> Result<(), String> {
    let tar = http::Client::new().get(url, m.payload.size, |done, _| progress((done * 100 / m.payload.size.max(1)) as u32))?;
    if tar.len() as u64 != m.payload.size {
        return Err(format!("payload is {} bytes, manifest says {}", tar.len(), m.payload.size));
    }
    let hash = sha256_hex(&tar);
    if hash != m.payload.sha256 {
        return Err(format!("payload hash {hash} does not match the signed manifest"));
    }
    install_tar(&tar, m, dest)
}

pub fn install_tar(tar: &[u8], m: &Manifest, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("new");
    let _ = std::fs::remove_dir_all(&tmp);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let (files, bytes) = bundle::extract(tar, &tmp, 4 * crate::manifest::MAX_PAYLOAD_BYTES).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp);
    })?;
    let exec = tmp.join(&m.exec);
    let ok = std::fs::metadata(&exec).is_ok_and(|md| md.is_file() && std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o111 != 0);
    if !ok {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("payload has no executable {:?}", m.exec));
    }
    // Stamp: which manifest this tree belongs to (re-verified before every launch).
    std::fs::write(tmp.with_extension("sha256"), &m.payload.sha256).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(dest);
    std::fs::rename(&tmp, dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::fs::rename(tmp.with_extension("sha256"), dest.with_extension("sha256")).map_err(|e| e.to_string())?;
    eprintln!("APPD payload {} {} files {} bytes", m.id, files, bytes);
    Ok(())
}

/// Whether `dest` holds the payload of `m` (stamp written by `install_tar`).
pub fn payload_present(m: &Manifest, dest: &Path) -> bool {
    dest.is_dir() && std::fs::read_to_string(dest.with_extension("sha256")).is_ok_and(|s| s.trim() == m.payload.sha256)
}

/// Fetch and verify the signed asset index (assets.json + assets.sig).
pub fn fetch_assets_index(c: &mut http::Client, store: &str, trusted: &[Trusted]) -> Result<(Vec<u8>, String, crate::assets::Index), String> {
    let url = http::join(store, "assets.json");
    let bytes = c.get(&url, crate::assets::MAX_INDEX_BYTES, |_, _| {})?;
    let sig = c.get(&http::join(store, "assets.sig"), 1024, |_, _| {})?;
    let sig = String::from_utf8(sig).map_err(|_| "assets.sig not text")?.trim().to_string();
    let idx = crate::assets::verify(trusted, &bytes, &sig)?;
    Ok((bytes, sig, idx))
}

/// Download asset `name` and check it against its signed entry.
pub fn fetch_asset(store: &str, name: &str, e: &crate::assets::Entry) -> Result<Vec<u8>, String> {
    // ?v= makes the URL change with the content: no stale copy from a cache on the way.
    let url = http::join(store, &format!("assets/{name}?v={}", &e.sha256[..16]));
    let data = http::Client::new().get(&url, e.size, |_, _| {})?;
    if data.len() as u64 != e.size || sha256_hex(&data) != e.sha256 {
        return Err(format!("{name}: content does not match the signed asset index"));
    }
    Ok(data)
}
