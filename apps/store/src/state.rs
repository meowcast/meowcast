//! Where the user is in the Store and what a key does there. No drawing and no system calls:
//! a key gives back the requests to send (`Action`), so the whole of it runs in tests.

use std::time::{Duration, Instant};

use meow_app::Key;

use crate::model::{AppEntry, Catalog};

pub const COLUMNS: usize = 3;
/// Rows of app cards on the screen at once.
pub const ROWS_SHOWN: usize = 3;
const URL_MAX: usize = 200;
const NOTICE_FOR: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tab {
    All,
    Installed,
    Sources,
}

pub const TABS: [(Tab, &str); 3] = [(Tab::All, "All apps"), (Tab::Installed, "Installed"), (Tab::Sources, "Repositories")];

/// The on-screen keyboard of the address field. "Del" and "OK" act; the rest is typed.
pub const KEYS: [[&str; 10]; 5] = [
    ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
    ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
    ["a", "s", "d", "f", "g", "h", "j", "k", "l", "-"],
    ["z", "x", "c", "v", "b", "n", "m", ".", "/", ":"],
    ["_", "~", "%", ".com", ".org", ".dev", ".io", "www.", "Del", "OK"],
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Button {
    Install,
    Open,
    Uninstall,
}

impl Button {
    pub fn label(self) -> &'static str {
        match self {
            Button::Install => "Install",
            Button::Open => "Open",
            Button::Uninstall => "Uninstall",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Layer {
    Browse,
    Detail { id: String, button: usize },
    /// "Install X?" with its sensitive permissions: Allow, Don't allow, Cancel.
    Consent { id: String, button: usize },
    Uninstall { id: String, button: usize },
    RepoMenu { repo: String, button: usize },
    RemoveRepo { repo: String, button: usize },
    AddUrl { text: String, row: usize, col: usize, busy: bool, error: String },
    /// What was found at the address: Add, Cancel.
    ConfirmAdd { url: String, name: String, fingerprint: String, apps: usize, button: usize },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A store request (`System::store`).
    Send(Vec<String>),
    Exit,
}

fn send(fields: &[&str]) -> Vec<Action> {
    vec![Action::Send(fields.iter().map(|f| f.to_string()).collect())]
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub tab: Tab,
    /// The row of tabs has the focus (otherwise the list under it).
    pub in_tabs: bool,
    pub app_sel: usize,
    /// The first row of cards on screen.
    pub scroll: usize,
    pub source_sel: usize,
    pub layer: Layer,
    pub notice: Option<(String, Instant)>,
    /// This app's own id: it offers no buttons for itself.
    pub own_id: String,
}

impl State {
    pub fn new(own_id: &str) -> State {
        State { tab: Tab::All, in_tabs: false, app_sel: 0, scroll: 0, source_sel: 0, layer: Layer::Browse, notice: None, own_id: own_id.to_string() }
    }

    /// The apps of the tab that is open.
    pub fn shown<'a>(&self, catalog: &'a Catalog) -> Vec<&'a AppEntry> {
        catalog.apps.iter().filter(|a| self.tab != Tab::Installed || a.installed()).collect()
    }

    pub fn buttons(&self, app: &AppEntry) -> Vec<Button> {
        match (app.installed(), app.id == self.own_id, app.system) {
            (false, _, _) => vec![Button::Install],
            (true, true, _) => Vec::new(),
            (true, false, true) => vec![Button::Open],
            (true, false, false) => vec![Button::Open, Button::Uninstall],
        }
    }

    /// The buttons of a repository's panel.
    pub fn repo_buttons(catalog: &Catalog, repo: &str) -> Vec<&'static str> {
        match catalog.repo(repo) {
            Some(r) if r.builtin => vec!["Check now", "Close"],
            Some(r) if r.enabled => vec!["Switch off", "Remove", "Close"],
            _ => vec!["Switch on", "Remove", "Close"],
        }
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now() + NOTICE_FOR));
    }

    /// Drop a notice whose time is up. True if one went.
    pub fn expire(&mut self, now: Instant) -> bool {
        let over = self.notice.as_ref().is_some_and(|n| now >= n.1);
        if over {
            self.notice = None;
        }
        over
    }

    /// After the lists changed: keep every selection on something that exists.
    pub fn settle(&mut self, catalog: &Catalog) {
        let count = self.shown(catalog).len();
        self.app_sel = self.app_sel.min(count.saturating_sub(1));
        // (Before the first list has arrived there is nothing to move the focus away from.)
        if count == 0 && self.tab != Tab::Sources && catalog.loaded {
            self.in_tabs = true;
        }
        let row = self.app_sel / COLUMNS;
        self.scroll = self.scroll.clamp(row.saturating_sub(ROWS_SHOWN - 1), row);
        self.source_sel = self.source_sel.min(catalog.repos.len());
        let gone = match &self.layer {
            Layer::Detail { id, .. } | Layer::Consent { id, .. } | Layer::Uninstall { id, .. } => catalog.app(id).is_none(),
            Layer::RepoMenu { repo, .. } | Layer::RemoveRepo { repo, .. } => catalog.repo(repo).is_none(),
            _ => false,
        };
        if gone {
            self.layer = Layer::Browse;
        }
        if let Layer::Detail { id, button } = &mut self.layer {
            let buttons: usize = catalog.app(id).map_or(0, |a| match (a.installed(), *id == self.own_id, a.system) {
                (false, _, _) | (true, false, true) => 1,
                (true, true, _) => 0,
                (true, false, false) => 2,
            });
            *button = (*button).min(buttons.saturating_sub(1));
        }
    }

    pub fn key(&mut self, key: &Key, catalog: &Catalog) -> Vec<Action> {
        let actions = match self.layer.clone() {
            Layer::Browse => self.browse_key(key, catalog),
            Layer::Detail { id, button } => self.detail_key(key, catalog, &id, button),
            Layer::Consent { id, button } => match choose(key, button, 3) {
                Choice::Move(button) => self.put(Layer::Consent { id, button }),
                Choice::Pick(2) | Choice::Back => self.put(Layer::Detail { id, button: 0 }),
                Choice::Pick(picked) => {
                    self.layer = Layer::Detail { id: id.clone(), button: 0 };
                    send(&["install", &id, if picked == 0 { "allow" } else { "deny" }])
                }
                Choice::None => Vec::new(),
            },
            Layer::Uninstall { id, button } => match choose(key, button, 2) {
                Choice::Move(button) => self.put(Layer::Uninstall { id, button }),
                Choice::Pick(0) => {
                    self.layer = Layer::Detail { id: id.clone(), button: 0 };
                    send(&["uninstall", &id])
                }
                Choice::Pick(_) | Choice::Back => self.put(Layer::Detail { id, button: 0 }),
                Choice::None => Vec::new(),
            },
            Layer::RepoMenu { repo, button } => {
                let buttons = State::repo_buttons(catalog, &repo);
                match choose(key, button, buttons.len()) {
                    Choice::Move(button) => self.put(Layer::RepoMenu { repo, button }),
                    Choice::Back => self.put(Layer::Browse),
                    Choice::Pick(picked) => match buttons[picked] {
                        "Check now" => {
                            self.layer = Layer::Browse;
                            self.say("Looking for new apps\u{2026}");
                            send(&["refresh"])
                        }
                        "Switch off" => send(&["enable", &repo, "0"]),
                        "Switch on" => send(&["enable", &repo, "1"]),
                        "Remove" => self.put(Layer::RemoveRepo { repo, button: 1 }),
                        _ => self.put(Layer::Browse),
                    },
                    Choice::None => Vec::new(),
                }
            }
            Layer::RemoveRepo { repo, button } => match choose(key, button, 2) {
                Choice::Move(button) => self.put(Layer::RemoveRepo { repo, button }),
                Choice::Pick(0) => {
                    self.layer = Layer::Browse;
                    send(&["remove", &repo])
                }
                Choice::Pick(_) | Choice::Back => self.put(Layer::RepoMenu { repo, button: 0 }),
                Choice::None => Vec::new(),
            },
            Layer::AddUrl { text, row, col, busy, error } => self.url_key(key, text, row, col, busy, error),
            Layer::ConfirmAdd { url, name, fingerprint, apps, button } => match choose(key, button, 2) {
                Choice::Move(button) => self.put(Layer::ConfirmAdd { url, name, fingerprint, apps, button }),
                Choice::Pick(0) => {
                    self.layer = Layer::Browse;
                    send(&["add", &url, &fingerprint])
                }
                Choice::Pick(_) | Choice::Back => self.put(Layer::AddUrl { text: url, row: 4, col: 9, busy: false, error: String::new() }),
                Choice::None => Vec::new(),
            },
        };
        self.settle(catalog);
        actions
    }

    fn put(&mut self, layer: Layer) -> Vec<Action> {
        self.layer = layer;
        Vec::new()
    }

    fn browse_key(&mut self, key: &Key, catalog: &Catalog) -> Vec<Action> {
        if *key == Key::Back {
            return vec![Action::Exit];
        }
        if self.in_tabs {
            let at = TABS.iter().position(|t| t.0 == self.tab).unwrap_or(0);
            let to = match key {
                Key::Left => at.saturating_sub(1),
                Key::Right => (at + 1).min(TABS.len() - 1),
                Key::Down | Key::Enter => {
                    self.in_tabs = self.tab != Tab::Sources && self.shown(catalog).is_empty();
                    at
                }
                _ => at,
            };
            if to != at {
                (self.tab, self.app_sel, self.scroll, self.source_sel) = (TABS[to].0, 0, 0, 0);
            }
            return Vec::new();
        }
        if self.tab == Tab::Sources {
            // The repositories, then the row that adds one.
            let rows = catalog.repos.len() + 1;
            match key {
                Key::Up if self.source_sel == 0 => self.in_tabs = true,
                Key::Up => self.source_sel -= 1,
                Key::Down => self.source_sel = (self.source_sel + 1).min(rows - 1),
                Key::Enter => {
                    self.layer = match catalog.repos.get(self.source_sel) {
                        Some(repo) => Layer::RepoMenu { repo: repo.id.clone(), button: 0 },
                        None => Layer::AddUrl { text: "https://".into(), row: 1, col: 0, busy: false, error: String::new() },
                    }
                }
                _ => {}
            }
            return Vec::new();
        }
        let shown = self.shown(catalog);
        let (sel, count) = (self.app_sel, shown.len());
        match key {
            Key::Left if sel % COLUMNS > 0 => self.app_sel -= 1,
            Key::Right if sel % COLUMNS < COLUMNS - 1 && sel + 1 < count => self.app_sel += 1,
            Key::Up if sel < COLUMNS => self.in_tabs = true,
            Key::Up => self.app_sel -= COLUMNS,
            // Down from above a short last row lands on its last card.
            Key::Down if sel / COLUMNS < count.saturating_sub(1) / COLUMNS => self.app_sel = (sel + COLUMNS).min(count - 1),
            Key::Enter => {
                if let Some(app) = shown.get(sel) {
                    self.layer = Layer::Detail { id: app.id.clone(), button: 0 };
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn detail_key(&mut self, key: &Key, catalog: &Catalog, id: &str, button: usize) -> Vec<Action> {
        let Some(app) = catalog.app(id) else { return self.put(Layer::Browse) };
        let buttons = self.buttons(app);
        match choose(key, button, buttons.len().max(1)) {
            Choice::Move(button) => self.put(Layer::Detail { id: id.to_string(), button }),
            Choice::Back => self.put(Layer::Browse),
            Choice::Pick(picked) => match buttons.get(picked) {
                // The permission question, when there is something to ask about.
                Some(Button::Install) if !app.sensitive().is_empty() => self.put(Layer::Consent { id: id.to_string(), button: 0 }),
                Some(Button::Install) => send(&["install", id, "allow"]),
                Some(Button::Open) => send(&["open", id]),
                Some(Button::Uninstall) => self.put(Layer::Uninstall { id: id.to_string(), button: 1 }),
                None => Vec::new(),
            },
            Choice::None => Vec::new(),
        }
    }

    fn url_key(&mut self, key: &Key, mut text: String, mut row: usize, mut col: usize, mut busy: bool, mut error: String) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut look_up = false;
        match key {
            Key::Back => return self.put(Layer::Browse),
            _ if busy => {}
            Key::Up => row = row.saturating_sub(1),
            Key::Down => row = (row + 1).min(KEYS.len() - 1),
            Key::Left => col = col.saturating_sub(1),
            Key::Right => col = (col + 1).min(KEYS[0].len() - 1),
            Key::Enter => match KEYS[row][col] {
                "Del" => {
                    text.pop();
                }
                "OK" => look_up = true,
                typed => text.push_str(typed),
            },
            // The phone's remote: typed text, its backspace key, and "done".
            Key::Other(other) => match other.strip_prefix("text:") {
                Some(typed) => text.extend(typed.chars().filter(|c| !c.is_control() && !c.is_whitespace())),
                None if other == "backspace" => {
                    text.pop();
                }
                None => {}
            },
        }
        if text.len() > URL_MAX {
            text = text.chars().take(URL_MAX).collect();
        }
        if look_up && text.trim_start_matches("https://").is_empty() {
            error = "Type the address of a repository first".into();
        } else if look_up {
            (busy, error) = (true, String::new());
            actions = send(&["probe", &text]);
        } else if !matches!(key, Key::Up | Key::Down | Key::Left | Key::Right) {
            error.clear();
        }
        self.layer = Layer::AddUrl { text, row, col, busy, error };
        actions
    }

    /// The answer to a request this app sent.
    pub fn result(&mut self, what: &str, target: &str, ok: bool, text: &[String], catalog: &Catalog) {
        let first = text.first().map_or("", String::as_str);
        let name = |id: &str| catalog.app(id).map_or(id.to_string(), |a| a.name.clone());
        match (what, ok) {
            ("probe", _) => {
                // Only while the address is still on the screen, waiting for this answer.
                let Layer::AddUrl { text: typed, busy: true, .. } = &self.layer else { return };
                self.layer = match (ok, text) {
                    (true, [name, fingerprint, apps, ..]) => Layer::ConfirmAdd { url: target.to_string(), name: name.clone(), fingerprint: fingerprint.clone(), apps: apps.parse().unwrap_or(0), button: 1 },
                    _ => Layer::AddUrl { text: typed.clone(), row: 4, col: 9, busy: false, error: sentence(if ok { "the answer made no sense" } else { first }) },
                };
            }
            ("install", true) => self.say(format!("{} is installed", name(target))),
            ("uninstall", true) => self.say("Removed, with its data"),
            ("add", true) => {
                (self.tab, self.in_tabs) = (Tab::Sources, false);
                self.source_sel = catalog.repos.len().saturating_sub(1);
                self.say("Repository added. Looking for its apps\u{2026}");
            }
            ("remove", true) => self.say("Repository removed"),
            (_, true) => {}
            (_, false) => self.say(sentence(first)),
        }
        self.settle(catalog);
    }
}

/// A reason from the system as a sentence: capital first, no trailing dot needed.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "It did not work".into(),
    }
}

enum Choice {
    Move(usize),
    Pick(usize),
    Back,
    None,
}

/// A row of `count` buttons with `at` focused.
fn choose(key: &Key, at: usize, count: usize) -> Choice {
    match key {
        Key::Left if at > 0 => Choice::Move(at - 1),
        Key::Right if at + 1 < count => Choice::Move(at + 1),
        Key::Enter => Choice::Pick(at),
        Key::Back => Choice::Back,
        _ => Choice::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample;

    fn sent(actions: &[Action]) -> Vec<String> {
        actions.iter().map(|a| match a { Action::Send(f) => f.join(" "), Action::Exit => "EXIT".into() }).collect()
    }

    fn press(state: &mut State, catalog: &Catalog, keys: &[Key]) -> Vec<String> {
        let mut all = Vec::new();
        for key in keys {
            all.extend(sent(&state.key(key, catalog)));
        }
        all
    }

    #[test]
    fn the_grid_moves_and_scrolls() {
        let c = sample();
        let mut s = State::new("dev.meow.store");
        s.settle(&c);
        assert_eq!((s.in_tabs, s.app_sel), (false, 0));
        // Seven apps: rows of 3, 3 and 1.
        press(&mut s, &c, &[Key::Right, Key::Right, Key::Right]);
        assert_eq!(s.app_sel, 2, "no wrapping at the row's end");
        press(&mut s, &c, &[Key::Down]);
        assert_eq!(s.app_sel, 5);
        press(&mut s, &c, &[Key::Down]);
        assert_eq!(s.app_sel, 6, "down lands on the short last row");
        press(&mut s, &c, &[Key::Down, Key::Right]);
        assert_eq!(s.app_sel, 6);
        press(&mut s, &c, &[Key::Up, Key::Up]);
        assert_eq!((s.app_sel, s.in_tabs), (0, false));
        press(&mut s, &c, &[Key::Up]);
        assert!(s.in_tabs);
        press(&mut s, &c, &[Key::Right]);
        assert_eq!(s.tab, Tab::Installed);
        assert_eq!(s.shown(&c).len(), 4, "installed, running and downloading ones");
        assert_eq!(press(&mut s, &c, &[Key::Back]), ["EXIT"]);
        // More rows than fit: the selected row stays on screen.
        let mut many = c.clone();
        for i in 0..12 {
            let mut app = many.apps[0].clone();
            app.id = format!("org.example.a{i}");
            many.apps.push(app);
        }
        let mut s = State::new("dev.meow.store");
        press(&mut s, &many, &[Key::Down, Key::Down, Key::Down, Key::Down]);
        assert_eq!((s.app_sel / COLUMNS, s.scroll), (4, 2));
        press(&mut s, &many, &[Key::Up, Key::Up, Key::Up]);
        assert_eq!((s.app_sel / COLUMNS, s.scroll), (1, 1));
    }

    #[test]
    fn installing_asks_about_sensitive_permissions() {
        let c = sample();
        let mut s = State::new("dev.meow.store");
        // Cast (the first card) wants the home network: a question first.
        assert!(press(&mut s, &c, &[Key::Enter, Key::Enter]).is_empty());
        assert_eq!(s.layer, Layer::Consent { id: "dev.meow.cast".into(), button: 0 });
        assert_eq!(press(&mut s, &c, &[Key::Right, Key::Enter]), ["install dev.meow.cast deny"]);
        assert!(matches!(s.layer, Layer::Detail { .. }));
        s.result("install", "dev.meow.cast", false, &["Cast needs the lan permission".into()], &c);
        assert_eq!(s.notice.as_ref().unwrap().0, "Cast needs the lan permission");
        press(&mut s, &c, &[Key::Enter, Key::Right, Key::Right, Key::Enter]);
        assert!(matches!(s.layer, Layer::Detail { .. }), "Cancel goes back to the app");
        assert_eq!(press(&mut s, &c, &[Key::Enter, Key::Enter]), ["install dev.meow.cast allow"]);
        // The inspector asks for nothing sensitive: it installs at once.
        let mut s = State::new("dev.meow.store");
        assert_eq!(press(&mut s, &c, &[Key::Right, Key::Right, Key::Enter, Key::Enter]), ["install dev.meow.inspector allow"]);
        s.result("install", "dev.meow.inspector", true, &["installed".into()], &c);
        assert_eq!(s.notice.as_ref().unwrap().0, "Sandbox Inspector is installed");
        assert!(s.expire(Instant::now() + Duration::from_secs(6)) && s.notice.is_none());
    }

    #[test]
    fn installed_apps_open_and_uninstall() {
        let c = sample();
        let mut s = State::new("dev.meow.store");
        // Radio is the second card.
        assert_eq!(press(&mut s, &c, &[Key::Right, Key::Enter, Key::Enter]), ["open org.example.radio"]);
        press(&mut s, &c, &[Key::Right, Key::Enter]);
        assert_eq!(s.layer, Layer::Uninstall { id: "org.example.radio".into(), button: 1 }, "Cancel is focused");
        assert!(press(&mut s, &c, &[Key::Enter]).is_empty());
        assert_eq!(press(&mut s, &c, &[Key::Right, Key::Enter, Key::Left, Key::Enter]), ["uninstall org.example.radio"]);
        // This app offers nothing for itself; a system app could only be opened.
        let store = c.app("dev.meow.store").unwrap();
        assert!(s.buttons(store).is_empty());
        assert_eq!(State::new("dev.meow.other").buttons(store), [Button::Open]);
        // The app vanishes while its page is open (its repository was removed elsewhere).
        let mut fewer = c.clone();
        fewer.apply("gone\torg.example.radio");
        s.settle(&fewer);
        assert_eq!(s.layer, Layer::Browse);
    }

    #[test]
    fn repositories_are_switched_removed_and_added() {
        let c = sample();
        let mut s = State::new("dev.meow.store");
        press(&mut s, &c, &[Key::Up, Key::Right, Key::Right, Key::Down]);
        assert_eq!((s.tab, s.in_tabs, s.source_sel), (Tab::Sources, false, 0));
        // The built-in one can only be asked again.
        assert_eq!(State::repo_buttons(&c, "meow"), ["Check now", "Close"]);
        assert_eq!(press(&mut s, &c, &[Key::Enter, Key::Enter]), ["refresh"]);
        assert_eq!(press(&mut s, &c, &[Key::Down, Key::Enter, Key::Enter]), ["enable r12345678 0"]);
        // Remove asks, with Cancel focused.
        assert!(press(&mut s, &c, &[Key::Right, Key::Enter, Key::Enter]).is_empty());
        assert!(matches!(s.layer, Layer::RepoMenu { .. }));
        assert_eq!(press(&mut s, &c, &[Key::Right, Key::Enter, Key::Left, Key::Enter]), ["remove r12345678"]);
        // The last row adds one: keyboard, phone text, look-up, confirmation.
        press(&mut s, &c, &[Key::Down, Key::Down, Key::Enter]);
        assert_eq!(s.layer, Layer::AddUrl { text: "https://".into(), row: 1, col: 0, busy: false, error: String::new() });
        assert!(press(&mut s, &c, &[Key::Down, Key::Down, Key::Down, Key::Down, Key::Right, Key::Right, Key::Right, Key::Right, Key::Right, Key::Right, Key::Right, Key::Right, Key::Right, Key::Right, Key::Enter]).is_empty(), "OK with nothing typed");
        assert!(matches!(&s.layer, Layer::AddUrl { error, .. } if error.starts_with("Type the address")));
        press(&mut s, &c, &[Key::Other("text:apps.example".into()), Key::Other("text:.orgg".into()), Key::Other("backspace".into())]);
        assert_eq!(press(&mut s, &c, &[Key::Enter]), ["probe https://apps.example.org"]);
        assert!(press(&mut s, &c, &[Key::Enter, Key::Left]).is_empty(), "keys wait for the answer");
        s.result("probe", "https://apps.example.org/", false, &["no app repository at this address (404)".into()], &c);
        assert!(matches!(&s.layer, Layer::AddUrl { busy: false, error, .. } if error.starts_with("No app repository")));
        press(&mut s, &c, &[Key::Enter]);
        s.result("probe", "https://apps.example.org/", true, &["Example apps".into(), "9f8e 7d6c 5b4a 3921".into(), "2".into()], &c);
        assert_eq!(s.layer, Layer::ConfirmAdd { url: "https://apps.example.org/".into(), name: "Example apps".into(), fingerprint: "9f8e 7d6c 5b4a 3921".into(), apps: 2, button: 1 });
        assert_eq!(press(&mut s, &c, &[Key::Left, Key::Enter]), ["add https://apps.example.org/ 9f8e 7d6c 5b4a 3921"]);
        s.result("add", "https://apps.example.org/", true, &[], &c);
        assert_eq!((s.tab, s.in_tabs, &s.layer), (Tab::Sources, false, &Layer::Browse));
        // The app starts before the lists arrive: the focus is on the first app when they do.
        let mut s = State::new("dev.meow.store");
        s.settle(&Catalog::default());
        s.settle(&c);
        assert!(!s.in_tabs);
        let mut empty = Catalog::default();
        empty.apply("end");
        s.settle(&empty);
        assert!(s.in_tabs, "nothing to show: the tabs have the focus");
        // An answer that comes after the user left the address screen changes nothing.
        s.result("probe", "https://apps.example.org/", true, &["X".into(), "k".into(), "1".into()], &c);
        assert_eq!(s.layer, Layer::Browse);
    }
}
