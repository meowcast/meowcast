//! What the system says about repositories and apps (`Event::Store` lines, listed at the top
//! of appd's daemon.rs), kept as the lists the screens draw.

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Repo {
    pub id: String,
    pub name: String,
    pub url: String,
    /// The fingerprint of the key its apps are signed with.
    pub fingerprint: String,
    pub enabled: bool,
    /// The built-in repository: always on, cannot be removed.
    pub builtin: bool,
    /// ok | waiting | error | off
    pub state: String,
    pub detail: String,
    pub apps: usize,
}

impl Repo {
    /// The words on the right of its row.
    pub fn status(&self) -> String {
        match self.state.as_str() {
            "off" => "Off".into(),
            "error" => "Can't be reached".into(),
            "waiting" => "Checking\u{2026}".into(),
            _ if self.apps == 1 => "1 app".into(),
            _ => format!("{} apps", self.apps),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Perm {
    pub name: String,
    pub sensitive: bool,
    pub required: bool,
    /// granted | denied | undecided, or "-" while the app is not installed.
    pub state: String,
    pub label: String,
    pub reason: String,
    pub extra: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppEntry {
    pub id: String,
    pub repo: String,
    pub name: String,
    pub summary: String,
    pub publisher: String,
    pub version: String,
    pub kib: u32,
    pub tint: u32,
    pub badge: (u32, u32),
    /// available | installed | fetching | running | error
    pub state: String,
    pub detail: String,
    /// Part of the system (this app): no install, no uninstall.
    pub system: bool,
    pub perms: Vec<Perm>,
}

impl AppEntry {
    pub fn installed(&self) -> bool {
        self.state != "available"
    }

    pub fn size(&self) -> String {
        if self.kib >= 1024 { format!("{:.1} MB", self.kib as f32 / 1024.0) } else { format!("{} KB", self.kib) }
    }

    /// The short line on its card and under its name.
    pub fn status(&self) -> String {
        match self.state.as_str() {
            "available" => format!("Get  \u{00b7}  {}", self.size()),
            "fetching" => format!("Downloading {}", self.detail),
            "error" => self.detail.clone(),
            _ if self.system => "Part of the system".into(),
            _ if !self.detail.is_empty() => "An update needs your approval".into(),
            _ => "Installed".into(),
        }
    }

    /// The permissions the user is asked about before an install.
    pub fn sensitive(&self) -> Vec<&Perm> {
        self.perms.iter().filter(|p| p.sensitive).collect()
    }
}

/// What a line changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// The lists changed (or the first full list is complete).
    Changed,
    /// The answer to a request: what, its target, whether it worked, and the text fields.
    Result { what: String, target: String, ok: bool, text: Vec<String> },
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    pub repos: Vec<Repo>,
    /// Sorted by name.
    pub apps: Vec<AppEntry>,
    /// The first full list has arrived.
    pub loaded: bool,
}

fn colour(text: &str) -> u32 {
    u32::from_str_radix(text.trim_start_matches('#'), 16).unwrap_or(0x2a2e32) & 0xff_ffff
}

impl Catalog {
    pub fn app(&self, id: &str) -> Option<&AppEntry> {
        self.apps.iter().find(|a| a.id == id)
    }

    pub fn repo(&self, id: &str) -> Option<&Repo> {
        self.repos.iter().find(|r| r.id == id)
    }

    pub fn repo_name(&self, id: &str) -> &str {
        self.repo(id).map_or("a repository that was removed", |r| r.name.as_str())
    }

    /// One `Event::Store` line.
    pub fn apply(&mut self, line: &str) -> Update {
        let f: Vec<&str> = line.split('\t').collect();
        let field = |i: usize| f.get(i).copied().unwrap_or("").to_string();
        match f[0] {
            "repo" if f.len() >= 10 => {
                let repo = Repo { id: field(1), name: field(2), url: field(3), fingerprint: field(4), enabled: f[5] == "1", builtin: f[6] == "1", state: field(7), detail: field(8), apps: f[9].parse().unwrap_or(0) };
                match self.repos.iter_mut().find(|r| r.id == repo.id) {
                    Some(known) => *known = repo,
                    None => self.repos.push(repo),
                }
                Update::Changed
            }
            "app" if f.len() >= 15 => {
                let perms = f[15..]
                    .iter()
                    .filter_map(|p| {
                        let q: Vec<&str> = p.split('|').collect();
                        (q.len() >= 7).then(|| Perm { name: q[0].into(), sensitive: q[1] == "sensitive", required: q[2] == "1", state: q[3].into(), label: q[4].into(), reason: q[5].into(), extra: q[6].into() })
                    })
                    .collect();
                let app = AppEntry {
                    id: field(1),
                    repo: field(2),
                    name: field(3),
                    summary: field(4),
                    publisher: field(5),
                    version: field(6),
                    kib: f[7].parse().unwrap_or(0),
                    // (f[8] is the launcher's glyph name: the cards here show the initial.)
                    tint: colour(f[9]),
                    badge: (colour(f[10]), colour(f[11])),
                    state: field(12),
                    detail: field(13),
                    system: f[14] == "system",
                    perms,
                };
                self.apps.retain(|a| a.id != app.id);
                let at = self.apps.partition_point(|a| (a.name.to_lowercase(), &a.id) < (app.name.to_lowercase(), &app.id));
                self.apps.insert(at, app);
                Update::Changed
            }
            "gone" => {
                self.apps.retain(|a| a.id != field(1));
                Update::Changed
            }
            "repo-gone" => {
                self.repos.retain(|r| r.id != field(1));
                Update::Changed
            }
            "end" => {
                self.loaded = true;
                Update::Changed
            }
            "result" if f.len() >= 4 => Update::Result { what: field(1), target: field(2), ok: f[3] == "ok", text: f[4..].iter().map(|t| t.to_string()).collect() },
            _ => Update::Nothing,
        }
    }
}

/// A catalogue as a device with one added repository has it: for the tests and for
/// `store ui-preview`.
pub fn sample() -> Catalog {
    let mut c = Catalog::default();
    for line in [
        "repo\tmeow\tmeow store\thttps://store.example.org/\t1a2b 3c4d 5e6f 7081\t1\t1\tok\t\t5",
        "repo\tr12345678\tExample apps\thttps://apps.example.org/tv/\t9f8e 7d6c 5b4a 3921\t1\t0\tok\t\t2",
        "app\tdev.meow.youtube\tmeow\tYouTube\tCast from the YouTube app\tmeow\t0.3.0\t1480\tplay\t#1c1c1c\t#ff4e45\t#c4302b\tinstalled\t\t-\tdisplay|normal|1|granted|Show full-screen pictures||\tinternet|sensitive|1|granted|Connect to youtube.com|Streams video|*.youtube.com\tbackground|sensitive|0|denied|Keep running in the background|Stays in your phone's Cast list|up to 16 MiB of memory",
        "app\tdev.meow.store\tmeow\tStore\tGet apps and choose where they come from\tmeow\t0.1.0\t620\tapps\t#141a2a\t#7c8cff\t#4a4fd8\trunning\t\tsystem\tdisplay|normal|1|granted|Show full-screen pictures||\tmanage-apps|sensitive|1|granted|Install and remove apps|Keeps the list|",
        "app\tdev.meow.cast\tmeow\tCast\tAirPlay, DLNA and Miracast receivers\tmeow\t0.1.0\t2100\tcast\t#121a2c\t#5b9bff\t#2f5fd0\tavailable\t2100 KiB\t-\tdisplay|normal|1|-|Show full-screen pictures||\tlan|sensitive|1|-|Find and talk to devices on your home network|Lets phones find this TV|listens on TCP 7000",
        "app\tdev.meow.inspector\tmeow\tSandbox Inspector\tWhat an app may and may not do\tmeow examples\t0.1.0\t575\tsearch\t#1f3b3a\t#4fb3e8\t#2a72c4\tavailable\t575 KiB\t-\tdisplay|normal|1|-|Show full-screen pictures||\tstorage|normal|1|-|Keep up to 64 KiB of its own data|Counts how often it was opened|64 KiB",
        "app\tdev.meow.sweettv\tmeow\tsweet.tv\tLive TV from your sweet.tv subscription\tmeow\t0.2.1\t2300\tplay\t#24141c\t#ff6aa6\t#c81e6a\tfetching\t40%\t-\tinternet|sensitive|1|granted|Connect to sweet.tv|Streams the channels|*.sweet.tv",
        "app\torg.example.clock\tr12345678\tWall Clock\tA large clock for the living room\tExample\t1.2\t310\tclock\t#202020\t#f3b04c\t#c8791e\tavailable\t310 KiB\t-\tdisplay|normal|1|-|Show full-screen pictures||",
        "app\torg.example.radio\tr12345678\tRadio\tInternet radio stations\tExample\t2.0.1\t940\tmusic\t#182420\t#4ad06a\t#1f8a43\tinstalled\t\t-\tinternet|sensitive|1|granted|Connect to radio.example.org|Plays the stations|radio.example.org",
        "end",
    ] {
        c.apply(line);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_build_the_lists() {
        let mut c = sample();
        assert!(c.loaded);
        assert_eq!(c.repos.len(), 2);
        assert!(c.repos[0].builtin && c.repos[0].enabled && c.repos[0].status() == "5 apps");
        let names: Vec<&str> = c.apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Cast", "Radio", "Sandbox Inspector", "Store", "sweet.tv", "Wall Clock", "YouTube"], "sorted by name, whatever the case");
        let youtube = c.app("dev.meow.youtube").unwrap();
        assert_eq!((youtube.perms.len(), youtube.size().as_str(), youtube.status().as_str()), (3, "1.4 MB", "Installed"));
        assert_eq!(youtube.sensitive().len(), 2);
        assert_eq!(youtube.badge, (0xff4e45, 0xc4302b));
        assert_eq!(c.app("dev.meow.cast").unwrap().status(), "Get  \u{00b7}  2.1 MB");
        assert_eq!(c.app("dev.meow.sweettv").unwrap().status(), "Downloading 40%");
        assert!(c.app("dev.meow.store").unwrap().system);
        assert_eq!(c.repo_name("r12345678"), "Example apps");
        // An app changes in place; one that leaves is gone; so is a repository.
        assert_eq!(c.apply("app\tdev.meow.cast\tmeow\tCast\t\tmeow\t0.1.0\t2100\tcast\t#121a2c\t#5b9bff\t#2f5fd0\tinstalled\t\t-"), Update::Changed);
        assert!(c.app("dev.meow.cast").unwrap().installed() && c.apps.len() == 7);
        c.apply("gone\torg.example.clock");
        c.apply("repo-gone\tr12345678");
        assert!(c.app("org.example.clock").is_none() && c.repos.len() == 1);
        c.apply("repo\tmeow\tmeow store\thttps://x/\tk\t1\t1\terror\tno route\t5");
        assert_eq!(c.repos[0].status(), "Can't be reached");
    }

    #[test]
    fn answers_and_junk() {
        let mut c = Catalog::default();
        assert_eq!(c.apply("result\tinstall\tdev.meow.cast\tok\tinstalled"), Update::Result { what: "install".into(), target: "dev.meow.cast".into(), ok: true, text: vec!["installed".into()] });
        match c.apply("result\tprobe\thttps://apps.example.org/\tok\tExample apps\t9f8e 7d6c 5b4a 3921\t2") {
            Update::Result { ok: true, text, .. } => assert_eq!(text, ["Example apps", "9f8e 7d6c 5b4a 3921", "2"]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.apply("result\tadd\tx\terror\tthe list is full"), Update::Result { ok: false, .. }));
        for junk in ["", "app\ttoo\tshort", "repo", "something\telse", "result"] {
            assert_eq!(c.apply(junk), Update::Nothing, "{junk:?}");
        }
    }
}
