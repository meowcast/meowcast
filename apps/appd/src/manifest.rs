//! App manifest: the signed description of an app. The store signs the exact bytes of
//! manifest.json (ed25519); the manifest pins the payload by SHA-256 and size, so one
//! signature covers the code. Validation is strict: unknown fields, unknown permissions and
//! anything outside the limits below are refused.

use crate::perms;
use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;
pub const MAX_PAYLOAD_BYTES: u64 = 48 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    /// Reverse-DNS id, e.g. "dev.meow.inspector".
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    /// Display version ("0.1.0").
    pub version: String,
    /// Monotonic: appd never installs a lower version_code than it has seen (rollback floor).
    pub version_code: u32,
    /// Executable, relative to the payload root ("bin/inspector").
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<Request>,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub appearance: Appearance,
    pub payload: Payload,
    #[serde(default)]
    pub publisher: String,
    /// A system service instead of an app the user opens: appd installs it by itself, keeps
    /// it out of the launcher and runs it when it is needed. "net" = the network service
    /// (apps/netsvc), which serves the apps that have the "internet" permission;
    /// "image" = the image service (apps/imagesvc), which decodes pictures for any
    /// app that asks, and is fetched and started when one first does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The app's background service (needs the "background" permission): a second, small
    /// instance of `exec` that appd keeps running while the app is not open, without screen,
    /// keys, decoder or sound. See README "Background services".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Background>,
    /// The apps whose "Run in background" switch this app may read and set (needs the
    /// "manage-receivers" permission): the Cast app lists the YouTube app here. appd
    /// answers for no other id, so the signature fixes what the permission reaches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub manages: Vec<String>,
}

/// Apps a manifest may list in "manages".
pub const MANAGES_MAX: usize = 8;
/// Ports per protocol a "lan" request may list.
pub const LAN_MAX_PORTS: usize = 16;

/// The fixed ports a "lan" app listens on. The sandbox refuses to bind any other fixed
/// port; ports the kernel picks (bind to port 0) are always allowed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    #[serde(default)]
    pub tcp: Vec<u16>,
    #[serde(default)]
    pub udp: Vec<u16>,
}

/// How the background instance is started and what it may use.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Background {
    /// Arguments instead of `args` (the instance also sees MEOW_MODE=service).
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "d_bg_mem")]
    pub memory_mb: u32,
    #[serde(default = "d_bg_pids")]
    pub pids: u32,
    #[serde(default = "d_bg_cpu")]
    pub cpu_weight: u32,
}
fn d_bg_mem() -> u32 {
    16
}
fn d_bg_pids() -> u32 {
    8
}
fn d_bg_cpu() -> u32 {
    50
}
/// Limits of a background instance: it stays small next to whatever is on screen.
pub const BACKGROUND_MAX_MB: u32 = 32;
pub const BACKGROUND_MAX_PIDS: u32 = 16;
pub const BACKGROUND_MAX_CPU: u32 = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub name: String,
    /// A required permission that is denied cancels the install; an optional one is left out.
    #[serde(default = "yes")]
    pub required: bool,
    /// Why the app wants it (shown to the user).
    #[serde(default)]
    pub reason: String,
    /// storage only.
    #[serde(default)]
    pub quota_kb: Option<u32>,
    /// internet only: the hosts the app may reach ("example.com", "*.example.com"), or the
    /// single entry "*" for an app that plays whatever address it is handed (a cast
    /// receiver). The network service connects to nothing else for this app, and never to
    /// the home network.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    /// lan only: the fixed ports the app listens on (see `Ports`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Ports>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default = "d_mem")]
    pub memory_mb: u32,
    #[serde(default = "d_tmp")]
    pub tmp_mb: u32,
    #[serde(default = "d_pids")]
    pub pids: u32,
    #[serde(default = "d_cpu")]
    pub cpu_weight: u32,
}
fn d_mem() -> u32 {
    48
}
fn d_tmp() -> u32 {
    4
}
fn d_pids() -> u32 {
    32
}
fn d_cpu() -> u32 {
    100
}
impl Default for Resources {
    fn default() -> Self {
        Resources { memory_mb: d_mem(), tmp_mb: d_tmp(), pids: d_pids(), cpu_weight: d_cpu() }
    }
}

/// How the launcher draws the app's card (built-in line glyphs; no image decoding needed).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Appearance {
    #[serde(default = "d_glyph")]
    pub glyph: String,
    #[serde(default = "d_tint")]
    pub tint: String,
    #[serde(default = "d_badge")]
    pub badge: [String; 2],
    #[serde(default = "d_shape")]
    pub shape: String,
}
fn d_glyph() -> String {
    "gear".into()
}
fn d_tint() -> String {
    "#2a2e32".into()
}
fn d_badge() -> [String; 2] {
    ["#7d848b".into(), "#464b51".into()]
}
fn d_shape() -> String {
    "circle".into()
}
impl Default for Appearance {
    fn default() -> Self {
        Appearance { glyph: d_glyph(), tint: d_tint(), badge: d_badge(), shape: d_shape() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub sha256: String,
    pub size: u64,
}

/// Glyph names the launcher can draw (ui/src/art.rs Glyph).
pub const GLYPHS: &[&str] = &[
    "search", "apps", "edit", "sliders", "volume", "wifi", "device", "power", "globe", "play", "music", "terminal",
    "cast", "photo", "folder", "archive", "headphones", "gear", "sun", "gamepad",
];

fn plain_text(field: &str, s: &str, max: usize, allow_empty: bool) -> Result<(), String> {
    if s.chars().count() > max || (!allow_empty && s.is_empty()) {
        return Err(format!("{field}: length must be {}..{max}", if allow_empty { 0 } else { 1 }));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err(format!("{field}: control characters"));
    }
    Ok(())
}

pub fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    (3..=64).contains(&id.len())
        && parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !p.starts_with('-')
        })
}

/// A relative path with plain components only (no "..", ".", empty or absolute parts).
pub fn valid_rel_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 200
        && !p.starts_with('/')
        && p.split('/').all(|c| {
            !c.is_empty() && c != "." && c != ".." && c.bytes().all(|b| b.is_ascii_graphic() && b != b'\\')
        })
}

fn colour(field: &str, s: &str) -> Result<(), String> {
    let ok = s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if ok { Ok(()) } else { Err(format!("{field}: expected #rrggbb")) }
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err("manifest too large".into());
        }
        let m: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("manifest: {e}"))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!("schema {} (this firmware supports {SCHEMA})", self.schema));
        }
        if !valid_id(&self.id) {
            return Err(format!("id {:?}: expected reverse-DNS [a-z0-9-] parts", self.id));
        }
        plain_text("name", &self.name, 32, false)?;
        plain_text("summary", &self.summary, 64, true)?;
        plain_text("version", &self.version, 32, false)?;
        plain_text("publisher", &self.publisher, 64, true)?;
        if !valid_rel_path(&self.exec) {
            return Err(format!("exec {:?}: expected a relative path inside the payload", self.exec));
        }
        if self.args.len() > 16 || self.args.iter().any(|a| a.len() > 256 || a.contains('\0')) {
            return Err("args: at most 16 arguments of 256 bytes".into());
        }
        let mut seen = Vec::new();
        for r in &self.permissions {
            if !perms::known(&r.name) {
                return Err(format!("permission {:?} is not supported by this firmware", r.name));
            }
            if seen.contains(&r.name.as_str()) {
                return Err(format!("permission {:?} listed twice", r.name));
            }
            seen.push(&r.name);
            plain_text("permission reason", &r.reason, 120, true)?;
            match (r.name.as_str(), r.quota_kb) {
                ("storage", Some(q)) if (1..=perms::STORAGE_MAX_KB).contains(&q) => {}
                ("storage", _) => return Err(format!("storage: quota_kb must be 1..{}", perms::STORAGE_MAX_KB)),
                (_, Some(_)) => return Err(format!("{}: quota_kb only applies to storage", r.name)),
                _ => {}
            }
            match (r.name.as_str(), r.hosts.len()) {
                // Any site: only as the whole list, so that it cannot hide among names.
                ("internet", 1) if r.hosts[0] == perms::ANY_HOST => {}
                ("internet", 1..=perms::INTERNET_MAX_HOSTS) => {
                    if let Some(bad) = r.hosts.iter().find(|h| !perms::valid_host_pattern(h)) {
                        return Err(format!("internet: host {bad:?}: expected example.com or *.example.com (or \"*\" alone for any site)"));
                    }
                }
                ("internet", _) => return Err(format!("internet: hosts must list 1..{} hosts", perms::INTERNET_MAX_HOSTS)),
                (_, 0) => {}
                _ => return Err(format!("{}: hosts only applies to internet", r.name)),
            }
            match (r.name.as_str(), &r.ports) {
                (_, None) => {}
                ("lan", Some(p)) => {
                    // Below 1024 an app could not bind anyway (it has no capabilities).
                    if p.tcp.len() > LAN_MAX_PORTS || p.udp.len() > LAN_MAX_PORTS || p.tcp.iter().chain(&p.udp).any(|port| *port < 1024) {
                        return Err(format!("lan: ports must list at most {LAN_MAX_PORTS} TCP and UDP ports of 1024 and up"));
                    }
                }
                _ => return Err(format!("{}: ports only applies to lan", r.name)),
            }
        }
        match (self.manages.len(), self.request("manage-receivers").is_some()) {
            (0, false) => {}
            (0, true) => return Err("permission \"manage-receivers\" needs a \"manages\" list".into()),
            (_, false) => return Err("manages: needs the \"manage-receivers\" permission".into()),
            (n, true) => {
                if n > MANAGES_MAX || self.manages.iter().any(|id| !valid_id(id) || *id == self.id) {
                    return Err(format!("manages: at most {MANAGES_MAX} ids of other apps"));
                }
            }
        }
        match self.service.as_deref() {
            None | Some("net") => {}
            // It decodes what apps hand it and needs nothing of the system to do so: it
            // runs with no permission at all, and a manifest that asks for one is refused.
            Some("image") if self.permissions.is_empty() => {}
            Some("image") => return Err("service \"image\" takes no permissions".into()),
            Some(other) => return Err(format!("service {other:?} is not supported by this firmware")),
        }
        match (&self.background, self.request("background").is_some()) {
            (Some(_), false) => return Err("background: needs the \"background\" permission".into()),
            (None, true) => return Err("permission \"background\" needs a \"background\" section".into()),
            (Some(_), true) if self.service.is_some() => return Err("background: not for system services".into()),
            (Some(b), true) => {
                if b.args.len() > 16 || b.args.iter().any(|a| a.len() > 256 || a.contains('\0')) {
                    return Err("background.args: at most 16 arguments of 256 bytes".into());
                }
                if !(4..=BACKGROUND_MAX_MB).contains(&b.memory_mb) {
                    return Err(format!("background.memory_mb must be 4..{BACKGROUND_MAX_MB}"));
                }
                if !(1..=BACKGROUND_MAX_PIDS).contains(&b.pids) {
                    return Err(format!("background.pids must be 1..{BACKGROUND_MAX_PIDS}"));
                }
                if !(1..=BACKGROUND_MAX_CPU).contains(&b.cpu_weight) {
                    return Err(format!("background.cpu_weight must be 1..{BACKGROUND_MAX_CPU}"));
                }
            }
            (None, false) => {}
        }
        let res = &self.resources;
        if !(8..=128).contains(&res.memory_mb) {
            return Err("resources.memory_mb must be 8..128".into());
        }
        if !(1..=32).contains(&res.tmp_mb) {
            return Err("resources.tmp_mb must be 1..32".into());
        }
        if !(1..=64).contains(&res.pids) {
            return Err("resources.pids must be 1..64".into());
        }
        if !(1..=1000).contains(&res.cpu_weight) {
            return Err("resources.cpu_weight must be 1..1000".into());
        }
        let ap = &self.appearance;
        if !GLYPHS.contains(&ap.glyph.as_str()) {
            return Err(format!("appearance.glyph {:?}: one of {}", ap.glyph, GLYPHS.join(", ")));
        }
        colour("appearance.tint", &ap.tint)?;
        colour("appearance.badge[0]", &ap.badge[0])?;
        colour("appearance.badge[1]", &ap.badge[1])?;
        if ap.shape != "circle" && ap.shape != "square" {
            return Err("appearance.shape: circle or square".into());
        }
        if self.payload.sha256.len() != 64 || !self.payload.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            return Err("payload.sha256: 64 lowercase hex digits".into());
        }
        if self.payload.size == 0 || self.payload.size > MAX_PAYLOAD_BYTES {
            return Err(format!("payload.size must be 1..{MAX_PAYLOAD_BYTES}"));
        }
        Ok(())
    }

    pub fn request(&self, name: &str) -> Option<&Request> {
        self.permissions.iter().find(|r| r.name == name)
    }

    /// The hosts of the "internet" request, as the network service takes them.
    pub fn internet_hosts(&self) -> String {
        self.request("internet").map(|r| r.hosts.join(",")).unwrap_or_default()
    }

    pub fn quota_kb(&self) -> u32 {
        self.request("storage").and_then(|r| r.quota_kb).unwrap_or(0)
    }

    /// The fixed ports of the "lan" request (none listed: none).
    pub fn lan_ports(&self) -> Ports {
        self.request("lan").and_then(|r| r.ports.clone()).unwrap_or_default()
    }

    /// Whether this app, holding "manage-receivers" or not (`granted`), may read and set
    /// the background switch of app `target`.
    pub fn may_manage(&self, granted: bool, target: &str) -> bool {
        granted && self.manages.iter().any(|id| id == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(extra: &str, permissions: &str) -> Result<Manifest, String> {
        let text = format!(
            r#"{{"schema":1,"id":"dev.meow.test","name":"Test","version":"1","version_code":1,"exec":"bin/test",
                "permissions":[{permissions}],"payload":{{"sha256":"{}","size":1}}{extra}}}"#,
            "0".repeat(64)
        );
        Manifest::parse(text.as_bytes())
    }

    #[test]
    fn internet_lists_its_hosts() {
        let m = manifest("", r#"{"name":"internet","hosts":["*.youtube.com","googlevideo.com"]}"#).unwrap();
        assert_eq!(m.internet_hosts(), "*.youtube.com,googlevideo.com");
        assert!(manifest("", r#"{"name":"internet"}"#).unwrap_err().contains("hosts must list"));
        for bad in ["*.", "*.com.", "localhost", "192.168.1.1", "a..b", "UPPER.example", "*.*.example.com", "x.example/path", "-a.example"] {
            let r = manifest("", &format!(r#"{{"name":"internet","hosts":["{bad}"]}}"#));
            assert!(r.is_err(), "{bad} must be refused");
        }
        assert!(manifest("", r#"{"name":"network","hosts":["a.example"]}"#).unwrap_err().contains("only applies to internet"));
        // Any site: "*" alone, and said so in the consent dialog.
        let any = manifest("", r#"{"name":"internet","hosts":["*"]}"#).unwrap();
        assert_eq!(any.internet_hosts(), "*");
        assert!(manifest("", r#"{"name":"internet","hosts":["*","a.example"]}"#).is_err());
        assert!(manifest("", r#"{"name":"internet","hosts":["a.example","*"]}"#).is_err());
        assert_eq!(perms::hosts_label(&["*".into()]), "any site on the internet");
        assert_eq!(perms::hosts_label(&["*.youtube.com".into(), "youtube.com".into(), "*.googlevideo.com".into()]), "youtube.com, googlevideo.com");
    }

    #[test]
    fn services_are_known_or_refused() {
        assert_eq!(manifest(r#","service":"net""#, r#"{"name":"network"}"#).unwrap().service.as_deref(), Some("net"));
        assert!(manifest(r#","service":"root-shell""#, "").unwrap_err().contains("not supported"));
        // The image service decodes untrusted pictures: it may not ask for anything.
        assert_eq!(manifest(r#","service":"image""#, "").unwrap().service.as_deref(), Some("image"));
        assert!(manifest(r#","service":"image""#, r#"{"name":"network"}"#).unwrap_err().contains("takes no permissions"));
        assert!(manifest(r#","service":"image""#, r#"{"name":"storage","quota_kb":64}"#).unwrap_err().contains("takes no permissions"));
        assert!(manifest("", "").unwrap().service.is_none());
        // An app for a newer firmware (a permission this one does not know) is refused whole.
        assert!(manifest("", r#"{"name":"teleport"}"#).is_err());
    }

    #[test]
    fn background_needs_its_permission_and_stays_small() {
        let perm = r#"{"name":"background","required":false,"reason":"Is ready for a cast"}"#;
        let m = manifest(r#","background":{}"#, perm).unwrap();
        let b = m.background.unwrap();
        assert_eq!((b.memory_mb, b.pids, b.cpu_weight, b.args.len()), (16, 8, 50, 0));
        let m = manifest(r#","background":{"args":["--service"],"memory_mb":24,"pids":12,"cpu_weight":80}"#, perm).unwrap();
        assert_eq!(m.background.unwrap().args, ["--service"]);
        assert_eq!(perms::level("background", 0), perms::Level::Sensitive);
        assert!(manifest(r#","background":{}"#, "").unwrap_err().contains("needs the"));
        assert!(manifest("", perm).unwrap_err().contains("needs a"));
        assert!(manifest(r#","background":{"memory_mb":64}"#, perm).unwrap_err().contains("memory_mb"));
        assert!(manifest(r#","background":{"pids":40}"#, perm).unwrap_err().contains("pids"));
        assert!(manifest(r#","background":{"cpu_weight":500}"#, perm).unwrap_err().contains("cpu_weight"));
        assert!(manifest(r#","background":{"display":true}"#, perm).is_err());
        assert!(manifest(r#","service":"net","background":{}"#, &format!(r#"{{"name":"network"}},{perm}"#)).unwrap_err().contains("system services"));
    }

    #[test]
    fn managing_other_apps_needs_the_permission_and_a_signed_list() {
        let perm = r#"{"name":"manage-receivers","required":false}"#;
        let m = manifest(r#","manages":["dev.meow.youtube"]"#, perm).unwrap();
        assert_eq!(perms::level("manage-receivers", 0), perms::Level::Sensitive);
        // Only with the grant, and only for what the manifest names.
        assert!(m.may_manage(true, "dev.meow.youtube"));
        assert!(!m.may_manage(false, "dev.meow.youtube"));
        assert!(!m.may_manage(true, "dev.meow.inspector") && !m.may_manage(true, "") && !m.may_manage(true, "dev.meow.test"));
        assert!(!manifest("", "").unwrap().may_manage(true, "dev.meow.youtube"));
        assert!(manifest(r#","manages":["dev.meow.youtube"]"#, "").unwrap_err().contains("needs the"));
        assert!(manifest("", perm).unwrap_err().contains("needs a"));
        assert!(manifest(r#","manages":["dev.meow.test"]"#, perm).unwrap_err().contains("other apps"), "not itself");
        assert!(manifest(r#","manages":["*"]"#, perm).is_err());
        let many = (0..9).map(|i| format!("\"dev.meow.a{i}\"")).collect::<Vec<_>>().join(",");
        assert!(manifest(&format!(r#","manages":[{many}]"#), perm).is_err());
    }

    #[test]
    fn lan_lists_the_ports_it_listens_on() {
        let m = manifest("", r#"{"name":"lan","ports":{"tcp":[7000,49200],"udp":[5353]}}"#).unwrap();
        assert_eq!(m.lan_ports(), Ports { tcp: vec![7000, 49200], udp: vec![5353] });
        assert_eq!(manifest("", r#"{"name":"lan"}"#).unwrap().lan_ports(), Ports::default());
        assert!(manifest("", r#"{"name":"lan","ports":{"tcp":[80]}}"#).unwrap_err().contains("1024"));
        assert!(manifest("", r#"{"name":"lan","ports":{"sctp":[5000]}}"#).is_err());
        assert!(manifest("", r#"{"name":"network","ports":{"tcp":[7000]}}"#).unwrap_err().contains("only applies to lan"));
        let many: Vec<String> = (2000..2017).map(|p| p.to_string()).collect();
        assert!(manifest("", &format!(r#"{{"name":"lan","ports":{{"udp":[{}]}}}}"#, many.join(","))).is_err());
    }
}
