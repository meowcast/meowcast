//! What appd keeps on the flash data partition (/data, JFFS2). Small files only, each
//! written atomically:
//!
//!   /data/appd/config.json                  optional {"store": "http://..."}
//!   /data/appd/installed/<id>/manifest.json the signed manifest, verbatim (re-verified at start)
//!   /data/appd/installed/<id>/manifest.sig
//!   /data/appd/installed/<id>/source.json   {"payload_url": ..., "repo": id} (unsigned; payload is
//!                                           hash-pinned, and the manifest must verify with
//!                                           that repository's key)
//!   /data/appd/repos.json                   the repositories the user added (repos.rs)
//!   /data/appd/installed/<id>/grants.json   the user's decisions
//!   /data/appd/uids.json                    {"<id>": uid}, stable per app, never reused
//!   /data/appd/floors.json                  {"<id>": highest version_code seen} (anti-rollback)
//!   /data/appd/recents                      app ids, most recently opened first (one per line)
//!   /data/appd/volume                       the system volume: "70" or "70 muted"
//!   /data/appdata/<id>.pack                 app storage snapshot (storage.rs)
//!
//! App payloads are never stored here: they are fetched into RAM (/apps) when needed.

use crate::manifest::Manifest;
use crate::sign::{self, Trusted};
use crate::util::write_atomic;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FIRST_UID: u32 = 10000;
pub const LAST_UID: u32 = 19999;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Grants {
    pub granted: Vec<String>,
    pub denied: Vec<String>,
    pub decided_at: u64,
    /// "Run in background" switched off by the user (Settings > Apps). The default, false,
    /// means: on as soon as the "background" permission is granted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background_off: bool,
}

impl Grants {
    pub fn has(&self, p: &str) -> bool {
        self.granted.iter().any(|g| g == p)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Source {
    pub payload_url: String,
    /// The repository the app was installed from (repos.rs); empty: the default one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub store: Option<String>,
}

/// The apps opened last, most recent first. Kept short, and written to flash only when the
/// order really changes (opening the same app again writes nothing).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recents {
    pub ids: Vec<String>,
}

pub const RECENTS_MAX: usize = 8;

impl Recents {
    pub fn parse(text: &str) -> Recents {
        let mut ids: Vec<String> = Vec::new();
        for line in text.lines().map(str::trim).filter(|l| crate::manifest::valid_id(l)) {
            if !ids.iter().any(|i| i == line) && ids.len() < RECENTS_MAX {
                ids.push(line.to_string());
            }
        }
        Recents { ids }
    }
    pub fn text(&self) -> String {
        self.ids.iter().map(|i| format!("{i}\n")).collect()
    }
    /// `id` was opened: it moves to the front. True if the order changed.
    pub fn opened(&mut self, id: &str) -> bool {
        if self.ids.first().is_some_and(|f| f == id) {
            return false;
        }
        self.ids.retain(|i| i != id);
        self.ids.insert(0, id.to_string());
        self.ids.truncate(RECENTS_MAX);
        true
    }
    /// Drop every id `keep` refuses (uninstalled apps). True if something went.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) -> bool {
        let before = self.ids.len();
        self.ids.retain(|i| keep(i));
        self.ids.len() != before
    }
}

/// The system volume as it is saved: the level stays when the sound is muted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    pub level: u8,
    pub muted: bool,
}

impl Default for Volume {
    fn default() -> Self {
        Volume { level: 100, muted: false }
    }
}

impl Volume {
    /// "70" or "70 muted" (the file); anything else is the default.
    pub fn parse(text: &str) -> Volume {
        let mut words = text.split_whitespace();
        match words.next().and_then(|w| w.parse::<u8>().ok()) {
            Some(level) => Volume { level: level.min(100), muted: words.next() == Some("muted") },
            None => Volume::default(),
        }
    }
    pub fn text(&self) -> String {
        format!("{}{}", self.level, if self.muted { " muted" } else { "" })
    }
    /// A `VOLUME` request: "" reads, "N" sets, "+N"/"-N" step, "mute" toggles, "mute on|off"
    /// sets. A new level (and a step, even at the end of the scale) switches the sound back on.
    pub fn apply(self, what: &str, arg: &str) -> Result<Volume, String> {
        let level = self.level as i32;
        let stepped = |by: Option<i32>| by.map(|by| Volume { level: (level + by).clamp(0, 100) as u8, muted: false });
        let new = match (what, arg) {
            ("", _) => Some(self),
            ("mute", "") => Some(Volume { muted: !self.muted, ..self }),
            ("mute", "on") => Some(Volume { muted: true, ..self }),
            ("mute", "off") => Some(Volume { muted: false, ..self }),
            ("mute", _) => None,
            _ => match (what.strip_prefix('+'), what.strip_prefix('-')) {
                (Some(step), _) => stepped(step.parse::<i32>().ok().filter(|s| *s >= 0)),
                (_, Some(step)) => stepped(step.parse::<i32>().ok().filter(|s| *s >= 0).map(|s| -s)),
                _ => what.parse::<i32>().ok().filter(|v| (0..=100).contains(v)).map(|v| Volume { level: v as u8, muted: false }),
            },
        };
        new.ok_or_else(|| format!("volume {what}{}{arg}: expected 0..100, +N, -N, mute, mute on or mute off", if arg.is_empty() { "" } else { " " }))
    }
}

pub struct Installed {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub signature: String,
    pub source: Source,
    pub grants: Grants,
}

pub struct State {
    pub root: PathBuf, // /data/appd
    pub appdata: PathBuf,
    pub volatile: bool,
}

fn read_json<T: for<'a> Deserialize<'a> + Default>(p: &Path) -> T {
    std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<(), String> {
    let b = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    write_atomic(p, &b, 0o600).map_err(|e| format!("{}: {e}", p.display()))
}

impl State {
    pub fn open(data: &Path) -> Result<State, String> {
        let root = data.join("appd");
        let appdata = data.join("appdata");
        for d in [&root, &root.join("installed"), &appdata] {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            std::fs::set_permissions(d, std::os::unix::fs::PermissionsExt::from_mode(0o700)).ok();
        }
        Ok(State { root, appdata, volatile: Path::new("/run/data-volatile").exists() })
    }

    pub fn config(&self) -> Config {
        read_json(&self.root.join("config.json"))
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join("installed").join(id)
    }

    /// All installed apps whose manifest still verifies against the keys of the repository
    /// it was installed from (`keys`: repository id to its keys). Records that fail
    /// (tampered, key or repository removed) are reported and ignored, never launched.
    pub fn load_installed(&self, keys: &BTreeMap<String, std::sync::Arc<Vec<Trusted>>>) -> (Vec<Installed>, Vec<String>) {
        let (mut out, mut errs) = (Vec::new(), Vec::new());
        let Ok(rd) = std::fs::read_dir(self.root.join("installed")) else { return (out, errs) };
        for e in rd.flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            let d = e.path();
            let r = (|| -> Result<Installed, String> {
                let manifest_bytes = std::fs::read(d.join("manifest.json")).map_err(|e| e.to_string())?;
                let signature = std::fs::read_to_string(d.join("manifest.sig")).map_err(|e| e.to_string())?.trim().to_string();
                let source: Source = read_json(&d.join("source.json"));
                let trusted = keys.get(crate::repos::from_record(&source.repo)).ok_or("the repository it came from is no longer on this device")?;
                sign::verify(trusted, &manifest_bytes, &signature)?;
                let manifest = Manifest::parse(&manifest_bytes)?;
                if manifest.id != id {
                    return Err("directory name does not match the manifest id".into());
                }
                Ok(Installed { manifest, manifest_bytes, signature, source, grants: read_json(&d.join("grants.json")) })
            })();
            match r {
                Ok(i) => out.push(i),
                Err(err) => errs.push(format!("{id}: {err}")),
            }
        }
        out.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        (out, errs)
    }

    pub fn floor(&self, id: &str) -> u32 {
        read_json::<BTreeMap<String, u32>>(&self.root.join("floors.json")).get(id).copied().unwrap_or(0)
    }

    pub fn install(&self, m: &Manifest, bytes: &[u8], sig: &str, source: &Source, grants: &Grants) -> Result<(), String> {
        let floor = self.floor(&m.id);
        if m.version_code < floor {
            return Err(format!("version_code {} is older than {} (rollback refused)", m.version_code, floor));
        }
        let d = self.dir(&m.id);
        write_atomic(&d.join("manifest.sig"), sig.as_bytes(), 0o600).map_err(|e| e.to_string())?;
        write_atomic(&d.join("manifest.json"), bytes, 0o600).map_err(|e| e.to_string())?;
        write_json(&d.join("source.json"), source)?;
        write_json(&d.join("grants.json"), grants)?;
        let mut floors: BTreeMap<String, u32> = read_json(&self.root.join("floors.json"));
        if floors.get(&m.id).copied().unwrap_or(0) < m.version_code {
            floors.insert(m.id.clone(), m.version_code);
            write_json(&self.root.join("floors.json"), &floors)?;
        }
        Ok(())
    }

    pub fn set_grants(&self, id: &str, g: &Grants) -> Result<(), String> {
        write_json(&self.dir(id).join("grants.json"), g)
    }

    /// Remove the install record and the app's data. The uid and rollback floor stay.
    pub fn uninstall(&self, id: &str) -> Result<(), String> {
        let _ = std::fs::remove_file(self.snapshot(id));
        std::fs::remove_dir_all(self.dir(id)).map_err(|e| e.to_string())
    }

    pub fn recents(&self) -> Recents {
        Recents::parse(&std::fs::read_to_string(self.root.join("recents")).unwrap_or_default())
    }

    pub fn save_recents(&self, r: &Recents) -> Result<(), String> {
        write_atomic(&self.root.join("recents"), r.text().as_bytes(), 0o600).map_err(|e| format!("recents: {e}"))
    }

    pub fn volume(&self) -> Volume {
        Volume::parse(&std::fs::read_to_string(self.root.join("volume")).unwrap_or_default())
    }

    pub fn save_volume(&self, v: Volume) -> Result<(), String> {
        write_atomic(&self.root.join("volume"), format!("{}\n", v.text()).as_bytes(), 0o600).map_err(|e| format!("volume: {e}"))
    }

    pub fn snapshot(&self, id: &str) -> PathBuf {
        self.appdata.join(format!("{id}.pack"))
    }

    /// Stable uid for `id` (allocated on first use).
    pub fn uid(&self, id: &str) -> Result<u32, String> {
        let p = self.root.join("uids.json");
        let mut map: BTreeMap<String, u32> = read_json(&p);
        if let Some(u) = map.get(id) {
            return Ok(*u);
        }
        let used: Vec<u32> = map.values().copied().collect();
        let uid = (FIRST_UID..=LAST_UID).find(|u| !used.contains(u)).ok_or("no free app uid")?;
        map.insert(id.to_string(), uid);
        write_json(&p, &map)?;
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recents_keep_the_order_of_opening() {
        let mut r = Recents::default();
        assert!(r.opened("dev.meow.youtube"));
        assert!(r.opened("dev.meow.inspector"));
        assert!(!r.opened("dev.meow.inspector"), "the same app again changes nothing (no flash write)");
        assert!(r.opened("dev.meow.youtube"));
        assert_eq!(r.ids, ["dev.meow.youtube", "dev.meow.inspector"]);
        for i in 0..20 {
            r.opened(&format!("dev.meow.app{i}"));
        }
        assert_eq!(r.ids.len(), RECENTS_MAX);
        assert_eq!(r.ids[0], "dev.meow.app19");
        assert!(!r.ids.iter().any(|i| i == "dev.meow.youtube"), "the oldest fall off");
        assert!(r.retain(|id| id != "dev.meow.app19"));
        assert!(!r.retain(|_| true));
        assert_eq!(r.ids[0], "dev.meow.app18");
    }

    #[test]
    fn recents_survive_the_file() {
        let mut r = Recents::default();
        r.opened("dev.meow.a");
        r.opened("dev.meow.b");
        assert_eq!(Recents::parse(&r.text()), r);
        // Junk, duplicates and overlong files are tolerated.
        let noisy = format!("dev.meow.b\n\n../etc/passwd\nNot An Id\ndev.meow.b\ndev.meow.a\n{}", "dev.meow.x\n".repeat(3));
        assert_eq!(Recents::parse(&noisy).ids, ["dev.meow.b", "dev.meow.a", "dev.meow.x"]);
        let long: String = (0..30).map(|i| format!("dev.meow.n{i}\n")).collect();
        assert_eq!(Recents::parse(&long).ids.len(), RECENTS_MAX);
        assert!(Recents::parse("").ids.is_empty());
    }

    #[test]
    fn recents_and_volume_on_disk() {
        let dir = std::env::temp_dir().join(format!("appd-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = State::open(&dir).unwrap();
        assert!(st.recents().ids.is_empty());
        assert_eq!(st.volume(), Volume::default());
        let mut r = st.recents();
        r.opened("dev.meow.youtube");
        st.save_recents(&r).unwrap();
        st.save_volume(Volume { level: 35, muted: true }).unwrap();
        let again = State::open(&dir).unwrap();
        assert_eq!(again.recents().ids, ["dev.meow.youtube"]);
        assert_eq!(again.volume(), Volume { level: 35, muted: true });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn volume_requests() {
        let v = Volume { level: 40, muted: false };
        assert_eq!(v.apply("", ""), Ok(v));
        assert_eq!(v.apply("75", "").unwrap().level, 75);
        assert_eq!(v.apply("+5", "").unwrap().level, 45);
        assert_eq!(v.apply("-50", "").unwrap().level, 0);
        assert_eq!(v.apply("+90", "").unwrap().level, 100);
        let m = v.apply("mute", "").unwrap();
        assert_eq!(m, Volume { level: 40, muted: true });
        assert_eq!(m.apply("mute", ""), Ok(v), "the level comes back");
        assert_eq!(m.apply("mute", "on"), Ok(m));
        assert_eq!(m.apply("mute", "off"), Ok(v));
        assert_eq!(m.apply("+5", ""), Ok(Volume { level: 45, muted: false }), "a step unmutes");
        assert_eq!(m.apply("40", ""), Ok(v), "setting a level unmutes");
        assert_eq!(Volume { level: 100, muted: true }.apply("+5", ""), Ok(Volume { level: 100, muted: false }));
        for bad in [("101", ""), ("loud", ""), ("+x", ""), ("--5", ""), ("mute", "maybe"), ("-", "")] {
            assert!(v.apply(bad.0, bad.1).is_err(), "{bad:?}");
        }
        assert_eq!(Volume::parse("70 muted\n"), Volume { level: 70, muted: true });
        assert_eq!(Volume::parse("70\n"), Volume { level: 70, muted: false });
        assert_eq!(Volume::parse("250"), Volume { level: 100, muted: false });
        assert_eq!(Volume::parse("garbage"), Volume::default());
        assert_eq!(Volume::parse(&Volume { level: 5, muted: true }.text()), Volume { level: 5, muted: true });
    }

    #[test]
    fn the_background_switch_is_on_unless_turned_off() {
        let g: Grants = serde_json::from_str(r#"{"granted":["background"],"denied":[],"decided_at":1}"#).unwrap();
        assert!(!g.background_off, "records of earlier firmware: on");
        assert!(!serde_json::to_string(&g).unwrap().contains("background_off"));
        let off = Grants { background_off: true, ..g };
        let back: Grants = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(back.background_off);
    }
}
