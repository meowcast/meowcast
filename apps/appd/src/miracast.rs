//! The Miracast broker: an app with the "wifi-direct" permission asks for the dongle to be
//! a Wi-Fi Display sink; meow-netd (root) owns the radio and does the Wi-Fi Direct work
//! (netd/src/p2p.rs). appd stands between them, so the sandbox needs no path to netd:
//!
//!   app -> appd   MIRACAST on pbc|pin NAME | MIRACAST disconnect | MIRACAST off
//!   appd -> app   MIRACAST state [detail]     now, and whenever it changes:
//!                 off | starting | ready | pin 12345670 | connecting [who] |
//!                 peer ip:port | unavailable why
//!
//! One app holds the receiver at a time, as an app, not as a process: when its background
//! service is stopped because the app opens (the service saw `peer` and asked for the
//! foreground), the Wi-Fi Direct link must survive until the app has started and asked
//! again. So a holder without a running instance keeps its request for [`ORPHAN`]; netd in
//! turn forgets a request that is not repeated ([`netd::KEEPALIVE_S`]), which covers appd
//! itself going away.

use meow_netd::p2p as netd;
use std::time::{Duration, Instant};

/// A holder may be without a running instance this long (the hand-over from the
/// background service to the app, or back).
const ORPHAN: Duration = Duration::from_secs(20);

#[derive(Default)]
pub struct Miracast {
    /// The app that asked, and what it asked for.
    holder: Option<(String, netd::Request)>,
    sent_at: Option<Instant>,
    /// The holder has no running instance since then.
    orphan_since: Option<Instant>,
    /// The state line last passed to the holder.
    told: String,
    /// The last request did not reach netd (it is not running).
    down: bool,
    /// The link functions, replaced in tests.
    link: Link,
}

/// How netd is reached.
#[derive(Clone, Copy)]
pub struct Link {
    send: fn(&netd::Ctl) -> Result<(), String>,
    read: fn() -> Option<String>,
}

impl Default for Link {
    fn default() -> Self {
        Link { send: netd::send_ctl, read: netd::read_state }
    }
}

impl Miracast {
    fn send(&mut self, ctl: &netd::Ctl, now: Instant) {
        self.sent_at = Some(now);
        self.down = (self.link.send)(ctl).is_err();
    }

    /// The state line for the holder right now.
    fn state(&self) -> String {
        if self.down {
            // netd's file would be what it wrote before it went away.
            return "unavailable\tthe network service is not running".into();
        }
        match (self.link.read)() {
            // netd has not seen the request yet (just sent, or netd started again).
            Some(state) if state != "off" => state,
            _ => "starting".into(),
        }
    }

    /// A request from app `app` (the fields after `MIRACAST`). The reply is the state line
    /// to send it.
    pub fn request(&mut self, app: &str, fields: &[&str], now: Instant) -> String {
        let theirs = self.holder.as_ref().is_some_and(|(holder, _)| holder == app);
        match fields {
            ["on", mode @ ("pbc" | "pin"), name] => {
                if self.holder.is_some() && !theirs && self.orphan_since.is_none() {
                    return "unavailable\tanother app is using Wi-Fi Direct".into();
                }
                let request = netd::Request { name: netd::clean_name(name), pairing: if *mode == "pin" { netd::Pairing::Pin } else { netd::Pairing::PushButton } };
                self.holder = Some((app.to_string(), request.clone()));
                self.orphan_since = None;
                self.send(&netd::Ctl::On(request), now);
                self.told = self.state();
                self.told.clone()
            }
            ["disconnect"] if theirs => {
                self.send(&netd::Ctl::Disconnect, now);
                self.state()
            }
            ["off"] if theirs => {
                self.holder = None;
                self.orphan_since = None;
                self.send(&netd::Ctl::Off, now);
                self.told = "off".into();
                "off".into()
            }
            ["off" | "disconnect"] => "off".into(),
            _ => "unavailable\tbad request".into(),
        }
    }

    /// An instance of `app` exited. `service`: its background service (the app is probably
    /// about to open); otherwise the app itself, whose session ends with it.
    pub fn exited(&mut self, app: &str, service: bool, now: Instant) {
        if self.holder.as_ref().is_some_and(|(holder, _)| holder == app) {
            self.orphan_since = Some(now);
            if !service && self.told.starts_with("peer") {
                self.send(&netd::Ctl::Disconnect, now);
            }
        }
    }

    /// Called from the daemon's timers. Some((app, state line)): tell every instance of
    /// that app.
    pub fn tick(&mut self, now: Instant) -> Option<(String, String)> {
        let (holder, request) = self.holder.clone()?;
        if self.orphan_since.is_some_and(|since| now.duration_since(since) > ORPHAN) {
            // Nobody took the receiver over: switch it off.
            self.holder = None;
            self.orphan_since = None;
            self.send(&netd::Ctl::Off, now);
            self.told = "off".into();
            return None;
        }
        if self.sent_at.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(netd::KEEPALIVE_S)) {
            self.send(&netd::Ctl::On(request), now);
        }
        let state = self.state();
        if state != self.told {
            self.told = state.clone();
            return Some((holder, state));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The link of the tests: what was sent, and what netd's state file says.
    static SENT: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static STATE: Mutex<Option<String>> = Mutex::new(None);
    static NETD_UP: Mutex<bool> = Mutex::new(true);
    static SERIAL: Mutex<()> = Mutex::new(());

    fn test_link() -> Link {
        Link {
            send: |ctl| {
                if *NETD_UP.lock().unwrap() {
                    SENT.lock().unwrap().push(netd::ctl_line(ctl));
                    Ok(())
                } else {
                    Err("no reader".into())
                }
            },
            read: || STATE.lock().unwrap().clone(),
        }
    }

    fn sent() -> Vec<String> {
        std::mem::take(&mut *SENT.lock().unwrap())
    }

    fn netd_says(state: &str) {
        *STATE.lock().unwrap() = Some(state.to_string());
    }

    fn fresh() -> (Miracast, std::sync::MutexGuard<'static, ()>) {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        sent();
        netd_says("off");
        *NETD_UP.lock().unwrap() = true;
        (Miracast { link: test_link(), ..Default::default() }, guard)
    }

    #[test]
    fn a_request_reaches_netd_and_states_come_back() {
        let (mut m, _guard) = fresh();
        let t = Instant::now();
        assert_eq!(m.tick(t), None, "nothing asked: nothing sent");
        assert!(sent().is_empty());
        assert_eq!(m.request("dev.meow.cast", &["on", "pbc", "Living Room"], t), "starting");
        assert_eq!(sent(), ["miracast on pbc Living Room"]);
        netd_says("ready");
        assert_eq!(m.tick(t + Duration::from_millis(250)), Some(("dev.meow.cast".into(), "ready".into())));
        assert_eq!(m.tick(t + Duration::from_millis(500)), None, "told once");
        assert!(sent().is_empty(), "the request is repeated only every few seconds");
        assert_eq!(m.tick(t + Duration::from_secs(6)), None);
        assert_eq!(sent(), ["miracast on pbc Living Room"]);
        netd_says("peer\t192.168.49.1:7236");
        assert_eq!(m.tick(t + Duration::from_secs(7)).unwrap().1, "peer\t192.168.49.1:7236");
        assert_eq!(m.request("dev.meow.cast", &["disconnect"], t + Duration::from_secs(8)), "peer\t192.168.49.1:7236");
        assert_eq!(sent(), ["miracast disconnect"]);
        assert_eq!(m.request("dev.meow.cast", &["off"], t + Duration::from_secs(9)), "off");
        assert_eq!(sent(), ["miracast off"]);
        assert_eq!(m.tick(t + Duration::from_secs(30)), None);
        assert!(sent().is_empty(), "switched off: silence");
    }

    #[test]
    fn bad_and_foreign_requests() {
        let (mut m, _guard) = fresh();
        let t = Instant::now();
        assert_eq!(m.request("a.b", &["on", "wps", "x"], t), "unavailable\tbad request");
        assert_eq!(m.request("a.b", &["on", "pbc"], t), "unavailable\tbad request");
        assert_eq!(m.request("a.b", &["off"], t), "off");
        assert!(sent().is_empty());
        m.request("a.b", &["on", "pin", "A\u{7}B"], t);
        assert_eq!(sent(), ["miracast on pin AB"]);
        // Another app cannot take or stop a receiver that is in use.
        assert_eq!(m.request("c.d", &["on", "pbc", "Mine"], t), "unavailable\tanother app is using Wi-Fi Direct");
        assert_eq!(m.request("c.d", &["off"], t), "off");
        assert_eq!(m.request("c.d", &["disconnect"], t), "off");
        assert!(sent().is_empty());
    }

    #[test]
    fn the_link_survives_the_hand_over_from_service_to_app() {
        let (mut m, _guard) = fresh();
        let t = Instant::now();
        m.request("dev.meow.cast", &["on", "pbc", "TV"], t);
        netd_says("peer\t192.168.49.1:7236");
        assert!(m.tick(t + Duration::from_secs(1)).is_some());
        sent();
        // The service is stopped so that the app can open: the session stays.
        m.exited("dev.meow.cast", true, t + Duration::from_secs(2));
        assert_eq!(m.tick(t + Duration::from_secs(3)), None);
        assert!(sent().is_empty());
        // The app asks for the same receiver and learns about the source at once.
        assert_eq!(m.request("dev.meow.cast", &["on", "pbc", "TV"], t + Duration::from_secs(4)), "peer\t192.168.49.1:7236");
        assert_eq!(sent(), ["miracast on pbc TV"]);
        assert_eq!(m.tick(t + Duration::from_secs(40)), None, "held again");
        assert_eq!(sent(), ["miracast on pbc TV"], "only the keep-alive");
        // The app closes in the middle of the session: the session ends, and if nothing
        // of the app comes back the receiver goes off.
        m.exited("dev.meow.cast", false, t + Duration::from_secs(41));
        assert_eq!(sent(), ["miracast disconnect"]);
        assert_eq!(m.tick(t + Duration::from_secs(50)), None);
        sent();
        assert_eq!(m.tick(t + Duration::from_secs(62)), None);
        assert_eq!(sent(), ["miracast off"]);
        assert_eq!(m.tick(t + Duration::from_secs(70)), None);
        assert!(sent().is_empty());
        // While orphaned, another app may take it.
        m.request("a.b", &["on", "pbc", "X"], t + Duration::from_secs(80));
        m.exited("a.b", false, t + Duration::from_secs(81));
        assert_eq!(m.request("c.d", &["on", "pbc", "Y"], t + Duration::from_secs(82)), "peer\t192.168.49.1:7236");
    }

    #[test]
    fn netd_missing_or_restarted() {
        let (mut m, _guard) = fresh();
        let t = Instant::now();
        *NETD_UP.lock().unwrap() = false;
        assert_eq!(m.request("dev.meow.cast", &["on", "pbc", "TV"], t), "unavailable\tthe network service is not running");
        assert_eq!(m.tick(t + Duration::from_secs(1)), None, "said already");
        // It comes up: the next keep-alive reaches it.
        *NETD_UP.lock().unwrap() = true;
        assert_eq!(m.tick(t + Duration::from_secs(6)), Some(("dev.meow.cast".into(), "starting".into())));
        assert_eq!(sent(), ["miracast on pbc TV"]);
        netd_says("unavailable\tno Wi-Fi adapter");
        assert_eq!(m.tick(t + Duration::from_secs(7)).unwrap().1, "unavailable\tno Wi-Fi adapter");
        // It goes away again: noticed with the next keep-alive.
        *NETD_UP.lock().unwrap() = false;
        assert_eq!(m.tick(t + Duration::from_secs(8)), None);
        assert_eq!(m.tick(t + Duration::from_secs(12)).unwrap().1, "unavailable\tthe network service is not running");
        // Its file still says what it wrote last; that is not believed while it is away.
        assert_eq!(m.tick(t + Duration::from_secs(13)), None);
        netd_says("ready");
        *NETD_UP.lock().unwrap() = true;
        assert_eq!(m.tick(t + Duration::from_secs(18)).unwrap().1, "ready");
    }
}
