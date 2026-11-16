//! Repositories: the places apps come from.
//!
//! The **default repository** is the meow store on Cloudflare. It is built in: its URL comes
//! from the firmware (daemon.rs, STORE_CONF) and its keys from /etc/appd/keys in the read-only
//! initramfs. It cannot be removed or switched off, and only it may carry system services,
//! the signed asset index and apps with the "manage-apps" permission (the Store app).
//!
//! The user can add **other repositories** in the Store app (or `appctl repo add URL`). One
//! is the same static file tree as the default store (store.rs) plus one more file:
//!
//!   repo.json     {"schema":1,"name":"Example apps","key":"<64 hex: ed25519 public key>"}
//!
//! Trust is on first use: adding a repository shows its name and the fingerprint of that key,
//! and the user confirms. The key is then pinned in /data/appd/repos.json; from then on every
//! manifest of the repository must be signed with it, and a different key in repo.json later
//! changes nothing. So a repository's host (or the network) can withhold apps, but cannot
//! publish under a repository the user added.
//!
//! What another repository cannot do, whatever it signs (`allowed`, and daemon.rs):
//!   - ship a system service or an app with "manage-apps";
//!   - replace an app of the default repository or of a repository added before it: an app
//!     id belongs to the first repository that lists it, and an installed app is only ever
//!     updated from the repository it was installed from (its key signs the update);
//!   - skip the consent dialog: its apps ask like any other.
//!
//! http:// is accepted only for private addresses (a store on the desk); everything else must
//! be https://, checked by meow-tls against the firmware's pinned root CAs.

use crate::http;
use crate::manifest::Manifest;
use crate::sign::{self, Trusted};
use crate::util::{sha256_hex, unhex, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The id of the built-in repository (never in repos.json).
pub const DEFAULT_ID: &str = "meow";
pub const DEFAULT_NAME: &str = "meow store";
/// Repositories the user may add.
pub const MAX_REPOS: usize = 8;
const MAX_META_BYTES: u64 = 4096;
const MAX_URL: usize = 200;

/// A repository the user added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    /// Stable short id derived from the URL ("r1a2b3c4d").
    pub id: String,
    /// Base URL, ending in '/'.
    pub url: String,
    /// The name its repo.json gave when it was added.
    pub name: String,
    /// The pinned ed25519 public key, hex.
    pub key: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    repos: Vec<Repo>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    schema: u32,
    name: String,
    key: String,
}

/// What a URL turned out to be (`probe`): shown to the user before the repository is added.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    pub url: String,
    pub name: String,
    pub key: String,
    pub apps: usize,
}

/// The repository ids as they are written in an installed app's source.json: the default
/// repository is the empty string there, so records of earlier firmware need no change.
pub fn from_record(repo: &str) -> &str {
    if repo.is_empty() { DEFAULT_ID } else { repo }
}

pub fn to_record(repo: &str) -> String {
    if repo == DEFAULT_ID { String::new() } else { repo.to_string() }
}

pub fn repo_id(url: &str) -> String {
    format!("r{}", &sha256_hex(url.as_bytes())[..8])
}

/// The key's fingerprint as people compare it: "1a2b 3c4d 5e6f 7081".
pub fn fingerprint(key_hex: &str) -> String {
    spaced(&unhex(key_hex).map(|raw| sign::key_id(&raw)).unwrap_or_default())
}

/// A key id (sign::key_id) in groups of four.
pub fn spaced(key_id: &str) -> String {
    key_id.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

fn private_host(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_private() || ip.is_loopback() || ip.is_link_local())
}

/// What the user typed, as a repository base URL: "example.com/apps" becomes
/// "https://example.com/apps/". A link to the index or to repo.json is accepted too.
pub fn clean_url(input: &str) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() || text.len() > MAX_URL {
        return Err(format!("the address must be 1 to {MAX_URL} characters"));
    }
    let lower = text.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("https://") || lower.starts_with("http://") { text.to_string() } else { format!("https://{text}") };
    let url = http::parse(&with_scheme).map_err(|_| "this is not a web address".to_string())?;
    if url.path.contains(['?', '#']) {
        return Err("the address must not contain ? or #".into());
    }
    let host = url.host.to_ascii_lowercase();
    if !url.tls && !private_host(&host) {
        return Err("the address must start with https://".into());
    }
    if !host.contains('.') && host != "localhost" {
        return Err("this is not a web address".into());
    }
    let mut path = url.path.clone();
    for file in ["index.json", "repo.json"] {
        if let Some(dir) = path.strip_suffix(file).filter(|d| d.ends_with('/')) {
            path = dir.to_string();
        }
    }
    if !path.ends_with('/') {
        path.push('/');
    }
    let default_port = if url.tls { 443 } else { 80 };
    let port = if url.port == default_port { String::new() } else { format!(":{}", url.port) };
    Ok(format!("{}://{host}{port}{path}", if url.tls { "https" } else { "http" }))
}

fn parse_meta(bytes: &[u8]) -> Result<(String, String), String> {
    let meta: Meta = serde_json::from_slice(bytes).map_err(|e| format!("repo.json: {e}"))?;
    if meta.schema != 1 {
        return Err(format!("repo.json: schema {}", meta.schema));
    }
    let name = meta.name.trim();
    if name.is_empty() || name.chars().count() > 40 || name.chars().any(|c| c.is_control()) {
        return Err("repo.json: name must be 1 to 40 characters".into());
    }
    let key = meta.key.trim().to_ascii_lowercase();
    public_key(&key).map_err(|e| format!("repo.json: key: {e}"))?;
    Ok((name.to_string(), key))
}

fn public_key(hex: &str) -> Result<ed25519_compact::PublicKey, String> {
    let raw = unhex(hex).filter(|raw| raw.len() == 32).ok_or("expected 64 hex digits")?;
    ed25519_compact::PublicKey::from_slice(&raw).map_err(|e| e.to_string())
}

/// repo.json for a store tree (`appd repo`, on the publishing machine).
pub fn meta_json(name: &str, key_hex: &str) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({ "schema": 1, "name": name, "key": key_hex.trim() })).map_err(|e| e.to_string())?;
    parse_meta(&bytes)?;
    Ok(bytes)
}

/// Fetch repo.json and the app index of `url` (already through `clean_url`). Nothing is
/// trusted or stored here: the result is what the user is asked about.
pub fn probe(url: &str) -> Result<Probe, String> {
    let mut client = http::Client::new();
    let meta = client.get(&http::join(url, "repo.json"), MAX_META_BYTES, |_, _| {}).map_err(|e| format!("no app repository at this address ({e})"))?;
    let (name, key) = parse_meta(&meta)?;
    let index = client.get(&http::join(url, "index.json"), 256 * 1024, |_, _| {}).map_err(|e| format!("the repository has no app list ({e})"))?;
    let apps = crate::store::index_len(&index)?;
    Ok(Probe { url: url.to_string(), name, key, apps })
}

impl Repo {
    pub fn from_probe(probe: &Probe) -> Repo {
        Repo { id: repo_id(&probe.url), url: probe.url.clone(), name: probe.name.clone(), key: probe.key.clone(), enabled: true }
    }

    /// The one key this repository's manifests must be signed with.
    pub fn trusted(&self) -> Result<Vec<Trusted>, String> {
        let key = public_key(&self.key)?;
        Ok(vec![Trusted { id: sign::key_id(key.as_ref()), key, file: self.url.clone() }])
    }

    fn valid(&self) -> bool {
        self.id == repo_id(&self.url) && clean_url(&self.url).is_ok_and(|u| u == self.url) && public_key(&self.key).is_ok() && !self.name.is_empty() && !self.name.contains(['\t', '\n'])
    }
}

/// Whether a repository other than the default may list `m` (module comment).
pub fn allowed(m: &Manifest) -> Result<(), String> {
    if m.service.is_some() {
        return Err("system services come only from the meow store".into());
    }
    if m.request("manage-apps").is_some() {
        return Err("the \"manage-apps\" permission is only for apps of the meow store".into());
    }
    Ok(())
}

/// The repositories the user added (/data/appd/repos.json). Entries that do not hold
/// together (edited by hand, damaged) are left out and reported.
pub fn load(root: &Path) -> (Vec<Repo>, Vec<String>) {
    let file: File = std::fs::read(root.join("repos.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let (mut repos, mut errors): (Vec<Repo>, Vec<String>) = (Vec::new(), Vec::new());
    for repo in file.repos.into_iter().take(MAX_REPOS) {
        if repo.valid() && !repos.iter().any(|r| r.id == repo.id) {
            repos.push(repo);
        } else {
            errors.push(format!("{} ({})", repo.url, repo.id));
        }
    }
    (repos, errors)
}

pub fn save(root: &Path, repos: &[Repo]) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&File { repos: repos.to_vec() }).map_err(|e| e.to_string())?;
    write_atomic(&root.join("repos.json"), &bytes, 0o600).map_err(|e| format!("repos.json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_people_type_become_base_urls() {
        for (typed, url) in [
            ("example.com/apps", "https://example.com/apps/"),
            ("  https://Example.COM  ", "https://example.com/"),
            ("https://example.com/store/index.json", "https://example.com/store/"),
            ("https://example.com/store/repo.json", "https://example.com/store/"),
            ("https://example.com:8443/a/", "https://example.com:8443/a/"),
            ("https://example.com:443/a", "https://example.com/a/"),
            ("http://192.168.2.1:8002", "http://192.168.2.1:8002/"),
            ("http://localhost:8000/", "http://localhost:8000/"),
        ] {
            assert_eq!(clean_url(typed).as_deref(), Ok(url), "{typed}");
            assert_eq!(clean_url(url).as_deref(), Ok(url), "cleaning twice changes nothing");
        }
        for bad in ["", "http://example.com/", "http://8.8.8.8/", "ftp://example.com/", "https://", "https://exa mple.com/", "https://example.com/?x=1", "https://example.com/#a", "store", "https://user@example.com/", &"a".repeat(300)] {
            assert!(clean_url(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(repo_id("https://example.com/"), repo_id("https://example.com/"));
        assert_ne!(repo_id("https://example.com/"), repo_id("https://example.com/b/"));
        assert!(crate::manifest::valid_id(&format!("x.{}", repo_id("https://example.com/"))), "an id is plain lower-case text");
    }

    #[test]
    fn repo_json_names_the_key() {
        let (_, public) = sign::keygen([7; 32]);
        let bytes = meta_json("Example apps", &public).unwrap();
        assert_eq!(parse_meta(&bytes).unwrap(), ("Example apps".to_string(), public.clone()));
        assert_eq!(fingerprint(&public).len(), 19, "four groups of four");
        assert!(meta_json("", &public).is_err());
        assert!(meta_json(&"n".repeat(41), &public).is_err());
        assert!(meta_json("Example", "abcd").is_err());
        assert!(parse_meta(br#"{"schema":2,"name":"x","key":""}"#).is_err());
        assert!(parse_meta(format!(r#"{{"schema":1,"name":"x","key":"{public}","extra":1}}"#).as_bytes()).is_err());
    }

    #[test]
    fn a_repository_signs_with_its_own_key_only() {
        let (seed, public) = sign::keygen([9; 32]);
        let (other_seed, _) = sign::keygen([10; 32]);
        let repo = Repo::from_probe(&Probe { url: "https://example.com/".into(), name: "Example".into(), key: public, apps: 0 });
        assert!(repo.valid());
        let trusted = repo.trusted().unwrap();
        let message = b"manifest bytes";
        assert!(sign::verify(&trusted, message, &sign::sign(&seed, message).unwrap()).is_ok());
        assert!(sign::verify(&trusted, message, &sign::sign(&other_seed, message).unwrap()).is_err());
    }

    #[test]
    fn other_repositories_cannot_ship_system_parts() {
        let manifest = |extra: &str, permissions: &str| {
            let text = format!(
                r#"{{"schema":1,"id":"com.example.app","name":"App","version":"1","version_code":1,"exec":"bin/app",
                    "permissions":[{permissions}],"payload":{{"sha256":"{}","size":1}}{extra}}}"#,
                "0".repeat(64)
            );
            Manifest::parse(text.as_bytes()).unwrap()
        };
        assert!(allowed(&manifest("", r#"{"name":"display"},{"name":"internet","hosts":["example.com"]}"#)).is_ok());
        assert!(allowed(&manifest(r#","service":"net""#, r#"{"name":"network"}"#)).is_err());
        assert!(allowed(&manifest(r#","service":"image""#, "")).is_err());
        assert!(allowed(&manifest("", r#"{"name":"manage-apps"}"#)).is_err());
    }

    #[test]
    fn the_list_survives_the_file_and_drops_what_does_not_hold() {
        let dir = std::env::temp_dir().join(format!("appd-repos-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).0.is_empty());
        let (_, public) = sign::keygen([3; 32]);
        let good = Repo::from_probe(&Probe { url: "https://example.com/".into(), name: "Example".into(), key: public.clone(), apps: 2 });
        let off = Repo { enabled: false, ..Repo::from_probe(&Probe { url: "https://example.org/a/".into(), name: "Other".into(), key: public.clone(), apps: 0 }) };
        // An id that does not belong to the URL, and a key that is none: both are dropped.
        let moved = Repo { url: "https://evil.example/".into(), ..good.clone() };
        let no_key = Repo { key: "zz".into(), ..Repo::from_probe(&Probe { url: "https://example.net/".into(), name: "N".into(), key: public, apps: 0 }) };
        save(&dir, &[good.clone(), off.clone(), moved, no_key]).unwrap();
        let (repos, errors) = load(&dir);
        assert_eq!(repos, [good, off]);
        assert_eq!(errors.len(), 2);
        assert_eq!((from_record(""), from_record("r12345678")), (DEFAULT_ID, "r12345678"));
        assert_eq!((to_record(DEFAULT_ID), to_record("r12345678")), (String::new(), "r12345678".to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
