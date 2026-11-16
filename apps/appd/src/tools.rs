//! Publishing tools (run on the build host, in the builder container):
//!   appd keygen KEYFILE                          new store signing key (KEYFILE + KEYFILE.pub)
//!   appd pack --manifest app.json --payload DIR --key KEYFILE --store WWW
//!       tar DIR, fill in payload.sha256/size, validate, sign, write
//!       WWW/apps/<id>/<version_code>/{manifest.json,manifest.sig,payload.tar}
//!   appd index --store WWW                       (re)write WWW/index.json (newest version per app)
//!   appd assets --store WWW --key KEYFILE        sign the asset index: WWW/assets.json (+ .sig)
//!                                                 listing SHA-256 and size of every WWW/assets file;
//!                                                 NAME.preview (a tiny JPEG) is embedded in NAME's
//!                                                 entry as base64 instead of being listed
//!   appd repo --store WWW --key KEYFILE --name NAME   write WWW/repo.json: the repository's name
//!                                                 and public key, which a device shows when
//!                                                 the user adds the store's URL (repos.rs)
//!   appd verify --store WWW --keys DIR           check every listed app and asset like the device does
//!   appd store-check --store URL --keys DIR      the same against a live store, with the device's
//!                                                 client code (keep-alive, https via meow-tls / APPD_TLS)

use crate::bundle;
use crate::manifest::Manifest;
use crate::sign;
use crate::util::{sha256_hex, write_atomic};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn opt(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone()).ok_or_else(|| format!("missing {name}"))
}

pub fn keygen(args: &[String]) -> Result<(), String> {
    let path = PathBuf::from(args.first().ok_or("usage: appd keygen KEYFILE")?);
    if path.exists() {
        return Err(format!("{} exists; refusing to overwrite a signing key", path.display()));
    }
    let (seed, public) = sign::keygen(sign::random32()?);
    write_atomic(&path, format!("{seed}\n").as_bytes(), 0o600).map_err(|e| e.to_string())?;
    let pubp = path.with_extension("pub");
    write_atomic(&pubp, format!("{public}\n").as_bytes(), 0o644).map_err(|e| e.to_string())?;
    println!("key {} -> {} (public {}, id {})", path.display(), pubp.display(), public, sign::key_id(&crate::util::unhex(&public).unwrap()));
    Ok(())
}

pub fn pack(args: &[String]) -> Result<(), String> {
    let manifest_path = opt(args, "--manifest")?;
    let payload = PathBuf::from(opt(args, "--payload")?);
    let key = std::fs::read_to_string(opt(args, "--key")?).map_err(|e| e.to_string())?;
    let www = PathBuf::from(opt(args, "--store")?);
    let tar = bundle::create(&payload)?;
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path).map_err(|e| format!("{manifest_path}: {e}"))?)
        .map_err(|e| format!("{manifest_path}: {e}"))?;
    v["schema"] = 1.into();
    v["payload"] = serde_json::json!({ "sha256": sha256_hex(&tar), "size": tar.len() });
    // Every published change needs a higher version_code (devices refuse a different
    // manifest under a code they already know). Same content as the newest: nothing to do.
    let id = v["id"].as_str().unwrap_or("").to_string();
    let newest = std::fs::read_dir(www.join("apps").join(&id))
        .ok()
        .and_then(|rd| rd.flatten().filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok()).max());
    if let Some(code) = newest {
        let old: serde_json::Value = serde_json::from_slice(&std::fs::read(www.join(format!("apps/{id}/{code}/manifest.json"))).unwrap_or_default()).unwrap_or_default();
        let mut same = v.clone();
        same["version_code"] = old["version_code"].clone();
        if same == old {
            println!("unchanged {id} (code {code})");
            return index(&["--store".into(), www.display().to_string()]);
        }
        if v["version_code"].as_u64().unwrap_or(0) <= code as u64 {
            println!("{id}: content changed, version_code {} -> {}", v["version_code"], code + 1);
            v["version_code"] = (code + 1).into();
        }
    }
    let bytes = serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?;
    let m = Manifest::parse(&bytes).map_err(|e| format!("{manifest_path}: {e}"))?;
    // The device must accept exactly what we publish: extract the payload once here too.
    let check = std::env::temp_dir().join(format!("appd-pack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&check);
    let r = crate::store::install_tar(&tar, &m, &check.join("payload"));
    let _ = std::fs::remove_dir_all(&check);
    r?;
    let sig = sign::sign(&key, &bytes)?;
    let dir = www.join("apps").join(&m.id).join(m.version_code.to_string());
    write_atomic(&dir.join("payload.tar"), &tar, 0o644).map_err(|e| e.to_string())?;
    write_atomic(&dir.join("manifest.json"), &bytes, 0o644).map_err(|e| e.to_string())?;
    write_atomic(&dir.join("manifest.sig"), format!("{sig}\n").as_bytes(), 0o644).map_err(|e| e.to_string())?;
    println!("packed {} {} (code {}): payload {} bytes sha256 {}", m.id, m.version, m.version_code, tar.len(), m.payload.sha256);
    index(&["--store".into(), www.display().to_string()])
}

pub fn index(args: &[String]) -> Result<(), String> {
    let www = PathBuf::from(opt(args, "--store")?);
    let mut newest: BTreeMap<String, u32> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(www.join("apps")) {
        for app in rd.flatten() {
            let id = app.file_name().to_string_lossy().into_owned();
            for ver in std::fs::read_dir(app.path()).map_err(|e| e.to_string())?.flatten() {
                if let Ok(code) = ver.file_name().to_string_lossy().parse::<u32>() {
                    if ver.path().join("manifest.sig").exists() && newest.get(&id).is_none_or(|c| code > *c) {
                        newest.insert(id.clone(), code);
                    }
                }
            }
        }
    }
    let apps: Vec<serde_json::Value> = newest
        .iter()
        .map(|(id, code)| {
            let base = format!("apps/{id}/{code}");
            serde_json::json!({ "id": id, "manifest": format!("{base}/manifest.json"), "signature": format!("{base}/manifest.sig"), "payload": format!("{base}/payload.tar") })
        })
        .collect();
    let idx = serde_json::json!({ "schema": 1, "apps": apps });
    write_atomic(&www.join("index.json"), &serde_json::to_vec_pretty(&idx).unwrap(), 0o644).map_err(|e| e.to_string())?;
    println!("index: {} app(s) in {}", newest.len(), www.join("index.json").display());
    Ok(())
}

pub fn verify(args: &[String]) -> Result<(), String> {
    let www = PathBuf::from(opt(args, "--store")?);
    let trusted = sign::load_trusted(Path::new(&opt(args, "--keys")?))?;
    let idx: serde_json::Value = serde_json::from_slice(&std::fs::read(www.join("index.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    for e in idx["apps"].as_array().ok_or("index: no apps")? {
        let rel = |k: &str| www.join(e[k].as_str().unwrap_or(""));
        let bytes = std::fs::read(rel("manifest")).map_err(|e| e.to_string())?;
        let sig = std::fs::read_to_string(rel("signature")).map_err(|e| e.to_string())?;
        let key = sign::verify(&trusted, &bytes, sig.trim())?;
        let m = Manifest::parse(&bytes)?;
        let tar = std::fs::read(rel("payload")).map_err(|e| e.to_string())?;
        if sha256_hex(&tar) != m.payload.sha256 || tar.len() as u64 != m.payload.size {
            return Err(format!("{}: payload does not match its manifest", m.id));
        }
        println!("ok {} {} key {}", m.id, m.version, key);
    }
    if let Ok(bytes) = std::fs::read(www.join("assets.json")) {
        let sig = std::fs::read_to_string(www.join("assets.sig")).map_err(|e| e.to_string())?;
        let key = sign::verify(&trusted, &bytes, sig.trim())?;
        let idx = crate::assets::Index::parse(&bytes)?;
        for (name, e) in &idx.assets {
            let data = std::fs::read(www.join("assets").join(name)).map_err(|e| format!("{name}: {e}"))?;
            if sha256_hex(&data) != e.sha256 || data.len() as u64 != e.size {
                return Err(format!("asset {name} does not match the signed index"));
            }
            println!("ok asset {name} ({} bytes) key {key}", e.size);
        }
    }
    Ok(())
}

pub fn repo(args: &[String]) -> Result<(), String> {
    let www = PathBuf::from(opt(args, "--store")?);
    let name = opt(args, "--name")?;
    let seed = std::fs::read_to_string(opt(args, "--key")?).map_err(|e| e.to_string())?;
    let seed: [u8; 32] = crate::util::unhex(&seed).and_then(|raw| raw.try_into().ok()).ok_or("key: expected 64 hex digits")?;
    let (_, public) = sign::keygen(seed);
    let bytes = crate::repos::meta_json(&name, &public)?;
    let path = www.join("repo.json");
    if std::fs::read(&path).ok().as_deref() != Some(&bytes[..]) {
        write_atomic(&path, &bytes, 0o644).map_err(|e| e.to_string())?;
    }
    println!("repo: {name:?}, key fingerprint {} in {}", crate::repos::fingerprint(&public), path.display());
    Ok(())
}

use crate::assets::MAX_ASSET_BYTES;

pub fn store_check(args: &[String]) -> Result<(), String> {
    let mut url = opt(args, "--store")?;
    if !url.ends_with('/') {
        url.push('/');
    }
    let trusted = sign::load_trusted(Path::new(&opt(args, "--keys")?))?;
    let t = std::time::Instant::now();
    let mut c = crate::http::Client::new();
    let (_, _, idx) = crate::store::fetch_assets_index(&mut c, &url, &trusted)?;
    println!("asset index: {} asset(s), {} ms", idx.assets.len(), t.elapsed().as_millis());
    let (listings, errors) = crate::store::refresh(&mut c, &url, &trusted)?;
    for l in &listings {
        println!("app {} {} (code {}) signed by {}", l.manifest.id, l.manifest.version, l.manifest.version_code, l.key_id);
    }
    for e in &errors {
        println!("REJECTED {e}");
    }
    println!("refresh: {} request(s) on {} connection(s), {} ms", c.requests, c.connects, t.elapsed().as_millis());
    for (name, e) in &idx.assets {
        let t = std::time::Instant::now();
        crate::store::fetch_asset(&url, name, e)?;
        println!("asset {name}: {} bytes ok{}, {} ms", e.size, if e.preview.is_some() { " (+ preview)" } else { "" }, t.elapsed().as_millis());
    }
    for l in &listings {
        let t = std::time::Instant::now();
        let tar = crate::http::get(&l.payload_url, l.manifest.payload.size, |_, _| {})?;
        if sha256_hex(&tar) != l.manifest.payload.sha256 {
            return Err(format!("{}: payload does not match its signed manifest", l.manifest.id));
        }
        println!("payload {}: {} bytes ok, {} ms", l.manifest.id, tar.len(), t.elapsed().as_millis());
    }
    if errors.is_empty() { Ok(()) } else { Err(format!("{} app(s) rejected", errors.len())) }
}

/// Standard base64 with padding (previews in assets.json).
fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

pub fn assets(args: &[String]) -> Result<(), String> {
    let www = PathBuf::from(opt(args, "--store")?);
    let key = std::fs::read_to_string(opt(args, "--key")?).map_err(|e| e.to_string())?;
    let root = www.join("assets");
    let mut map = BTreeMap::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() { root.clone() } else { root.join(&rel) };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if e.file_type().map_err(|e| e.to_string())?.is_dir() {
                stack.push(path);
                continue;
            }
            if !crate::manifest::valid_rel_path(&path) {
                return Err(format!("asset name not allowed: {path}"));
            }
            if path.ends_with(".preview") {
                continue; // embedded in its asset's entry below
            }
            let data = std::fs::read(e.path()).map_err(|e| e.to_string())?;
            if data.len() as u64 > MAX_ASSET_BYTES {
                return Err(format!("{path}: larger than {MAX_ASSET_BYTES} bytes"));
            }
            let mut entry = serde_json::json!({ "sha256": sha256_hex(&data), "size": data.len() });
            if let Ok(p) = std::fs::read(e.path().with_file_name(format!("{}.preview", e.file_name().to_string_lossy()))) {
                let b64 = base64(&p);
                if b64.len() > crate::assets::MAX_PREVIEW_CHARS {
                    return Err(format!("{path}.preview: {} bytes is too large for a preview", p.len()));
                }
                entry["preview"] = b64.into();
            }
            map.insert(path, entry);
        }
    }
    for (name, e) in &map {
        if let Some(p) = e.get("preview").and_then(|p| p.as_str()) {
            println!("assets: {name} has a {} byte preview", p.len() * 3 / 4);
        }
    }
    let idx = serde_json::to_vec_pretty(&serde_json::json!({ "schema": 1, "assets": map })).unwrap();
    let sig = sign::sign(&key, &idx)?;
    write_atomic(&www.join("assets.json"), &idx, 0o644).map_err(|e| e.to_string())?;
    write_atomic(&www.join("assets.sig"), format!("{sig}\n").as_bytes(), 0o644).map_err(|e| e.to_string())?;
    println!("assets: {} file(s) signed in {}", map.len(), www.join("assets.json").display());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_rfc4648() {
        for (i, o) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::base64(i.as_bytes()), o);
        }
    }
}
