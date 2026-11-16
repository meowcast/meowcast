//! What a paired pilot (the phone remote) can see and do beyond the plain navigation keys:
//! the system volume and mute, the installed apps and the one on screen, keys and text that go
//! straight to that app. The same JSON messages arrive over the LAN WebSocket (http.rs) and,
//! wrapped by the relay, over the cloud link (cloud.rs).
//!
//!   pilot -> dongle
//!     {t:"key", k}            up down left right ok back home: the launcher's key FIFO (the
//!                             launcher passes them to the app on screen);
//!                             play rew fwd bksp: only to the app on screen, as its KEY enter,
//!                             left, right, backspace (nothing happens in the launcher)
//!     {t:"ping", n}           -> {t:"pong", n}: the pilot's heartbeat and round-trip time
//!     {t:"vol", d}            step the system volume by d; {t:"vol", set} sets 0-100
//!     {t:"mute"}              toggle; {t:"mute", on} sets (appd's `VOLUME mute on|off`; with an
//!                             appd that has none: volume 0, then back to the level before)
//!     {t:"launch", id}        put an installed app on screen, closing the one that has it
//!                             (appd's `FOREGROUND id`; older appd: Home, then `LAUNCH id`)
//!     {t:"text", s}           text for the app on screen: its KEY "text:<s>"
//!                             -> {t:"text", ok}
//!   dongle -> pilot (when it connects, and when something changes)
//!     {t:"state", v:2, caps, appd, vol, muted, fg, fgname, note, note_n}
//!     {t:"apps", apps:[{id, name, st}]}
//!
//! appd owns all of this; netd is one of its `ctl` clients (appd.rs). appd tells only the
//! launcher about changes, so netd asks once a second while a pilot is connected.

use crate::appd::{self, App, Client};
use crate::http;
use crate::state::Shared;
use crate::util::log;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// What this dongle understands (the pilot shows only controls that are listed).
pub const CAPS: &[&str] = &["ping", "vol", "mute", "media", "text", "apps"];
const POLL: Duration = Duration::from_secs(1);
const ASK_TIMEOUT: Duration = Duration::from_secs(2);
/// The relay carries messages up to 4096 bytes: keep the app list well inside that.
const MAX_APPS: usize = 24;
pub const MAX_TEXT: usize = 256;
/// Volume after "unmute" when netd does not know the level from before (appd without a mute).
const UNMUTE_DEFAULT: u8 = 30;

#[derive(Clone, Debug, Default, PartialEq)]
struct Tv {
    /// appd answers.
    appd: bool,
    volume: Option<u8>,
    muted: bool,
    /// The app that has the screen.
    fg: Option<String>,
    apps: Vec<App>,
}

#[derive(Default)]
struct State {
    tv: Tv,
    /// Bumped when anything in {t:"state"} / {t:"apps"} changes.
    seq: u64,
    apps_seq: u64,
    /// A one-line message for the phone (a launch that failed); note_n tells a new one.
    note: String,
    note_n: u64,
    /// The level to come back to after netd's own mute (volume 0).
    unmute_to: Option<u8>,
    kick: bool,
}

#[derive(Default)]
struct Conn {
    client: Option<Client>,
    /// Whether this appd has `VOLUME mute` (None: not tried yet).
    native_mute: Option<bool>,
    /// App id -> it is a system service (not launchable, not shown).
    services: HashMap<String, bool>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
static CONN: Mutex<Option<Conn>> = Mutex::new(None);
static LAUNCHING: AtomicBool = AtomicBool::new(false);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(STATE.lock().unwrap().get_or_insert_with(State::default))
}

enum Fail {
    /// appd is not running or did not answer.
    Down,
    /// appd said ERR.
    Refused(String),
}

/// One command on the shared control connection (reconnects once: appd may have restarted).
fn ask(cmd: &str) -> Result<(Vec<String>, String), Fail> {
    let mut g = CONN.lock().unwrap();
    let conn = g.get_or_insert_with(Conn::default);
    for _ in 0..2 {
        if conn.client.is_none() {
            conn.client = Client::connect().ok();
        }
        let Some(c) = &conn.client else { return Err(Fail::Down) };
        match c.request(cmd, ASK_TIMEOUT) {
            Ok(r) => return r.map_err(Fail::Refused),
            Err(e) => {
                conn.client = None;
                // The command may have arrived: do not send a volume step twice.
                if e.kind() == std::io::ErrorKind::TimedOut {
                    break;
                }
            }
        }
    }
    Err(Fail::Down)
}

/// (state sequence, app-list sequence): a connection sends again when they move.
pub fn seq() -> (u64, u64) {
    with(|s| (s.seq, s.apps_seq))
}

/// Ask appd now rather than at the next tick (a pilot connected, a command was sent).
pub fn kick() {
    with(|s| s.kick = true);
    WAKE.notify_all();
}

pub fn state_msg() -> Value {
    with(|s| state_json(s))
}

fn state_json(s: &State) -> Value {
    let fgname = s.tv.fg.as_ref().and_then(|id| s.tv.apps.iter().find(|a| &a.id == id)).map(|a| a.name.clone());
    json!({
        "t": "state", "v": 2, "caps": CAPS, "appd": s.tv.appd,
        "vol": s.tv.volume, "muted": s.tv.muted,
        "fg": s.tv.fg, "fgname": fgname,
        "note": s.note, "note_n": s.note_n,
    })
}

pub fn apps_msg() -> Value {
    with(|s| {
        let apps: Vec<Value> = s.tv.apps.iter().take(MAX_APPS).map(|a| json!({"id": a.id, "name": clip(&a.name, 40), "st": a.state})).collect();
        json!({"t": "apps", "apps": apps})
    })
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn note(text: &str) {
    log(format!("netd: pilot: {text}"));
    with(|s| {
        s.note = clip(text, 120);
        s.note_n += 1;
        s.seq += 1;
    });
}

/// Store what appd said about the volume ("40" or "40 muted"). Without a mute switch in appd,
/// "muted" is volume 0.
fn apply_volume(level: u8, muted: Option<bool>) {
    let native = native_mute() == Some(true);
    with(|s| {
        let muted = muted.unwrap_or(!native && level == 0);
        if level > 0 && muted_by_netd(s) {
            s.unmute_to = None; // someone raised it on the TV: nothing to restore
        }
        if s.tv.volume != Some(level) || s.tv.muted != muted {
            s.tv.volume = Some(level);
            s.tv.muted = muted;
            s.seq += 1;
        }
    });
}

fn muted_by_netd(s: &State) -> bool {
    s.unmute_to.is_some()
}

fn native_mute() -> Option<bool> {
    CONN.lock().unwrap().get_or_insert_with(Conn::default).native_mute
}

fn set_native_mute(v: bool) {
    CONN.lock().unwrap().get_or_insert_with(Conn::default).native_mute = Some(v);
}

/// Does appd's refusal of a made-up `VOLUME mute ?` name the mute forms ("... mute, mute on
/// or mute off")? An appd without mute answers "expected 0..100, +N or -N". Asking this way
/// changes nothing on the TV.
fn refusal_knows_mute(text: &str) -> bool {
    text.contains("mute on")
}

fn volume_cmd(arg: &str) {
    match ask(&format!("VOLUME\t{arg}")) {
        Ok((_, text)) => {
            if let Some((level, muted)) = appd::parse_volume(&text) {
                apply_volume(level, muted);
            }
        }
        Err(Fail::Refused(e)) => log(format!("netd: pilot: volume {arg}: {e}")),
        Err(Fail::Down) => {}
    }
}

/// The command for a volume step of `d` from the current state: a step up from netd's own
/// mute goes back to the level before it, as TV remotes do.
fn step_arg(volume: Option<u8>, unmute_to: Option<u8>, d: i64) -> String {
    let d = d.clamp(-25, 25);
    match (volume, unmute_to) {
        (Some(0), Some(back)) if d > 0 => back.to_string(),
        _ if d >= 0 => format!("+{d}"),
        _ => format!("{d}"),
    }
}

fn mute(want: Option<bool>) {
    let (cur, volume, back) = with(|s| (s.tv.muted, s.tv.volume, s.unmute_to));
    let want = want.unwrap_or(!cur);
    if want == cur {
        return;
    }
    if native_mute() != Some(false) {
        // appd's own mute keeps the level and shows on the TV.
        match ask(if want { "VOLUME\tmute\ton" } else { "VOLUME\tmute\toff" }) {
            Ok((_, text)) => {
                set_native_mute(true);
                let (level, muted) = appd::parse_volume(&text).unwrap_or((volume.unwrap_or(0), Some(want)));
                apply_volume(level, muted);
                return;
            }
            Err(Fail::Refused(_)) => set_native_mute(false),
            Err(Fail::Down) => return,
        }
    }
    // An appd without mute: volume 0, and back to the level from before.
    if want {
        with(|s| s.unmute_to = volume.filter(|v| *v > 0));
        volume_cmd("0");
    } else {
        volume_cmd(&back.unwrap_or(UNMUTE_DEFAULT).to_string());
        with(|s| s.unmute_to = None);
    }
}

/// A key name of the app on screen (appd's `KEY name`, as `appctl key`): false when no app
/// with the `input` permission has the screen.
fn app_key(name: &str) -> bool {
    ask(&format!("KEY\t{name}")).is_ok()
}

/// Characters an app may get as text: no control characters (TAB separates appd's fields).
fn clean_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

/// A key from a pilot. None: not a key this firmware knows. Some(false): nobody took it (the
/// launcher is not running, or no app is on screen for an app-only key).
pub fn key(k: &str) -> Option<bool> {
    Some(match k {
        "play" => app_key("enter"),
        "rew" => app_key("left"),
        "fwd" => app_key("right"),
        "bksp" => app_key("backspace"),
        _ => http::press(http::key_byte(k)?),
    })
}

fn launch(id: String) {
    let (known, fg) = with(|s| (s.tv.apps.iter().any(|a| a.id == id), s.tv.fg.clone()));
    if !known || fg.as_deref() == Some(id.as_str()) {
        return;
    }
    if LAUNCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let r = (|| -> Result<(), String> {
            // An appd with background services closes what is in front and opens the app
            // itself (`FOREGROUND id`, as `appctl foreground`).
            match ask(&format!("FOREGROUND\t{id}")) {
                Ok(_) => return Ok(()),
                Err(Fail::Down) => return Err("The TV is busy. Try again.".into()),
                Err(Fail::Refused(e)) if !e.starts_with("unknown command") => return Err(format!("Could not open the app: {e}")),
                Err(Fail::Refused(_)) => {}
            }
            if fg.is_some() {
                // Older appd: only the launcher may close the app on screen (HOME). Press Home,
                // then wait until appd has taken the screen back.
                http::press(b'm');
                let t0 = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    match ask("PS") {
                        Ok((lines, _)) if !lines.iter().filter_map(|l| appd::parse_running(l)).any(|(_, display)| display) => break,
                        Err(Fail::Down) => return Err("The TV is busy. Try again.".into()),
                        _ if t0.elapsed() > Duration::from_secs(6) => return Err("The app on screen did not close.".into()),
                        _ => {}
                    }
                }
                // The launcher redraws before it can hand the screen over again.
                std::thread::sleep(Duration::from_millis(600));
            }
            // Its own connection: appd answers when the app has started (it may fetch it first).
            let c = Client::connect().map_err(|_| "The TV is busy. Try again.".to_string())?;
            match c.request(&format!("LAUNCH\t{id}"), Duration::from_secs(90)) {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(format!("Could not open the app: {e}")),
                Err(_) => Err("The app did not start in time.".into()),
            }
        })();
        if let Err(e) = r {
            note(&e);
        }
        LAUNCHING.store(false, Ordering::SeqCst);
        kick();
    });
}

/// A message from a pilot other than a key. The reply goes to that pilot only; changes of the
/// TV's state reach every pilot through `seq()`.
pub fn handle(m: &Value) -> Option<Value> {
    match m["t"].as_str()? {
        "ping" => return Some(json!({"t": "pong", "n": m["n"]})),
        "vol" => {
            if let Some(v) = m["set"].as_u64() {
                with(|s| s.unmute_to = None);
                volume_cmd(&v.min(100).to_string());
            } else if let Some(d) = m["d"].as_i64() {
                let (volume, back) = with(|s| (s.tv.volume, s.unmute_to));
                volume_cmd(&step_arg(volume, back, d));
            }
        }
        "mute" => mute(m["on"].as_bool()),
        "launch" => launch(m["id"].as_str()?.to_string()),
        "text" => {
            let s = clean_text(m["s"].as_str()?);
            let ok = !s.is_empty() && app_key(&format!("text:{s}"));
            return Some(json!({"t": "text", "ok": ok}));
        }
        _ => {}
    }
    None
}

/// Ask appd for everything a pilot shows.
fn poll() {
    let mut tv = with(|s| s.tv.clone());
    if native_mute().is_none() {
        if let Err(Fail::Refused(e)) = ask("VOLUME\tmute\t?") {
            set_native_mute(refusal_knows_mute(&e));
        }
    }
    match ask("VOLUME") {
        Ok((_, text)) => {
            tv.appd = true;
            if let Some((level, muted)) = appd::parse_volume(&text) {
                apply_volume(level, muted);
            }
        }
        Err(_) => tv.appd = false,
    }
    if tv.appd {
        if let Ok((lines, _)) = ask("PS") {
            tv.fg = lines.iter().filter_map(|l| appd::parse_running(l)).find(|(_, display)| *display).map(|(id, _)| id);
        }
        if let Ok((lines, _)) = ask("LIST") {
            tv.apps = lines.iter().filter_map(|l| appd::parse_app(l)).filter(|a| matches!(a.state.as_str(), "installed" | "running") && !is_service(&a.id)).collect();
        }
    } else {
        tv.fg = None;
    }
    with(|s| {
        // The volume went in through apply_volume.
        tv.volume = s.tv.volume;
        tv.muted = s.tv.muted;
        if tv.apps != s.tv.apps {
            s.apps_seq += 1;
            s.seq += 1; // fgname comes from the list
        }
        if tv != s.tv {
            s.seq += 1;
        }
        s.tv = tv;
    });
}

/// System services (the network service) are in appd's list but are not apps to open. LIST
/// does not mark them; INFO does. Asked once per app id.
fn is_service(id: &str) -> bool {
    if let Some(known) = CONN.lock().unwrap().get_or_insert_with(Conn::default).services.get(id) {
        return *known;
    }
    let Ok((lines, _)) = ask(&format!("INFO\t{id}")) else { return false };
    let service = lines.iter().any(|l| l.starts_with("INFO\tsystem service"));
    CONN.lock().unwrap().get_or_insert_with(Conn::default).services.insert(id.to_string(), service);
    service
}

/// The polling thread: only while a pilot is connected, and right after a kick.
pub fn run(shared: Shared) {
    loop {
        let kicked = {
            let g = STATE.lock().unwrap();
            let (mut g, _) = WAKE.wait_timeout_while(g, POLL, |s| !s.as_ref().is_some_and(|s| s.kick)).unwrap();
            std::mem::take(&mut g.get_or_insert_with(State::default).kick)
        };
        let pilots = {
            let n = shared.0.lock().unwrap();
            n.online + n.cloud_pilots
        };
        if kicked || pilots > 0 {
            poll();
        } else if let Some(c) = CONN.lock().unwrap().as_mut() {
            c.client = None; // nobody is looking: no idle connection in appd
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps() {
        assert_eq!(step_arg(Some(40), None, 5), "+5");
        assert_eq!(step_arg(Some(40), None, -5), "-5");
        assert_eq!(step_arg(Some(40), None, 1000), "+25");
        // Up from netd's mute: back to the level before it.
        assert_eq!(step_arg(Some(0), Some(60), 5), "60");
        assert_eq!(step_arg(Some(0), Some(60), -5), "-5");
        assert_eq!(step_arg(Some(0), None, 5), "+5");
    }

    #[test]
    fn mute_probe() {
        // The two appd generations' answers to `VOLUME mute ?`.
        assert!(refusal_knows_mute("volume mute ?: expected 0..100, +N, -N, mute, mute on or mute off"));
        assert!(!refusal_knows_mute("volume mute: expected 0..100, +N or -N"));
    }

    #[test]
    fn text_is_cleaned() {
        assert_eq!(clean_text("a\tb\nc\u{0}d"), "abcd");
        assert_eq!(clean_text("zażółć gęślą"), "zażółć gęślą");
        assert_eq!(clean_text(&"x".repeat(1000)).len(), MAX_TEXT);
    }

    #[test]
    fn replies_and_unknown() {
        assert_eq!(handle(&json!({"t": "ping", "n": 7})), Some(json!({"t": "pong", "n": 7})));
        assert_eq!(handle(&json!({"t": "nonsense"})), None);
        assert_eq!(handle(&json!({"no": "type"})), None);
        assert_eq!(key("nonsense"), None);
    }

    #[test]
    fn state_shape() {
        let mut s = State::default();
        s.tv.appd = true;
        s.tv.volume = Some(35);
        s.tv.fg = Some("dev.meow.youtube".into());
        s.tv.apps = vec![App { id: "dev.meow.youtube".into(), name: "YouTube".into(), state: "running".into() }];
        let v = state_json(&s);
        assert_eq!(v["t"], "state");
        assert_eq!(v["v"], 2);
        assert_eq!(v["vol"], 35);
        assert_eq!(v["muted"], false);
        assert_eq!(v["fgname"], "YouTube");
        assert!(v["caps"].as_array().unwrap().iter().any(|c| c == "vol"));
        // Small enough for the relay's message limit.
        assert!(v.to_string().len() < 1024);
    }
}
