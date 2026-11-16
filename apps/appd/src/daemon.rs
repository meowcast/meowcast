//! appd: the app manager daemon (root). One poll loop on the main thread; downloads run on
//! worker threads and report back through a channel plus a wake-up pipe.
//!
//! Control socket /run/appd/ui.sock (SEQPACKET, root peers only). Fields are TAB-separated.
//!   client -> appd
//!     HELLO ui|ctl            the UI gets the app list now and every change as it happens
//!     LIST | PS | REFRESH | INFO id | LOGS id
//!     LAUNCH id               UI: installs first (asks CONSENT) if needed; ctl: installed apps only
//!     INSTALL id [all]        ctl: install; "all" also grants the sensitive permissions
//!     CONSENT id allow|deny   UI: answer to CONSENT
//!     UNINSTALL id | STOP id | GRANT id perm | REVOKE id perm
//!     KEY name                key for the foreground app (needs "input"); appctl key for tests
//!     HOME                    UI: close the foreground app, take the screen back
//!     BACKGROUND [id on|off]  the "Run in background" switch of an app ("on" also grants the
//!                             "background" permission: the user decides here); without
//!                             arguments ctl gets one BG line per app that can run in the background
//!     RECENTS                 ctl: the apps opened last, most recent first
//!     FOREGROUND id [payload] ctl: what an app's background service asks for (tests)
//!     DISPLAY_RELEASED | DISPLAY_RECLAIMED   UI: replies to the two display requests below
//!     VOLUME [n|+d|-d|mute [on|off]]   the system volume, 0-100: read, set or step it, or
//!                             mute it (the level is kept; a new level or a step unmutes). One
//!                             setting for the whole device; every app follows it
//!     NET                     ctl: a dial socket to the network service, any host (reply
//!                             NET ok + fd; development tools run from a root shell)
//!     IMAGE                   ctl: a connection to the image service (reply IMAGE ok + fd, or
//!                             ERR why; tools run from a root shell)
//!     REPO [list] | REPO add URL | REPO remove id | REPO enable id 0|1
//!                             ctl: the repositories apps come from (repos.rs). "add" looks the
//!                             URL up and trusts the key its repo.json names
//!     ASSET name              a signed asset (wallpaper, font, ...): reply ASSET name ok + fd,
//!                             or ASSET name error why (assets.rs). While the file is not here
//!                             (offline, downloading), ASSET name preview BASE64 first if the
//!                             index has one. ASSET_UPDATED name later if a new index changes it
//!   appd -> client
//!     APP id name summary glyph tint badge0 badge1 shape state detail    (state: available,
//!         installed, fetching, running, error)          APPS_END          REMOVE id
//!     CONSENT id name perm|level|required|label|reason ...   ask the user before installing
//!     RELEASE_DISPLAY id      drop DRM master now (an app gets the screen), then DISPLAY_RELEASED
//!     RECLAIM_DISPLAY id      take DRM master back and redraw, then DISPLAY_RECLAIMED
//!     FOREGROUND id|-         NOTICE text            OK [text] | ERR text   (ctl replies)
//!     VOLUME n muted show     the system volume, muted 0|1 (to the UI: at HELLO with show=0,
//!                             and with show=1 whenever something sets it: the volume pop-up)
//!     RECENTS id ...          UI: the apps opened last, most recent first (at HELLO and on change)
//!     APPINFO id version bg svc perm|level|required|state|label|reason|extra ...
//!                             UI: details of an installed app for Settings > Apps. bg: none
//!                             (cannot run in the background), off, on; svc: running, starting,
//!                             failing or stopped; state: granted, denied or undecided
//!
//! The UI is told about installed apps only: what else the repositories offer is the Store
//! app's business. That app (an app of the built-in repository with "manage-apps") is installed
//! by appd itself and talks to it on its broker:
//!   app -> appd   STORE subscribe            everything now (repo and app lines, then STORE end),
//!                                            and every change from then on
//!                 STORE refresh | install id allow|deny | uninstall id | open id
//!                 STORE probe URL | add URL FINGERPRINT | remove repo | enable repo 0|1
//!   appd -> app   STORE repo id name url fingerprint enabled default state detail apps
//!                 STORE app id repo name summary publisher version kib glyph tint badge0 badge1
//!                           state detail flags perm|level|required|state|label|reason|extra ...
//!                 STORE gone id | STORE repo-gone id | STORE end
//!                 STORE result what target ok|error text...   (probe ok: name fingerprint apps)
//!
//! Background services: an app with the "background" permission and a "background" section in
//! its manifest has a second, small instance (`<id>@bg`, MEOW_MODE=service) that appd keeps
//! running while the app is closed and the switch is on (service.rs: restart with backoff).
//! It never runs next to the app itself: opening the app stops it first (they share /data),
//! and it comes back when the app has closed. The service can ask for the app to be opened
//! (`FOREGROUND payload` on its broker): appd closes whatever is in front and launches the app
//! the normal way; the payload reaches the app as MEOW_LAUNCH_PAYLOAD.
//!
//! The network service (an installed manifest with "service": "net", apps/netsvc) is
//! a sandboxed app that appd installs, fetches and starts by itself. An app with "internet"
//! inherits a dial socket (fd 4); appd gives the other end to the service with the app's id
//! and the hosts of its signed manifest (`CLIENT id hosts` + fd on the service's broker). When
//! the service starts again (crash, update) every running app gets a new dial (`NET` + fd).
//!
//! The image service (an installed manifest with "service": "image", apps/imagesvc)
//! decodes pictures (WebP, JPEG) for apps, in a sandbox with no permission. Nothing of it is
//! fetched or running until an app asks (`IMAGE` on its broker): appd answers `IMAGE ok` with
//! one end of a stream socket, fetches the payload into RAM if it is not there, starts the
//! service and gives it the other end (`CLIENT id` + fd on the service's broker). The service
//! leaves by itself when no app is connected; a failure closes the socket and the app shows
//! what it can without it.
//!
//! Miracast: an app with "wifi-direct" asks on its broker for the device to be a Wi-Fi
//! Display sink (`MIRACAST on pbc|pin NAME | disconnect | off`); appd passes that to meow-netd
//! and the state back (`MIRACAST state [detail]`): miracast.rs.
//!
//! Display handover: the UI drops DRM master; appd opens the KMS card, which makes appd's
//! open file the master, and passes that same open file to the app over its broker. appd
//! can therefore revoke the screen at any moment (DROP_MASTER on its own copy), whatever the
//! app does, and hands it back to the UI before the app's framebuffers go away.

use crate::ipc::{self, Recv};
use crate::manifest::Manifest;
use crate::perms::{self, Level};
use crate::repos::{self, DEFAULT_ID, Repo};
use crate::sandbox::{BROKER_FD, Spec};
use crate::sign::Trusted;
use crate::service::Supervisor;
use crate::state::{Grants, Recents, Source, State, Volume};
use crate::store::{self, Listing};
use crate::util::{cstr, field, fs_free, fs_size, now_secs, sha256_hex};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

pub const SOCKET: &str = "/run/appd/ui.sock";
const APPS_RAM: &str = "/apps";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const KEYS_DIR: &str = "/etc/appd/keys";
/// The store URL baked in by os/build.sh (MEOW_STORE_URL in config.env): one line, e.g.
/// https://store.example.org/. Overrides: APPD_STORE, then
/// /data/appd/config.json {"store": ...} (e.g. a `wrangler dev` on the Mac).
const STORE_CONF: &str = "/etc/appd/store.conf";
/// A missing or empty STORE_CONF means no store.
/// Memory for all apps together; the launcher, Mesa and the kernel keep the rest of 256 MiB.
const APPS_MEMORY_MAX: u64 = 144 << 20;
/// Flash kept free for appd's own records and JFFS2 garbage collection.
const DATA_RESERVE: u64 = 768 << 10;
const STOP_GRACE: Duration = Duration::from_secs(3);
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(3);
const SYNC_MIN_INTERVAL: Duration = Duration::from_secs(10);
const REFRESH_EVERY: Duration = Duration::from_secs(15 * 60);
const REFRESH_RETRY: Duration = Duration::from_secs(30);
/// Until the store answered once (appd starts before the network is up).
const REFRESH_RETRY_FIRST: Duration = Duration::from_secs(5);
/// While there is no network (http::not_ready, a cheap check): look again this often, so the
/// store is asked within a second of Wi-Fi/DHCP/NTP coming up.
const NET_POLL: Duration = Duration::from_secs(1);
/// A failed asset download is tried again after this, doubling up to ASSET_RETRY_MAX.
const ASSET_RETRY_MIN: Duration = Duration::from_secs(3);
const ASSET_RETRY_MAX: Duration = Duration::from_secs(60);
const LOG_CAP: usize = 32 * 1024;
/// The network service could not be fetched or started: try again after this.
const NET_RETRY: Duration = Duration::from_secs(5);
/// The image service could not be fetched or started, or died at once: apps are told it is
/// unavailable for this long (they show what they can without it), then it is tried again.
const IMG_RETRY: Duration = Duration::from_secs(20);
/// Sockets one client may have waiting for the image service.
const IMG_HELD_PER_CLIENT: usize = 4;
/// The volume is written to flash this long after its last change (a slider sends many).
const VOLUME_SAVE_AFTER: Duration = Duration::from_secs(4);
/// A background service may ask for the foreground this often.
const FOREGROUND_MIN_INTERVAL: Duration = Duration::from_secs(5);
/// A foreground request waits this long for the screen (another app closing, a launch).
const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(15);
/// Suffix of a background instance's name in `running`, cgroups and logs. '@' cannot be part
/// of an app id.
const BG_SUFFIX: &str = "@bg";
const DRM_IOCTL_SET_MASTER: libc::c_ulong = 0x641e;
const DRM_IOCTL_DROP_MASTER: libc::c_ulong = 0x641f;

#[derive(Clone, PartialEq)]
enum Status {
    Idle,
    Fetching(u32),
    Running,
    Error(String),
}

struct App {
    manifest: Manifest,
    manifest_bytes: Vec<u8>,
    signature: String,
    payload_url: String,
    /// The repository it comes from (repos.rs): DEFAULT_ID, or one the user added.
    repo: String,
    installed: bool,
    grants: Grants,
    in_store: bool,
    /// A newer store version waiting for the user (asks for more permissions).
    update_blocked: Option<u32>,
    status: Status,
}

/// A repository the user added, and how the last look at it went.
struct RepoState {
    repo: Repo,
    trusted: Arc<Vec<Trusted>>,
    /// Why the last refresh failed.
    error: Option<String>,
    /// It answered during this run.
    seen: bool,
}

/// Who asked for something that is answered later (a repository being looked up).
#[derive(Clone)]
enum Asker {
    Conn(u64),
    /// A running instance with "manage-apps" (the Store app), by instance name.
    App(String),
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    New,
    Ui,
    Ctl,
}

struct Conn {
    token: u64,
    fd: OwnedFd,
    kind: Kind,
}

enum Display {
    /// The UI (or nobody) has the screen.
    Free,
    /// Waiting for the UI to drop master for app `id`.
    Releasing { id: String, since: Instant },
    /// appd holds the master file; app `id` may have it.
    Held { id: String, fd: OwnedFd },
    /// Master dropped, waiting for the UI to take it back; then the file is closed (dropped).
    Reclaiming { _fd: OwnedFd, since: Instant },
}

/// One running sandbox: an app in front, an app's background service, or a system service.
struct Running {
    /// Instance name: the app id, or `<app id>@bg` for a background service.
    id: String,
    /// The app it belongs to.
    app: String,
    /// A background service (no screen, keys, decoder or sound).
    bg: bool,
    /// The payload it was started from (a newer one restarts a background service).
    payload_sha: String,
    /// Do not save its /data when it exits (the app is being uninstalled).
    discard: bool,
    child: std::process::Child,
    pidfd: OwnedFd,
    broker: OwnedFd,
    log_rd: OwnedFd,
    log: VecDeque<u8>,
    display: bool,
    input: bool,
    internet: bool,
    /// The Store app, after `STORE subscribe`: it is told about every change.
    store_sub: bool,
    storage: Option<PathBuf>,
    started: Instant,
    stop_at: Option<Instant>,
    last_sync: Option<Instant>,
    sync_due: Option<Instant>,
    persisted_hash: Option<String>,
    cgroup: PathBuf,
    sandbox_root: PathBuf,
}

enum Stage {
    /// Waiting for the app's background service to exit (they share /data).
    ServiceStop,
    Consent,
    Fetch,
    Display,
}

struct Pending {
    id: String,
    stage: Stage,
    requester: u64,
}

enum Done {
    /// One repository's listing (its id).
    Refreshed(String, Result<(Vec<Listing>, Vec<String>), String>),
    /// Every repository was asked.
    RefreshEnd,
    /// A repository URL was looked up: who asked, whether to add it at once, the URL.
    Probed(Asker, bool, String, Result<repos::Probe, String>),
    Assets(Result<(Vec<u8>, String, crate::assets::Index), String>),
    AssetFetched(String, Result<Vec<u8>, String>),
    Progress(String, u32),
    Fetched(String, Result<(), String>),
}

/// A client waiting for an asset.
struct Waiter {
    token: u64,
    name: String,
    previewed: bool,
}

pub struct Daemon {
    state: State,
    trusted: Arc<Vec<Trusted>>,
    /// None: no store configured (offline firmware).
    store: Option<String>,
    store_ok_once: bool,
    apps: BTreeMap<String, App>,
    conns: Vec<Conn>,
    next_token: u64,
    listener: OwnedFd,
    wake_rd: OwnedFd,
    wake_wr: Arc<OwnedFd>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    running: Vec<Running>,
    display: Display,
    pending: Option<Pending>,
    refreshing: bool,
    next_refresh: Instant,
    store_error: Option<String>,
    /// Logs of apps that exited (last one per app), for `appctl logs`.
    old_logs: BTreeMap<String, Vec<u8>>,
    /// Verified asset index (from the store, or the copy saved on flash), and its bytes.
    asset_index: Option<(crate::assets::Index, Vec<u8>)>,
    /// The index came from the store during this run (not only the copy saved on flash): only
    /// then is a name it lacks really unknown. Before, the client waits (an older saved index
    /// may predate the asset).
    asset_index_fresh: bool,
    asset_error: Option<String>,
    /// Clients waiting for an asset (index not known yet, or download running).
    asset_waiters: Vec<Waiter>,
    asset_fetching: Vec<String>,
    /// Assets handed out (ASSET_UPDATED goes to the launcher when they change).
    assets_served: Vec<String>,
    /// Waiters left after a failed download: try again then (doubling backoff).
    asset_retry_at: Option<Instant>,
    asset_backoff: Duration,
    /// Last http::not_ready result: a change to "ready" refreshes at once.
    net_ready: bool,
    /// Dial sockets (the network service's ends) waiting for the service to run:
    /// (client id, hosts, socket).
    net_held: Vec<(String, String, OwnedFd)>,
    net_retry_at: Option<Instant>,
    /// Payload hash of the running network service (an update is swapped in when no app
    /// is using it).
    net_running: Option<String>,
    /// Sockets (the image service's ends) waiting for the service to run: (client id, socket).
    img_held: Vec<(String, OwnedFd)>,
    /// The image service could not be fetched or started: requests are refused until then.
    img_retry_at: Option<Instant>,
    /// The system volume (/data/appd/volume).
    volume: Volume,
    volume_save_at: Option<Instant>,
    /// The apps opened last (/data/appd/recents).
    recents: Recents,
    /// Background services by app id: when to start them (again).
    bg: BTreeMap<String, Supervisor>,
    /// An app is to be opened, whatever is in front: (app id, payload, when). A background
    /// service asked (with a payload), or the Store app did for an app the user chose.
    fg_request: Option<(String, Option<String>, Instant)>,
    /// When each app's service last asked (rate limit).
    fg_asked: BTreeMap<String, Instant>,
    /// The Miracast receiver an app asked for (miracast.rs; netd does the Wi-Fi Direct work).
    miracast: crate::miracast::Miracast,
    /// The payload for the next launch of this app (MEOW_LAUNCH_PAYLOAD).
    launch_payload: Option<(String, String)>,
    /// What running instances attached for their next foreground request (`ATTACH`), by
    /// instance name: see handover.rs.
    attached: BTreeMap<String, crate::handover::Handover>,
    /// The sockets and data that go with `fg_request`, then with `launch_payload`: the app
    /// inherits them. Closed here when the launch does not happen.
    fg_handover: Option<(String, crate::handover::Handover)>,
    /// The APPINFO line last sent to the UI per app (sent again only when it changes).
    info_sent: BTreeMap<String, String>,
    /// The repositories the user added (/data/appd/repos.json).
    repos: Vec<RepoState>,
    /// The repository URL last looked up: what `add` may add.
    probe: Option<repos::Probe>,
    /// A refresh was asked for while one was running.
    refresh_again: bool,
    /// The apps the UI knows (it gets installed apps only; REMOVE when one leaves).
    ui_listed: BTreeSet<String>,
}

fn bg_key(id: &str) -> String {
    format!("{id}{BG_SUFFIX}")
}

fn log(msg: &str) {
    eprintln!("APPD {msg}");
}

fn write_file(p: impl AsRef<Path>, s: &str) -> Result<(), String> {
    std::fs::write(p.as_ref(), s).map_err(|e| format!("{}: {e}", p.as_ref().display()))
}

pub fn run() -> ! {
    match Daemon::new() {
        Ok(mut d) => d.main_loop(),
        Err(e) => {
            log(&format!("FATAL {e}"));
            std::process::exit(1);
        }
    }
}

impl Daemon {
    fn new() -> Result<Daemon, String> {
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
        let state = State::open(Path::new("/data"))?;
        // APPD_KEYS: development override (root only can set it; appd-session passes it).
        let keys = std::env::var("APPD_KEYS").unwrap_or_else(|_| KEYS_DIR.into());
        let trusted = crate::sign::load_trusted(Path::new(&keys))?;
        if trusted.is_empty() {
            return Err(format!("no trusted store keys in {keys}"));
        }
        for t in &trusted {
            log(&format!("trusted key {} ({})", t.id, t.file));
        }
        let trusted = Arc::new(trusted);
        // The repositories the user added, each with the one key pinned when it was added.
        let mut keys: BTreeMap<String, Arc<Vec<Trusted>>> = BTreeMap::new();
        keys.insert(DEFAULT_ID.to_string(), trusted.clone());
        let (added, dropped) = repos::load(&state.root);
        for d in dropped {
            log(&format!("repository entry ignored: {d}"));
        }
        let mut repo_states = Vec::new();
        for repo in added {
            match repo.trusted() {
                Ok(t) => {
                    log(&format!("repository {} {} ({}), key {}{}", repo.id, repo.url, repo.name, t[0].id, if repo.enabled { "" } else { ", switched off" }));
                    let t = Arc::new(t);
                    keys.insert(repo.id.clone(), t.clone());
                    repo_states.push(RepoState { repo, trusted: t, error: None, seen: false });
                }
                Err(e) => log(&format!("repository {} ignored: {e}", repo.url)),
            }
        }
        let cfg = state.config();
        let baked = std::fs::read_to_string(STORE_CONF).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let store = std::env::var("APPD_STORE").ok().or(cfg.store).or(baked).map(|s| if s.ends_with('/') { s } else { format!("{s}/") });
        if let Some(u) = &store {
            crate::http::parse(u).map_err(|e| format!("store URL: {e}"))?;
        }
        std::fs::create_dir_all("/run/appd").map_err(|e| e.to_string())?;
        std::fs::set_permissions("/run/appd", std::os::unix::fs::PermissionsExt::from_mode(0o700)).ok();
        setup_ram_dir()?;
        setup_cgroups()?;
        let listener = ipc::listen(SOCKET).map_err(|e| format!("{SOCKET}: {e}"))?;
        let mut p = [0; 2];
        if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            return Err("pipe".into());
        }
        let (wake_rd, wake_wr) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
        let (tx, rx) = mpsc::channel();
        let mut d = Daemon {
            state,
            trusted,
            store,
            store_ok_once: false,
            apps: BTreeMap::new(),
            conns: Vec::new(),
            next_token: 1,
            listener,
            wake_rd,
            wake_wr: Arc::new(wake_wr),
            tx,
            rx,
            running: Vec::new(),
            display: Display::Free,
            pending: None,
            refreshing: false,
            next_refresh: Instant::now() + Duration::from_millis(500),
            store_error: None,
            old_logs: BTreeMap::new(),
            asset_index: None,
            asset_index_fresh: false,
            asset_error: None,
            asset_waiters: Vec::new(),
            asset_fetching: Vec::new(),
            assets_served: Vec::new(),
            asset_retry_at: None,
            asset_backoff: ASSET_RETRY_MIN,
            net_ready: false,
            net_held: Vec::new(),
            net_retry_at: None,
            net_running: None,
            img_held: Vec::new(),
            img_retry_at: None,
            volume: Volume::default(),
            volume_save_at: None,
            recents: Recents::default(),
            bg: BTreeMap::new(),
            fg_request: None,
            fg_asked: BTreeMap::new(),
            miracast: Default::default(),
            launch_payload: None,
            attached: BTreeMap::new(),
            fg_handover: None,
            info_sent: BTreeMap::new(),
            repos: repo_states,
            probe: None,
            refresh_again: false,
            ui_listed: BTreeSet::new(),
        };
        d.volume = d.state.volume();
        d.recents = d.state.recents();
        if let Some(bytes) = crate::assets::drop_flash_cache() {
            log(&format!("removed the flash asset cache of earlier firmware ({bytes} bytes freed)"));
        }
        d.asset_index = crate::assets::load_saved(&d.state.root, &d.trusted);
        if let Some((idx, _)) = &d.asset_index {
            log(&format!("asset index from flash: {} asset(s)", idx.assets.len()));
        }
        let (inst, errs) = d.state.load_installed(&keys);
        for e in errs {
            log(&format!("installed record rejected: {e}"));
        }
        for i in inst {
            let repo = repos::from_record(&i.source.repo).to_string();
            if let Some(e) = (repo != DEFAULT_ID).then(|| repos::allowed(&i.manifest).err()).flatten() {
                log(&format!("installed record rejected: {}: {e}", i.manifest.id));
                continue;
            }
            d.apps.insert(
                i.manifest.id.clone(),
                App {
                    manifest: i.manifest,
                    manifest_bytes: i.manifest_bytes,
                    signature: i.signature,
                    payload_url: i.source.payload_url,
                    repo,
                    installed: true,
                    grants: i.grants,
                    in_store: false,
                    update_blocked: None,
                    status: Status::Idle,
                },
            );
        }
        // Apps that are gone leave the recents (in memory; the file follows the next change).
        d.recents.retain(|id| d.apps.get(id).is_some_and(|a| a.installed && a.manifest.service.is_none()));
        log(&format!(
            "ready: {} installed, store {}, {} added repositories, data {}",
            d.apps.len(),
            d.store.as_deref().unwrap_or("(none)"),
            d.repos.len(),
            if d.state.volatile { "VOLATILE (tmpfs)" } else { "flash" }
        ));
        Ok(d)
    }

    // ---------------------------------------------------------------- main loop

    fn main_loop(&mut self) -> ! {
        loop {
            let now = Instant::now();
            self.maybe_refresh(now);
            self.timers(now);
            let mut fds = vec![
                libc::pollfd { fd: self.listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.wake_rd.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            for c in &self.conns {
                fds.push(libc::pollfd { fd: c.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
            }
            let base_r = fds.len();
            // Snapshot: handlers below may start or remove apps.
            let run_ids: Vec<String> = self.running.iter().map(|r| r.id.clone()).collect();
            for r in &self.running {
                for fd in [r.pidfd.as_raw_fd(), r.broker.as_raw_fd(), r.log_rd.as_raw_fd()] {
                    fds.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
                }
            }
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 250) };
            if fds[0].revents != 0 {
                self.accept();
            }
            if fds[1].revents != 0 {
                let mut b = [0u8; 64];
                while unsafe { libc::read(self.wake_rd.as_raw_fd(), b.as_mut_ptr() as *mut _, b.len()) } > 0 {}
                while let Ok(d) = self.rx.try_recv() {
                    self.job_done(d);
                }
            }
            let conn_ready: Vec<u64> =
                self.conns.iter().zip(&fds[2..base_r]).filter(|(_, p)| p.revents != 0).map(|(c, _)| c.token).collect();
            for t in conn_ready {
                self.conn_readable(t);
            }
            let mut exited = Vec::new();
            let mut brokers = Vec::new();
            let mut logs = Vec::new();
            for (i, id) in run_ids.iter().enumerate() {
                let p = &fds[base_r + i * 3..base_r + i * 3 + 3];
                if p[0].revents != 0 {
                    exited.push(id.clone());
                }
                if p[1].revents != 0 {
                    brokers.push(id.clone());
                }
                if p[2].revents != 0 {
                    logs.push(id.clone());
                }
            }
            for id in logs {
                self.read_log(&id);
            }
            for id in brokers {
                self.broker_readable(&id);
            }
            for id in exited {
                self.on_exit(&id);
            }
        }
    }

    fn timers(&mut self, now: Instant) {
        if let Display::Releasing { id, since } = &self.display {
            if now.duration_since(*since) > DISPLAY_TIMEOUT {
                let id = id.clone();
                self.display = Display::Free;
                self.fail_pending(&id, "the launcher did not release the screen");
            }
        }
        if let Display::Reclaiming { since, .. } = &self.display {
            if now.duration_since(*since) > DISPLAY_TIMEOUT {
                log("UI did not reclaim the screen in time; closing the app's display");
                self.display = Display::Free;
            }
        }
        let kill: Vec<String> = self.running.iter().filter(|r| r.stop_at.is_some_and(|t| now >= t)).map(|r| r.id.clone()).collect();
        for id in kill {
            log(&format!("{id}: did not quit in time, killing"));
            if let Some(r) = self.running.iter_mut().find(|r| r.id == id) {
                let _ = write_file(r.cgroup.join("cgroup.kill"), "1");
                r.stop_at = None;
            }
        }
        let due: Vec<String> = self.running.iter().filter(|r| r.sync_due.is_some_and(|t| now >= t)).map(|r| r.id.clone()).collect();
        for id in due {
            let _ = self.sync_storage(&id, false);
        }
        if !self.net_held.is_empty() {
            self.net_ensure();
        }
        if !self.img_held.is_empty() {
            self.img_ensure();
        }
        if self.volume_save_at.is_some_and(|t| now >= t) {
            self.volume_save_at = None;
            if self.state.volume() != self.volume {
                if let Err(e) = self.state.save_volume(self.volume) {
                    log(&format!("volume not saved: {e}"));
                }
            }
        }
        self.fg_tick(now);
        if let Some((app, state)) = self.miracast.tick(now) {
            for r in self.running.iter().filter(|r| r.app == app) {
                let _ = ipc::send(r.broker.as_raw_fd(), &format!("MIRACAST\t{state}"), None);
            }
        }
        self.bg_ensure(now);
        if self.asset_retry_at.is_some_and(|t| now >= t) && self.net_ready {
            self.asset_retry_at = None;
            self.serve_waiters(None);
        }
    }

    fn store_url(&self) -> String {
        self.store.clone().unwrap_or_default()
    }

    /// Start a refresh when one is due and the network is there. Without a network nothing
    /// is spawned: the check is a /proc read, done every NET_POLL.
    fn maybe_refresh(&mut self, now: Instant) {
        if self.refreshing || self.store.is_none() {
            return;
        }
        let why = crate::http::not_ready(&self.store_url());
        let ready = why.is_none();
        if ready && !self.net_ready {
            // The network just came up: don't wait for the retry backoff.
            self.next_refresh = self.next_refresh.min(now);
            if !self.asset_waiters.is_empty() {
                self.asset_retry_at = Some(now); // timers() serves them on this pass
            }
        }
        self.net_ready = ready;
        if now < self.next_refresh {
            return;
        }
        match why {
            None => self.start_refresh(),
            Some(w) => {
                self.store_error = Some(w.to_string());
                self.next_refresh = now + NET_POLL;
            }
        }
    }

    fn wake(&self) -> impl Fn() + Send + 'static {
        let w = self.wake_wr.clone();
        move || unsafe {
            libc::write(w.as_raw_fd(), b"x".as_ptr() as *const _, 1);
        }
    }

    // ---------------------------------------------------------------- store

    fn start_refresh(&mut self) {
        self.refreshing = true;
        let (tx, wake, url, trusted) = (self.tx.clone(), self.wake(), self.store_url(), self.trusted.clone());
        let others: Vec<(String, String, Arc<Vec<Trusted>>)> = self.repos.iter().filter(|s| s.repo.enabled).map(|s| (s.repo.id.clone(), s.repo.url.clone(), s.trusted.clone())).collect();
        std::thread::spawn(move || {
            // One connection for everything; the asset index first, so the wallpaper download
            // (its own connection, in parallel) starts while the app list is still loading.
            let t = Instant::now();
            let mut c = crate::http::Client::new();
            let assets = store::fetch_assets_index(&mut c, &url, &trusted);
            let _ = tx.send(Done::Assets(assets));
            wake();
            let apps = store::refresh(&mut c, &url, &trusted);
            if apps.is_ok() {
                log(&format!("refresh: {} request(s) on {} connection(s) in {} ms", c.requests, c.connects, t.elapsed().as_millis()));
            }
            let _ = tx.send(Done::Refreshed(DEFAULT_ID.to_string(), apps));
            wake();
            // Then the repositories the user added, each on its own connection and checked
            // against its own key. One that fails does not hold the others back.
            for (id, url, keys) in others {
                let listing = store::refresh(&mut crate::http::Client::new(), &url, &keys);
                let _ = tx.send(Done::Refreshed(id, listing));
                wake();
            }
            let _ = tx.send(Done::RefreshEnd);
            wake();
        });
    }

    fn job_done(&mut self, d: Done) {
        match d {
            Done::Refreshed(repo, r) if repo != DEFAULT_ID => self.repo_refreshed(&repo, r),
            Done::RefreshEnd => {
                self.refreshing = false;
                if std::mem::take(&mut self.refresh_again) {
                    self.start_refresh();
                }
            }
            Done::Probed(asker, add, url, r) => match r {
                Ok(p) => {
                    let fingerprint = repos::fingerprint(&p.key);
                    log(&format!("repository {}: {:?}, key fingerprint {fingerprint}, {} app(s)", p.url, p.name, p.apps));
                    self.probe = Some(p.clone());
                    if add {
                        let r = self.repo_add(&p.url, &fingerprint);
                        self.answer(&asker, "add", &p.url, r);
                    } else {
                        self.answer(&asker, "probe", &p.url, Ok(format!("{}\t{fingerprint}\t{}", field(&p.name), p.apps)));
                    }
                }
                Err(e) => self.answer(&asker, if add { "add" } else { "probe" }, &url, Err(e)),
            },
            Done::Refreshed(_, r) => {
                match r {
                    Ok((listings, errors)) => {
                        for e in &errors {
                            log(&format!("store entry rejected: {e}"));
                        }
                        for l in &listings {
                            log(&format!("store: {} {} (code {}) signed by {}", l.manifest.id, l.manifest.version, l.manifest.version_code, l.key_id));
                        }
                        self.store_error = None;
                        self.store_ok_once = true;
                        self.merge_store(DEFAULT_ID, listings);
                        self.next_refresh = Instant::now() + REFRESH_EVERY;
                    }
                    Err(e) => {
                        if self.store_error.as_deref() != Some(&e) {
                            log(&format!("store unreachable: {e}"));
                        }
                        self.store_error = Some(e);
                        self.next_refresh = Instant::now() + if self.store_ok_once { REFRESH_RETRY } else { REFRESH_RETRY_FIRST };
                    }
                }
                self.store_push_repo(DEFAULT_ID);
            }
            Done::Assets(r) => self.assets_refreshed(r),
            Done::AssetFetched(name, r) => self.asset_fetched(&name, r),
            Done::Progress(id, pct) => {
                if let Some(a) = self.apps.get_mut(&id) {
                    a.status = Status::Fetching(pct);
                }
                self.push_app(&id);
            }
            Done::Fetched(id, r) => {
                let pending_here = self.pending.as_ref().is_some_and(|p| p.id == id && matches!(p.stage, Stage::Fetch));
                match r {
                    Ok(()) => {
                        if let Some(a) = self.apps.get_mut(&id) {
                            a.status = Status::Idle;
                        }
                        if pending_here {
                            self.continue_launch(&id);
                        }
                    }
                    Err(e) => {
                        log(&format!("{id}: download failed: {e}"));
                        if let Some(a) = self.apps.get_mut(&id).filter(|a| matches!(a.status, Status::Fetching(_))) {
                            a.status = Status::Idle;
                        }
                        if pending_here {
                            let why = self.download_problem(&id);
                            self.fail_pending(&id, &why);
                        }
                        if self.net_service().as_deref() == Some(id.as_str()) {
                            self.net_retry_at = Some(Instant::now() + NET_RETRY);
                        }
                        if self.image_service().as_deref() == Some(id.as_str()) {
                            self.img_failed();
                        }
                        if self.bg_wanted(&id) {
                            // The service's payload did not arrive: try again later.
                            self.bg.entry(id.clone()).or_default().start_failed(Instant::now());
                        }
                    }
                }
                self.push_app(&id);
                if self.is_service(&id) {
                    self.net_swap();
                    self.net_ensure();
                    self.img_ensure();
                }
            }
        }
    }

    /// Take in what repository `repo` lists now.
    fn merge_store(&mut self, repo: &str, listings: Vec<Listing>) {
        let mut seen = Vec::new();
        for l in listings {
            let id = l.manifest.id.clone();
            // An app id belongs to one repository: an installed app keeps the one it was
            // installed from, and nothing takes an id away from the default repository.
            if let Some(a) = self.apps.get(&id).filter(|a| a.repo != repo) {
                if a.installed || repo != DEFAULT_ID {
                    log(&format!("{repo}: {id} is already provided by {}: ignored", a.repo));
                    continue;
                }
                self.apps.remove(&id);
            }
            seen.push(id.clone());
            match self.apps.get_mut(&id) {
                None => {
                    self.apps.insert(
                        id.clone(),
                        App {
                            manifest: l.manifest,
                            manifest_bytes: l.manifest_bytes,
                            signature: l.signature,
                            payload_url: l.payload_url,
                            repo: repo.to_string(),
                            installed: false,
                            grants: Grants::default(),
                            in_store: true,
                            update_blocked: None,
                            status: Status::Idle,
                        },
                    );
                }
                Some(a) => {
                    a.in_store = true;
                    let newer = l.manifest.version_code > a.manifest.version_code;
                    if !a.installed {
                        if l.manifest.version_code >= a.manifest.version_code {
                            (a.manifest, a.manifest_bytes, a.signature, a.payload_url) = (l.manifest, l.manifest_bytes, l.signature, l.payload_url);
                        }
                    } else if newer && (a.status != Status::Running || a.manifest.service.is_some()) {
                        // (A running service takes the new record now; net_swap restarts it
                        // on the new payload when no app is using it.)
                        // Automatic update only when it asks for nothing beyond what the user
                        // granted (same rule as a fresh install's normal permissions).
                        let extra: Vec<String> = l
                            .manifest
                            .permissions
                            .iter()
                            .filter(|r| perms::level(&r.name, r.quota_kb.unwrap_or(0)) == Level::Sensitive && !a.grants.has(&r.name))
                            // "internet" is less than the "network" the user already allowed.
                            .filter(|r| !(r.name == "internet" && a.grants.has("network")))
                            // An optional one does not hold the update back: it stays
                            // undecided (not granted) until the user is asked at the next
                            // launch, or decides in Settings > Apps.
                            .filter(|r| r.required)
                            .map(|r| r.name.clone())
                            .collect();
                        if extra.is_empty() {
                            let mut g = a.grants.clone();
                            for r in &l.manifest.permissions {
                                let covered = r.name == "internet" && g.has("network") && !g.denied.contains(&r.name);
                                if (perms::level(&r.name, r.quota_kb.unwrap_or(0)) == Level::Normal || covered) && !g.has(&r.name) {
                                    g.granted.push(r.name.clone());
                                }
                            }
                            g.granted.retain(|p| l.manifest.request(p).is_some());
                            g.denied.retain(|p| l.manifest.request(p).is_some());
                            match self.state.install(&l.manifest, &l.manifest_bytes, &l.signature, &Source { payload_url: l.payload_url.clone(), repo: repos::to_record(repo) }, &g) {
                                Ok(()) => {
                                    log(&format!("{id}: updated {} -> {}", a.manifest.version, l.manifest.version));
                                    (a.manifest, a.manifest_bytes, a.signature, a.payload_url, a.grants) =
                                        (l.manifest, l.manifest_bytes, l.signature, l.payload_url, g);
                                    a.update_blocked = None;
                                }
                                Err(e) => log(&format!("{id}: update refused: {e}")),
                            }
                        } else {
                            a.update_blocked = Some(l.manifest.version_code);
                            log(&format!("{id}: update {} needs approval for {}", l.manifest.version, extra.join(",")));
                        }
                    } else if !newer {
                        a.payload_url = l.payload_url;
                    }
                }
            }
        }
        let gone: Vec<String> = self.apps.iter().filter(|(id, a)| a.repo == repo && !a.installed && !seen.contains(id)).map(|(id, _)| id.clone()).collect();
        for id in gone {
            self.app_gone(&id);
        }
        for a in self.apps.values_mut() {
            if a.repo == repo && a.installed && !seen.contains(&a.manifest.id) {
                a.in_store = false;
            }
        }
        // System services are installed without asking: they are part of the system, signed
        // like everything else, and get exactly what their manifest lists.
        // So is the Store app (`system_app`): without it nothing else could be installed.
        let services: Vec<String> =
            self.apps.iter().filter(|(_, a)| (a.manifest.service.is_some() || Self::system_app(a)) && a.repo == DEFAULT_ID && !a.installed && a.in_store).map(|(id, _)| id.clone()).collect();
        for id in services {
            let installed = Self::grants_for(&self.apps[&id].manifest, true).and_then(|g| self.do_install(&id, g));
            if let Err(e) = installed {
                log(&format!("{id}: system part not installed: {e}"));
            }
        }
        let ids: Vec<String> = self.apps.iter().filter(|(_, a)| a.repo == repo).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.push_app(&id);
        }
        self.broadcast_ui("APPS_END");
        self.net_prefetch();
        self.net_swap();
        self.net_ensure();
    }

    // ---------------------------------------------------------------- repositories and the Store app

    /// An app that is part of the system: the Store app. It comes from the built-in
    /// repository, is installed without asking and cannot be uninstalled.
    fn system_app(a: &App) -> bool {
        a.repo == DEFAULT_ID && a.manifest.service.is_none() && a.manifest.request("manage-apps").is_some()
    }

    fn refresh_soon(&mut self) {
        if self.refreshing {
            self.refresh_again = true;
        } else {
            self.start_refresh();
        }
    }

    fn repo_refreshed(&mut self, repo: &str, r: Result<(Vec<Listing>, Vec<String>), String>) {
        // (Removed or switched off while the refresh ran: nothing to do.)
        let Some(i) = self.repos.iter().position(|s| s.repo.id == repo && s.repo.enabled) else { return };
        match r {
            Ok((listings, errors)) => {
                for e in &errors {
                    log(&format!("{repo}: entry rejected: {e}"));
                }
                let mut kept = Vec::new();
                for l in listings {
                    match repos::allowed(&l.manifest) {
                        Ok(()) => kept.push(l),
                        Err(e) => log(&format!("{repo}: {} rejected: {e}", l.manifest.id)),
                    }
                }
                log(&format!("{repo}: {} app(s) from {}", kept.len(), self.repos[i].repo.url));
                self.repos[i].error = None;
                self.repos[i].seen = true;
                self.merge_store(repo, kept);
            }
            Err(e) => {
                if self.repos[i].error.as_deref() != Some(&e) {
                    log(&format!("{repo}: unreachable: {e}"));
                }
                self.repos[i].error = Some(e);
            }
        }
        self.store_push_repo(repo);
    }

    fn save_repos(&self) -> Result<(), String> {
        let list: Vec<Repo> = self.repos.iter().map(|s| s.repo.clone()).collect();
        repos::save(&self.state.root, &list)
    }

    fn repo_known(&self, url: &str) -> Result<(), String> {
        if url == self.store_url() || self.repos.iter().any(|s| s.repo.url == url) {
            return Err("this repository is already on the list".into());
        }
        if self.repos.len() >= repos::MAX_REPOS {
            return Err(format!("the list is full ({} repositories): remove one first", repos::MAX_REPOS));
        }
        Ok(())
    }

    /// Look a repository URL up (on a worker thread); `add`: and add it at once (appctl).
    fn repo_probe(&mut self, asker: Asker, input: &str, add: bool) {
        let what = if add { "add" } else { "probe" };
        let url = match repos::clean_url(input) {
            Ok(url) => url,
            Err(e) => return self.answer(&asker, what, input, Err(e)),
        };
        if let Err(e) = self.repo_known(&url) {
            return self.answer(&asker, what, &url, Err(e));
        }
        if crate::http::not_ready(&url).is_some() {
            return self.answer(&asker, what, &url, Err("no internet connection".into()));
        }
        let (tx, wake) = (self.tx.clone(), self.wake());
        std::thread::spawn(move || {
            let r = repos::probe(&url);
            let _ = tx.send(Done::Probed(asker, add, url, r));
            wake();
        });
    }

    /// Add the repository that was just looked up. The caller names the fingerprint the
    /// user saw, so what is added is what was shown.
    fn repo_add(&mut self, url: &str, fingerprint: &str) -> Result<String, String> {
        let p = self.probe.clone().filter(|p| p.url == url && repos::fingerprint(&p.key) == fingerprint).ok_or("look the address up again first")?;
        self.repo_known(&p.url)?;
        let repo = Repo::from_probe(&p);
        let trusted = Arc::new(repo.trusted()?);
        let id = repo.id.clone();
        self.repos.push(RepoState { repo, trusted, error: None, seen: false });
        if let Err(e) = self.save_repos() {
            self.repos.pop();
            return Err(e);
        }
        self.probe = None;
        log(&format!("repository {id} added: {} ({:?}), key fingerprint {fingerprint}", p.url, p.name));
        self.store_push_repo(&id);
        self.refresh_soon();
        Ok(format!("added {} as {id}, key fingerprint {fingerprint}", field(&p.name)))
    }

    /// Remove a repository the user added, with the apps installed from it and their data.
    fn repo_remove(&mut self, id: &str) -> Result<String, String> {
        if id == DEFAULT_ID {
            return Err(format!("the {} is part of the system", repos::DEFAULT_NAME));
        }
        let i = self.repos.iter().position(|s| s.repo.id == id).ok_or_else(|| format!("unknown repository {id}"))?;
        let mine: Vec<String> = self.apps.iter().filter(|(_, a)| a.repo == id).map(|(app, _)| app.clone()).collect();
        if let Some(open) = mine.iter().find(|app| self.running.iter().any(|r| r.id == **app)) {
            return Err(format!("{} is open: close it first", self.apps[open].manifest.name));
        }
        for app in &mine {
            if self.apps.get(app).is_some_and(|a| a.installed) {
                self.uninstall(app)?;
            }
        }
        for app in &mine {
            if self.apps.contains_key(app) {
                self.app_gone(app);
            }
        }
        let gone = self.repos.remove(i);
        self.save_repos()?;
        log(&format!("repository {id} removed ({})", gone.repo.url));
        self.store_send(&format!("STORE\trepo-gone\t{id}"));
        Ok("removed".into())
    }

    /// Switch a repository off (its apps leave the list; installed ones stay and work, but
    /// get no updates) or on again.
    fn repo_enable(&mut self, id: &str, on: bool) -> Result<String, String> {
        if id == DEFAULT_ID {
            return Err(format!("the {} is always on", repos::DEFAULT_NAME));
        }
        let i = self.repos.iter().position(|s| s.repo.id == id).ok_or_else(|| format!("unknown repository {id}"))?;
        if self.repos[i].repo.enabled != on {
            self.repos[i].repo.enabled = on;
            if let Err(e) = self.save_repos() {
                self.repos[i].repo.enabled = !on;
                return Err(e);
            }
            (self.repos[i].error, self.repos[i].seen) = (None, false);
            log(&format!("repository {id} switched {}", if on { "on" } else { "off" }));
            if on {
                self.refresh_soon();
            } else {
                let offered: Vec<String> = self.apps.iter().filter(|(_, a)| a.repo == id && !a.installed).map(|(app, _)| app.clone()).collect();
                for app in offered {
                    self.app_gone(&app);
                }
                for a in self.apps.values_mut().filter(|a| a.repo == id) {
                    a.in_store = false;
                }
            }
        }
        self.store_push_repo(id);
        Ok(if on { "on" } else { "off" }.into())
    }

    /// `STORE repo ...`: one repository as the Store app shows it.
    fn store_repo_line(&self, id: &str) -> Option<String> {
        let apps = self.apps.values().filter(|a| a.repo == id && a.manifest.service.is_none()).count();
        if id == DEFAULT_ID {
            let (state, detail) = match (&self.store, &self.store_error, self.store_ok_once) {
                (None, _, _) => ("error", "this firmware has no app store".to_string()),
                (_, Some(e), _) => ("error", e.clone()),
                (_, None, true) => ("ok", String::new()),
                (_, None, false) => ("waiting", String::new()),
            };
            let key = self.trusted.first().map(|t| repos::spaced(&t.id)).unwrap_or_default();
            return Some(format!("STORE\trepo\t{DEFAULT_ID}\t{}\t{}\t{key}\t1\t1\t{state}\t{}\t{apps}", repos::DEFAULT_NAME, field(&self.store_url()), field(&detail)));
        }
        let s = self.repos.iter().find(|s| s.repo.id == id)?;
        let (state, detail) = match (s.repo.enabled, &s.error, s.seen) {
            (false, _, _) => ("off", String::new()),
            (_, Some(e), _) => ("error", e.clone()),
            (_, None, true) => ("ok", String::new()),
            (_, None, false) => ("waiting", String::new()),
        };
        Some(format!("STORE\trepo\t{id}\t{}\t{}\t{}\t{}\t0\t{state}\t{}\t{apps}", field(&s.repo.name), field(&s.repo.url), repos::fingerprint(&s.repo.key), s.repo.enabled as u8, field(&detail)))
    }

    fn store_repo_lines(&self) -> Vec<String> {
        std::iter::once(DEFAULT_ID.to_string()).chain(self.repos.iter().map(|s| s.repo.id.clone())).filter_map(|id| self.store_repo_line(&id)).collect()
    }

    /// `STORE app ...`: one app as the Store app shows it, installed or not.
    fn store_app_line(&self, id: &str) -> String {
        let a = &self.apps[id];
        let m = &a.manifest;
        let (state, detail) = Self::app_state(a);
        let ap = &m.appearance;
        let head = format!(
            "STORE\tapp\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{state}\t{}\t{}",
            m.id,
            a.repo,
            field(&m.name),
            field(&m.summary),
            field(&m.publisher),
            field(&m.version),
            m.payload.size.div_ceil(1024),
            ap.glyph,
            ap.tint,
            ap.badge[0],
            ap.badge[1],
            field(&detail),
            if Self::system_app(a) { "system" } else { "-" }
        );
        let clean = |s: &str| field(s).replace('|', "/");
        let perms = |reasons: bool| -> String {
            m.permissions
                .iter()
                .map(|r| {
                    let q = r.quota_kb.unwrap_or(0);
                    format!(
                        "\t{}|{}|{}|{}|{}|{}|{}",
                        r.name,
                        perms::level(&r.name, q).as_str(),
                        r.required as u8,
                        if a.installed { Self::grant_state(&a.grants, &r.name) } else { "-" },
                        clean(&Self::perm_label(r)),
                        if reasons { clean(&r.reason) } else { String::new() },
                        clean(&self.perm_extra(m, r))
                    )
                })
                .collect()
        };
        // One message holds it all, or the reasons are left out.
        let full = format!("{head}{}", perms(true));
        if full.len() <= ipc::MAX_MSG - 64 { full } else { format!("{head}{}", perms(false)) }
    }

    /// To every subscribed Store app.
    fn store_send(&self, msg: &str) {
        for r in self.running.iter().filter(|r| r.store_sub && r.stop_at.is_none()) {
            let _ = ipc::send(r.broker.as_raw_fd(), msg, None);
        }
    }

    fn store_push_repo(&self, id: &str) {
        if let Some(l) = self.store_repo_line(id) {
            self.store_send(&l);
        }
    }

    /// An app left the lists altogether (not installed, and no repository offers it).
    fn app_gone(&mut self, id: &str) {
        self.apps.remove(id);
        self.info_sent.remove(id);
        if self.ui_listed.remove(id) {
            self.broadcast_ui(&format!("REMOVE\t{id}"));
        }
        self.store_send(&format!("STORE\tgone\t{id}"));
    }

    /// The answer to something a control client or the Store app asked for.
    fn answer(&mut self, asker: &Asker, what: &str, target: &str, r: Result<String, String>) {
        match asker {
            Asker::Conn(token) => self.reply(*token, r),
            Asker::App(instance) => {
                let Some(broker) = self.running.iter().find(|r| r.id == *instance).map(|r| r.broker.as_raw_fd()) else { return };
                let (status, text) = match r {
                    Ok(text) => ("ok", text),
                    Err(e) => ("error", field(&e)),
                };
                let _ = ipc::send(broker, &format!("STORE\tresult\t{}\t{}\t{status}\t{text}", field(what), field(target)), None);
            }
        }
    }

    /// The Store app installs an app: `allow` is the user's answer to the permission
    /// question the Store app showed (as the launcher's dialog was before it).
    fn store_install(&mut self, id: &str, allow: bool) -> Result<String, String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if a.manifest.service.is_some() {
            return Err(format!("{id} is a system service"));
        }
        if a.installed {
            return Ok("already installed".into());
        }
        let g = Self::grants_for(&a.manifest, allow)?;
        self.do_install(id, g)?;
        // Payloads live in RAM: fetch it now, so that opening the app does not wait.
        let a = &self.apps[id];
        if self.net_ready && !a.payload_url.is_empty() && !matches!(a.status, Status::Fetching(_)) && !store::payload_present(&a.manifest, &payload_dir(id)) {
            self.start_fetch(id);
            self.push_app(id);
        }
        Ok("installed".into())
    }

    /// The Store app opens an app the user chose: the Store closes, the launcher takes the
    /// screen back and the app starts the usual way.
    fn store_open(&mut self, id: &str) -> Result<String, String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if !a.installed || a.manifest.service.is_some() {
            return Err(format!("{} is not installed", a.manifest.name));
        }
        if self.running.iter().any(|r| r.id == id) {
            return Err(format!("{} is already open", a.manifest.name));
        }
        if self.pending.is_some() || self.fg_request.is_some() {
            return Err("another app is being opened".into());
        }
        self.fg_request = Some((id.to_string(), None, Instant::now()));
        Ok("opening".into())
    }

    /// A `STORE ...` request of the Store app `instance` (it holds "manage-apps").
    fn store_request(&mut self, instance: &str, f: &[&str]) {
        let arg = |i: usize| f.get(i).copied().unwrap_or("");
        let asker = Asker::App(instance.to_string());
        let (what, target) = (arg(1), arg(2));
        let r = match what {
            "subscribe" => {
                let Some(r) = self.running.iter_mut().find(|r| r.id == instance) else { return };
                r.store_sub = true;
                let broker = r.broker.as_raw_fd();
                let mut lines = self.store_repo_lines();
                lines.extend(self.apps.iter().filter(|(_, a)| a.manifest.service.is_none()).map(|(id, _)| self.store_app_line(id)));
                lines.push("STORE\tend".into());
                for l in lines {
                    let _ = ipc::send(broker, &l, None);
                }
                return;
            }
            "refresh" => {
                self.refresh_soon();
                return;
            }
            "probe" => return self.repo_probe(asker, target, false),
            "install" => self.store_install(target, arg(3) == "allow"),
            "uninstall" => self.uninstall(target),
            "open" => self.store_open(target),
            "add" => self.repo_add(target, arg(3)),
            "remove" => self.repo_remove(target),
            "enable" => self.repo_enable(target, arg(3) == "1"),
            other => Err(format!("unknown store request {}", field(other))),
        };
        if let Err(e) = &r {
            log(&format!("{instance}: store {what} {}: {e}", field(target)));
        }
        self.answer(&asker, what, target, r);
    }

    // ---------------------------------------------------------------- network service

    fn is_service(&self, id: &str) -> bool {
        self.apps.get(id).is_some_and(|a| a.manifest.service.is_some())
    }

    /// The installed network service.
    fn net_service(&self) -> Option<String> {
        self.apps.iter().find(|(_, a)| a.installed && a.manifest.service.as_deref() == Some("net")).map(|(id, _)| id.clone())
    }

    // ---------------------------------------------------------------- image service

    /// The installed image service.
    fn image_service(&self) -> Option<String> {
        self.apps.iter().find(|(_, a)| a.installed && a.manifest.service.as_deref() == Some("image")).map(|(id, _)| id.clone())
    }

    /// A connection to the image service for client `id`: the client's end. The service's
    /// end is held until the service runs (`img_ensure`). Any app may ask: the service
    /// turns bytes the app already has into pixels, and has nothing else to give.
    fn img_connect(&mut self, id: &str) -> Result<OwnedFd, String> {
        if self.image_service().is_none() {
            return Err("the image service is not installed (the store does not have it, or has not been reached)".into());
        }
        if self.img_retry_at.is_some_and(|t| Instant::now() < t) {
            return Err("the image service could not be started".into());
        }
        // A client that asks again has given up on its earlier sockets.
        while self.img_held.iter().filter(|h| h.0 == id).count() >= IMG_HELD_PER_CLIENT {
            let oldest = self.img_held.iter().position(|h| h.0 == id).unwrap();
            self.img_held.remove(oldest);
        }
        let (client, service) = std::os::unix::net::UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        self.img_held.push((id.to_string(), service.into()));
        Ok(client.into())
    }

    /// The image service cannot be had now: the waiting apps carry on without it (their
    /// sockets close), and requests are refused for a while.
    fn img_failed(&mut self) {
        self.img_held.clear();
        self.img_retry_at = Some(Instant::now() + IMG_RETRY);
    }

    /// Hand the held sockets to the image service, starting (and first fetching) it if
    /// needed: this is the only place that fetches or starts it, so it costs nothing until
    /// an app wants a picture decoded.
    fn img_ensure(&mut self) {
        if self.img_held.is_empty() {
            return;
        }
        let Some(sid) = self.image_service() else { return self.img_held.clear() };
        if let Some(r) = self.running.iter().find(|r| r.id == sid) {
            if r.stop_at.is_none() {
                let broker = r.broker.as_raw_fd();
                for (id, socket) in self.img_held.drain(..) {
                    if let Err(e) = ipc::send(broker, &format!("CLIENT\t{id}"), Some(socket.as_raw_fd())) {
                        log(&format!("{sid}: client {id} not passed: {e}"));
                    }
                }
            }
            return; // (stopping: on_exit starts it again for whoever waits)
        }
        let a = &self.apps[&sid];
        if matches!(a.status, Status::Fetching(_)) {
            return;
        }
        if !store::payload_present(&a.manifest, &payload_dir(&sid)) {
            if a.payload_url.is_empty() || !self.net_ready {
                log(&format!("{sid}: wanted, but there is no network to fetch it"));
                self.img_failed();
            } else {
                self.start_fetch(&sid);
            }
            return;
        }
        match self.try_spawn(&sid, false) {
            Ok(r) => {
                log(&format!("{sid}: image service started pid {}", r.child.id()));
                self.running.push(r);
                if let Some(a) = self.apps.get_mut(&sid) {
                    a.status = Status::Running;
                }
                self.img_ensure();
            }
            Err(e) => {
                log(&format!("{sid}: image service did not start: {e}"));
                self.img_failed();
            }
        }
    }

    // ---------------------------------------------------------------- network service (dials)

    /// A new dial socket for client `id`, limited to `hosts`. The service's end is held
    /// until the service runs (`net_ensure`); the client's end is returned.
    fn net_dial(&mut self, id: &str, hosts: &str) -> Result<OwnedFd, String> {
        let (client, service) = ipc::socketpair().map_err(|e| format!("socketpair: {e}"))?;
        self.net_held.push((id.to_string(), hosts.to_string(), service));
        Ok(client)
    }

    /// Hand the held dials to the network service, starting (and first fetching) it if
    /// needed. Called again from the timers while dials are held, so every failure is
    /// retried: the clients simply wait on their dial.
    fn net_ensure(&mut self) {
        if self.net_held.is_empty() {
            return;
        }
        let Some(sid) = self.net_service() else { return }; // the store was not reached yet
        if let Some(r) = self.running.iter().find(|r| r.id == sid) {
            if r.stop_at.is_none() {
                let broker = r.broker.as_raw_fd();
                for (id, hosts, dial) in self.net_held.drain(..) {
                    if let Err(e) = ipc::send(broker, &format!("CLIENT\t{id}\t{hosts}"), Some(dial.as_raw_fd())) {
                        log(&format!("{sid}: client {id} not passed: {e}"));
                    }
                }
            }
            return; // (stopping for an update: on_exit starts the new one)
        }
        let now = Instant::now();
        if self.net_retry_at.is_some_and(|t| now < t) {
            return;
        }
        let a = &self.apps[&sid];
        if matches!(a.status, Status::Fetching(_)) {
            return;
        }
        if !store::payload_present(&a.manifest, &payload_dir(&sid)) {
            if a.payload_url.is_empty() || !self.net_ready {
                self.net_retry_at = Some(now + NET_RETRY);
            } else {
                self.start_fetch(&sid);
            }
            return;
        }
        let sha = a.manifest.payload.sha256.clone();
        match self.try_spawn(&sid, false) {
            Ok(r) => {
                log(&format!("{sid}: network service started pid {}", r.child.id()));
                self.running.push(r);
                self.net_running = Some(sha);
                if let Some(a) = self.apps.get_mut(&sid) {
                    a.status = Status::Running;
                }
                self.net_ensure();
            }
            Err(e) => {
                log(&format!("{sid}: network service did not start: {e}"));
                self.net_retry_at = Some(now + NET_RETRY);
            }
        }
    }

    /// Fetch the service's payload ahead of the first app that needs it (it lives in RAM).
    fn net_prefetch(&mut self) {
        let Some(sid) = self.net_service() else { return };
        let wanted = !self.net_held.is_empty() || self.apps.values().any(|a| a.installed && a.grants.has("internet"));
        let a = &self.apps[&sid];
        if wanted && self.net_ready && !a.payload_url.is_empty() && !matches!(a.status, Status::Fetching(_)) && !store::payload_present(&a.manifest, &payload_dir(&sid)) {
            self.start_fetch(&sid);
        }
    }

    /// A newer service payload is here and no app is using the running one: restart it.
    fn net_swap(&mut self) {
        let Some(sid) = self.net_service() else { return };
        let a = &self.apps[&sid];
        let newer_here = self.net_running.as_ref().is_some_and(|sha| *sha != a.manifest.payload.sha256) && store::payload_present(&a.manifest, &payload_dir(&sid));
        // (Background services do not hold it back: they get a new dial and retry.)
        if newer_here && !self.running.iter().any(|r| r.internet && !r.bg) && self.running.iter().any(|r| r.id == sid && r.stop_at.is_none()) {
            log(&format!("{sid}: restarting on version {}", a.manifest.version));
            self.stop(&sid);
        }
    }

    // ---------------------------------------------------------------- volume

    /// Something set the system volume: tell the UI and every app, also when the value is
    /// the same (a step at the end of the scale): each of them shows the volume pop-up.
    fn set_volume(&mut self, volume: Volume) {
        let volume = Volume { level: volume.level.min(100), ..volume };
        if volume != self.volume {
            self.volume = volume;
            self.volume_save_at = Some(Instant::now() + VOLUME_SAVE_AFTER);
        }
        self.broadcast_ui(&self.volume_line(true));
        // MUTE first, so an app has both values when VOLUME (the event) arrives.
        for r in self.running.iter().filter(|r| r.stop_at.is_none() && !self.is_service(&r.app)) {
            let _ = ipc::send(r.broker.as_raw_fd(), &format!("MUTE\t{}", volume.muted as u8), None);
            let _ = ipc::send(r.broker.as_raw_fd(), &format!("VOLUME\t{}", volume.level), None);
        }
    }

    fn volume_line(&self, show: bool) -> String {
        format!("VOLUME\t{}\t{}\t{}", self.volume.level, self.volume.muted as u8, show as u8)
    }

    // ---------------------------------------------------------------- recents

    fn recents_line(&self) -> String {
        let mut l = "RECENTS".to_string();
        for id in &self.recents.ids {
            l.push('\t');
            l.push_str(id);
        }
        l
    }

    /// The recents changed: save them (a small file, only on a real change) and tell the UI.
    fn recents_changed(&mut self) {
        if self.state.recents() != self.recents {
            if let Err(e) = self.state.save_recents(&self.recents) {
                log(&e);
            }
        }
        let l = self.recents_line();
        self.broadcast_ui(&l);
    }

    // ---------------------------------------------------------------- background services

    /// App `id` should have its background service running: installed, the manifest has one,
    /// the permission is granted and the user has not switched it off.
    fn bg_wanted(&self, id: &str) -> bool {
        self.apps.get(id).is_some_and(|a| a.installed && a.manifest.service.is_none() && a.manifest.background.is_some() && a.grants.has("background") && !a.grants.background_off)
    }

    fn bg_running(&self, id: &str) -> bool {
        let key = bg_key(id);
        self.running.iter().any(|r| r.id == key)
    }

    /// "running", "starting", "failing" or "stopped" (APPINFO, `appctl background`).
    fn bg_state(&self, id: &str) -> &'static str {
        if self.bg_running(id) {
            "running"
        } else if !self.bg_wanted(id) {
            "stopped"
        } else if self.bg.get(id).is_some_and(|s| s.failures >= 2) {
            "failing"
        } else {
            "starting"
        }
    }

    /// Start, stop and update the background services. Called from the timers: every check
    /// here is cheap (maps and one small stamp file per service that is down).
    fn bg_ensure(&mut self, now: Instant) {
        let ids: Vec<String> = self
            .apps
            .iter()
            .filter(|(id, a)| a.manifest.background.is_some() || self.bg.contains_key(*id))
            .map(|(id, _)| id.clone())
            .chain(self.running.iter().filter(|r| r.bg).map(|r| r.app.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for id in ids {
            let key = bg_key(&id);
            let wanted = self.bg_wanted(&id);
            let running = self.running.iter().find(|r| r.id == key).map(|r| (r.stop_at.is_some(), r.payload_sha.clone()));
            let app_busy = self.running.iter().any(|r| r.id == id) || self.pending.as_ref().is_some_and(|p| p.id == id) || self.fg_request.as_ref().is_some_and(|r| r.0 == id);
            match (wanted, running) {
                (false, Some((stopping, _))) => {
                    if !stopping {
                        log(&format!("{key}: no longer wanted, stopping"));
                        self.stop(&key);
                    }
                }
                (false, None) => {
                    self.bg.remove(&id);
                }
                (true, Some((stopping, sha))) => {
                    // A new version was installed: fetch it, then restart the service on it.
                    let a = &self.apps[&id];
                    if stopping || app_busy || sha == a.manifest.payload.sha256 || matches!(a.status, Status::Fetching(_)) {
                        continue;
                    }
                    if store::payload_present(&a.manifest, &payload_dir(&id)) {
                        log(&format!("{key}: restarting on version {}", a.manifest.version));
                        self.stop(&key);
                    } else if !a.payload_url.is_empty() && self.net_ready && self.bg.get(&id).is_none_or(|s| s.not_before.is_none_or(|t| now >= t)) {
                        self.start_fetch(&id);
                        self.push_app(&id);
                    }
                }
                (true, None) => {
                    if app_busy {
                        continue; // never next to the app itself
                    }
                    let sup = self.bg.entry(id.clone()).or_default();
                    if !sup.due(now) {
                        continue;
                    }
                    let a = &self.apps[&id];
                    if matches!(a.status, Status::Fetching(_)) {
                        continue;
                    }
                    if !store::payload_present(&a.manifest, &payload_dir(&id)) {
                        // Payloads live in RAM: after a boot the service waits for the store.
                        if a.payload_url.is_empty() || !self.net_ready {
                            self.bg.get_mut(&id).unwrap().not_before = Some(now + NET_RETRY);
                        } else {
                            self.start_fetch(&id);
                            self.push_app(&id);
                        }
                        continue;
                    }
                    match self.try_spawn(&id, true) {
                        Ok(r) => {
                            log(&format!("{key}: background service started pid {}", r.child.id()));
                            self.running.push(r);
                            self.bg.get_mut(&id).unwrap().started(now);
                            self.net_ensure();
                        }
                        Err(e) => {
                            let wait = self.bg.get_mut(&id).unwrap().start_failed(now);
                            log(&format!("{key}: background service did not start: {e}; again in {} s", wait.as_secs()));
                        }
                    }
                    self.push_app(&id);
                }
            }
        }
    }

    /// The "Run in background" switch. Switching it on also grants the permission: this is
    /// the user deciding (Settings > Apps, or `appctl background`).
    fn set_background(&mut self, id: &str, on: bool) -> Result<String, String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if !a.installed {
            return Err(format!("{id} is not installed"));
        }
        if a.manifest.background.is_none() {
            return Err(format!("{} cannot run in the background", a.manifest.name));
        }
        let mut g = a.grants.clone();
        g.background_off = !on;
        if on && !g.has("background") {
            g.denied.retain(|p| p != "background");
            g.granted.push("background".into());
        }
        g.decided_at = now_secs();
        self.state.set_grants(id, &g)?;
        self.apps.get_mut(id).unwrap().grants = g;
        log(&format!("{id}: run in background {}", if on { "on" } else { "off" }));
        if on {
            self.bg.entry(id.to_string()).or_default().reset();
        }
        self.bg_ensure(Instant::now());
        self.push_app(id);
        Ok(if on { "on" } else { "off" }.into())
    }

    /// A background service (or `appctl foreground`) asks for its app to be opened.
    fn request_foreground(&mut self, id: &str, payload: &str, now: Instant) -> Result<(), String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if !a.installed || a.manifest.service.is_some() {
            return Err(format!("{id} is not an installed app"));
        }
        let payload = crate::service::clean_payload(payload).ok_or("the payload must be one line of at most 4096 bytes")?;
        if self.running.iter().any(|r| r.id == id) {
            return Err("the app is already open".into());
        }
        if self.fg_asked.get(id).is_some_and(|t| now.duration_since(*t) < FOREGROUND_MIN_INTERVAL) {
            return Err("asked a moment ago".into());
        }
        if let Some((other, _, _)) = self.fg_request.as_ref().filter(|r| r.0 != id) {
            return Err(format!("{other} is being opened"));
        }
        self.fg_asked.insert(id.to_string(), now);
        log(&format!("{id}: foreground requested ({} byte payload)", payload.len()));
        self.fg_request = Some((id.to_string(), Some(payload), now));
        self.fg_tick(now);
        Ok(())
    }

    /// Carry out a foreground request: whatever is in front is closed first (policy: the
    /// request wins, like a cast taking over the TV), then the app is launched the normal way.
    fn fg_tick(&mut self, now: Instant) {
        // Handed-over sockets live only as long as the request, or the launch it became.
        if self.fg_handover.as_ref().is_some_and(|h| self.fg_request.as_ref().is_none_or(|r| r.0 != h.0) && self.pending.as_ref().is_none_or(|p| p.id != h.0)) {
            self.fg_handover = None;
        }
        let Some((id, _, since)) = self.fg_request.clone() else { return };
        if now.duration_since(since) > FOREGROUND_TIMEOUT {
            log(&format!("{id}: foreground request dropped (the screen did not become free)"));
            self.fg_request = None;
            self.fg_handover = None;
            return;
        }
        if self.pending.is_some() {
            return; // another launch is on its way: wait for it
        }
        if let Some(front) = self.running.iter().find(|r| r.display).map(|r| (r.id.clone(), r.stop_at.is_some())) {
            if !front.1 {
                log(&format!("{}: closing for {id}", front.0));
                self.stop(&front.0);
            }
            return;
        }
        if !matches!(self.display, Display::Free) {
            return; // the launcher is taking the screen back
        }
        let (id, payload, _) = self.fg_request.take().unwrap();
        self.launch_payload = payload.map(|p| (id.clone(), p));
        self.launch(&id, 0, false);
        if self.pending.as_ref().is_none_or(|p| p.id != id) && !self.running.iter().any(|r| r.id == id) {
            self.launch_payload = None; // it did not even start to launch
            self.fg_handover = None;
        }
    }

    fn start_fetch(&mut self, id: &str) {
        let Some(a) = self.apps.get_mut(id) else { return };
        a.status = Status::Fetching(0);
        let (m, url) = (a.manifest.clone(), a.payload_url.clone());
        let dest = payload_dir(id);
        let (tx, wake) = (self.tx.clone(), self.wake());
        let id = id.to_string();
        std::thread::spawn(move || {
            let mut last = 0;
            let r = store::fetch_payload(&m, &url, &dest, |pct| {
                if pct >= last + 5 {
                    last = pct;
                    let _ = tx.send(Done::Progress(id.clone(), pct));
                    wake();
                }
            });
            let _ = tx.send(Done::Fetched(id, r));
            wake();
        });
    }

    // ---------------------------------------------------------------- assets

    fn send_fd_to(&mut self, token: u64, msg: &str, fd: std::os::fd::RawFd) {
        let Some(i) = self.conns.iter().position(|c| c.token == token) else { return };
        if ipc::send(self.conns[i].fd.as_raw_fd(), msg, Some(fd)).is_err() {
            let c = self.conns.remove(i);
            self.asset_waiters.retain(|w| w.token != c.token);
            self.on_disconnect(c.kind);
        }
    }

    /// A client asks for an asset. Answered from RAM when it is here; otherwise the client
    /// waits (with the preview, when the index has one) until the download succeeds.
    fn request_asset(&mut self, token: u64, name: &str) {
        if !self.asset_waiters.iter().any(|w| w.token == token && w.name == name) {
            self.asset_waiters.push(Waiter { token, name: name.to_string(), previewed: false });
        }
        self.serve_waiters(Some(name));
    }

    /// Go through the waiters (all, or those for `only`): serve what is here, send previews
    /// not sent yet, answer names the index does not have, start the downloads that are due.
    fn serve_waiters(&mut self, only: Option<&str>) {
        let Some((idx, _)) = &self.asset_index else {
            return; // no signed index yet (first boot, store not reached): wait for it
        };
        let mut serve: Vec<(u64, String, Result<PathBuf, String>)> = Vec::new();
        let mut previews: Vec<(u64, String, String)> = Vec::new();
        let mut fetch: Vec<(String, crate::assets::Entry)> = Vec::new();
        let can_fetch = self.net_ready && self.store.is_some() && self.asset_retry_at.is_none();
        for w in self.asset_waiters.iter_mut().filter(|w| only.is_none_or(|n| n == w.name)) {
            let Some(e) = idx.assets.get(&w.name) else {
                if self.asset_index_fresh {
                    serve.push((w.token, w.name.clone(), Err("not in the asset index".into())));
                }
                continue;
            };
            if let Some(p) = crate::assets::cached(e) {
                serve.push((w.token, w.name.clone(), Ok(p)));
                continue;
            }
            if let (Some(p), false) = (&e.preview, w.previewed) {
                previews.push((w.token, w.name.clone(), p.clone()));
                w.previewed = true;
            }
            if can_fetch && !self.asset_fetching.contains(&w.name) && !fetch.iter().any(|(n, _)| *n == w.name) {
                fetch.push((w.name.clone(), e.clone()));
            }
        }
        for (t, n, p) in previews {
            self.send_to(t, &format!("ASSET\t{}\tpreview\t{p}", field(&n)));
        }
        for (t, n, r) in serve {
            self.asset_waiters.retain(|w| !(w.token == t && w.name == n));
            match r {
                Ok(p) => self.serve_asset(t, &n, &p),
                Err(e) => self.send_to(t, &format!("ASSET\t{}\terror\t{}", field(&n), field(&e))),
            }
        }
        for (name, e) in fetch {
            self.asset_fetching.push(name.clone());
            let (tx, wake, url) = (self.tx.clone(), self.wake(), self.store_url());
            std::thread::spawn(move || {
                let t = Instant::now();
                let r = store::fetch_asset(&url, &name, &e);
                if r.is_ok() {
                    log(&format!("asset {name}: {} bytes in {} ms", e.size, t.elapsed().as_millis()));
                }
                let _ = tx.send(Done::AssetFetched(name, r));
                wake();
            });
        }
    }

    fn serve_asset(&mut self, token: u64, name: &str, path: &Path) {
        match std::fs::File::open(path) {
            Ok(f) => {
                self.send_fd_to(token, &format!("ASSET\t{}\tok", field(name)), f.as_raw_fd());
                if !self.assets_served.iter().any(|n| n == name) {
                    self.assets_served.push(name.to_string());
                }
            }
            Err(e) => self.send_to(token, &format!("ASSET\t{}\terror\t{}", field(name), field(&e.to_string()))),
        }
    }

    fn assets_refreshed(&mut self, r: Result<(Vec<u8>, String, crate::assets::Index), String>) {
        let (bytes, sig, idx) = match r {
            Ok(x) => x,
            Err(e) => {
                if self.asset_error.as_deref() != Some(&e) {
                    log(&format!("asset index unavailable: {e}"));
                }
                self.asset_error = Some(e);
                return;
            }
        };
        self.asset_error = None;
        self.asset_index_fresh = true;
        // The store answered: downloads may go ahead now, whatever the backoff said.
        self.asset_retry_at = None;
        if self.asset_index.as_ref().is_some_and(|(_, b)| *b == bytes) {
            return self.serve_waiters(None);
        }
        let changed: Vec<String> = self
            .assets_served
            .iter()
            .filter(|n| self.asset_index.as_ref().and_then(|(old, _)| old.assets.get(*n)) != idx.assets.get(*n))
            .cloned()
            .collect();
        if let Err(e) = crate::assets::save(&self.state.root, &bytes, &sig) {
            log(&format!("asset index not saved: {e}"));
        }
        log(&format!("asset index: {} asset(s)", idx.assets.len()));
        self.asset_index = Some((idx, bytes));
        for n in changed {
            self.broadcast_ui(&format!("ASSET_UPDATED\t{}", field(&n)));
        }
        self.serve_waiters(None);
    }

    fn asset_fetched(&mut self, name: &str, r: Result<Vec<u8>, String>) {
        self.asset_fetching.retain(|n| n != name);
        let entry = self.asset_index.as_ref().and_then(|(i, _)| i.assets.get(name).cloned());
        let stored = match (r, entry) {
            (Ok(data), Some(e)) if sha256_hex(&data) == e.sha256 => crate::assets::store(&e, &data),
            (Ok(_), _) => Err("asset changed during the download".into()),
            (Err(e), _) => Err(e),
        };
        match stored {
            Ok(_) => {
                self.asset_backoff = ASSET_RETRY_MIN;
                self.serve_waiters(Some(name));
            }
            Err(e) => {
                // The clients keep waiting (showing the preview meanwhile); try again later.
                log(&format!("asset {name}: {e}; again in {} s", self.asset_backoff.as_secs()));
                self.asset_retry_at = Some(Instant::now() + self.asset_backoff);
                self.asset_backoff = (self.asset_backoff * 2).min(ASSET_RETRY_MAX);
            }
        }
    }

    // ---------------------------------------------------------------- clients

    fn accept(&mut self) {
        while let Ok(fd) = ipc::accept(self.listener.as_raw_fd()) {
            if ipc::peer_uid(fd.as_raw_fd()) != Some(0) {
                continue; // only root (the UI, appctl) may control apps
            }
            let token = self.next_token;
            self.next_token += 1;
            self.conns.push(Conn { token, fd, kind: Kind::New });
        }
    }

    fn send_to(&mut self, token: u64, msg: &str) {
        let Some(i) = self.conns.iter().position(|c| c.token == token) else { return };
        if ipc::send(self.conns[i].fd.as_raw_fd(), msg, None).is_err() {
            let c = self.conns.remove(i);
            self.asset_waiters.retain(|w| w.token != c.token);
            self.on_disconnect(c.kind);
        }
    }

    fn broadcast_ui(&mut self, msg: &str) {
        let tokens: Vec<u64> = self.conns.iter().filter(|c| c.kind == Kind::Ui).map(|c| c.token).collect();
        for t in tokens {
            self.send_to(t, msg);
        }
    }

    fn ui_token(&self) -> Option<u64> {
        self.conns.iter().find(|c| c.kind == Kind::Ui).map(|c| c.token)
    }

    fn reply(&mut self, token: u64, r: Result<String, String>) {
        let msg = match r {
            Ok(s) if s.is_empty() => "OK".to_string(),
            Ok(s) => format!("OK\t{s}"),
            Err(e) => format!("ERR\t{}", field(&e)),
        };
        self.send_to(token, &msg);
    }

    fn conn_readable(&mut self, token: u64) {
        loop {
            let Some(c) = self.conns.iter().find(|c| c.token == token) else { return };
            match ipc::recv(c.fd.as_raw_fd()) {
                Ok(Recv::Msg(m, _fd)) => self.command(token, &m),
                Ok(Recv::WouldBlock) => return,
                Ok(Recv::Closed) | Err(_) => {
                    if let Some(i) = self.conns.iter().position(|c| c.token == token) {
                        let c = self.conns.remove(i);
                        self.asset_waiters.retain(|w| w.token != c.token);
                        self.on_disconnect(c.kind);
                    }
                    return;
                }
            }
        }
    }

    fn on_disconnect(&mut self, kind: Kind) {
        if kind != Kind::Ui || self.ui_token().is_some() {
            return;
        }
        // The launcher went away: an app must not keep the screen with nobody to return to.
        if let Some(id) = self.foreground() {
            log(&format!("UI disconnected: closing {id}"));
            self.stop(&id);
        }
        if matches!(self.display, Display::Reclaiming { .. } | Display::Releasing { .. }) {
            self.display = Display::Free;
        }
        if let Some(p) = self.pending.take() {
            if let Some(a) = self.apps.get_mut(&p.id) {
                if a.status != Status::Running {
                    a.status = Status::Idle;
                }
            }
        }
    }

    fn command(&mut self, token: u64, msg: &str) {
        let f: Vec<&str> = msg.split('\t').collect();
        let arg = |i: usize| f.get(i).copied().unwrap_or("");
        let kind = self.conns.iter().find(|c| c.token == token).map_or(Kind::New, |c| c.kind);
        match (f[0], kind) {
            ("HELLO", _) => {
                let k = if arg(1) == "ui" { Kind::Ui } else { Kind::Ctl };
                if k == Kind::Ui && self.ui_token().is_some() {
                    self.send_to(token, "ERR\tanother launcher is connected");
                    return;
                }
                if let Some(c) = self.conns.iter_mut().find(|c| c.token == token) {
                    c.kind = k;
                }
                self.send_to(token, &format!("WELCOME\tappd {}", env!("CARGO_PKG_VERSION")));
                if k == Kind::Ui {
                    // Installed apps only: what else there is to get is the Store app's list.
                    self.ui_listed = self.apps.iter().filter(|(_, a)| a.installed && a.manifest.service.is_none()).map(|(id, _)| id.clone()).collect();
                    let lines: Vec<String> = self.ui_listed.iter().map(|id| self.app_line(id)).collect();
                    for l in lines {
                        self.send_to(token, &l);
                    }
                    self.send_to(token, "APPS_END");
                    self.send_to(token, &self.volume_line(false));
                    self.send_to(token, &self.recents_line());
                    // Details of the installed apps (Settings > Apps), all of them anew.
                    let installed: Vec<String> = self.apps.iter().filter(|(_, a)| a.installed && a.manifest.service.is_none()).map(|(id, _)| id.clone()).collect();
                    for id in installed {
                        let l = self.info_line(&id);
                        self.info_sent.insert(id, l.clone());
                        self.send_to(token, &l);
                    }
                }
            }
            (_, Kind::New) => self.send_to(token, "ERR\tsay HELLO first"),
            ("LIST", _) => {
                let lines: Vec<String> = self.apps.keys().map(|id| self.app_line(id)).collect();
                for l in lines {
                    self.send_to(token, &l);
                }
                let s = match &self.store_error {
                    Some(e) => format!("store {} unreachable: {e}", self.store_url()),
                    None => format!("store {}", self.store_url()),
                };
                self.reply(token, Ok(s));
            }
            ("PS", _) => {
                let lines: Vec<String> = self
                    .running
                    .iter()
                    .map(|r| {
                        let mem = std::fs::read_to_string(r.cgroup.join("memory.current")).unwrap_or_default();
                        let peak = std::fs::read_to_string(r.cgroup.join("memory.peak")).unwrap_or_default();
                        let pids = std::fs::read_to_string(r.cgroup.join("pids.current")).unwrap_or_default();
                        format!(
                            "RUNNING\t{}\tpid={}\tsecs={}\tmem_kb={}\tpeak_kb={}\tpids={}\tdisplay={}{}",
                            r.id,
                            r.child.id(),
                            r.started.elapsed().as_secs(),
                            mem.trim().parse::<u64>().unwrap_or(0) / 1024,
                            peak.trim().parse::<u64>().unwrap_or(0) / 1024,
                            pids.trim(),
                            r.display,
                            if r.bg { "\tbackground" } else { "" }
                        )
                    })
                    .collect();
                for l in lines {
                    self.send_to(token, &l);
                }
                self.reply(token, Ok(String::new()));
            }
            ("REFRESH", _) => {
                if !self.refreshing {
                    self.start_refresh();
                }
                self.reply(token, Ok("refreshing".into()));
            }
            ("INFO", _) => {
                let r = self.info(arg(1));
                match r {
                    Ok(lines) => {
                        for l in lines {
                            self.send_to(token, &format!("INFO\t{}", field(&l)));
                        }
                        self.reply(token, Ok(String::new()));
                    }
                    Err(e) => self.reply(token, Err(e)),
                }
            }
            ("LOGS", _) => {
                let id = arg(1);
                let data: Vec<u8> = match self.running.iter().find(|r| r.id == id) {
                    Some(r) => r.log.iter().copied().collect(),
                    None => self.old_logs.get(id).cloned().unwrap_or_default(),
                };
                for chunk in String::from_utf8_lossy(&data).as_bytes().chunks(4000) {
                    let s = String::from_utf8_lossy(chunk).into_owned();
                    self.send_to(token, &format!("LOG\t{s}"));
                }
                self.reply(token, Ok(String::new()));
            }
            ("LAUNCH", k) => self.launch(arg(1), token, k == Kind::Ui),
            ("INSTALL", Kind::Ctl) => {
                let r = self.install(arg(1), arg(2) == "all");
                self.reply(token, r);
            }
            ("CONSENT", Kind::Ui) => self.consent(arg(1), arg(2) == "allow"),
            ("UNINSTALL", _) => {
                let r = self.uninstall(arg(1));
                self.reply(token, r);
            }
            ("STOP", _) => {
                let id = arg(1).to_string();
                let r = if self.running.iter().any(|r| r.id == id) {
                    self.stop(&id);
                    Ok(String::new())
                } else {
                    Err(format!("{id} is not running"))
                };
                self.reply(token, r);
            }
            ("GRANT", k) | ("REVOKE", k) => {
                let r = self.set_grant(arg(1), arg(2), f[0] == "GRANT");
                if arg(2) == "background" && r.is_ok() {
                    if let Some(s) = self.bg.get_mut(arg(1)) {
                        s.reset();
                    }
                    self.bg_ensure(Instant::now());
                }
                self.push_app(arg(1));
                // The launcher follows APPINFO; it gets only failures, as a notice.
                match (k, r) {
                    (Kind::Ui, Ok(_)) => {}
                    (Kind::Ui, Err(e)) => self.send_to(token, &format!("NOTICE\t{}", field(&e))),
                    (_, r) => self.reply(token, r),
                }
            }
            ("KEY", k) => {
                let sent = match self.running.iter().find(|r| r.display && r.input) {
                    Some(r) => ipc::send(r.broker.as_raw_fd(), &format!("KEY\t{}", field(arg(1))), None).is_ok(),
                    None => false,
                };
                if k == Kind::Ctl {
                    self.reply(token, if sent { Ok(String::new()) } else { Err("no foreground app with input".into()) });
                }
            }
            ("HOME", Kind::Ui) => {
                if let Some(id) = self.foreground() {
                    self.stop(&id);
                }
            }
            ("DISPLAY_RELEASED", Kind::Ui) => self.display_released(),
            ("ASSET", _) => self.request_asset(token, arg(1)),
            ("VOLUME", k) => match self.volume.apply(arg(1), arg(2)) {
                Ok(v) => {
                    if !arg(1).is_empty() {
                        self.set_volume(v);
                    }
                    if k == Kind::Ctl {
                        self.reply(token, Ok(self.volume.text()));
                    }
                }
                Err(e) => self.reply(token, Err(field(&e))),
            },
            ("BACKGROUND", k) if arg(1).is_empty() => {
                let lines: Vec<String> = self
                    .apps
                    .iter()
                    .filter(|(_, a)| a.installed && a.manifest.background.is_some())
                    .map(|(id, a)| {
                        let sup = self.bg.get(id);
                        format!(
                            "BG\t{id}\t{}\tservice={}\tstarts={}\tfailures={}\tpermission={}",
                            if self.bg_wanted(id) { "on" } else { "off" },
                            self.bg_state(id),
                            sup.map_or(0, |s| s.starts),
                            sup.map_or(0, |s| s.failures),
                            if a.grants.has("background") { "granted" } else { "not granted" }
                        )
                    })
                    .collect();
                for l in lines {
                    self.send_to(token, &l);
                }
                if k == Kind::Ctl {
                    self.reply(token, Ok(String::new()));
                }
            }
            ("BACKGROUND", k) => {
                let r = match arg(2) {
                    "on" => self.set_background(arg(1), true),
                    "off" => self.set_background(arg(1), false),
                    other => Err(format!("background {}: expected on or off", field(other))),
                };
                if k == Kind::Ctl {
                    self.reply(token, r);
                } else if let Err(e) = r {
                    self.send_to(token, &format!("NOTICE\t{}", field(&e)));
                }
            }
            ("REPO", Kind::Ctl) => match arg(1) {
                "" | "list" => {
                    for l in self.store_repo_lines() {
                        self.send_to(token, &l.replacen("STORE\trepo", "REPO", 1));
                    }
                    self.reply(token, Ok(String::new()));
                }
                "add" => self.repo_probe(Asker::Conn(token), arg(2), true),
                "remove" => {
                    let r = self.repo_remove(arg(2));
                    self.reply(token, r);
                }
                "enable" => {
                    let r = self.repo_enable(arg(2), arg(3) == "1");
                    self.reply(token, r);
                }
                other => self.reply(token, Err(format!("repo {}: expected list, add, remove or enable", field(other)))),
            },
            ("RECENTS", _) => {
                let l = self.recents_line();
                self.send_to(token, &l);
                self.reply(token, Ok(String::new()));
            }
            ("FOREGROUND", Kind::Ctl) => {
                let r = self.request_foreground(arg(1), arg(2), Instant::now()).map(|_| "requested".to_string());
                self.reply(token, r);
            }
            ("NET", Kind::Ctl) => {
                if self.net_service().is_none() {
                    return self.reply(token, Err("the network service is not installed yet (the store has not been reached)".into()));
                }
                // Tools that never came back must not pile up while the service is down.
                while self.net_held.iter().filter(|h| h.0 == "ctl").count() >= 8 {
                    let oldest = self.net_held.iter().position(|h| h.0 == "ctl").unwrap();
                    self.net_held.remove(oldest);
                }
                match self.net_dial("ctl", "*") {
                    Ok(dial) => {
                        self.send_fd_to(token, "NET\tok", dial.as_raw_fd());
                        self.net_ensure();
                    }
                    Err(e) => self.reply(token, Err(e)),
                }
            }
            ("IMAGE", Kind::Ctl) => match self.img_connect("ctl") {
                Ok(socket) => {
                    self.send_fd_to(token, "IMAGE\tok", socket.as_raw_fd());
                    self.img_ensure();
                }
                Err(e) => self.reply(token, Err(e)),
            },
            ("DISPLAY_RECLAIMED", Kind::Ui) => {
                if let Display::Reclaiming { .. } = self.display {
                    self.display = Display::Free; // drops (closes) appd's copy of the master file
                }
            }
            (other, _) => self.send_to(token, &format!("ERR\tunknown command {}", field(other))),
        }
    }

    /// State and detail of an app, as the launcher and the Store app show them.
    fn app_state(a: &App) -> (&'static str, String) {
        match &a.status {
            Status::Running => ("running", String::new()),
            Status::Fetching(p) => ("fetching", format!("{p}%")),
            Status::Error(e) => ("error", e.clone()),
            Status::Idle if a.installed => ("installed", a.update_blocked.map(|v| format!("update {v} needs approval")).unwrap_or_default()),
            Status::Idle => ("available", format!("{} KiB", a.manifest.payload.size.div_ceil(1024))),
        }
    }

    fn app_line(&self, id: &str) -> String {
        let a = &self.apps[id];
        let m = &a.manifest;
        let (state, detail) = Self::app_state(a);
        let ap = &m.appearance;
        format!(
            "APP\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            m.id,
            field(&m.name),
            field(&m.summary),
            ap.glyph,
            ap.tint,
            ap.badge[0],
            ap.badge[1],
            ap.shape,
            state,
            field(&detail)
        )
    }

    fn push_app(&mut self, id: &str) {
        // System services are shown nowhere.
        if self.apps.get(id).is_some_and(|a| a.manifest.service.is_none()) {
            self.store_send(&self.store_app_line(id));
            // The launcher shows installed apps; the rest is in the Store app.
            if self.apps[id].installed {
                self.ui_listed.insert(id.to_string());
                let l = self.app_line(id);
                self.broadcast_ui(&l);
                let l = self.info_line(id);
                if self.info_sent.get(id) != Some(&l) {
                    self.info_sent.insert(id.to_string(), l.clone());
                    self.broadcast_ui(&l);
                }
            } else {
                self.info_sent.remove(id);
                if self.ui_listed.remove(id) {
                    self.broadcast_ui(&format!("REMOVE\t{id}"));
                }
            }
        }
    }

    /// `granted`, `denied` or `undecided` (an optional permission the user was not asked yet).
    fn grant_state(g: &Grants, perm: &str) -> &'static str {
        if g.has(perm) {
            "granted"
        } else if g.denied.iter().any(|p| p == perm) {
            "denied"
        } else {
            "undecided"
        }
    }

    /// APPINFO: what Settings > Apps shows for an installed app.
    fn info_line(&self, id: &str) -> String {
        let a = &self.apps[id];
        let m = &a.manifest;
        let bg = match (&m.background, self.bg_wanted(id)) {
            (None, _) => "none",
            (Some(_), false) => "off",
            (Some(_), true) => "on",
        };
        let mut l = format!("APPINFO\t{}\t{}\t{bg}\t{}", m.id, field(&m.version), self.bg_state(id));
        let clean = |s: &str| field(s).replace('|', "/");
        for r in &m.permissions {
            let q = r.quota_kb.unwrap_or(0);
            let extra = self.perm_extra(m, r);
            l.push_str(&format!(
                "\t{}|{}|{}|{}|{}|{}|{}",
                r.name,
                perms::level(&r.name, q).as_str(),
                r.required as u8,
                Self::grant_state(&a.grants, &r.name),
                clean(&Self::perm_label(r)),
                clean(&r.reason),
                clean(&extra)
            ));
        }
        l
    }

    /// What a permission reaches, in a few words: the hosts of "internet", the quota of
    /// "storage", the memory of "background", the apps of "manage-receivers", the ports of "lan".
    fn perm_extra(&self, m: &Manifest, r: &crate::manifest::Request) -> String {
        match r.name.as_str() {
            "internet" if r.hosts.len() == 1 && r.hosts[0] == perms::ANY_HOST => "any site".to_string(),
            "internet" => r.hosts.join(", "),
            "storage" => format!("{} KiB", r.quota_kb.unwrap_or(0)),
            "background" => m.background.as_ref().map_or(String::new(), |b| format!("up to {} MiB of memory", b.memory_mb)),
            // Which apps, by the name the user knows when they are in the store.
            "manage-receivers" => m.manages.iter().map(|id| self.apps.get(id).map_or(id.clone(), |a| a.manifest.name.clone())).collect::<Vec<_>>().join(", "),
            "lan" => {
                let ports = m.lan_ports();
                let list = |kind: &str, ports: &[u16]| ports.iter().map(|p| format!("{kind} {p}")).collect::<Vec<_>>();
                let all = [list("TCP", &ports.tcp), list("UDP", &ports.udp)].concat();
                if all.is_empty() { String::new() } else { format!("listens on {}", all.join(", ")) }
            }
            _ => String::new(),
        }
    }

    fn info(&self, id: &str) -> Result<Vec<String>, String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        let m = &a.manifest;
        let mut out = vec![
            format!("{} {} ({}), version_code {}, publisher {:?}", m.id, m.version, m.name, m.version_code, m.publisher),
            format!("installed={} in_store={} repository {} payload {} bytes sha256 {}", a.installed, a.in_store, a.repo, m.payload.size, m.payload.sha256),
            format!("resources: memory {} MiB, tmp {} MiB, pids {}, cpu.weight {}", m.resources.memory_mb, m.resources.tmp_mb, m.resources.pids, m.resources.cpu_weight),
        ];
        if a.installed {
            out.push(format!("uid {}", self.state.uid(id).unwrap_or(0)));
        }
        for r in &m.permissions {
            let q = r.quota_kb.unwrap_or(0);
            let st = if a.grants.has(&r.name) { "GRANTED" } else if a.grants.denied.contains(&r.name) { "denied" } else if a.installed { "undecided" } else { "-" };
            out.push(format!(
                "permission {} [{}{}] {}: {} — {}",
                r.name,
                perms::level(&r.name, q).as_str(),
                if r.required { ", required" } else { ", optional" },
                st,
                Self::perm_label(r),
                perms::def(&r.name).map_or("", |d| d.effect)
            ));
        }
        if m.service.is_some() {
            out.push(format!("system service {:?}: installed and started by appd", m.service.as_deref().unwrap_or("")));
        }
        if let Some(b) = &m.background {
            out.push(format!(
                "background service: memory {} MiB, pids {}, cpu.weight {}, args {:?}; run in background {}, service {}",
                b.memory_mb,
                b.pids,
                b.cpu_weight,
                b.args,
                if self.bg_wanted(id) { "ON" } else if a.grants.has("background") { "off (switched off)" } else { "off (permission not granted)" },
                self.bg_state(id)
            ));
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- install / grants

    /// Grants for an install: every normal permission, plus the sensitive ones if `allow`.
    fn grants_for(m: &Manifest, allow: bool) -> Result<Grants, String> {
        let mut g = Grants { decided_at: now_secs(), ..Default::default() };
        for r in &m.permissions {
            let lvl = perms::level(&r.name, r.quota_kb.unwrap_or(0));
            if lvl == Level::Normal || allow {
                g.granted.push(r.name.clone());
            } else if r.required {
                return Err(format!("{} needs the {} permission", m.name, r.name));
            } else {
                g.denied.push(r.name.clone());
            }
        }
        Ok(g)
    }

    fn check_quota(&self, m: &Manifest, g: &Grants) -> Result<(), String> {
        if !g.has("storage") {
            return Ok(());
        }
        let others: u64 = self
            .apps
            .values()
            .filter(|a| a.installed && a.manifest.id != m.id && a.grants.has("storage"))
            .map(|a| a.manifest.quota_kb() as u64 * 1024)
            .sum();
        let cap = fs_size(Path::new("/data")).unwrap_or(0).saturating_sub(DATA_RESERVE);
        let want = m.quota_kb() as u64 * 1024;
        if others + want > cap {
            return Err(format!("not enough device storage: {} KiB requested, {} KiB left for apps", want / 1024, cap.saturating_sub(others) / 1024));
        }
        Ok(())
    }

    fn do_install(&mut self, id: &str, g: Grants) -> Result<(), String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        self.check_quota(&a.manifest, &g)?;
        self.state.install(&a.manifest, &a.manifest_bytes, &a.signature, &Source { payload_url: a.payload_url.clone(), repo: repos::to_record(&a.repo) }, &g)?;
        let a = self.apps.get_mut(id).unwrap();
        a.installed = true;
        log(&format!("{id}: installed {} granted [{}] denied [{}]", a.manifest.version, g.granted.join(","), g.denied.join(",")));
        a.grants = g;
        self.push_app(id);
        Ok(())
    }

    fn install(&mut self, id: &str, all: bool) -> Result<String, String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if a.installed {
            return Ok("already installed".into());
        }
        let g = Self::grants_for(&a.manifest, all)?;
        let summary = format!("granted [{}] denied [{}]", g.granted.join(","), g.denied.join(","));
        self.do_install(id, g)?;
        Ok(summary)
    }

    /// One line for the consent dialog and `appctl info`.
    fn perm_label(r: &crate::manifest::Request) -> String {
        match r.name.as_str() {
            "internet" => format!("Connect to {}", perms::hosts_label(&r.hosts)),
            name => perms::label(name, r.quota_kb.unwrap_or(0)),
        }
    }

    fn uninstall(&mut self, id: &str) -> Result<String, String> {
        if self.is_service(id) {
            return Err(format!("{id} is a system service"));
        }
        if self.running.iter().any(|r| r.id == id) {
            return Err(format!("{id} is running: close it first (appctl stop {id})"));
        }
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if !a.installed {
            return Err(format!("{id} is not installed"));
        }
        if Self::system_app(a) {
            return Err(format!("{} is part of the system", a.manifest.name));
        }
        // Its background service goes with it, now, and without saving its data again.
        let key = bg_key(id);
        if let Some(r) = self.running.iter_mut().find(|r| r.id == key) {
            r.discard = true;
            r.stop_at = Some(Instant::now());
            let _ = write_file(r.cgroup.join("cgroup.kill"), "1");
            self.on_exit(&key);
        }
        self.bg.remove(id);
        self.fg_asked.remove(id);
        if self.recents.retain(|r| r != id) {
            self.recents_changed();
        }
        let a = self.apps.get_mut(id).unwrap();
        self.state.uninstall(id)?;
        let _ = std::fs::remove_dir_all(payload_dir(id));
        let _ = std::fs::remove_file(payload_dir(id).with_extension("sha256"));
        a.installed = false;
        a.grants = Grants::default();
        let keep = a.in_store;
        if keep {
            self.push_app(id);
        } else {
            self.app_gone(id);
        }
        log(&format!("{id}: uninstalled (data removed)"));
        Ok(String::new())
    }

    fn set_grant(&mut self, id: &str, perm: &str, grant: bool) -> Result<String, String> {
        let a = self.apps.get(id).ok_or_else(|| format!("unknown app {id}"))?;
        if !a.installed {
            return Err(format!("{id} is not installed"));
        }
        let r = a.manifest.request(perm).ok_or_else(|| format!("{id} does not request {perm}"))?;
        if !grant && r.required {
            return Err(format!("{perm} is required by {id}: uninstall instead"));
        }
        let mut g = a.grants.clone();
        g.granted.retain(|p| p != perm);
        g.denied.retain(|p| p != perm);
        if grant { g.granted.push(perm.into()) } else { g.denied.push(perm.into()) }
        g.decided_at = now_secs();
        if grant {
            self.check_quota(&a.manifest, &g)?;
        }
        self.state.set_grants(id, &g)?;
        self.apps.get_mut(id).unwrap().grants = g;
        log(&format!("{id}: {} {perm}", if grant { "granted" } else { "revoked" }));
        let note = if self.running.iter().any(|r| r.app == id) && perm != "background" { "takes effect at the next start" } else { "" };
        Ok(note.into())
    }

    // ---------------------------------------------------------------- launch

    fn foreground(&self) -> Option<String> {
        self.running.iter().find(|r| r.display).map(|r| r.id.clone())
    }

    fn launch(&mut self, id: &str, token: u64, from_ui: bool) {
        let Some(a) = self.apps.get(id) else {
            return self.reply(token, Err(format!("unknown app {id}")));
        };
        if a.manifest.service.is_some() {
            return self.reply(token, Err(format!("{id} is a system service (appd starts it when an app needs it)")));
        }
        if self.running.iter().any(|r| r.id == id) {
            return self.reply(token, Err(format!("{id} is already running")));
        }
        if self.pending.is_some() {
            return self.reply(token, Err("another launch is in progress".into()));
        }
        let wants_display = a.manifest.request("display").is_some();
        if wants_display && self.foreground().is_some() {
            return self.reply(token, Err("another app has the screen".into()));
        }
        self.pending = Some(Pending { id: id.to_string(), stage: Stage::Consent, requester: token });
        // Installed, and an update brought optional permissions the user was never asked
        // about: ask now (the launcher's dialog), then open the app either way.
        let undecided: Vec<String> = a
            .manifest
            .permissions
            .iter()
            .filter(|r| perms::level(&r.name, r.quota_kb.unwrap_or(0)) == Level::Sensitive && Self::grant_state(&a.grants, &r.name) == "undecided")
            .map(|r| r.name.clone())
            .collect();
        if !a.installed || (from_ui && !undecided.is_empty()) {
            if !from_ui {
                self.pending = None;
                return self.reply(token, Err(format!("{id} is not installed (appctl install {id} [all])")));
            }
            let needs_consent = a.manifest.permissions.iter().any(|r| perms::level(&r.name, r.quota_kb.unwrap_or(0)) == Level::Sensitive);
            let mut msg = format!("CONSENT\t{}\t{}", a.manifest.id, field(&a.manifest.name));
            if a.installed {
                msg.push_str("\tmode=update");
            }
            for r in a.manifest.permissions.iter().filter(|r| !a.installed || undecided.contains(&r.name)) {
                let q = r.quota_kb.unwrap_or(0);
                msg.push_str(&format!(
                    "\t{}|{}|{}|{}|{}",
                    r.name,
                    perms::level(&r.name, q).as_str(),
                    r.required as u8,
                    field(&Self::perm_label(r)).replace('|', "/"),
                    field(&r.reason).replace('|', "/")
                ));
            }
            let _ = needs_consent; // the UI always confirms an install, even with normal permissions only
            self.send_to(token, &msg);
            return;
        }
        self.continue_launch(id);
    }

    fn consent(&mut self, id: &str, allow: bool) {
        let Some(p) = self.pending.as_ref().filter(|p| p.id == id && matches!(p.stage, Stage::Consent)) else { return };
        let token = p.requester;
        let Some(a) = self.apps.get(id) else { return };
        if a.installed {
            // The answer for the optional permissions of an update: recorded, then launch.
            let mut g = a.grants.clone();
            for r in &a.manifest.permissions {
                if perms::level(&r.name, r.quota_kb.unwrap_or(0)) == Level::Sensitive && Self::grant_state(&g, &r.name) == "undecided" {
                    if allow { g.granted.push(r.name.clone()) } else { g.denied.push(r.name.clone()) }
                }
            }
            g.decided_at = now_secs();
            match self.state.set_grants(id, &g) {
                Ok(()) => {
                    log(&format!("{id}: update permissions {}: granted [{}] denied [{}]", if allow { "allowed" } else { "refused" }, g.granted.join(","), g.denied.join(",")));
                    self.apps.get_mut(id).unwrap().grants = g;
                }
                Err(e) => log(&format!("{id}: grants not saved: {e}")),
            }
            self.push_app(id);
            return self.continue_launch(id);
        }
        let g = match Self::grants_for(&a.manifest, allow) {
            Ok(g) => g,
            Err(e) => {
                // Denied a required permission: not installed, nothing changed.
                self.pending = None;
                log(&format!("{id}: install cancelled: {e}"));
                self.send_to(token, &format!("NOTICE\t{}", field(&e)));
                self.push_app(id);
                return;
            }
        };
        if let Err(e) = self.do_install(id, g) {
            return self.fail_pending(id, &e);
        }
        self.continue_launch(id);
    }

    /// What to tell the user when an app cannot be downloaded (TV notice).
    fn download_problem(&self, id: &str) -> String {
        let name = self.apps.get(id).map_or(id.to_string(), |a| a.manifest.name.clone());
        if self.store.is_none() {
            return format!("{name} can't be downloaded: this firmware has no app store");
        }
        match crate::http::not_ready(&self.store_url()) {
            Some(_) => format!("No internet connection. Connect to Wi-Fi in Settings to open {name}"),
            None if !self.store_ok_once => format!("The meow store isn't reachable yet. Try {name} again in a moment"),
            None => format!("Couldn't download {name}. Try again in a moment"),
        }
    }

    fn fail_pending(&mut self, id: &str, why: &str) {
        log(&format!("{id}: launch failed: {why}"));
        let token = self.pending.take().filter(|p| p.id == id).map(|p| p.requester);
        if self.launch_payload.as_ref().is_some_and(|p| p.0 == id) {
            self.launch_payload = None;
        }
        if self.fg_handover.as_ref().is_some_and(|h| h.0 == id) {
            self.fg_handover = None; // the sockets close; the service's copies went with it
        }
        if let Some(a) = self.apps.get_mut(id) {
            a.status = Status::Error(why.to_string());
        }
        self.push_app(id);
        if let Some(t) = token {
            let is_ui = self.conns.iter().any(|c| c.token == t && c.kind == Kind::Ui);
            if is_ui {
                self.send_to(t, &format!("NOTICE\t{}", field(why)));
            } else {
                self.reply(t, Err(why.to_string()));
            }
        }
    }

    fn continue_launch(&mut self, id: &str) {
        let Some(a) = self.apps.get(id) else { return };
        let m = a.manifest.clone();
        let display = a.grants.has("display");
        // The app's background service goes first: the two share /data, and whatever the
        // service holds (a session with a server) passes to the app.
        let key = bg_key(id);
        if self.running.iter().any(|r| r.id == key) {
            if let Some(p) = self.pending.as_mut() {
                p.stage = Stage::ServiceStop;
            }
            self.stop(&key);
            return;
        }
        if let Some(p) = self.pending.as_mut() {
            p.stage = Stage::Fetch;
        }
        if !store::payload_present(&m, &payload_dir(id)) {
            // Apps live in RAM only: after a reboot they come from the store again.
            if a.payload_url.is_empty() || !self.net_ready {
                let why = self.download_problem(id);
                return self.fail_pending(id, &why);
            }
            self.start_fetch(id);
            self.push_app(id);
            return;
        }
        if display {
            if let Some(p) = self.pending.as_mut() {
                p.stage = Stage::Display;
            }
            match (&self.display, self.ui_token()) {
                (Display::Free, Some(ui)) => {
                    self.display = Display::Releasing { id: id.to_string(), since: Instant::now() };
                    self.send_to(ui, &format!("RELEASE_DISPLAY\t{id}"));
                    return;
                }
                (Display::Free, None) => {} // no launcher: take the screen directly
                _ => return self.fail_pending(id, "the screen is busy"),
            }
            match open_master() {
                Ok(fd) => self.display = Display::Held { id: id.to_string(), fd },
                Err(e) => return self.fail_pending(id, &e),
            }
        }
        self.spawn(id);
    }

    fn display_released(&mut self) {
        let Display::Releasing { id, .. } = &self.display else { return };
        let id = id.clone();
        match open_master() {
            Ok(fd) => {
                self.display = Display::Held { id: id.clone(), fd };
                self.spawn(&id);
            }
            Err(e) => {
                self.display = Display::Free;
                if let Some(ui) = self.ui_token() {
                    self.send_to(ui, &format!("RECLAIM_DISPLAY\t{id}"));
                }
                self.fail_pending(&id, &e);
            }
        }
    }

    fn spawn(&mut self, id: &str) {
        match self.try_spawn(id, false) {
            Ok(r) => {
                log(&format!("{id}: started pid {} (display={} storage={})", r.child.id(), r.display, r.storage.is_some()));
                let token = self.pending.take().map(|p| p.requester);
                self.running.push(r);
                if let Some(a) = self.apps.get_mut(id) {
                    a.status = Status::Running;
                }
                if self.recents.opened(id) {
                    self.recents_changed();
                }
                self.push_app(id);
                if self.running.last().is_some_and(|r| r.display) {
                    self.broadcast_ui(&format!("FOREGROUND\t{id}"));
                }
                self.net_ensure(); // its dial, if it has "internet"
                if let Some(t) = token {
                    if self.conns.iter().any(|c| c.token == t && c.kind == Kind::Ctl) {
                        self.reply(t, Ok("started".into()));
                    }
                }
            }
            Err(e) => {
                self.give_back_display(id);
                self.fail_pending(id, &e);
            }
        }
    }

    /// Start app `id` in its sandbox; `bg`: its background service instead (the manifest's
    /// "background" section: other arguments, tight limits, no screen, keys, decoder or sound).
    fn try_spawn(&mut self, id: &str, bg: bool) -> Result<Running, String> {
        let a = self.apps.get(id).ok_or("unknown app")?;
        let m = a.manifest.clone();
        let mut g = a.grants.clone();
        let key = if bg { bg_key(id) } else { id.to_string() };
        let (memory_mb, pids, cpu_weight, args) = match (&m.background, bg) {
            (Some(b), true) => (b.memory_mb, b.pids, b.cpu_weight, b.args.clone()),
            (None, true) => return Err("no background section".into()),
            (_, false) => (m.resources.memory_mb, m.resources.pids, m.resources.cpu_weight, m.args.clone()),
        };
        if bg {
            g.granted.retain(|p| !matches!(p.as_str(), "display" | "input" | "media"));
        }
        let payload = if bg { None } else { self.launch_payload.take_if(|p| p.0 == id).map(|p| p.1) };
        // Dropped (closed) when this function returns, however it returns: by then the
        // sandbox helper has its copies, or the launch failed.
        let handover = if bg { None } else { self.fg_handover.take_if(|h| h.0 == id).map(|h| h.1) };
        // Re-check the RAM payload against the signed manifest right before running it.
        if !store::payload_present(&m, &payload_dir(id)) {
            return Err("payload missing".into());
        }
        let uid = self.state.uid(id)?;
        let cg = Path::new(CGROUP_ROOT).join("apps").join(&key);
        if cg.exists() {
            let _ = write_file(cg.join("cgroup.kill"), "1");
            std::thread::sleep(Duration::from_millis(50));
            let _ = std::fs::remove_dir(&cg);
        }
        std::fs::create_dir(&cg).map_err(|e| format!("cgroup: {e}"))?;
        write_file(cg.join("memory.max"), &(memory_mb as u64 * 1024 * 1024).to_string())?;
        let _ = write_file(cg.join("memory.swap.max"), "0");
        write_file(cg.join("memory.oom.group"), "1")?;
        write_file(cg.join("pids.max"), &pids.to_string())?;
        write_file(cg.join("cpu.weight"), &cpu_weight.to_string())?;

        let storage = if g.has("storage") {
            let mnt = PathBuf::from(format!("/run/appd/storage/{id}"));
            if let Err(e) = crate::storage::mount_and_load(&mnt, m.quota_kb(), uid, &self.state.snapshot(id)) {
                log(&format!("{id}: storage: {e}"));
                if !is_mount(&mnt) {
                    let _ = std::fs::remove_dir(&cg);
                    return Err(format!("storage: {e}"));
                }
            }
            Some(mnt)
        } else {
            None
        };
        let persisted_hash = storage.as_ref().and_then(|s| crate::storage::pack(s).ok()).map(|(b, _)| sha256_hex(&b));

        let mut env = vec![
            ("PATH".to_string(), "/app/bin".to_string()),
            ("HOME".into(), if storage.is_some() { "/data".into() } else { "/tmp".into() }),
            ("TMPDIR".into(), "/tmp".into()),
            ("XDG_RUNTIME_DIR".into(), "/tmp".into()),
            ("MEOW_APP_ID".into(), id.to_string()),
            ("MEOW_BROKER_FD".into(), BROKER_FD.to_string()),
            ("MEOW_PERMISSIONS".into(), g.granted.join(",")),
            ("MEOW_VOLUME".into(), self.volume.level.to_string()),
            ("MEOW_MUTED".into(), (self.volume.muted as u8).to_string()),
            ("MEOW_MODE".into(), if bg { "service".into() } else { "app".into() }),
            ("MEOW_FONTS".into(), "/usr/share/fonts/noto".into()),
            // The launcher's splash colour: the screen shows it until the app's first frame.
            ("MEOW_SPLASH".into(), m.appearance.tint.clone()),
            // Mesa: PP-stream cache (see ui/README.md), and no shader disk cache in the quota.
            ("LIMA_PLB_PP_STREAM_CACHE_SIZE".into(), "4194304".into()),
            ("MESA_SHADER_CACHE_DISABLE".into(), "true".into()),
        ];
        if let Ok(tz) = std::env::var("TZ") {
            env.push(("TZ".into(), tz));
        }
        if let Some(p) = payload {
            env.push(("MEOW_LAUNCH_PAYLOAD".into(), p));
        }
        let sandbox_root = PathBuf::from(format!("/run/appd/sandbox/{key}"));
        let mut spec = Spec {
            id: id.to_string(),
            uid,
            root: sandbox_root.display().to_string(),
            payload: payload_dir(id).display().to_string(),
            exec: m.exec.clone(),
            args,
            env,
            cgroup: cg.display().to_string(),
            // "lan" shares the namespace too (perms.rs says why it is its own name).
            network: g.has("network") || g.has("lan"),
            display: g.has("display"),
            media: g.has("media"),
            storage: storage.as_ref().map(|p| p.display().to_string()),
            tmp_mb: if bg { 1 } else { m.resources.tmp_mb },
            net_fd: g.has("internet"),
            launch_fds: handover.as_ref().map_or(0, |h| h.count() as u32),
            // "lan" without "network": only the manifest's fixed ports can be bound.
            lan_ports: (g.has("lan") && !g.has("network")).then(|| {
                let ports = m.lan_ports();
                (ports.tcp, ports.udp)
            }),
        };
        // What the app's background service handed over, on LAUNCH_FD and up.
        let mut launch_raw = [-1i32; crate::handover::MAX_FDS];
        if let Some(h) = &handover {
            spec.env.extend(h.env(crate::sandbox::LAUNCH_FD));
            for (slot, fd) in launch_raw.iter_mut().zip(h.raw()) {
                *slot = fd;
            }
        }
        // "internet": the app's dial socket to the network service, inherited on NET_FD.
        let dial = if g.has("internet") { Some(self.net_dial(&key, &m.internet_hosts())?) } else { None };
        let dial_raw = dial.as_ref().map(|d| d.as_raw_fd());
        if dial.is_some() {
            spec.env.push(("MEOW_NET_FD".into(), crate::sandbox::NET_FD.to_string()));
        }
        let (mine, theirs) = ipc::socketpair().map_err(|e| format!("socketpair: {e}"))?;
        let mut p = [0; 2];
        if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return Err("pipe".into());
        }
        let (log_rd, log_wr) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
        unsafe { libc::fcntl(log_rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let theirs_raw = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new("/proc/self/exe");
        cmd.arg("sandbox-exec")
            .env_clear()
            .env("APPD_SPEC", serde_json::to_string(&spec).map_err(|e| e.to_string())?)
            .stdin(std::process::Stdio::null())
            .stdout(log_wr.try_clone().map_err(|e| e.to_string())?)
            .stderr(log_wr);
        unsafe {
            cmd.pre_exec(move || {
                // Only async-signal-safe calls here: put the broker end on fd 3 and the dial
                // on fd 4, inheritable. Both move out of the way first, so neither target
                // can be the other's source.
                let high = |fd: i32| -> std::io::Result<i32> {
                    let moved = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
                    if moved < 0 { Err(std::io::Error::last_os_error()) } else { Ok(moved) }
                };
                // The handed-over fds move above every number that is assigned below.
                let mut launch = launch_raw;
                for fd in launch.iter_mut().filter(|fd| **fd >= 0) {
                    *fd = libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 64);
                    if *fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                let broker = high(theirs_raw)?;
                let dial = match dial_raw {
                    Some(fd) => Some(high(fd)?),
                    None => None,
                };
                if libc::dup2(broker, BROKER_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(dial) = dial {
                    if libc::dup2(dial, crate::sandbox::NET_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (i, fd) in launch.iter().enumerate().filter(|(_, fd)| **fd >= 0) {
                    if libc::dup2(*fd, crate::sandbox::LAUNCH_FD + i as i32) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let spawned = cmd.spawn().map_err(|e| format!("spawn sandbox: {e}")).and_then(|child| {
            let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) } as i32;
            if pidfd < 0 { Err(format!("pidfd_open: {}", std::io::Error::last_os_error())) } else { Ok((child, pidfd)) }
        });
        drop(theirs);
        drop(dial);
        let (child, pidfd) = match spawned {
            Ok(started) => started,
            Err(e) => {
                self.net_held.retain(|h| h.0 != key);
                return Err(e);
            }
        };
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
        let _ = ipc::send(mine.as_raw_fd(), &format!("WELCOME\t{id}\t{}", g.granted.join(",")), None);
        Ok(Running {
            id: key,
            app: id.to_string(),
            bg,
            payload_sha: m.payload.sha256.clone(),
            discard: false,
            child,
            pidfd,
            broker: mine,
            log_rd,
            log: VecDeque::new(),
            display: g.has("display"),
            input: g.has("input"),
            internet: g.has("internet"),
            store_sub: false,
            storage,
            started: Instant::now(),
            stop_at: None,
            last_sync: None,
            sync_due: None,
            persisted_hash,
            cgroup: cg,
            sandbox_root,
        })
    }

    /// Hand the screen back to the UI after app `id` (exit, HOME, failed start).
    fn give_back_display(&mut self, id: &str) {
        let held = matches!(&self.display, Display::Held { id: h, .. } if h == id);
        if !held {
            return;
        }
        let Display::Held { fd, .. } = std::mem::replace(&mut self.display, Display::Free) else { unreachable!() };
        // Revoke first: from here on the app's KMS calls fail, whatever it does.
        unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_DROP_MASTER as _, 0) };
        match self.ui_token() {
            Some(ui) => {
                self.display = Display::Reclaiming { _fd: fd, since: Instant::now() };
                self.send_to(ui, &format!("RECLAIM_DISPLAY\t{id}"));
            }
            None => drop(fd),
        }
    }

    fn stop(&mut self, id: &str) {
        let Some(r) = self.running.iter_mut().find(|r| r.id == id) else { return };
        if r.stop_at.is_none() {
            let _ = ipc::send(r.broker.as_raw_fd(), "QUIT", None);
            r.stop_at = Some(Instant::now() + STOP_GRACE);
        }
        let display = r.display;
        if display {
            self.give_back_display(id);
            self.broadcast_ui("FOREGROUND\t-");
        }
    }

    // ---------------------------------------------------------------- running apps

    fn read_log(&mut self, id: &str) {
        let Some(r) = self.running.iter_mut().find(|r| r.id == id) else { return };
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(r.log_rd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                return;
            }
            r.log.extend(&buf[..n as usize]);
            while r.log.len() > LOG_CAP {
                r.log.pop_front();
            }
        }
    }

    fn broker_readable(&mut self, id: &str) {
        loop {
            let Some(r) = self.running.iter().find(|r| r.id == id) else { return };
            let msg = match ipc::recv(r.broker.as_raw_fd()) {
                Ok(Recv::Msg(m, passed)) => (m, passed),
                _ => return,
            };
            let (msg, passed) = msg;
            let f: Vec<&str> = msg.split('\t').collect();
            let fd = r.broker.as_raw_fd();
            let (app, bg) = (r.app.clone(), r.bg);
            let mut perms: Vec<String> = self.apps.get(&app).map(|a| a.grants.granted.clone()).unwrap_or_default();
            if bg {
                perms.retain(|p| !matches!(p.as_str(), "display" | "input" | "media"));
            }
            match f[0] {
                "HELLO" => {
                    let _ = ipc::send(fd, &format!("WELCOME\t{app}\t{}", perms.join(",")), None);
                }
                "DISPLAY" => {
                    let reply = match &self.display {
                        _ if !perms.iter().any(|p| p == "display") => Err("permission display not granted"),
                        Display::Held { id: h, fd: dfd } if h == id => Ok(dfd.as_raw_fd()),
                        _ => Err("not in the foreground"),
                    };
                    let _ = match reply {
                        Ok(dfd) => ipc::send(fd, "DISPLAY\tok", Some(dfd)),
                        Err(e) => ipc::send(fd, &format!("DISPLAY\tdenied\t{e}"), None),
                    };
                }
                "SYNC" => {
                    let r = self.sync_storage(id, true);
                    let _ = ipc::send(fd, &format!("SYNC\t{}", field(&r)), None);
                }
                "LOG" => {
                    let line = format!("{}\n", f.get(1).unwrap_or(&""));
                    if let Some(r) = self.running.iter_mut().find(|r| r.id == id) {
                        r.log.extend(line.as_bytes());
                        while r.log.len() > LOG_CAP {
                            r.log.pop_front();
                        }
                    }
                }
                "VOLUME" => {
                    // One volume for the whole device: an app that plays sound may set it.
                    if let (true, Some(v)) = (perms.iter().any(|p| p == "media"), f.get(1).and_then(|v| v.parse::<u8>().ok())) {
                        self.set_volume(Volume { level: v, muted: false });
                    }
                }
                "MUTE" => {
                    if let (true, Some(m)) = (perms.iter().any(|p| p == "media"), f.get(1).map(|m| *m == "1")) {
                        self.set_volume(Volume { muted: m, ..self.volume });
                    }
                }
                "BACKGROUND" => {
                    // `BACKGROUND get|set id [0|1]`: another app's "Run in background" switch,
                    // for an app that holds "manage-receivers" and whose signed manifest
                    // lists that app ("manages"). The answer is the switch as it is now:
                    // absent (not installed, or no background service), on, off.
                    let target = f.get(2).copied().unwrap_or("");
                    let granted = perms.iter().any(|p| p == "manage-receivers");
                    let state = if !self.apps.get(&app).is_some_and(|a| a.manifest.may_manage(granted, target)) {
                        "denied"
                    } else {
                        if let (Some(&"set"), Some(on)) = (f.get(1), f.get(3).map(|v| *v == "1")) {
                            match self.set_background(target, on) {
                                Ok(_) => log(&format!("{app}: switched run in background of {target} {}", if on { "on" } else { "off" })),
                                Err(e) => log(&format!("{app}: run in background of {target}: {e}")),
                            }
                        }
                        match self.apps.get(target) {
                            Some(a) if a.installed && a.manifest.background.is_some() => {
                                if self.bg_wanted(target) { "on" } else { "off" }
                            }
                            _ => "absent",
                        }
                    };
                    let _ = ipc::send(fd, &format!("BACKGROUND\t{}\t{state}", field(target)), None);
                }
                "ATTACH" => {
                    // A socket or a data block for the next FOREGROUND request (handover.rs).
                    // No reply: a refused attachment refuses that request.
                    let sockets = perms.iter().any(|p| p == "lan" || p == "network");
                    self.attached.entry(id.to_string()).or_default().attach(f.get(1).copied().unwrap_or(""), passed, sockets);
                }
                "MIRACAST" => {
                    // Be a Miracast sink (miracast.rs): netd owns the radio, the app only asks.
                    let state = if perms.iter().any(|p| p == "wifi-direct") { self.miracast.request(&app, &f[1..], Instant::now()) } else { "unavailable\tpermission wifi-direct not granted".into() };
                    let _ = ipc::send(fd, &format!("MIRACAST\t{state}"), None);
                }
                "IMAGE" => {
                    // A connection to the image service, which is fetched and started now
                    // if this is the first app to want a picture decoded.
                    let _ = match self.img_connect(id) {
                        Ok(socket) => ipc::send(fd, "IMAGE\tok", Some(socket.as_raw_fd())),
                        Err(e) => ipc::send(fd, &format!("IMAGE\tunavailable\t{}", field(&e)), None),
                    };
                    self.img_ensure();
                }
                "FOREGROUND" => {
                    // A background service: "open my app now" (a cast started, a call came in).
                    let handover = self.attached.remove(id).unwrap_or_default().take();
                    let r = match (bg, &handover) {
                        (false, _) => Err("only a background service asks for the foreground".into()),
                        (true, Err(e)) => Err(format!("attachment refused: {e}")),
                        (true, Ok(_)) => self.request_foreground(&app, f.get(1).copied().unwrap_or(""), Instant::now()),
                    };
                    // The app cannot have been started yet (its service, this one, is still
                    // running and goes first), so the set is in place before any launch.
                    if let (Ok(()), Ok(h)) = (&r, handover) {
                        if !h.is_empty() {
                            log(&format!("{app}: {} file descriptors handed over with the request", h.count()));
                        }
                        // Kept only while the request or the launch it became is alive.
                        if self.fg_request.as_ref().is_some_and(|r| r.0 == app) || self.pending.as_ref().is_some_and(|p| p.id == app) {
                            self.fg_handover = Some((app.clone(), h));
                        }
                    }
                    let _ = match r {
                        Ok(()) => ipc::send(fd, "FOREGROUND\tok", None),
                        Err(e) => ipc::send(fd, &format!("FOREGROUND\tdenied\t{}", field(&e)), None),
                    };
                }
                "STORE" => {
                    // The Store app: only an app of the built-in repository that holds
                    // "manage-apps", and only while it is the app itself (not a service).
                    if !bg && perms.iter().any(|p| p == "manage-apps") && self.apps.get(&app).is_some_and(Self::system_app) {
                        self.store_request(id, &f);
                    } else {
                        let _ = ipc::send(fd, &format!("STORE\tresult\t{}\t\terror\tpermission manage-apps not granted", field(f.get(1).copied().unwrap_or(""))), None);
                    }
                }
                "EXIT" => {}
                _ => {
                    let _ = ipc::send(fd, "ERR\tunknown request", None);
                }
            }
        }
    }

    /// Save the app's /data to flash if it changed. `requested`: the app asked (rate-limited;
    /// a sync inside the interval is deferred to its end).
    fn sync_storage(&mut self, id: &str, requested: bool) -> String {
        let Some(r) = self.running.iter_mut().find(|r| r.id == id) else { return "error not running".into() };
        let Some(mnt) = r.storage.clone() else { return "error no storage permission".into() };
        let now = Instant::now();
        if requested {
            if let Some(last) = r.last_sync {
                if now.duration_since(last) < SYNC_MIN_INTERVAL {
                    r.sync_due = Some(last + SYNC_MIN_INTERVAL);
                    return "deferred".into();
                }
            }
        }
        r.sync_due = None;
        r.last_sync = Some(now);
        let prev = r.persisted_hash.clone();
        let snapshot = self.state.snapshot(&r.app);
        match persist(&mnt, &snapshot, prev.as_deref()) {
            Ok(Some(h)) => {
                if let Some(r) = self.running.iter_mut().find(|r| r.id == id) {
                    r.persisted_hash = Some(h);
                }
                "ok".into()
            }
            Ok(None) => "unchanged".into(),
            Err(e) => {
                log(&format!("{id}: storage sync failed: {e}"));
                format!("error {e}")
            }
        }
    }

    fn on_exit(&mut self, id: &str) {
        let Some(i) = self.running.iter().position(|r| r.id == id) else { return };
        self.read_log(id);
        let mut r = self.running.remove(i);
        let code = r.child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let secs = r.started.elapsed().as_secs_f32();
        let peak = std::fs::read_to_string(r.cgroup.join("memory.peak")).unwrap_or_default().trim().parse::<u64>().unwrap_or(0);
        let oom = std::fs::read_to_string(r.cgroup.join("memory.events"))
            .unwrap_or_default()
            .lines()
            .any(|l| l.starts_with("oom_kill ") && !l.ends_with(" 0"));
        log(&format!("{id}: exited code={code} after {secs:.1}s peak_mem_kb={} oom={oom}", peak / 1024));
        let tail: String = String::from_utf8_lossy(&r.log.iter().copied().collect::<Vec<u8>>()).lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
        if !tail.is_empty() {
            log(&format!("{id}: log tail: {tail}"));
        }
        // Make sure nothing of the app survives, then persist its storage.
        let _ = write_file(r.cgroup.join("cgroup.kill"), "1");
        if let Some(mnt) = r.storage.take() {
            if !r.discard {
                if let Err(e) = persist(&mnt, &self.state.snapshot(&r.app), r.persisted_hash.as_deref()) {
                    log(&format!("{id}: storage save failed: {e}"));
                }
            }
            crate::storage::unmount(&mnt);
        }
        for _ in 0..20 {
            if std::fs::remove_dir(&r.cgroup).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = std::fs::remove_dir(&r.sandbox_root);
        self.old_logs.insert(id.to_string(), r.log.iter().copied().collect());
        // Its dial, if the network service never took it.
        self.net_held.retain(|h| h.0 != id);
        self.img_held.retain(|h| h.0 != id);
        self.attached.remove(id);
        if self.apps.get(id).is_some_and(|a| a.manifest.service.as_deref() == Some("image")) {
            // The image service left (nobody was connected) or died (a picture it could not
            // survive): the apps that were connected see their sockets close and ask again.
            // One that died twice in a row is not started a third time at once.
            if r.stop_at.is_none() && code != 0 && secs < 5.0 {
                self.img_failed();
            }
        } else if self.is_service(id) {
            // The network service is gone (an update, or a crash): every app that uses it
            // gets a new dial, and the service is started again for them.
            self.net_running = None;
            if r.stop_at.is_none() {
                self.net_retry_at = Some(Instant::now() + Duration::from_secs(1));
            }
            let clients: Vec<(String, String)> = self
                .running
                .iter()
                .filter(|r| r.internet)
                .filter_map(|r| self.apps.get(&r.app).map(|a| (r.id.clone(), a.manifest.internet_hosts())))
                .collect();
            for (client, hosts) in clients {
                let Ok(dial) = self.net_dial(&client, &hosts) else { continue };
                if let Some(r) = self.running.iter().find(|r| r.id == client) {
                    let _ = ipc::send(r.broker.as_raw_fd(), "NET", Some(dial.as_raw_fd()));
                }
            }
        }
        self.miracast.exited(&r.app, r.bg, Instant::now());
        if r.display {
            self.give_back_display(id);
            self.broadcast_ui("FOREGROUND\t-");
        }
        if r.bg {
            // A background service: when it comes back is the supervisor's business; the
            // app's own state (its card) is not touched. A launch waiting for it goes on.
            let app = r.app.clone();
            let now = Instant::now();
            let wait = self.bg.entry(app.clone()).or_default().exited(now, r.stop_at.is_some());
            if r.stop_at.is_none() {
                log(&format!("{id}: died; again in {} s", wait.as_secs()));
            }
            if self.pending.as_ref().is_some_and(|p| p.id == app && matches!(p.stage, Stage::ServiceStop)) {
                self.continue_launch(&app);
            }
            self.push_app(&app);
            self.net_swap();
            self.net_ensure();
            return;
        }
        let sandbox_failed = code == 70 && secs < 5.0;
        if let Some(a) = self.apps.get_mut(id) {
            a.status = if oom {
                Status::Error("ran out of memory".into())
            } else if sandbox_failed {
                Status::Error(tail.rsplit(" | ").next().unwrap_or("sandbox setup failed").chars().take(60).collect())
            } else if code != 0 && r.stop_at.is_none() {
                Status::Error(format!("stopped with code {code}"))
            } else {
                Status::Idle
            };
        }
        self.push_app(id);
        self.net_swap();
        self.net_ensure();
        self.img_ensure();
    }
}

/// Pack `mnt`; write the snapshot if it differs from `prev_hash`. Returns the new hash.
fn persist(mnt: &Path, snapshot: &Path, prev_hash: Option<&str>) -> Result<Option<String>, String> {
    let (bytes, _) = crate::storage::pack(mnt)?;
    let h = sha256_hex(&bytes);
    if prev_hash == Some(h.as_str()) {
        return Ok(None);
    }
    let old = std::fs::metadata(snapshot).map(|m| m.len()).unwrap_or(0);
    let free = fs_free(snapshot.parent().unwrap_or(Path::new("/data"))).unwrap_or(0);
    if (bytes.len() as u64) > free + old {
        return Err(format!("flash full ({} bytes needed, {} free)", bytes.len(), free));
    }
    crate::util::write_atomic(snapshot, &bytes, 0o600).map_err(|e| e.to_string())?;
    Ok(Some(h))
}

fn payload_dir(id: &str) -> PathBuf {
    Path::new(APPS_RAM).join(id).join("payload")
}

fn is_mount(p: &Path) -> bool {
    let Ok(m) = std::fs::read_to_string("/proc/self/mountinfo") else { return false };
    let s = p.display().to_string();
    m.lines().any(|l| l.split(' ').nth(4) == Some(s.as_str()))
}

/// RAM home of app payloads: its own tmpfs (only pages in use cost memory), root only.
fn setup_ram_dir() -> Result<(), String> {
    let p = Path::new(APPS_RAM);
    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    if !is_mount(p) {
        let (t, o) = (crate::util::cstring("tmpfs"), crate::util::cstring("size=64m,mode=0711"));
        let r = unsafe { libc::mount(t.as_ptr(), cstr(p).as_ptr(), t.as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, o.as_ptr() as *const _) };
        if r < 0 {
            return Err(format!("mount {APPS_RAM}: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn setup_cgroups() -> Result<(), String> {
    let root = Path::new(CGROUP_ROOT);
    if !root.join("cgroup.controllers").exists() {
        return Err("cgroup2 is not mounted on /sys/fs/cgroup".into());
    }
    write_file(root.join("cgroup.subtree_control"), "+memory +pids +cpu")?;
    let apps = root.join("apps");
    std::fs::create_dir_all(&apps).map_err(|e| e.to_string())?;
    write_file(apps.join("cgroup.subtree_control"), "+memory +pids +cpu")?;
    write_file(apps.join("memory.max"), &APPS_MEMORY_MAX.to_string())?;
    // Leftovers of a previous appd (crash/restart): kill them.
    if let Ok(rd) = std::fs::read_dir(&apps) {
        for e in rd.flatten().filter(|e| e.path().is_dir()) {
            let _ = write_file(e.path().join("cgroup.kill"), "1");
            std::thread::sleep(Duration::from_millis(50));
            let _ = std::fs::remove_dir(e.path());
        }
    }
    Ok(())
}

/// Open the KMS card; succeeds only if this open file becomes (or is) the DRM master.
fn open_master() -> Result<OwnedFd, String> {
    let mut cards: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("card") && !n.contains('-'))
        .collect();
    cards.sort();
    let card = cards
        .into_iter()
        .find(|c| std::fs::read_dir(format!("/sys/class/drm/{c}")).is_ok_and(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&format!("{c}-")))))
        .ok_or("no KMS card with a connector")?;
    let path = format!("/dev/dri/{card}");
    let fd = unsafe { libc::open(crate::util::cstring(&path).as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!("{path}: {}", std::io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_SET_MASTER as _, 0) } < 0 {
        return Err(format!("the screen is still held by another program ({})", std::io::Error::last_os_error()));
    }
    Ok(fd)
}

