//! Wi-Fi manager: the station (wpa_supplicant on the first wireless interface, home network
//! saved in /data/net/wpa.conf) and the setup access point (a second wpa_supplicant in AP mode
//! on the driver's concurrent interface, udhcpd, captive DNS).
//!
//! The setup network runs while no network is saved, while the saved one has been unreachable
//! for SETUP_AFTER_S, or when asked (TV: Settings > Wi-Fi > Set up with phone). Once the station
//! is connected it stays up a little longer, so the phone that sent the password can show the
//! result and pick up its pairing link, then it goes away and the phone returns to its usual
//! network.

use crate::state::{self, Setup, Shared, Wifi};
use crate::util::{self, log};
use crate::{dns, p2p, wpa};
use std::fs::File;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

pub const WPA_CONF: &str = "/data/net/wpa.conf";
pub const SETUP_IP: &str = "192.168.73.1";
const SETUP_PREFIX: &str = "192.168.73";
/// The saved network has been unreachable this long: offer the setup network again.
const SETUP_AFTER_S: f64 = 45.0;
/// After the station connected: keep the setup network while a phone is on it, at most this long.
const SETUP_LINGER_S: f64 = 150.0;
/// ... and close it this long after the last phone left.
const SETUP_IDLE_S: f64 = 20.0;
const JOIN_TIMEOUT_S: f64 = 35.0;
const DHCP_TIMEOUT_S: f64 = 20.0;

pub enum Cmd {
    Join { ssid: String, psk: String, reply: Sender<Result<String, String>> },
    Forget,
    Setup,
    Scan,
    /// Miracast's Wi-Fi Direct side (p2p.rs), asked for by appd.
    Miracast(p2p::Ctl),
}

/// Options that keep P2P and WPS out of the station and the setup access point when the
/// installed wpa_supplicant has them (a build without them refuses the options).
const NO_P2P_CONF: &str = "/run/netd/no-p2p.conf";

pub(crate) struct Proc {
    child: Child,
    ctrl: Option<wpa::Ctrl>,
    pub(crate) iface: String,
}

impl Proc {
    pub(crate) fn spawn(iface: &str, conf: &str) -> Option<Proc> {
        Self::spawn_with(iface, conf, None)
    }

    /// `extra`: a second configuration file whose options are added to `conf` (-I).
    fn spawn_with(iface: &str, conf: &str, extra: Option<&str>) -> Option<Proc> {
        let log_path = format!("/run/netd/wpa-{iface}.log");
        let out = File::create(&log_path).ok()?;
        let err = out.try_clone().ok()?;
        match Command::new("wpa_supplicant")
            .args(["-i", iface, "-D", "nl80211", "-c", conf, "-t"])
            .args(extra.iter().flat_map(|path| ["-I", path]))
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
        {
            Ok(child) => {
                log(format!("netd: wpa_supplicant on {iface} (pid {}, log {log_path})", child.id()));
                Some(Proc { child, ctrl: None, iface: iface.to_string() })
            }
            Err(e) => {
                log(format!("netd: wpa_supplicant: {e}"));
                None
            }
        }
    }

    pub(crate) fn ctrl(&mut self) -> Option<&wpa::Ctrl> {
        if self.ctrl.is_none() && std::path::Path::new(&format!("{}/{}", wpa::CTRL_DIR, self.iface)).exists() {
            match wpa::Ctrl::open(&self.iface) {
                Ok(c) => self.ctrl = Some(c),
                Err(e) => log(format!("netd: control socket {}: {e}", self.iface)),
            }
        }
        self.ctrl.as_ref()
    }

    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.ctrl = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Ap {
    wpa: Proc,
    dhcpd: Option<Child>,
    dns_stop: Arc<AtomicBool>,
    started: f64,
    last_client: f64,
}

impl Drop for Ap {
    fn drop(&mut self) {
        self.dns_stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.dhcpd.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = util::run("ip", &["addr", "flush", "dev", &self.wpa.iface]);
    }
}

struct Join {
    id: String,
    ssid: String,
    reply: Sender<Result<String, String>>,
    deadline: f64,
}

struct Mgr {
    shared: Shared,
    sta: Option<Proc>,
    ap: Option<Ap>,
    dhcp: Option<Child>,
    join: Option<Join>,
    /// When the station last lost (or never had) its connection.
    down_since: f64,
    connected_at: f64,
    dhcp_since: f64,
    forced_setup: bool,
    setup_psk: String,
    restart_after: f64,
    last_signal: f64,
    /// Miracast's Wi-Fi Direct on the second interface. It runs nothing until appd asks.
    p2p: p2p::Runner,
}

/// Wireless network interfaces (cfg80211 gives each a phy80211 link), station candidate first.
fn wireless() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| std::path::Path::new(&format!("/sys/class/net/{n}/phy80211")).exists())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|n| (n != "wlan0", n.clone()));
    v
}

pub fn run(shared: Shared, rx: Receiver<Cmd>) {
    util::mkdir_private("/data/net");
    if !std::path::Path::new(WPA_CONF).exists() {
        let _ = util::write_atomic(std::path::Path::new(WPA_CONF), b"ctrl_interface=/run/wpa_supplicant\nupdate_config=1\n", 0o600);
    }
    let mut m = Mgr {
        shared,
        sta: None,
        ap: None,
        dhcp: None,
        join: None,
        down_since: util::uptime(),
        connected_at: 0.0,
        dhcp_since: 0.0,
        forced_setup: false,
        setup_psk: util::random_digits(8),
        restart_after: 0.0,
        last_signal: 0.0,
        p2p: p2p::Runner::new(),
    };
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(cmd) => m.command(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        m.tick();
    }
}

impl Mgr {
    fn set(&self, f: impl FnOnce(&mut state::Net)) {
        {
            let mut n = self.shared.0.lock().unwrap();
            f(&mut n);
        }
        state::changed(&self.shared);
    }

    fn wifi(&self) -> Wifi {
        self.shared.0.lock().unwrap().wifi
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Join { ssid, psk, reply } => self.start_join(ssid, psk, reply),
            Cmd::Forget => {
                if let Some(c) = self.sta.as_mut().and_then(|p| p.ctrl()) {
                    let _ = c.ok("REMOVE_NETWORK all");
                    let _ = c.ok("SAVE_CONFIG");
                }
                self.stop_dhcp();
                self.down_since = util::uptime();
                self.set(|n| {
                    n.wifi = Wifi::Idle;
                    n.ssid.clear();
                    n.ip.clear();
                    n.saved.clear();
                    n.error.clear();
                });
                log("netd: saved network forgotten");
            }
            Cmd::Setup => {
                self.forced_setup = true;
                log("netd: setup network requested");
            }
            Cmd::Scan => {
                if let Some(c) = self.sta.as_mut().and_then(|p| p.ctrl()) {
                    let _ = c.request("SCAN");
                }
            }
            Cmd::Miracast(ctl) => self.p2p.command(ctl, util::uptime()),
        }
    }

    fn tick(&mut self) {
        let now = util::uptime();
        let usb = util::read("/run/usb-role");
        let ifaces = wireless();
        let sta_if = ifaces.first().cloned();
        let ap_if = ifaces.get(1).cloned();
        {
            let mut n = self.shared.0.lock().unwrap();
            if n.usb_role != usb || n.adapter != sta_if {
                n.usb_role = usb;
                n.adapter = sta_if.clone();
                drop(n);
                state::changed(&self.shared);
            }
        }
        let Some(sta_if) = sta_if else {
            if self.sta.is_some() || self.ap.is_some() {
                log("netd: wireless interface gone");
                self.ap = None;
                self.sta = None;
                self.stop_dhcp();
                self.fail_join("adapter_gone");
                self.set(|n| {
                    n.wifi = Wifi::None;
                    n.setup = None;
                    n.ip.clear();
                });
            }
            self.p2p.tick(now, &p2p::Radio { adapter: false, station_ready: false, interface: None, setup: false, connected: false, freq: 0 });
            return;
        };

        // Station supplicant: start, restart after a crash.
        if self.sta.as_mut().is_some_and(|p| !p.alive()) {
            log("netd: station wpa_supplicant exited; restarting in 2 s");
            self.sta = None;
            self.stop_dhcp();
            self.restart_after = now + 2.0;
        }
        if self.sta.is_none() && now >= self.restart_after {
            let _ = util::run("ip", &["link", "set", &sta_if, "up"]);
            // The station never does P2P (that is the second interface's business).
            let no_p2p = p2p::supported() && std::fs::write(NO_P2P_CONF, "p2p_disabled=1\n").is_ok();
            self.sta = Proc::spawn_with(&sta_if, WPA_CONF, no_p2p.then_some(NO_P2P_CONF));
            self.restart_after = now + 5.0;
            self.set(|n| n.wifi = Wifi::Starting);
        }
        self.station_events(now);
        self.dhcp_progress(now);
        self.join_deadline(now);
        self.setup_policy(now, ap_if.clone());
        // Miracast shares the second interface with the setup network, which goes first
        // (`start_ap` stops Wi-Fi Direct): without a network there is nothing to cast to.
        let (connected, freq) = {
            let n = self.shared.0.lock().unwrap();
            (n.wifi == Wifi::Connected, n.freq)
        };
        let station_ready = self.sta.as_ref().is_some_and(|p| p.ctrl.is_some());
        self.p2p.tick(now, &p2p::Radio { adapter: true, station_ready, interface: ap_if.as_deref(), setup: self.ap.is_some(), connected, freq });
    }

    /// A request to the station supplicant (None while it is not reachable).
    fn req(&mut self, cmd: &str) -> Option<String> {
        self.sta.as_mut().and_then(|p| p.ctrl()).and_then(|c| c.request(cmd).ok())
    }

    fn station_events(&mut self, now: f64) {
        let (fresh, events) = {
            let Some(p) = self.sta.as_mut() else { return };
            let fresh = p.ctrl.is_none();
            let Some(c) = p.ctrl() else { return };
            // Just connected to the supplicant: saved networks, current state, a first scan.
            let info = fresh.then(|| {
                let saved = saved_networks(c);
                let st = c.request("STATUS").unwrap_or_default();
                let _ = c.request("SCAN");
                (saved, st)
            });
            (info, c.events())
        };
        if let Some((saved, st)) = fresh {
            let completed = wpa::kv(&st, "wpa_state").as_deref() == Some("COMPLETED");
            let ssid = wpa::kv(&st, "ssid").unwrap_or_default();
            log(format!("netd: station ready, {} saved network(s){}", saved.len(), if completed { format!(", connected to {ssid}") } else { String::new() }));
            let empty = saved.is_empty();
            self.set(|n| {
                n.saved = saved;
                n.wifi = if completed { Wifi::Dhcp } else if empty { Wifi::Idle } else { Wifi::Connecting };
            });
            if completed {
                self.on_connected(now);
            }
        }
        let mut connected = false;
        let mut scan = false;
        for e in &events {
            if e.starts_with("CTRL-EVENT-CONNECTED") {
                connected = true;
            } else if e.starts_with("CTRL-EVENT-DISCONNECTED") {
                connected = false;
                if matches!(self.wifi(), Wifi::Connected | Wifi::Dhcp) {
                    log(format!("netd: {e}"));
                    self.stop_dhcp();
                    self.down_since = now;
                    let idle = self.shared.0.lock().unwrap().saved.is_empty();
                    self.set(|n| {
                        n.wifi = if idle { Wifi::Idle } else { Wifi::Connecting };
                        n.ip.clear();
                        n.signal = 0;
                    });
                }
            } else if e.starts_with("CTRL-EVENT-SSID-TEMP-DISABLED") && e.contains("reason=WRONG_KEY") {
                log(format!("netd: {e}"));
                if self.join.is_some() {
                    self.fail_join("wrong_password");
                } else {
                    self.set(|n| {
                        n.wifi = Wifi::Failed;
                        n.error = "The saved Wi-Fi password was rejected".into();
                    });
                }
            } else if e.starts_with("CTRL-EVENT-SCAN-RESULTS") {
                scan = true;
            } else if e.starts_with("CTRL-EVENT-NETWORK-NOT-FOUND") && matches!(self.wifi(), Wifi::Connecting) {
                let saved = !self.shared.0.lock().unwrap().saved.is_empty();
                if saved && self.join.is_none() && now - self.down_since > SETUP_AFTER_S {
                    self.set(|n| {
                        n.wifi = Wifi::Failed;
                        n.error = format!("{} is not in range", n.saved.first().cloned().unwrap_or_default());
                    });
                }
            }
        }
        if scan {
            if let Some(r) = self.req("SCAN_RESULTS") {
                let items = wpa::parse_scan(&r);
                self.set(|n| {
                    n.scan = items;
                    n.scan_at = now;
                });
            }
        }
        if connected {
            self.on_connected(now);
        }
        // Signal strength while connected, every 5 s.
        if self.wifi() == Wifi::Connected && now - self.last_signal > 5.0 {
            self.last_signal = now;
            let rssi: Option<i32> = self.req("SIGNAL_POLL").and_then(|r| wpa::kv(&r, "RSSI")).and_then(|v| v.parse().ok());
            if let Some(rssi) = rssi.filter(|r| *r != self.shared.0.lock().unwrap().signal) {
                self.set(|n| n.signal = rssi);
            }
        }
    }

    fn on_connected(&mut self, now: f64) {
        let Some(p) = self.sta.as_mut() else { return };
        let st = p.ctrl.as_ref().and_then(|c| c.request("STATUS").ok()).unwrap_or_default();
        let ssid = wpa::kv(&st, "ssid").unwrap_or_default();
        let freq = wpa::kv(&st, "freq").and_then(|f| f.parse().ok()).unwrap_or(0);
        let iface = p.iface.clone();
        log(format!("netd: associated with {ssid} ({freq} MHz); DHCP on {iface}"));
        self.stop_dhcp();
        self.dhcp = Command::new("udhcpc")
            .args(["-i", &iface, "-f", "-S", "-t", "5", "-T", "3", "-A", "5", "-x", "hostname:meow", "-s", "/usr/share/udhcpc/default.script"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| log(format!("netd: udhcpc: {e}")))
            .ok();
        self.dhcp_since = now;
        self.set(|n| {
            n.wifi = Wifi::Dhcp;
            n.ssid = ssid;
            n.freq = freq;
            n.error.clear();
        });
    }

    fn dhcp_progress(&mut self, now: f64) {
        if self.wifi() != Wifi::Dhcp {
            // A lease can also change or vanish while connected.
            if self.wifi() == Wifi::Connected {
                let iface = self.sta.as_ref().map(|p| p.iface.clone()).unwrap_or_default();
                let ip = util::ipv4_of(&iface).map(|a| a.to_string()).unwrap_or_default();
                if ip != self.shared.0.lock().unwrap().ip {
                    self.set(|n| n.ip = ip);
                }
            }
            return;
        }
        let iface = self.sta.as_ref().map(|p| p.iface.clone()).unwrap_or_default();
        if let Some(ip) = util::ipv4_of(&iface) {
            let ip = ip.to_string();
            log(format!("netd: connected, {iface} {ip}"));
            self.connected_at = now;
            self.set(|n| {
                n.wifi = Wifi::Connected;
                n.ip = ip.clone();
            });
            if let Some(j) = self.join.take() {
                self.finish_join(j, ip);
            }
        } else if now - self.dhcp_since > DHCP_TIMEOUT_S && self.join.is_some() {
            self.fail_join("no_ip");
        }
    }

    fn stop_dhcp(&mut self) {
        if let Some(mut c) = self.dhcp.take() {
            let _ = c.kill();
            let _ = c.wait();
            if let Some(p) = &self.sta {
                let _ = util::run("ip", &["-4", "addr", "flush", "dev", &p.iface]);
            }
        }
    }

    fn start_join(&mut self, ssid: String, psk: String, reply: Sender<Result<String, String>>) {
        if self.join.is_some() {
            let _ = reply.send(Err("busy".into()));
            return;
        }
        let Some(c) = self.sta.as_mut().and_then(|p| p.ctrl()) else {
            let _ = reply.send(Err("no_adapter".into()));
            return;
        };
        let r = (|| -> Result<String, String> {
            let id = c.request("ADD_NETWORK").map_err(|e| e.to_string())?.trim().to_string();
            if id.parse::<u32>().is_err() {
                return Err(format!("ADD_NETWORK: {id}"));
            }
            c.ok(&format!("SET_NETWORK {id} ssid {}", wpa::quote(&ssid)))?;
            if psk.is_empty() {
                c.ok(&format!("SET_NETWORK {id} key_mgmt NONE"))?;
            } else if psk.len() == 64 && psk.bytes().all(|b| b.is_ascii_hexdigit()) {
                c.ok(&format!("SET_NETWORK {id} psk {psk}"))?;
            } else {
                c.ok(&format!("SET_NETWORK {id} psk {}", wpa::quote(&psk)))?;
            }
            // Hidden networks too; WPA2 and WPA2/WPA3 transition networks (no SAE here).
            c.ok(&format!("SET_NETWORK {id} scan_ssid 1"))?;
            c.ok(&format!("SELECT_NETWORK {id}"))?;
            Ok(id)
        })();
        match r {
            Ok(id) => {
                log(format!("netd: joining {ssid} (network {id})"));
                self.join = Some(Join { id, ssid: ssid.clone(), reply, deadline: util::uptime() + JOIN_TIMEOUT_S });
                self.stop_dhcp();
                self.set(|n| {
                    n.wifi = Wifi::Connecting;
                    n.ssid = ssid;
                    n.ip.clear();
                    n.error.clear();
                });
            }
            Err(e) => {
                log(format!("netd: join {ssid}: {e}"));
                let _ = reply.send(Err("failed".into()));
            }
        }
    }

    fn join_deadline(&mut self, now: f64) {
        if self.join.as_ref().is_some_and(|j| now > j.deadline) {
            let seen = {
                let n = self.shared.0.lock().unwrap();
                let ssid = &self.join.as_ref().unwrap().ssid;
                n.scan.iter().any(|s| &s.ssid == ssid)
            };
            self.fail_join(if seen { "timeout" } else { "not_found" });
        }
    }

    /// Success: the new network replaces the old ones and is saved.
    fn finish_join(&mut self, j: Join, ip: String) {
        let saved = self.sta.as_mut().and_then(|p| p.ctrl()).map(|c| {
            for other in saved_ids(c) {
                if other != j.id {
                    let _ = c.ok(&format!("REMOVE_NETWORK {other}"));
                }
            }
            let _ = c.ok("ENABLE_NETWORK all");
            if let Err(e) = c.ok("SAVE_CONFIG") {
                log(format!("netd: {e}"));
            }
            saved_networks(c)
        });
        if let Some(saved) = saved {
            self.set(|n| n.saved = saved);
        }
        log(format!("netd: joined {} ({ip})", j.ssid));
        let _ = j.reply.send(Ok(ip));
    }

    /// Failure: drop the new network, go back to the saved one(s).
    fn fail_join(&mut self, why: &str) {
        let Some(j) = self.join.take() else { return };
        log(format!("netd: joining {} failed: {why}", j.ssid));
        if let Some(c) = self.sta.as_mut().and_then(|p| p.ctrl()) {
            let _ = c.ok(&format!("REMOVE_NETWORK {}", j.id));
            let _ = c.ok("ENABLE_NETWORK all");
            let _ = c.request("REASSOCIATE");
        }
        self.stop_dhcp();
        self.down_since = util::uptime();
        let saved_empty = self.shared.0.lock().unwrap().saved.is_empty();
        let msg = match why {
            "wrong_password" => format!("Wrong password for {}", j.ssid),
            "not_found" => format!("{} was not found", j.ssid),
            "no_ip" => format!("{} gave no IP address", j.ssid),
            _ => format!("Could not join {}", j.ssid),
        };
        self.set(|n| {
            n.wifi = if saved_empty { Wifi::Idle } else { Wifi::Connecting };
            n.ssid.clear();
            n.error = msg;
        });
        let _ = j.reply.send(Err(why.to_string()));
    }

    fn setup_policy(&mut self, now: f64, ap_if: Option<String>) {
        let (wifi, saved_empty) = {
            let n = self.shared.0.lock().unwrap();
            (n.wifi, n.saved.is_empty())
        };
        if self.sta.as_ref().is_none_or(|p| p.ctrl.is_none()) {
            return;
        }
        let connected = wifi == Wifi::Connected;
        let want = if let Some(ap) = &self.ap {
            if self.join.is_some() || !connected {
                true
            } else {
                // Connected: linger for the phone that set it up, then close.
                let since = self.connected_at.max(ap.started);
                let idle = ap.last_client < since && now - since > SETUP_IDLE_S || ap.last_client >= since && now - ap.last_client > SETUP_IDLE_S;
                let clients = self.shared.0.lock().unwrap().setup.as_ref().map_or(0, |s| s.clients);
                !(now - since > SETUP_LINGER_S || (clients == 0 && idle))
            }
        } else {
            !connected && (self.forced_setup || saved_empty || now - self.down_since > SETUP_AFTER_S)
        };
        if want && self.ap.is_none() {
            match ap_if {
                Some(i) => self.start_ap(&i, now),
                None => {
                    let msg = "The Wi-Fi adapter has no second interface for the setup network";
                    if self.shared.0.lock().unwrap().error != msg {
                        log(format!("netd: {msg}"));
                        self.set(|n| n.error = msg.into());
                    }
                }
            }
        } else if !want && self.ap.is_some() {
            log("netd: closing the setup network");
            self.ap = None;
            self.forced_setup = false;
            self.set(|n| n.setup = None);
        }
        // Count the phones on the setup network.
        let (dead, delta) = {
            let Some(ap) = self.ap.as_mut() else { return };
            if ap.dhcpd.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None))) {
                log("netd: udhcpd exited");
                ap.dhcpd = None;
            }
            if !ap.wpa.alive() {
                (true, 0)
            } else {
                let mut delta = 0i32;
                for e in ap.wpa.ctrl().map(|c| c.events()).unwrap_or_default() {
                    if e.starts_with("AP-STA-CONNECTED") || e.starts_with("AP-STA-DISCONNECTED") {
                        delta += if e.starts_with("AP-STA-CONNECTED") { 1 } else { -1 };
                        log(format!("netd: setup network: {e}"));
                    } else if e.starts_with("AP-ENABLED") || e.starts_with("AP-DISABLED") || e.starts_with("CTRL-EVENT-CONNECTED") {
                        log(format!("netd: setup network: {e}"));
                    }
                }
                if delta != 0 {
                    ap.last_client = now;
                }
                (false, delta)
            }
        };
        if dead {
            log("netd: setup network wpa_supplicant exited");
            self.ap = None;
            self.set(|n| n.setup = None);
        } else if delta != 0 {
            self.set(|n| {
                if let Some(s) = n.setup.as_mut() {
                    s.clients = (s.clients as i32 + delta).max(0) as u32;
                }
            });
        }
    }

    fn start_ap(&mut self, iface: &str, now: f64) {
        if self.p2p.active() {
            log("netd: the setup network takes the second interface from Miracast");
            self.p2p.stop();
        }
        let mac = util::read(format!("/sys/class/net/{iface}/address")).replace(':', "").to_uppercase();
        let ssid = format!("MEOW-{}", mac.get(mac.len().saturating_sub(4)..).unwrap_or("0000"));
        // One radio: the access point must use the station's channel.
        let (wifi, freq) = {
            let n = self.shared.0.lock().unwrap();
            (n.wifi, n.freq)
        };
        let freq = if wifi == Wifi::Connected && (2412..=2484).contains(&freq) { freq } else { 2437 };
        // A supplicant with P2P also has WPS, which it would switch on for this network.
        let (no_p2p, no_wps) = if p2p::supported() { ("p2p_disabled=1\n", "\twps_disabled=1\n") } else { ("", "") };
        let conf = format!(
            "ctrl_interface={}\n{no_p2p}network={{\n\tssid={}\n\tmode=2\n\tfrequency={freq}\n\tkey_mgmt=WPA-PSK\n\tproto=RSN\n\tpairwise=CCMP\n\tgroup=CCMP\n\tpsk=\"{}\"\n{no_wps}}}\n",
            wpa::CTRL_DIR,
            wpa::quote(&ssid),
            self.setup_psk
        );
        if let Err(e) = util::write_atomic(std::path::Path::new("/run/netd/ap.conf"), conf.as_bytes(), 0o600) {
            log(format!("netd: ap.conf: {e}"));
            return;
        }
        let _ = util::run("ip", &["addr", "flush", "dev", iface]);
        if let Err(e) = util::run("ip", &["addr", "add", &format!("{SETUP_IP}/24"), "dev", iface]).and_then(|_| util::run("ip", &["link", "set", iface, "up"])) {
            log(format!("netd: {e}"));
        }
        let Some(wpa) = Proc::spawn(iface, "/run/netd/ap.conf") else { return };
        let dhcpd_conf = format!(
            "start {SETUP_PREFIX}.10\nend {SETUP_PREFIX}.60\ninterface {iface}\nmax_leases 50\nlease_file /run/netd/udhcpd.leases\n\
             opt subnet 255.255.255.0\nopt router {SETUP_IP}\nopt dns {SETUP_IP}\nopt lease 600\n"
        );
        let _ = std::fs::write("/run/netd/udhcpd.conf", dhcpd_conf);
        let _ = std::fs::write("/run/netd/udhcpd.leases", b"");
        let dhcpd = Command::new("udhcpd")
            .args(["-f", "/run/netd/udhcpd.conf"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| log(format!("netd: udhcpd: {e}")))
            .ok();
        let stop = Arc::new(AtomicBool::new(false));
        dns::spawn(SETUP_IP.parse().unwrap(), stop.clone());
        log(format!("netd: setup network {ssid} on {iface} ({freq} MHz)"));
        self.ap = Some(Ap { wpa, dhcpd, dns_stop: stop, started: now, last_client: 0.0 });
        let psk = self.setup_psk.clone();
        let iface = iface.to_string();
        self.set(|n| n.setup = Some(Setup { iface, ssid, psk, ip: SETUP_IP.into(), clients: 0 }));
    }
}

fn saved_ids(c: &wpa::Ctrl) -> Vec<String> {
    c.request("LIST_NETWORKS")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| l.split('\t').next().map(|s| s.to_string()))
        .filter(|s| s.parse::<u32>().is_ok())
        .collect()
}

fn saved_networks(c: &wpa::Ctrl) -> Vec<String> {
    c.request("LIST_NETWORKS")
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| l.split('\t').nth(1).map(wpa::unescape_ssid))
        .collect()
}

/// A peer on the setup network (it knew the password shown on the TV).
pub fn is_setup_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => a.octets()[..3] == [192, 168, 73],
        std::net::IpAddr::V6(a) => a.to_ipv4_mapped().is_some_and(|a| a.octets()[..3] == [192, 168, 73]),
    }
}
