//! Wi-Fi Direct for Miracast: the privileged half of the Cast app's Miracast sink.
//!
//! The app (sandboxed, apps/cast) speaks RTSP and RTP; this module makes the
//! dongle visible as a Wi-Fi Display sink, lets a source connect (push button or PIN),
//! gives the link addresses and tells the app where the source's RTSP server is. The app
//! asks through appd (`MIRACAST ...` on its broker, permission "wifi-direct"); appd writes
//! `miracast on pbc|pin NAME`, `miracast disconnect` and `miracast off` into netd's control
//! FIFO and reads the one-line state from [`STATE_FILE`].
//!
//! Three layers, the first two without any I/O so that they are tested on the build host:
//! - the vocabulary: supplicant configuration and commands, event parser, WFD subelements;
//! - [`Machine`]: what to do on each event (answer, DHCP, time-outs) and the state to show;
//! - [`Runner`]: owns the wpa_supplicant on the driver's P2P interface and the DHCP child.
//!
//! The RTL8733BU vendor driver offers P2P only on its second interface (`rtw_sel_p2p_iface`
//! = 1, our `wlan1`), without a P2P device or group interface of its own, and one channel for
//! both interfaces. So: the setup access point and Miracast exclude each other (setup wins),
//! the group is formed on `wlan1` itself (`p2p_no_group_iface=1`), and while the station is
//! connected the group has to be on the station's channel (wpa_supplicant offers only that
//! one in the negotiation). The station's own supplicant is never touched.
//!
//! While nobody asked for Miracast nothing here runs: no process, no P2P command.
//! **Not tested on hardware** (no Wi-Fi adapter was attached when this was written).

use crate::util::{self, log};
use crate::wifi::Proc;
use std::process::{Child, Command, Stdio};

/// The state for appd: one line, `state` or `state<TAB>detail`.
pub const STATE_FILE: &str = "/run/netd/miracast";
/// appd repeats its request this often; without one for [`LEASE_S`] the receiver is
/// switched off (appd or the app went away without saying so).
pub const KEEPALIVE_S: u64 = 5;
const LEASE_S: f64 = 20.0;
const CONF: &str = "/run/netd/p2p.conf";
const DHCP_SCRIPT: &str = "/run/netd/p2p-dhcp.sh";
const DHCP_LEASE: &str = "/run/netd/p2p.lease";
const DHCPD_CONF: &str = "/run/netd/p2p-udhcpd.conf";
const DHCPD_LEASES: &str = "/run/netd/p2p-udhcpd.leases";
/// Our address when we own the group, and the sources' range.
const GO_IP: &str = "192.168.49.1";
const GO_PREFIX: &str = "192.168.49";
/// A source has this long to finish the push button or PIN exchange (WPS allows 120 s).
const NEGOTIATION_S: f64 = 120.0;
/// ... and the group this long to get addresses.
const ADDRESS_S: f64 = 30.0;
/// The station lost its network this long while Wi-Fi Direct was on: suspect Wi-Fi Direct.
const STATION_LOST_S: f64 = 20.0;
const RETRY_S: f64 = 30.0;

/// The RTSP port a WFD source listens on unless its information element says otherwise.
pub const DEFAULT_CONTROL_PORT: u16 = 7236;

// ---------------------------------------------------------------------------------------
// Vocabulary

/// The WFD Device Information subelement of a primary sink that is available for a
/// session, as the hex string of wpa_supplicant's `WFD_SUBELEM_SET 0`: length (6), device
/// information bitmap, RTSP control port, maximum throughput in Mbit/s.
pub fn device_info_subelement(control_port: u16, throughput: u16) -> String {
    // Bits 1:0 = 01 primary sink, bits 5:4 = 01 available for WFD session. No content
    // protection (bit 8): HDCP is not implemented.
    let info: u16 = 0x0011;
    format!("0006{info:04x}{control_port:04x}{throughput:04x}")
}

/// (is a source, RTSP control port) from a peer's `wfd_dev_info=0x...` (the six bytes of
/// its Device Information subelement).
pub fn parse_device_info(hex: &str) -> Option<(bool, u16)> {
    let hex = hex.trim_start_matches("0x");
    let value = |at: usize| u16::from_str_radix(hex.get(at..at + 4)?, 16).ok();
    let (info, port) = (value(0)?, value(4)?);
    // Device type 00 = source, 11 = dual role.
    Some((matches!(info & 3, 0 | 3), if port == 0 { DEFAULT_CONTROL_PORT } else { port }))
}

/// The same from `wfd_subelems=` of a `P2P_PEER` reply: subelements one after another, each
/// one byte of id, two of length, then the body. Device Information is id 0.
pub fn parse_subelements(hex: &str) -> Option<(bool, u16)> {
    let mut at = 0;
    while let (Some(id), Some(length)) = (hex.get(at..at + 2), hex.get(at + 2..at + 6)) {
        let length = usize::from_str_radix(length, 16).ok()? * 2;
        let body = hex.get(at + 6..at + 6 + length)?;
        if id == "00" {
            return parse_device_info(body);
        }
        at += 6 + length;
    }
    None
}

/// How a source proves it is allowed to connect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pairing {
    /// Anyone nearby may connect ("push button" without a button).
    PushButton,
    /// The TV shows an eight-digit PIN that is typed on the source.
    Pin,
}

/// A device name as wpa_supplicant and WPS take it: no control characters, 32 bytes.
pub fn clean_name(name: &str) -> String {
    let mut clean = String::new();
    for c in name.trim().chars().filter(|c| !c.is_control()) {
        if clean.len() + c.len_utf8() > 32 {
            break;
        }
        clean.push(c);
    }
    if clean.is_empty() { "meow".into() } else { clean }
}

/// 2.4 GHz channel of a frequency (0: none of 1 to 13).
pub fn channel_of(freq: u32) -> u32 {
    if (2412..=2472).contains(&freq) && (freq - 2407) % 5 == 0 { (freq - 2407) / 5 } else { 0 }
}

/// (listen channel, operating channel) next to a station on `station_freq` (0: not
/// connected). Sources look for sinks on the social channels 1, 6 and 11, so the listen
/// channel is one of those: the station's own if it is one (then listening costs the
/// station nothing). A group we own has to be on the station's channel (one radio).
pub fn channels(station_freq: u32) -> (u32, u32) {
    let station = channel_of(station_freq);
    let listen = if matches!(station, 1 | 6 | 11) { station } else { 6 };
    (listen, if station != 0 { station } else { listen })
}

/// The configuration of the wpa_supplicant that runs the P2P interface.
pub fn supplicant_config(name: &str, pairing: Pairing, station_freq: u32) -> String {
    let (listen, operating) = channels(station_freq);
    format!(
        "ctrl_interface={}\n\
         device_name={}\n\
         device_type=7-0050F204-1\n\
         config_methods={}\n\
         p2p_no_group_iface=1\n\
         p2p_go_intent=1\n\
         p2p_listen_reg_class=81\n\
         p2p_listen_channel={listen}\n\
         p2p_oper_reg_class=81\n\
         p2p_oper_channel={operating}\n\
         p2p_go_ht40=0\n\
         persistent_reconnect=1\n",
        crate::wpa::CTRL_DIR,
        clean_name(name),
        // Primary device type above: category 7 (displays), Wi-Fi Alliance OUI, subcategory
        // 1 (television). A PIN is always one that we show.
        if pairing == Pairing::Pin { "display" } else { "virtual_push_button" },
    )
}

/// Control commands that make the running P2P interface a Wi-Fi Display sink.
pub fn supplicant_setup() -> Vec<String> {
    vec!["SET wifi_display 1".to_string(), format!("WFD_SUBELEM_SET 0 {}", device_info_subelement(DEFAULT_CONTROL_PORT, 50))]
}

/// Commands that make the sink discoverable. `continuous`: nothing else needs the radio on
/// another channel (no station, or the station is on the listen channel), so stay in the
/// listen state; otherwise visit the listen channel for 450 ms of every second, which
/// leaves the station half of the air time.
pub fn listen_commands(continuous: bool) -> Vec<String> {
    if continuous { vec!["P2P_EXT_LISTEN 500 1000".to_string(), "P2P_LISTEN".to_string()] } else { vec!["P2P_STOP_FIND".to_string(), "P2P_EXT_LISTEN 450 1000".to_string()] }
}

/// Commands that withdraw the sink again before its supplicant is stopped.
pub fn supplicant_teardown() -> Vec<String> {
    vec!["P2P_STOP_FIND".to_string(), "P2P_EXT_LISTEN".to_string(), "P2P_GROUP_REMOVE *".to_string(), "SET wifi_display 0".to_string()]
}

#[derive(Debug, Clone, PartialEq)]
pub enum P2pEvent {
    /// A peer became known (`port`: the RTSP port of its WFD device information).
    DeviceFound { peer: String, name: String, port: Option<u16> },
    /// A source asks to connect with the push-button method.
    PushButtonRequest { peer: String, name: String },
    /// A source asks to connect and expects us to display this PIN.
    ShowPin { peer: String, pin: String, name: String },
    /// Group owner negotiation started by the peer (`password_id` 4 = push button,
    /// 1 = PIN that we display, 5 = PIN that the peer displays).
    NegotiationRequest { peer: String, password_id: u32 },
    /// The group exists. `client`: we joined the source's group (the source is the DHCP
    /// server); otherwise we own it (we are). `our_address`/`mask`/`go_address`: set when
    /// the group owner handed out addresses during the handshake.
    GroupStarted { interface: String, client: bool, go_device: String, our_address: Option<String>, mask: Option<String>, go_address: Option<String> },
    GroupRemoved { interface: String },
    /// A station joined the group we own (`address`: its interface address, the one DHCP
    /// sees; `device`: its P2P device address).
    PeerJoined { address: String, device: String },
    PeerLeft { address: String },
    Failed(String),
}

fn is_mac(s: &str) -> bool {
    s.len() == 17 && s.split(':').count() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
}

/// `name='...'` of an event line (the name may hold spaces and quotes).
fn quoted_name(line: &str) -> String {
    let Some(start) = line.find(" name='").map(|i| i + 7) else { return String::new() };
    let rest = &line[start..];
    let end = rest.find("' config_methods=").or_else(|| rest.rfind('\'')).unwrap_or(rest.len());
    rest[..end].chars().filter(|c| !c.is_control()).take(40).collect()
}

/// Parse one unsolicited wpa_supplicant control interface line (the `<3>` priority prefix
/// may be present).
pub fn parse_event(line: &str) -> Option<P2pEvent> {
    let line = line.strip_prefix('<').and_then(|l| l.split_once('>')).map_or(line, |(_, rest)| rest);
    let mut words = line.split_whitespace();
    let name = words.next()?;
    let rest: Vec<&str> = words.collect();
    let value = |key: &str| rest.iter().find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix('='))).map(String::from);
    let peer = rest.first().filter(|s| is_mac(s)).map(|s| s.to_string());
    match name {
        "P2P-DEVICE-FOUND" => Some(P2pEvent::DeviceFound {
            // The device address is what later events and commands name.
            peer: value("p2p_dev_addr").filter(|a| is_mac(a)).or(peer)?,
            name: quoted_name(line),
            port: value("wfd_dev_info").and_then(|info| parse_device_info(&info)).map(|(_, port)| port),
        }),
        "P2P-PROV-DISC-PBC-REQ" => Some(P2pEvent::PushButtonRequest { peer: peer?, name: quoted_name(line) }),
        "P2P-PROV-DISC-SHOW-PIN" => {
            Some(P2pEvent::ShowPin { peer: peer?, pin: rest.get(1).filter(|p| p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit()))?.to_string(), name: quoted_name(line) })
        }
        "P2P-GO-NEG-REQUEST" => Some(P2pEvent::NegotiationRequest { peer: peer?, password_id: value("dev_passwd_id")?.parse().ok()? }),
        "P2P-GROUP-STARTED" => {
            // The network name is the peer's to choose and may hold spaces ("DIRECT-ab
            // go_ip_addr=..."): take the fields only from what follows its closing quote
            // (a quote inside the name is escaped, so the last `" freq=` is the real one).
            let tail = line.rfind("\" freq=").map_or("", |at| &line[at + 1..]);
            let value = |key: &str| tail.split(' ').find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix('='))).map(String::from);
            let address = |key: &str| value(key).filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok());
            Some(P2pEvent::GroupStarted {
                interface: rest.first()?.to_string(),
                client: match *rest.get(1)? {
                    "client" => true,
                    "GO" => false,
                    _ => return None,
                },
                go_device: value("go_dev_addr").filter(|a| is_mac(a)).unwrap_or_default(),
                our_address: address("ip_addr"),
                mask: address("ip_mask"),
                go_address: address("go_ip_addr"),
            })
        }
        "P2P-GROUP-REMOVED" => Some(P2pEvent::GroupRemoved { interface: rest.first()?.to_string() }),
        "AP-STA-CONNECTED" => Some(P2pEvent::PeerJoined { address: peer?, device: value("p2p_dev_addr").unwrap_or_default() }),
        "AP-STA-DISCONNECTED" => Some(P2pEvent::PeerLeft { address: peer? }),
        "P2P-GO-NEG-FAILURE" | "P2P-GROUP-FORMATION-FAILURE" | "WPS-FAIL" | "WPS-TIMEOUT" | "WPS-OVERLAP-DETECTED" => Some(P2pEvent::Failed(line.to_string())),
        _ => None,
    }
}

/// The command that answers a connection request, or `None` if this event needs none.
/// `pin` is the PIN on screen (for `Pairing::Pin`).
pub fn answer(event: &P2pEvent, pairing: Pairing, pin: &str) -> Option<String> {
    match (event, pairing) {
        (P2pEvent::PushButtonRequest { peer, .. }, Pairing::PushButton) => Some(format!("P2P_CONNECT {peer} pbc go_intent=1")),
        (P2pEvent::NegotiationRequest { peer, password_id: 4 }, Pairing::PushButton) => Some(format!("P2P_CONNECT {peer} pbc go_intent=1")),
        (P2pEvent::ShowPin { peer, pin, .. }, Pairing::Pin) => Some(format!("P2P_CONNECT {peer} {pin} display go_intent=1")),
        // The peer will enter the PIN we display.
        (P2pEvent::NegotiationRequest { peer, password_id: 1 }, Pairing::Pin) if pin.len() == 8 => Some(format!("P2P_CONNECT {peer} {pin} display go_intent=1")),
        // A method we do not accept: say no, so the source reports it at once.
        (P2pEvent::PushButtonRequest { peer, .. } | P2pEvent::ShowPin { peer, .. } | P2pEvent::NegotiationRequest { peer, .. }, _) => Some(format!("P2P_REJECT {peer}")),
        _ => None,
    }
}

/// An eight-digit WPS PIN: seven random digits and the checksum digit.
pub fn wps_pin(random: u32) -> String {
    let body = random % 10_000_000;
    let mut accumulator = 0;
    let mut digits = body;
    for i in 0..7 {
        accumulator += (digits % 10) * if i % 2 == 0 { 3 } else { 1 };
        digits /= 10;
    }
    format!("{body:07}{}", (10 - accumulator % 10) % 10)
}

/// Prefix length of a dotted netmask (24 if it is not one).
fn prefix_of(mask: &str) -> u32 {
    mask.parse::<std::net::Ipv4Addr>().map_or(24, |m| u32::from(m).leading_ones())
}

/// The address udhcpd gave the station `mac`, from its lease file (busybox: an 8-byte time,
/// then 36-byte records: expiry, address, hardware address, host name, padding).
pub fn lease_of(file: &[u8], mac: &str) -> Option<std::net::Ipv4Addr> {
    let wanted: Vec<u8> = mac.split(':').filter_map(|b| u8::from_str_radix(b, 16).ok()).collect();
    if wanted.len() != 6 {
        return None;
    }
    file.get(8..)?.chunks_exact(36).find(|record| record[8..14] == wanted[..]).map(|record| std::net::Ipv4Addr::new(record[4], record[5], record[6], record[7]))
}

// ---------------------------------------------------------------------------------------
// The request and the state, as they cross to appd

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub name: String,
    pub pairing: Pairing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ctl {
    /// Be a sink (also the keep-alive: appd repeats it).
    On(Request),
    /// End the session with the connected source; stay visible.
    Disconnect,
    Off,
}

/// A line of the control FIFO: `miracast on pbc|pin NAME`, `miracast disconnect`,
/// `miracast off`.
pub fn parse_ctl(line: &str) -> Option<Ctl> {
    let rest = line.trim().strip_prefix("miracast ")?;
    let (verb, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    match verb {
        "off" => Some(Ctl::Off),
        "disconnect" => Some(Ctl::Disconnect),
        "on" => {
            let (mode, name) = rest.split_once(' ').unwrap_or((rest, ""));
            let pairing = match mode {
                "pbc" => Pairing::PushButton,
                "pin" => Pairing::Pin,
                _ => return None,
            };
            Some(Ctl::On(Request { name: clean_name(name), pairing }))
        }
        _ => None,
    }
}

/// The line appd writes for a request.
pub fn ctl_line(ctl: &Ctl) -> String {
    match ctl {
        Ctl::Off => "miracast off".into(),
        Ctl::Disconnect => "miracast disconnect".into(),
        Ctl::On(r) => format!("miracast on {} {}", if r.pairing == Pairing::Pin { "pin" } else { "pbc" }, clean_name(&r.name)),
    }
}

/// What the app shows and acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Off,
    /// Asked for, but it cannot run: the reason, for a person.
    Unavailable(String),
    Starting,
    /// Visible to sources under the device name.
    Ready,
    /// A source waits for this PIN to be typed.
    Pin(String),
    /// A source is connecting (its name, if it gave one).
    Connecting(String),
    /// A source is connected: its RTSP server, `ip:port`.
    Peer(String),
}

impl Status {
    /// `state` or `state<TAB>detail`, without a line end.
    pub fn line(&self) -> String {
        let clean = |s: &str| s.chars().map(|c| if c.is_control() { ' ' } else { c }).take(120).collect::<String>();
        match self {
            Status::Off => "off".into(),
            Status::Unavailable(why) => format!("unavailable\t{}", clean(why)),
            Status::Starting => "starting".into(),
            Status::Ready => "ready".into(),
            Status::Pin(pin) => format!("pin\t{}", clean(pin)),
            Status::Connecting(name) => format!("connecting\t{}", clean(name)),
            Status::Peer(address) => format!("peer\t{}", clean(address)),
        }
    }
}

/// appd's side of the link: write a request into netd's control FIFO. An error means netd
/// is not running (a FIFO without a reader), which appd reports as "unavailable".
pub fn send_ctl(ctl: &Ctl) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut fifo = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(crate::CTL).map_err(|e| e.to_string())?;
    // One short write: atomic on a FIFO, so lines of two writers never mix.
    fifo.write_all(format!("{}\n", ctl_line(ctl)).as_bytes()).map_err(|e| e.to_string())
}

/// appd's side of the link: the state line netd last wrote.
pub fn read_state() -> Option<String> {
    let text = std::fs::read_to_string(STATE_FILE).ok()?;
    let line = text.lines().next()?.trim_end();
    (!line.is_empty()).then(|| line.to_string())
}

// ---------------------------------------------------------------------------------------
// The state machine

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A control command for the supplicant.
    Command(String),
    /// Ask the supplicant what it knows about this peer (`P2P_PEER`), for its RTSP port.
    QueryPeer(String),
    /// Become discoverable (again).
    Listen,
    /// Give the group interface this address (the group owner assigned it).
    Configure { interface: String, address: String, prefix: u32 },
    /// We joined the source's group: get an address from its DHCP server.
    DhcpClient { interface: String },
    /// We own the group: take our address and serve DHCP.
    DhcpServer { interface: String },
    /// Stop DHCP and drop the group interface's addresses.
    StopDhcp,
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Listening,
    /// A source was answered; `pin`: on screen until the group exists.
    Negotiating { peer: String, pin: Option<String> },
    /// The group exists and is getting addresses. `station`: the source's interface
    /// address in a group we own (its DHCP lease is found by it).
    /// `owner`: the group owner's address, when the handshake already gave it.
    Addressing { interface: String, client: bool, peer: String, station: Option<String>, owner: Option<String> },
    Session { interface: String, client: bool, station: Option<String>, address: String },
}

struct Known {
    peer: String,
    name: String,
    port: Option<u16>,
}

/// Decides what to do on each supplicant event. No I/O: [`Runner`] carries the actions out.
pub struct Machine {
    pairing: Pairing,
    /// The PIN to show when a source asks for one without having been given one.
    pin: String,
    phase: Phase,
    /// When the phase began.
    since: f64,
    known: Vec<Known>,
}

impl Machine {
    pub fn new(pairing: Pairing, pin: String, now: f64) -> Machine {
        Machine { pairing, pin, phase: Phase::Listening, since: now, known: Vec::new() }
    }

    fn enter(&mut self, phase: Phase, now: f64) {
        self.phase = phase;
        self.since = now;
    }

    fn remember(&mut self, peer: &str, name: &str, port: Option<u16>) {
        match self.known.iter_mut().find(|k| k.peer == peer) {
            Some(known) => {
                if !name.is_empty() {
                    known.name = name.to_string();
                }
                known.port = port.or(known.port);
            }
            None => {
                // Every phone nearby that looks for displays ends up here: keep the latest.
                if self.known.len() >= 16 {
                    self.known.remove(0);
                }
                self.known.push(Known { peer: peer.to_string(), name: name.to_string(), port });
            }
        }
    }

    /// What `P2P_PEER` said about a peer.
    pub fn peer_info(&mut self, peer: &str, port: u16) {
        self.remember(peer, "", Some(port));
    }

    fn name_of(&self, peer: &str) -> String {
        self.known.iter().find(|k| k.peer == peer).map(|k| k.name.clone()).unwrap_or_default()
    }

    fn port_of(&self, peer: &str) -> u16 {
        self.known.iter().find(|k| k.peer == peer).and_then(|k| k.port).unwrap_or(DEFAULT_CONTROL_PORT)
    }

    pub fn status(&self) -> Status {
        match &self.phase {
            Phase::Listening => Status::Ready,
            Phase::Negotiating { pin: Some(pin), .. } => Status::Pin(pin.clone()),
            Phase::Negotiating { peer, .. } | Phase::Addressing { peer, .. } => Status::Connecting(self.name_of(peer)),
            Phase::Session { address, .. } => Status::Peer(address.clone()),
        }
    }

    /// In a group we own, the station whose DHCP lease is awaited.
    pub fn awaited_station(&self) -> Option<&str> {
        match &self.phase {
            Phase::Addressing { client: false, station: Some(station), .. } => Some(station),
            _ => None,
        }
    }

    /// We joined a group and wait for the DHCP client.
    pub fn awaits_lease(&self) -> bool {
        matches!(self.phase, Phase::Addressing { client: true, owner: None, .. })
    }

    pub fn event(&mut self, event: &P2pEvent, now: f64) -> Vec<Action> {
        match event {
            P2pEvent::DeviceFound { peer, name, port } => {
                self.remember(peer, name, *port);
                Vec::new()
            }
            P2pEvent::PushButtonRequest { peer, .. } | P2pEvent::ShowPin { peer, .. } | P2pEvent::NegotiationRequest { peer, .. } => {
                if let P2pEvent::PushButtonRequest { name, .. } | P2pEvent::ShowPin { name, .. } = event {
                    self.remember(peer, name, None);
                }
                match &self.phase {
                    // One source at a time.
                    Phase::Addressing { .. } | Phase::Session { .. } => return vec![Action::Command(format!("P2P_REJECT {peer}"))],
                    // The same source again (its negotiation request after its provision
                    // discovery): it was answered already.
                    Phase::Negotiating { peer: current, .. } if current == peer => return Vec::new(),
                    _ => {}
                }
                let Some(command) = answer(event, self.pairing, &self.pin) else { return Vec::new() };
                if !command.starts_with("P2P_CONNECT ") {
                    return vec![Action::Command(command)];
                }
                let pin = match event {
                    P2pEvent::ShowPin { pin, .. } => Some(pin.clone()),
                    P2pEvent::NegotiationRequest { .. } if self.pairing == Pairing::Pin => Some(self.pin.clone()),
                    _ => None,
                };
                let mut actions = Vec::new();
                if matches!(self.phase, Phase::Negotiating { .. }) {
                    // Another source was half way: the newest request wins.
                    actions.push(Action::Command("P2P_CANCEL".into()));
                }
                self.enter(Phase::Negotiating { peer: peer.clone(), pin }, now);
                actions.extend([Action::QueryPeer(peer.clone()), Action::Command(command)]);
                actions
            }
            P2pEvent::GroupStarted { interface, client, go_device, our_address, mask, go_address } => {
                // Also without a request before it: a source that was paired earlier may
                // start the group it remembers (persistent_reconnect).
                let negotiated = match &self.phase {
                    Phase::Negotiating { peer, .. } => peer.clone(),
                    _ => String::new(),
                };
                let peer = if *client && !go_device.is_empty() { go_device.clone() } else { negotiated };
                let mut actions = vec![Action::StopDhcp];
                if !peer.is_empty() {
                    actions.push(Action::QueryPeer(peer.clone()));
                }
                match (client, our_address, go_address) {
                    (true, Some(ours), Some(owner)) => {
                        actions.push(Action::Configure { interface: interface.clone(), address: ours.clone(), prefix: prefix_of(mask.as_deref().unwrap_or("")) });
                        // The session starts once the peer query above was answered
                        // (`handshake_owner`, then `address`).
                        self.enter(Phase::Addressing { interface: interface.clone(), client: true, peer, station: None, owner: Some(owner.clone()) }, now);
                    }
                    (true, ..) => {
                        actions.push(Action::DhcpClient { interface: interface.clone() });
                        self.enter(Phase::Addressing { interface: interface.clone(), client: true, peer, station: None, owner: None }, now);
                    }
                    (false, ..) => {
                        actions.push(Action::DhcpServer { interface: interface.clone() });
                        self.enter(Phase::Addressing { interface: interface.clone(), client: false, peer, station: None, owner: None }, now);
                    }
                }
                actions
            }
            P2pEvent::PeerJoined { address, device } => {
                if let Phase::Addressing { client: false, peer, station, .. } = &mut self.phase {
                    *station = Some(address.clone());
                    if !device.is_empty() {
                        *peer = device.clone();
                        return vec![Action::QueryPeer(device.clone())];
                    }
                }
                Vec::new()
            }
            P2pEvent::PeerLeft { address } => match &self.phase {
                // A group we own outlives its only member: end it, so the source can start anew.
                Phase::Addressing { interface, client: false, station: Some(station), .. } | Phase::Session { interface, client: false, station: Some(station), .. } if station == address => {
                    let interface = interface.clone();
                    self.leave(now, Some(interface))
                }
                _ => Vec::new(),
            },
            P2pEvent::GroupRemoved { .. } => match self.phase {
                Phase::Addressing { .. } | Phase::Session { .. } => self.leave(now, None),
                _ => Vec::new(),
            },
            P2pEvent::Failed(_) => match &self.phase {
                Phase::Negotiating { .. } => {
                    self.enter(Phase::Listening, now);
                    vec![Action::Listen]
                }
                // The key exchange inside the new group failed: the supplicant removes it.
                _ => Vec::new(),
            },
        }
    }

    /// The group owner's address when the handshake gave it (no DHCP needed).
    pub fn handshake_owner(&self) -> Option<String> {
        match &self.phase {
            Phase::Addressing { owner, .. } => owner.clone(),
            _ => None,
        }
    }

    /// Back to listening; `remove`: the group interface to remove first.
    fn leave(&mut self, now: f64, remove: Option<String>) -> Vec<Action> {
        self.enter(Phase::Listening, now);
        let mut actions: Vec<Action> = remove.into_iter().map(|interface| Action::Command(format!("P2P_GROUP_REMOVE {interface}"))).collect();
        actions.extend([Action::StopDhcp, Action::Listen]);
        actions
    }

    /// The source's address is known (DHCP, or the handshake): the session can start.
    pub fn address(&mut self, source: &str, now: f64) {
        if let Phase::Addressing { interface, client, peer, station, .. } = &self.phase {
            let address = format!("{source}:{}", self.port_of(peer));
            let phase = Phase::Session { interface: interface.clone(), client: *client, station: station.clone(), address };
            self.enter(phase, now);
        }
    }

    /// The app asked to end the session (or a half-made connection).
    pub fn disconnect(&mut self, now: f64) -> Vec<Action> {
        match &self.phase {
            Phase::Listening => Vec::new(),
            Phase::Negotiating { .. } => {
                self.enter(Phase::Listening, now);
                vec![Action::Command("P2P_CANCEL".into()), Action::Listen]
            }
            Phase::Addressing { interface, .. } | Phase::Session { interface, .. } => {
                let interface = interface.clone();
                self.leave(now, Some(interface))
            }
        }
    }

    /// Time-outs.
    pub fn tick(&mut self, now: f64) -> Vec<Action> {
        match &self.phase {
            Phase::Negotiating { .. } if now - self.since > NEGOTIATION_S => {
                self.enter(Phase::Listening, now);
                vec![Action::Command("P2P_CANCEL".into()), Action::Listen]
            }
            Phase::Addressing { interface, .. } if now - self.since > ADDRESS_S => {
                let interface = interface.clone();
                self.leave(now, Some(interface))
            }
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------
// The runner

/// What the Wi-Fi manager knows about the radio, each tick.
pub struct Radio<'a> {
    /// A Wi-Fi adapter exists.
    pub adapter: bool,
    /// The station's supplicant answers: only then is it known whether the setup network
    /// needs the second interface.
    pub station_ready: bool,
    /// The driver's second interface: the only one it does P2P on.
    pub interface: Option<&'a str>,
    /// The setup access point uses that interface.
    pub setup: bool,
    /// The station is connected, on this frequency.
    pub connected: bool,
    pub freq: u32,
}

/// Whether the installed wpa_supplicant was built with P2P and Wi-Fi Display (an older
/// firmware's was not; its configuration parser refuses the P2P options).
pub fn supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let path = std::env::var("PATH").unwrap_or_default();
        let binary = path.split(':').chain(["/usr/sbin", "/usr/bin", "/sbin", "/bin"]).filter(|d| !d.is_empty()).find_map(|d| std::fs::read(format!("{d}/wpa_supplicant")).ok());
        let has = |needle: &[u8]| binary.as_ref().is_some_and(|b| b.windows(needle.len()).any(|w| w == needle));
        has(b"p2p_no_group_iface") && has(b"WFD_SUBELEM_SET")
    })
}

/// An event line for the log, without the group's key.
fn redact(line: &str) -> String {
    line.split(' ').filter(|w| !w.starts_with("psk=") && !w.starts_with("passphrase=")).collect::<Vec<_>>().join(" ")
}

pub struct Runner {
    want: Option<Request>,
    /// The request expires then unless appd repeats it.
    lease_until: f64,
    proc: Option<Proc>,
    spawned_at: f64,
    /// The station frequency the supplicant's channels were chosen for.
    conf_freq: u32,
    machine: Option<Machine>,
    dhcp: Option<Child>,
    dhcp_interface: String,
    written: Option<Status>,
    /// After a failed start: the reason, and when to try again.
    problem: Option<(String, f64)>,
    /// (station connected, frequency) the listen commands were chosen for.
    listen_for: Option<(bool, u32)>,
    station_was_connected: bool,
    station_lost_at: Option<f64>,
    /// The station lost its network while Wi-Fi Direct ran: stay off until then.
    paused_until: f64,
    last_poll: f64,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    /// Everything off, nothing written yet.
    fn blank() -> Runner {
        Runner {
            want: None,
            lease_until: 0.0,
            proc: None,
            spawned_at: 0.0,
            conf_freq: 0,
            machine: None,
            dhcp: None,
            dhcp_interface: String::new(),
            written: None,
            problem: None,
            listen_for: None,
            station_was_connected: false,
            station_lost_at: None,
            paused_until: 0.0,
            last_poll: 0.0,
        }
    }

    pub fn new() -> Runner {
        let mut runner = Runner::blank();
        runner.set(Status::Off);
        runner
    }

    /// The P2P interface is in use (the setup network must stop it first).
    pub fn active(&self) -> bool {
        self.proc.is_some()
    }

    fn set(&mut self, status: Status) {
        if self.written.as_ref() != Some(&status) {
            log(format!("netd: miracast: {}", status.line().replace('\t', " ")));
            if let Err(e) = util::write_atomic(std::path::Path::new(STATE_FILE), format!("{}\n", status.line()).as_bytes(), 0o600) {
                log(format!("netd: {STATE_FILE}: {e}"));
            }
            self.written = Some(status);
        }
    }

    pub fn command(&mut self, ctl: Ctl, now: f64) {
        match ctl {
            Ctl::On(request) => {
                self.lease_until = now + LEASE_S;
                if self.want.as_ref() != Some(&request) {
                    log(format!("netd: miracast: requested as {:?} ({:?})", request.name, request.pairing));
                    // A new name or pairing method: start afresh with it.
                    self.stop();
                    self.problem = None;
                    self.want = Some(request);
                }
            }
            Ctl::Off => {
                if self.want.take().is_some() {
                    log("netd: miracast: switched off");
                }
                self.paused_until = 0.0;
                self.problem = None;
                self.stop();
                self.set(Status::Off);
            }
            Ctl::Disconnect => {
                if let Some(machine) = self.machine.as_mut() {
                    let actions = machine.disconnect(now);
                    self.perform(actions, None);
                }
            }
        }
    }

    /// Stop the supplicant and DHCP and give the interface back as it was found. Called
    /// for every way out: switched off, setup network, errors, the owner gone.
    pub fn stop(&mut self) {
        self.stop_dhcp();
        self.machine = None;
        self.listen_for = None;
        self.station_was_connected = false;
        self.station_lost_at = None;
        let Some(mut proc) = self.proc.take() else { return };
        if proc.alive() {
            if let Some(ctrl) = proc.ctrl() {
                // Leave the group and withdraw the sink, then let the supplicant end by
                // itself: it puts the interface back into station mode on the way out.
                // (One that does not answer is not asked again: it is killed below.)
                let _ = supplicant_teardown().iter().chain([&"TERMINATE".to_string()]).try_for_each(|command| ctrl.request(command).map(|_| ()));
            }
        }
        for _ in 0..20 {
            if !proc.alive() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let interface = proc.iface.clone();
        drop(proc);
        let _ = util::run("ip", &["-4", "addr", "flush", "dev", &interface]);
        log(format!("netd: miracast: Wi-Fi Direct stopped on {interface}"));
    }

    fn fail(&mut self, why: String, now: f64) {
        log(format!("netd: miracast: {why}; again in {RETRY_S} s"));
        self.stop();
        self.set(Status::Unavailable(why.clone()));
        self.problem = Some((why, now + RETRY_S));
    }

    fn start(&mut self, request: &Request, interface: &str, radio: &Radio, now: f64) {
        let freq = if radio.connected { radio.freq } else { 0 };
        let conf = supplicant_config(&request.name, request.pairing, freq);
        if let Err(e) = util::write_atomic(std::path::Path::new(CONF), conf.as_bytes(), 0o600) {
            return self.fail(format!("Wi-Fi Direct could not start ({CONF}: {e})"), now);
        }
        let _ = util::run("ip", &["-4", "addr", "flush", "dev", interface]);
        let _ = util::run("ip", &["link", "set", interface, "up"]);
        match Proc::spawn(interface, CONF) {
            Some(proc) => {
                let (listen, operating) = channels(freq);
                log(format!("netd: miracast: Wi-Fi Direct on {interface} as {:?}, listen channel {listen}, operating channel {operating}", clean_name(&request.name)));
                self.proc = Some(proc);
                self.spawned_at = now;
                self.conf_freq = freq;
                self.set(Status::Starting);
            }
            None => self.fail("Wi-Fi Direct could not start (wpa_supplicant)".into(), now),
        }
    }

    fn listen(&mut self, radio: Option<&Radio>) {
        // Without the radio's state at hand (a request from appd): as last time.
        let key = radio.map(|r| (r.connected, r.freq)).or(self.listen_for).unwrap_or((false, 0));
        self.listen_for = Some(key);
        let (connected, freq) = key;
        let continuous = !connected || channel_of(freq) == channels(self.conf_freq).0;
        for command in listen_commands(continuous) {
            self.request(&command);
        }
    }

    fn request(&mut self, command: &str) -> Option<String> {
        let reply = self.proc.as_mut().and_then(|p| p.ctrl()).and_then(|c| c.request(command).ok());
        let short: String = reply.as_deref().unwrap_or("no answer").trim().chars().take(60).collect();
        if !command.starts_with("P2P_PEER ") {
            log(format!("netd: miracast: > {command}: {short}"));
        }
        reply
    }

    fn stop_dhcp(&mut self) {
        if let Some(mut child) = self.dhcp.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !self.dhcp_interface.is_empty() {
            let _ = util::run("ip", &["-4", "addr", "flush", "dev", &self.dhcp_interface]);
            self.dhcp_interface.clear();
        }
        let _ = std::fs::remove_file(DHCP_LEASE);
    }

    fn perform(&mut self, actions: Vec<Action>, radio: Option<&Radio>) {
        for action in actions {
            match action {
                Action::Command(command) => {
                    self.request(&command);
                }
                Action::QueryPeer(peer) => {
                    let port = self.request(&format!("P2P_PEER {peer}")).and_then(|r| crate::wpa::kv(&r, "wfd_subelems")).and_then(|hex| parse_subelements(&hex));
                    if let (Some((_, port)), Some(machine)) = (port, self.machine.as_mut()) {
                        machine.peer_info(&peer, port);
                    }
                }
                Action::Listen => self.listen(radio),
                Action::StopDhcp => self.stop_dhcp(),
                Action::Configure { interface, address, prefix } => {
                    let _ = util::run("ip", &["-4", "addr", "flush", "dev", &interface]);
                    if let Err(e) = util::run("ip", &["addr", "add", &format!("{address}/{prefix}"), "dev", &interface]) {
                        log(format!("netd: miracast: {e}"));
                    }
                    self.dhcp_interface = interface;
                }
                Action::DhcpClient { interface } => {
                    // Our own script: the address only. The default one would also move
                    // the default route and the name servers to the source.
                    let script = format!(
                        "#!/bin/sh\ncase \"$1\" in\nbound|renew)\n\tip -4 addr flush dev \"$interface\"\n\tip addr add \"$ip/${{mask:-24}}\" dev \"$interface\"\n\t\
                         echo \"$serverid $ip\" > {DHCP_LEASE}.new && mv {DHCP_LEASE}.new {DHCP_LEASE}\n\t;;\ndeconfig)\n\tip -4 addr flush dev \"$interface\"\n\trm -f {DHCP_LEASE}\n\t;;\nesac\n"
                    );
                    let _ = util::write_atomic(std::path::Path::new(DHCP_SCRIPT), script.as_bytes(), 0o700);
                    let _ = std::fs::remove_file(DHCP_LEASE);
                    self.dhcp = Command::new("udhcpc")
                        .args(["-i", &interface, "-f", "-t", "10", "-T", "2", "-A", "3", "-x", "hostname:meow", "-s", DHCP_SCRIPT])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|e| log(format!("netd: miracast: udhcpc: {e}")))
                        .ok();
                    log(format!("netd: miracast: joined the source's group; DHCP client on {interface}"));
                    self.dhcp_interface = interface;
                }
                Action::DhcpServer { interface } => {
                    let _ = util::run("ip", &["-4", "addr", "flush", "dev", &interface]);
                    if let Err(e) = util::run("ip", &["addr", "add", &format!("{GO_IP}/24"), "dev", &interface]) {
                        log(format!("netd: miracast: {e}"));
                    }
                    // No router and no name server: the link carries the cast and nothing else.
                    let conf = format!(
                        "start {GO_PREFIX}.10\nend {GO_PREFIX}.20\ninterface {interface}\nmax_leases 11\nlease_file {DHCPD_LEASES}\npidfile /run/netd/p2p-udhcpd.pid\nopt subnet 255.255.255.0\nopt lease 3600\n"
                    );
                    let _ = std::fs::write(DHCPD_CONF, conf);
                    let _ = std::fs::write(DHCPD_LEASES, b"");
                    self.dhcp = Command::new("udhcpd")
                        .args(["-f", DHCPD_CONF])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|e| log(format!("netd: miracast: udhcpd: {e}")))
                        .ok();
                    log(format!("netd: miracast: owning the group; DHCP server on {interface} ({GO_IP})"));
                    self.dhcp_interface = interface;
                }
            }
        }
    }

    /// Has the group got its addresses?
    fn progress(&mut self, now: f64) {
        let Some(machine) = self.machine.as_mut() else { return };
        if let Some(owner) = machine.handshake_owner() {
            machine.address(&owner, now);
        } else if machine.awaits_lease() {
            // "server address" as the DHCP client's script wrote it.
            let lease = util::read(DHCP_LEASE);
            if let Some(server) = lease.split_whitespace().next().filter(|s| s.parse::<std::net::Ipv4Addr>().is_ok()) {
                machine.address(server, now);
            }
        } else if let Some(station) = machine.awaited_station().map(String::from) {
            if now - self.last_poll >= 0.5 {
                self.last_poll = now;
                // udhcpd writes its leases when asked to.
                if let Some(child) = &self.dhcp {
                    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGUSR1) };
                }
                if let Some(address) = std::fs::read(DHCPD_LEASES).ok().and_then(|file| lease_of(&file, &station)) {
                    machine.address(&address.to_string(), now);
                }
            }
        }
    }

    pub fn tick(&mut self, now: f64, radio: &Radio) {
        if self.want.is_some() && now > self.lease_until {
            log("netd: miracast: the request was not renewed; switching off");
            self.want = None;
        }
        // Switched off: nothing below runs, so no P2P command is ever sent.
        let Some(request) = self.want.clone() else {
            if self.proc.is_some() {
                self.stop();
            }
            self.set(Status::Off);
            return;
        };
        let obstacle = if !supported() {
            Some("this firmware has no Wi-Fi Direct support".to_string())
        } else if !radio.adapter {
            Some("no Wi-Fi adapter".to_string())
        } else if !radio.station_ready {
            Some("Wi-Fi is starting".to_string())
        } else if radio.interface.is_none() {
            Some("the Wi-Fi adapter has no second interface".to_string())
        } else if radio.setup {
            Some("Wi-Fi setup in progress".to_string())
        } else if now < self.paused_until {
            Some("the Wi-Fi connection was lost while it was on; trying again in a few minutes".to_string())
        } else {
            self.problem.as_ref().filter(|(_, until)| now < *until).map(|(why, _)| why.clone())
        };
        if let Some(why) = obstacle {
            if self.proc.is_some() {
                self.stop();
            }
            self.set(Status::Unavailable(why));
            return;
        }
        let interface = radio.interface.unwrap_or_default();
        if self.proc.as_mut().is_some_and(|p| !p.alive()) {
            return self.fail("Wi-Fi Direct stopped unexpectedly".into(), now);
        }
        if self.proc.as_ref().is_some_and(|p| p.iface != interface) {
            self.stop();
        }
        if self.proc.is_none() {
            return self.start(&request, interface, radio, now);
        }
        if self.machine.is_none() {
            let setup = match self.proc.as_mut().and_then(|p| p.ctrl()) {
                Some(ctrl) => supplicant_setup().iter().try_for_each(|command| ctrl.ok(command)),
                None if now - self.spawned_at > 10.0 => Err("no control socket".to_string()),
                None => return,
            };
            if let Err(e) = setup {
                return self.fail(format!("Wi-Fi Direct could not start ({e})"), now);
            }
            let mut random = [0u8; 4];
            util::random_bytes(&mut random);
            self.machine = Some(Machine::new(request.pairing, wps_pin(u32::from_le_bytes(random)), now));
            self.listen(Some(radio));
        }

        // The station must not suffer: if it loses its network while Wi-Fi Direct is on
        // and does not get it back, Wi-Fi Direct steps aside for a while.
        if radio.connected {
            self.station_was_connected = true;
            self.station_lost_at = None;
        } else if self.station_was_connected && now - *self.station_lost_at.get_or_insert(now) > STATION_LOST_S {
            log("netd: miracast: the station lost its network; Wi-Fi Direct pauses for 5 minutes");
            self.paused_until = now + 300.0;
            self.stop();
            return;
        }

        let listening = self.machine.as_ref().is_some_and(|m| m.status() == Status::Ready);
        if listening && radio.connected && channels(radio.freq) != channels(self.conf_freq) {
            // The station is on another channel than the one this was set up for.
            log("netd: miracast: the station changed channel; starting Wi-Fi Direct again");
            self.stop();
            return;
        }
        if listening && self.listen_for != Some((radio.connected, radio.freq)) {
            self.listen(Some(radio));
        }

        let lines = self.proc.as_mut().and_then(|p| p.ctrl()).map(|c| c.events()).unwrap_or_default();
        for line in lines {
            if line.starts_with("P2P-") || line.starts_with("AP-STA-") || line.starts_with("WPS-") {
                log(format!("netd: miracast: {}", redact(&line)));
            }
            let Some(event) = parse_event(&line) else { continue };
            let actions = self.machine.as_mut().map(|m| m.event(&event, now)).unwrap_or_default();
            self.perform(actions, Some(radio));
        }
        self.progress(now);
        let actions = self.machine.as_mut().map(|m| m.tick(now)).unwrap_or_default();
        self.perform(actions, Some(radio));
        if self.dhcp.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None))) {
            log("netd: miracast: the DHCP process exited");
            self.dhcp = None;
        }
        if let Some(status) = self.machine.as_ref().map(|m| m.status()) {
            self.set(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIXEL: &str = "02:1a:11:f4:8e:20";
    const PIXEL_FOUND: &str = "<3>P2P-DEVICE-FOUND 02:1a:11:f4:8e:20 p2p_dev_addr=02:1a:11:f4:8e:20 pri_dev_type=10-0050F204-5 name='Pixel 7' config_methods=0x188 dev_capab=0x25 group_capab=0x0 wfd_dev_info=0x00101c450032 new=1 pcea_cap_info=0x0 bootstrap_methods=0x0 pasn_type=0x0";

    /// Feed a transcript; the actions of all its lines.
    fn feed(machine: &mut Machine, now: f64, lines: &[&str]) -> Vec<Action> {
        lines.iter().filter_map(|line| parse_event(line)).flat_map(|event| machine.event(&event, now)).collect()
    }

    fn commands(actions: &[Action]) -> Vec<&str> {
        actions.iter().filter_map(|a| if let Action::Command(c) = a { Some(c.as_str()) } else { None }).collect()
    }

    #[test]
    fn information_element() {
        assert_eq!(device_info_subelement(7236, 50), "000600111c440032");
        assert_eq!(parse_device_info("0x00101c440032"), Some((true, 7236)));
        assert_eq!(parse_device_info("01111c450006"), Some((false, 7237)));
        assert_eq!(parse_device_info("0x0010"), None);
        // P2P_PEER: device information (id 0), then coupled sink information (id 6).
        assert_eq!(parse_subelements("00000600101c45003206000700000000000000"), Some((true, 7237)));
        assert_eq!(parse_subelements("0600070000000000000000000600101c440032"), Some((true, 7236)));
        assert_eq!(parse_subelements("060007000000"), None);
        assert_eq!(parse_subelements(""), None);
        assert_eq!(parse_subelements("zz"), None);
        assert_eq!(supplicant_setup(), ["SET wifi_display 1", "WFD_SUBELEM_SET 0 000600111c440032"]);
    }

    #[test]
    fn configuration_follows_the_station() {
        assert_eq!(channel_of(2412), 1);
        assert_eq!(channel_of(2472), 13);
        assert_eq!((channel_of(2484), channel_of(5180), channel_of(2413)), (0, 0, 0));
        // No station: channel 6 for both. A station on a social channel: listen there too.
        assert_eq!(channels(0), (6, 6));
        assert_eq!(channels(2462), (11, 11));
        // A station elsewhere: listen on 6, but a group of ours is on the station's channel.
        assert_eq!(channels(2422), (6, 3));
        let conf = supplicant_config("Living\nRoom", Pairing::PushButton, 2422);
        assert!(conf.starts_with("ctrl_interface=/run/wpa_supplicant\ndevice_name=LivingRoom\ndevice_type=7-0050F204-1\nconfig_methods=virtual_push_button\n"), "{conf}");
        for line in ["p2p_no_group_iface=1", "p2p_go_intent=1", "p2p_listen_channel=6", "p2p_oper_channel=3"] {
            assert!(conf.lines().any(|l| l == line), "{line} in {conf}");
        }
        assert!(supplicant_config("x", Pairing::Pin, 0).contains("\nconfig_methods=display\n"));
        // Nothing here may touch saved networks, and the name cannot add lines.
        assert!(!conf.contains("update_config") && !conf.contains("network="));
        assert_eq!(clean_name("  "), "meow");
        assert_eq!(clean_name(&"é".repeat(40)).len(), 32);
        // Continuous listening only when the station does not need another channel.
        assert_eq!(listen_commands(true), ["P2P_EXT_LISTEN 500 1000", "P2P_LISTEN"]);
        assert!(!listen_commands(false).iter().any(|c| c == "P2P_LISTEN"));
    }

    #[test]
    fn supplicant_events() {
        let pbc = parse_event("<3>P2P-PROV-DISC-PBC-REQ 02:1a:11:f4:8e:20 p2p_dev_addr=02:1a:11:f4:8e:20 pri_dev_type=10-0050F204-5 name='Pixel 7' config_methods=0x188 dev_capab=0x25 group_capab=0x0").unwrap();
        assert_eq!(pbc, P2pEvent::PushButtonRequest { peer: PIXEL.into(), name: "Pixel 7".into() });
        assert_eq!(answer(&pbc, Pairing::PushButton, "").as_deref(), Some("P2P_CONNECT 02:1a:11:f4:8e:20 pbc go_intent=1"));
        assert_eq!(answer(&pbc, Pairing::Pin, "12345670").as_deref(), Some("P2P_REJECT 02:1a:11:f4:8e:20"));

        let negotiation = parse_event("P2P-GO-NEG-REQUEST 02:1a:11:f4:8e:20 dev_passwd_id=4 go_intent=14").unwrap();
        assert_eq!(negotiation, P2pEvent::NegotiationRequest { peer: PIXEL.into(), password_id: 4 });
        let with_pin = P2pEvent::NegotiationRequest { peer: PIXEL.into(), password_id: 1 };
        assert_eq!(answer(&with_pin, Pairing::Pin, "12345670").as_deref(), Some("P2P_CONNECT 02:1a:11:f4:8e:20 12345670 display go_intent=1"));

        let show = parse_event("<3>P2P-PROV-DISC-SHOW-PIN 02:1a:11:f4:8e:20 48031947 p2p_dev_addr=02:1a:11:f4:8e:20 pri_dev_type=1-0050F204-1 name='Ann's PC' config_methods=0x188 dev_capab=0x25 group_capab=0x0").unwrap();
        assert_eq!(show, P2pEvent::ShowPin { peer: PIXEL.into(), pin: "48031947".into(), name: "Ann's PC".into() });
        assert_eq!(answer(&show, Pairing::Pin, "").as_deref(), Some("P2P_CONNECT 02:1a:11:f4:8e:20 48031947 display go_intent=1"));
        assert_eq!(parse_event("P2P-PROV-DISC-SHOW-PIN 02:1a:11:f4:8e:20 4803"), None);

        assert_eq!(parse_event(PIXEL_FOUND), Some(P2pEvent::DeviceFound { peer: PIXEL.into(), name: "Pixel 7".into(), port: Some(7237) }));

        let started = parse_event("<3>P2P-GROUP-STARTED wlan1 client ssid=\"DIRECT-xy-Pixel 7\" freq=2437 psk=00aa go_dev_addr=02:1a:11:f4:8e:20 [PERSISTENT] ip_addr=192.168.49.200 ip_mask=255.255.255.0 go_ip_addr=192.168.49.1").unwrap();
        assert_eq!(
            started,
            P2pEvent::GroupStarted {
                interface: "wlan1".into(),
                client: true,
                go_device: PIXEL.into(),
                our_address: Some("192.168.49.200".into()),
                mask: Some("255.255.255.0".into()),
                go_address: Some("192.168.49.1".into())
            }
        );
        assert_eq!(answer(&started, Pairing::PushButton, ""), None);
        let owner = parse_event("P2P-GROUP-STARTED wlan1 GO ssid=\"DIRECT-ab\" freq=2412 passphrase=\"x\" go_dev_addr=aa:bb:cc:dd:ee:ff").unwrap();
        assert!(matches!(owner, P2pEvent::GroupStarted { client: false, our_address: None, .. }));
        // A network name cannot smuggle in addresses.
        let forged = parse_event("P2P-GROUP-STARTED wlan1 client ssid=\"DIRECT-ab go_ip_addr=10.0.0.1 ip_addr=10.0.0.2\" freq=2412 go_dev_addr=aa:bb:cc:dd:ee:ff").unwrap();
        assert!(matches!(forged, P2pEvent::GroupStarted { our_address: None, go_address: None, .. }));
        let forged = parse_event("P2P-GROUP-STARTED wlan1 client ssid=\"a\\\" freq=1 go_ip_addr=10.0.0.1\" freq=2412 go_dev_addr=aa:bb:cc:dd:ee:ff").unwrap();
        assert!(matches!(forged, P2pEvent::GroupStarted { go_address: None, .. }));
        assert_eq!(parse_event("P2P-GROUP-STARTED wlan1"), None);

        assert_eq!(
            parse_event("<3>AP-STA-CONNECTED 02:1a:11:f4:0e:20 p2p_dev_addr=02:1a:11:f4:8e:20"),
            Some(P2pEvent::PeerJoined { address: "02:1a:11:f4:0e:20".into(), device: PIXEL.into() })
        );
        assert_eq!(parse_event("<3>AP-STA-DISCONNECTED 02:1a:11:f4:0e:20 p2p_dev_addr=02:1a:11:f4:8e:20"), Some(P2pEvent::PeerLeft { address: "02:1a:11:f4:0e:20".into() }));
        assert_eq!(parse_event("<3>P2P-GROUP-REMOVED wlan1 client reason=GO_ENDING_SESSION"), Some(P2pEvent::GroupRemoved { interface: "wlan1".into() }));
        assert!(matches!(parse_event("P2P-GO-NEG-FAILURE status=7"), Some(P2pEvent::Failed(_))));
        assert!(matches!(parse_event("<3>WPS-FAIL msg=8 config_error=18"), Some(P2pEvent::Failed(_))));
        for other in ["<3>CTRL-EVENT-SCAN-RESULTS", "", "<3>P2P-GO-NEG-SUCCESS role=client freq=2437 ht40=0 peer_dev=02:1a:11:f4:8e:20 peer_iface=02:1a:11:f4:0e:20 wps_method=PBC", "P2P-PROV-DISC-PBC-REQ not-an-address"] {
            assert_eq!(parse_event(other), None, "{other}");
        }
        assert_eq!(redact("P2P-GROUP-STARTED wlan1 GO ssid=\"DIRECT-ab\" freq=2412 passphrase=\"secret12\" psk=00ff go_dev_addr=aa:bb:cc:dd:ee:ff"), "P2P-GROUP-STARTED wlan1 GO ssid=\"DIRECT-ab\" freq=2412 go_dev_addr=aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn pin_checksum() {
        // The WPS specification's example PIN.
        assert_eq!(wps_pin(1234567), "12345670");
        assert_eq!(wps_pin(4803194), "48031943");
        assert_eq!(wps_pin(u32::MAX).len(), 8);
        assert_eq!(wps_pin(3), "00000031");
    }

    #[test]
    fn requests_and_states_cross_as_lines() {
        let on = Ctl::On(Request { name: "Living Room".into(), pairing: Pairing::Pin });
        assert_eq!(ctl_line(&on), "miracast on pin Living Room");
        assert_eq!(parse_ctl("miracast on pin Living Room\n"), Some(on));
        assert_eq!(parse_ctl("miracast on pbc"), Some(Ctl::On(Request { name: "meow".into(), pairing: Pairing::PushButton })));
        assert_eq!(parse_ctl(&ctl_line(&Ctl::Off)), Some(Ctl::Off));
        assert_eq!(parse_ctl(&ctl_line(&Ctl::Disconnect)), Some(Ctl::Disconnect));
        for bad in ["miracast", "miracast on", "miracast on wps x", "miracast up", "setup"] {
            assert_eq!(parse_ctl(bad), None, "{bad}");
        }
        assert_eq!(Status::Off.line(), "off");
        assert_eq!(Status::Unavailable("no Wi-Fi\nadapter".into()).line(), "unavailable\tno Wi-Fi adapter");
        assert_eq!(Status::Pin("12345670".into()).line(), "pin\t12345670");
        assert_eq!(Status::Peer("192.168.49.1:7236".into()).line(), "peer\t192.168.49.1:7236");
    }

    #[test]
    fn dhcp_server_leases() {
        let mut file = vec![0u8; 8];
        for (expires, address, mac) in [(3600u32, [192, 168, 49, 11], [0x9a, 0x5f, 0xd3, 0x11, 0xa2, 0x33]), (3600, [192, 168, 49, 10], [0x02, 0x1a, 0x11, 0xf4, 0x0e, 0x20])] {
            file.extend(expires.to_be_bytes());
            file.extend(address);
            file.extend(mac);
            file.extend([0u8; 22]);
        }
        assert_eq!(lease_of(&file, "02:1A:11:f4:0e:20"), Some("192.168.49.10".parse().unwrap()));
        assert_eq!(lease_of(&file, "9a:5f:d3:11:a2:33"), Some("192.168.49.11".parse().unwrap()));
        assert_eq!(lease_of(&file, "00:00:00:00:00:01"), None);
        assert_eq!(lease_of(&file[..30], "02:1a:11:f4:0e:20"), None);
        assert_eq!(lease_of(&[], "02:1a:11:f4:0e:20"), None);
        assert_eq!(lease_of(&file, "nonsense"), None);
        assert_eq!((prefix_of("255.255.255.0"), prefix_of("255.255.0.0"), prefix_of("")), (24, 16, 24));
    }

    /// An Android phone: a negotiation request without provision discovery, we join its
    /// group and its DHCP server gives the address.
    #[test]
    fn push_button_session_as_the_client() {
        let mut machine = Machine::new(Pairing::PushButton, "12345670".into(), 0.0);
        assert_eq!(machine.status(), Status::Ready);
        let actions = feed(&mut machine, 1.0, &[PIXEL_FOUND, "<3>P2P-GO-NEG-REQUEST 02:1a:11:f4:8e:20 dev_passwd_id=4 go_intent=15"]);
        assert_eq!(actions, [Action::QueryPeer(PIXEL.into()), Action::Command("P2P_CONNECT 02:1a:11:f4:8e:20 pbc go_intent=1".into())]);
        assert_eq!(machine.status(), Status::Connecting("Pixel 7".into()));
        let actions = feed(
            &mut machine,
            3.0,
            &[
                "<3>P2P-GO-NEG-SUCCESS role=client freq=2437 ht40=0 peer_dev=02:1a:11:f4:8e:20 peer_iface=02:1a:11:f4:0e:20 wps_method=PBC",
                "<3>P2P-GROUP-FORMATION-SUCCESS ",
                "<3>CTRL-EVENT-CONNECTED - Connection to 02:1a:11:f4:0e:20 completed [id=0 id_str=]",
                "<3>P2P-GROUP-STARTED wlan1 client ssid=\"DIRECT-Xy-Pixel 7\" freq=2437 psk=4f1e go_dev_addr=02:1a:11:f4:8e:20 [PERSISTENT]",
            ],
        );
        assert_eq!(actions, [Action::StopDhcp, Action::QueryPeer(PIXEL.into()), Action::DhcpClient { interface: "wlan1".into() }]);
        assert!(machine.awaits_lease() && machine.handshake_owner().is_none());
        assert_eq!(machine.status(), Status::Connecting("Pixel 7".into()));
        // Another phone asks meanwhile: one source at a time.
        assert_eq!(commands(&feed(&mut machine, 3.5, &["<3>P2P-GO-NEG-REQUEST 5e:00:00:00:00:01 dev_passwd_id=4 go_intent=7"])), ["P2P_REJECT 5e:00:00:00:00:01"]);
        machine.address("192.168.49.1", 4.0);
        // The port is the one its device information named.
        assert_eq!(machine.status(), Status::Peer("192.168.49.1:7237".into()));
        assert!(machine.tick(4000.0).is_empty(), "a session has no time limit");
        let actions = feed(&mut machine, 60.0, &["<3>CTRL-EVENT-DISCONNECTED bssid=02:1a:11:f4:0e:20 reason=3 locally_generated=1", "<3>P2P-GROUP-REMOVED wlan1 client reason=GO_ENDING_SESSION"]);
        assert_eq!(actions, [Action::StopDhcp, Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);
    }

    /// A Windows computer with a PIN: provision discovery, then we end up owning the group.
    #[test]
    fn pin_session_as_the_group_owner() {
        let mut machine = Machine::new(Pairing::Pin, "12345670".into(), 0.0);
        let pc = "9a:5f:d3:11:22:33";
        let show = "<3>P2P-PROV-DISC-SHOW-PIN 9a:5f:d3:11:22:33 48031947 p2p_dev_addr=9a:5f:d3:11:22:33 pri_dev_type=1-0050F204-1 name='DESKTOP-ABC' config_methods=0x188 dev_capab=0x25 group_capab=0x0";
        let actions = feed(&mut machine, 1.0, &[show]);
        assert_eq!(actions, [Action::QueryPeer(pc.into()), Action::Command("P2P_CONNECT 9a:5f:d3:11:22:33 48031947 display go_intent=1".into())]);
        assert_eq!(machine.status(), Status::Pin("48031947".into()));
        // Its negotiation request follows: already answered.
        assert!(feed(&mut machine, 1.2, &["<3>P2P-GO-NEG-REQUEST 9a:5f:d3:11:22:33 dev_passwd_id=1 go_intent=14", show]).is_empty());
        assert_eq!(machine.status(), Status::Pin("48031947".into()));
        // The PIN stays up while the user types it, within the time WPS allows.
        assert!(machine.tick(100.0).is_empty());
        let actions = feed(&mut machine, 30.0, &["<3>P2P-GO-NEG-SUCCESS role=GO freq=2412 ht40=0 peer_dev=9a:5f:d3:11:22:33 peer_iface=9a:5f:d3:11:a2:33 wps_method=Display", "<3>P2P-GROUP-STARTED wlan1 GO ssid=\"DIRECT-ab\" freq=2412 passphrase=\"q1w2e3r4\" go_dev_addr=00:e0:4c:87:33:01"]);
        assert_eq!(actions, [Action::StopDhcp, Action::QueryPeer(pc.into()), Action::DhcpServer { interface: "wlan1".into() }]);
        assert_eq!(machine.status(), Status::Connecting("DESKTOP-ABC".into()));
        assert_eq!(machine.awaited_station(), None, "nobody joined yet");
        let actions = feed(&mut machine, 32.0, &["<3>WPS-REG-SUCCESS 9a:5f:d3:11:a2:33 0e3b...", "<3>AP-STA-CONNECTED 9a:5f:d3:11:a2:33 p2p_dev_addr=9a:5f:d3:11:22:33"]);
        assert_eq!(actions, [Action::QueryPeer(pc.into())]);
        assert_eq!(machine.awaited_station(), Some("9a:5f:d3:11:a2:33"));
        machine.address("192.168.49.10", 33.0);
        assert_eq!(machine.status(), Status::Peer("192.168.49.10:7236".into()));
        // Someone else's station leaving changes nothing; the source's ends the group.
        assert!(feed(&mut machine, 40.0, &["<3>AP-STA-DISCONNECTED 00:00:00:00:00:09"]).is_empty());
        let actions = feed(&mut machine, 50.0, &["<3>AP-STA-DISCONNECTED 9a:5f:d3:11:a2:33 p2p_dev_addr=9a:5f:d3:11:22:33"]);
        assert_eq!(actions, [Action::Command("P2P_GROUP_REMOVE wlan1".into()), Action::StopDhcp, Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);
        assert!(feed(&mut machine, 50.5, &["<3>P2P-GROUP-REMOVED wlan1 GO reason=REQUESTED"]).is_empty());
    }

    #[test]
    fn requests_that_do_not_fit_are_refused() {
        let mut machine = Machine::new(Pairing::Pin, "12345670".into(), 0.0);
        let pbc = "<3>P2P-PROV-DISC-PBC-REQ 02:1a:11:f4:8e:20 p2p_dev_addr=02:1a:11:f4:8e:20 pri_dev_type=10-0050F204-5 name='Pixel 7' config_methods=0x188 dev_capab=0x25 group_capab=0x0";
        assert_eq!(feed(&mut machine, 1.0, &[pbc]), [Action::Command("P2P_REJECT 02:1a:11:f4:8e:20".into())]);
        assert_eq!(machine.status(), Status::Ready);
        // A PIN the source shows is not accepted either (nothing to type it with).
        assert_eq!(commands(&feed(&mut machine, 2.0, &["<3>P2P-GO-NEG-REQUEST 02:1a:11:f4:8e:20 dev_passwd_id=5 go_intent=7"])), ["P2P_REJECT 02:1a:11:f4:8e:20"]);
        // A source that will type our PIN without having asked for one first: show ours.
        feed(&mut machine, 3.0, &["<3>P2P-GO-NEG-REQUEST 02:1a:11:f4:8e:20 dev_passwd_id=1 go_intent=7"]);
        assert_eq!(machine.status(), Status::Pin("12345670".into()));

        let mut open = Machine::new(Pairing::PushButton, "12345670".into(), 0.0);
        let show = "<3>P2P-PROV-DISC-SHOW-PIN 02:1a:11:f4:8e:20 48031947 p2p_dev_addr=02:1a:11:f4:8e:20";
        assert_eq!(commands(&feed(&mut open, 1.0, &[show])), ["P2P_REJECT 02:1a:11:f4:8e:20"]);
        // A second source while the first is half way: the newer one wins.
        feed(&mut open, 2.0, &[pbc]);
        let actions = feed(&mut open, 3.0, &["<3>P2P-PROV-DISC-PBC-REQ 5e:00:00:00:00:01 p2p_dev_addr=5e:00:00:00:00:01 pri_dev_type=10-0050F204-5 name='Tab' config_methods=0x188 dev_capab=0x25 group_capab=0x0"]);
        assert_eq!(commands(&actions), ["P2P_CANCEL", "P2P_CONNECT 5e:00:00:00:00:01 pbc go_intent=1"]);
        assert_eq!(open.status(), Status::Connecting("Tab".into()));
    }

    #[test]
    fn failures_and_time_outs_return_to_listening() {
        let request = "<3>P2P-GO-NEG-REQUEST 02:1a:11:f4:8e:20 dev_passwd_id=4 go_intent=15";
        let mut machine = Machine::new(Pairing::PushButton, String::new(), 0.0);
        feed(&mut machine, 1.0, &[request]);
        // No common channel (the source is on 5 GHz and the station holds us on 2.4).
        assert_eq!(feed(&mut machine, 2.0, &["<3>P2P-GO-NEG-FAILURE status=7"]), [Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);
        // A failure with nothing going on is not ours.
        assert!(feed(&mut machine, 2.5, &["<3>P2P-GROUP-FORMATION-FAILURE"]).is_empty());

        feed(&mut machine, 10.0, &[request]);
        assert!(machine.tick(10.0 + NEGOTIATION_S - 1.0).is_empty());
        assert_eq!(machine.tick(10.0 + NEGOTIATION_S + 1.0), [Action::Command("P2P_CANCEL".into()), Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);

        // The group exists but no address comes: remove it.
        feed(&mut machine, 200.0, &[request, "<3>P2P-GROUP-STARTED wlan1 client ssid=\"DIRECT-Xy\" freq=2437 psk=4f1e go_dev_addr=02:1a:11:f4:8e:20"]);
        assert!(machine.tick(200.0 + ADDRESS_S - 1.0).is_empty());
        assert_eq!(machine.tick(200.0 + ADDRESS_S + 1.0), [Action::Command("P2P_GROUP_REMOVE wlan1".into()), Action::StopDhcp, Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);
        machine.address("192.168.49.1", 300.0);
        assert_eq!(machine.status(), Status::Ready, "a late lease starts nothing");

        // The key exchange failing inside the group: the supplicant removes the group.
        feed(&mut machine, 400.0, &[request, "<3>P2P-GROUP-STARTED wlan1 GO ssid=\"DIRECT-ab\" freq=2412 passphrase=\"x\" go_dev_addr=00:e0:4c:87:33:01"]);
        assert!(feed(&mut machine, 401.0, &["<3>WPS-FAIL msg=8 config_error=18"]).is_empty());
        assert_eq!(feed(&mut machine, 402.0, &["<3>P2P-GROUP-FORMATION-FAILURE", "<3>P2P-GROUP-REMOVED wlan1 GO reason=FORMATION_FAILED"]), [Action::StopDhcp, Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);
    }

    #[test]
    fn the_app_ends_a_session_and_a_remembered_group_starts_one() {
        let mut machine = Machine::new(Pairing::PushButton, String::new(), 0.0);
        assert!(machine.disconnect(0.5).is_empty());
        // A source that was paired before starts its remembered group: no request first,
        // and the owner hands out the addresses in the handshake.
        let started = "<3>P2P-GROUP-STARTED wlan1 client ssid=\"DIRECT-Xy-Pixel 7\" freq=2462 psk=4f1e go_dev_addr=02:1a:11:f4:8e:20 [PERSISTENT] ip_addr=192.168.49.213 ip_mask=255.255.255.0 go_ip_addr=192.168.49.1";
        let actions = feed(&mut machine, 1.0, &["<3>P2P-INVITATION-RECEIVED sa=02:1a:11:f4:8e:20 persistent=0", started]);
        assert_eq!(actions, [Action::StopDhcp, Action::QueryPeer(PIXEL.into()), Action::Configure { interface: "wlan1".into(), address: "192.168.49.213".into(), prefix: 24 }]);
        assert!(!machine.awaits_lease());
        machine.peer_info(PIXEL, 7250);
        let owner = machine.handshake_owner().unwrap();
        machine.address(&owner, 1.5);
        assert_eq!(machine.status(), Status::Peer("192.168.49.1:7250".into()));
        assert_eq!(machine.handshake_owner(), None);
        assert_eq!(machine.disconnect(9.0), [Action::Command("P2P_GROUP_REMOVE wlan1".into()), Action::StopDhcp, Action::Listen]);
        assert_eq!(machine.status(), Status::Ready);
        // The removal we asked for is reported back: nothing more to do.
        assert!(feed(&mut machine, 9.5, &["<3>P2P-GROUP-REMOVED wlan1 client reason=REQUESTED"]).is_empty());

        feed(&mut machine, 20.0, &["<3>P2P-GO-NEG-REQUEST 02:1a:11:f4:8e:20 dev_passwd_id=4 go_intent=15"]);
        assert_eq!(machine.disconnect(21.0), [Action::Command("P2P_CANCEL".into()), Action::Listen]);
    }

    /// With nothing asked for, the runner runs nothing: no process, no command, whatever
    /// the radio looks like. (Every path that sends a command needs `proc`.)
    #[test]
    fn switched_off_means_idle() {
        let mut runner = Runner { written: Some(Status::Off), ..Runner::blank() };
        let radio = Radio { adapter: true, station_ready: true, interface: Some("wlan1"), setup: false, connected: true, freq: 2437 };
        for second in 0..50 {
            runner.tick(second as f64, &radio);
        }
        runner.command(Ctl::Disconnect, 60.0);
        runner.command(Ctl::Off, 61.0);
        assert!(!runner.active() && runner.machine.is_none() && runner.dhcp.is_none());
        assert_eq!(runner.written, Some(Status::Off));
    }
}
