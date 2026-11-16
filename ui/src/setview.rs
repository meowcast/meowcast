//! The Settings view: categories on the left, the selected category's settings on a frosted
//! panel on the right, plus a picker (lists of choices) and a two-button dialog.
//!
//! Same rendering model as the home screen: every frame is a list of items with stable ids
//! (2_000_000+), diffed by main.rs, so moving the focus repaints only the rows it touches.
//! All art is drawn once on a start-up worker (`render_art`) and every static string is
//! rasterised there too (`text_runs`), so opening Settings never rasterises on the render path.
//!
//! D-pad: Up/Down move, Right/Enter go into a category and activate a row, Left/Back go back
//! out, Home closes. Values change at once; main.rs applies and saves them (`Cmd`).

use crate::apps::{AppDetail, AppInfo, Background};
use crate::art::{self, Glyph};
use crate::atlas::{Atlas, Image, Region};
use crate::gallery::Gallery;
use crate::gpu::Gpu;
use crate::home::{FOCUS_COLOR, Frame, Key, Slices, clip, nine, nine_ex, quad, solid};
use crate::settings::{self, DIMS, OUTPUTS, Settings, SysInfo, ZONES};
use crate::text::{Face, Fits, Text};
use crate::util::{RectF, Tween, rgba};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cat {
    Wifi,
    Network,
    Display,
    Wallpaper,
    Time,
    Apps,
    Developer,
    About,
}

impl Cat {
    pub const ALL: [Cat; 8] = [Cat::Wifi, Cat::Network, Cat::Display, Cat::Wallpaper, Cat::Time, Cat::Apps, Cat::Developer, Cat::About];
    pub fn title(self) -> &'static str {
        match self {
            Cat::Wifi => "Wi-Fi",
            Cat::Network => "Network",
            Cat::Display => "Display & sound",
            Cat::Wallpaper => "Wallpaper",
            Cat::Time => "Date & time",
            Cat::Apps => "Apps",
            Cat::Developer => "Developer",
            Cat::About => "About",
        }
    }
    fn blurb(self) -> &'static str {
        match self {
            Cat::Wifi => "Wireless networks",
            Cat::Network => "Addresses of the active connection",
            Cat::Display => "Resolution, volume, background and screen dimming",
            Cat::Wallpaper => "The background of the home screen",
            Cat::Time => "Time zone and clock format",
            Cat::Apps => "Installed apps, their permissions and what runs in the background",
            Cat::Developer => "Tools for testing the firmware",
            Cat::About => "This device",
        }
    }
    pub fn glyph(self) -> Glyph {
        match self {
            Cat::Wifi => Glyph::Wifi,
            Cat::Network => Glyph::Lan,
            Cat::Display => Glyph::Monitor,
            Cat::Wallpaper => Glyph::Photo,
            Cat::Time => Glyph::Clock,
            Cat::Apps => Glyph::Apps,
            Cat::Developer => Glyph::Terminal,
            Cat::About => Glyph::Info,
        }
    }
    fn index(self) -> usize {
        Cat::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }
}

// Layout (logical 1920x1080 px).
const LEFT_X: f32 = 110.0;
const HEADER_BASE: f32 = 300.0;
const CAT_Y: f32 = 336.0;
const CAT_H: f32 = 78.0;
const CAT_HL_X: f32 = 94.0;
const CAT_HL_W: f32 = 452.0;
const PANEL_X: f32 = 600.0;
const PANEL_Y: f32 = 252.0;
const PANEL_W: f32 = 1210.0;
const PANEL_H: f32 = 764.0;
const PAD_X: f32 = 48.0;
const CONTENT_Y: f32 = PANEL_Y + 140.0;
const CONTENT_BOTTOM: f32 = PANEL_Y + PANEL_H - 20.0;
const ROW_H: f32 = 96.0;
const INFO_H: f32 = 58.0;
const PERM_H: f32 = 78.0;
const EMPTY_H: f32 = 300.0;
const SWITCH_W: f32 = 72.0;
const SWITCH_H: f32 = 38.0;
const KNOB_D: f32 = 30.0;
/// A picker is as wide as its widest option needs, between these (see `PickLayout`).
const PICK_MIN_W: f32 = 480.0;
const PICK_MAX_W: f32 = PANEL_W - 32.0;
const PICK_ITEM: f32 = 68.0;
/// Room kept for the label of an information row; its value has the rest of the line.
const INFO_LABEL_W: f32 = 300.0;
/// Gap between a row's texts and what is on its right (value, switch).
const ROW_GAP: f32 = 32.0;
const PICK_MAX: usize = 8;
// The wallpaper grid: tiles of the thumbnail sheet's cell size (1:1 texels at 1080p).
const WALL_COLS: usize = 4;
const TILE_W: f32 = 264.0;
const TILE_H: f32 = 148.0;
const TILE_GAP_Y: f32 = 20.0;
const TILE_PITCH: f32 = TILE_H + TILE_GAP_Y;
const GRID_Y: f32 = CONTENT_Y + 6.0;
/// Height of the grid's window: three rows (more wallpapers scroll).
const GRID_H: f32 = 3.0 * TILE_H + 2.0 * TILE_GAP_Y;

const OPEN_MS: u64 = 280;
const MOVE_MS: u64 = 140;
const SWITCH_MS: u64 = 170;
const FADE_MS: f32 = 160.0;
const POP_MS: f32 = 150.0;
const SEARCH_MS: u64 = 1200;
/// How long the user has to keep a new resolution before it reverts.
pub const KEEP_SECS: u64 = 15;

const ID: u32 = 2_000_000;
/// Ids of the panel and everything in it.
const ID_CAT: u32 = ID + 10;
const ID_CAT_HL: u32 = ID + 100;
const ID_HEAD: u32 = ID + 200;
const ID_ROW_HL: u32 = ID + 290;
const ID_ROW: u32 = ID + 300;
const ID_PICK: u32 = ID + 500;
const ID_DIALOG: u32 = ID + 700;
/// The wallpaper grid: placeholders +i, thumbnails +30+i (i < gallery::MAX_WALLS), then the rest.
const ID_WALL: u32 = ID + 800;

// Strings (also rasterised at start-up, see text_runs).
const S_SETTINGS: &str = "Settings";
const S_RESOLUTION: &str = "Resolution";
const S_RESOLUTION_SUB: &str = "The launcher restarts in the new mode";
const S_DEPTH: &str = "Background depth effect";
const S_DEPTH_SUB: &str = "Blur and darken the wallpaper below the first row";
const S_DIM: &str = "Dim the screen when idle";
const S_DIM_SUB: &str = "Any button wakes it up";
const S_VOLUME: &str = "Volume";
const S_VOLUME_SUB: &str = "For everything this device plays; apps and phones change it too";
/// The steps of the volume picker (the system volume is 0-100).
pub const VOLUMES: [u32; 11] = [100, 90, 80, 70, 60, 50, 40, 30, 20, 10, 0];
const S_ZONE: &str = "Time zone";
const S_CLOCK24: &str = "24-hour clock";
const S_CLOCK24_SUB: &str = "13:00 instead of 1:00 PM";
const S_HUD: &str = "Performance overlay";
const S_HUD_SUB: &str = "Frame rate, late frames, GPU clock and temperature";
const S_RESTART: &str = "Restart the launcher";
const S_RESTART_SUB: &str = "Reloads the home screen and its settings";
const S_SEARCH: &str = "Search again";
const S_SEARCHING: &str = "Searching…";
const S_NO_WIFI: &str = "No Wi-Fi adapter found";
const S_NO_WIFI_TEXT: &str = "Use the dongle's original cable (the one with the Wi-Fi adapter), then restart.";
const S_NO_WIFI_USB: &str = "The USB port is connected to a computer. Wi-Fi needs the dongle's original cable.";
const S_WIFI_SETUP: &str = "Set up with phone";
const S_WIFI_SETUP_SUB: &str = "Choose a network and pair the pilot app on your phone";
const S_WIFI_PAIR: &str = "Pair a phone";
const S_WIFI_PAIR_SUB: &str = "Shows a code to scan: the phone becomes a remote";
const S_WIFI_FORGET: &str = "Forget this network";
const S_UNPAIR: &str = "Unpair all phones";
const S_NETDEBUG: &str = "Network debugging";
const S_NETDEBUG_SUB: &str = "SSH access for one hour; turns off after restart";
const S_RESET: &str = "Factory reset";
const S_RESET_SUB: &str = "Erases Wi-Fi networks, paired phones, apps and all settings";
const S_RESET_TITLE: &str = "Erase everything on this device?";
const S_RESET_BODY: &str = "Wi-Fi networks, paired phones, apps with their data and all settings will be erased, and the TV starts as new. This cannot be undone.";
const S_RESET_YES: &str = "Erase everything";
const S_UNINSTALL: &str = "Uninstall";
const S_UNINSTALL_SUB: &str = "Removes the app and everything it saved on this device";
const S_BACKGROUND: &str = "Run in background";
const S_BG_APPS: &str = "Background apps";
const S_PERMISSIONS: &str = "Permissions";
const S_BG_ON: &str = "Runs in background";
const AMBER: u32 = 0xf3b04c;
const S_KEEP_TITLE: &str = "Keep this resolution?";
const S_KEEP: &str = "Keep";
const S_REVERT: &str = "Revert";
const S_CANCEL: &str = "Cancel";
const S_WALL_LOADING: &str = "Loading wallpapers…";
const S_WALL_OFFLINE: &str = "No internet connection";
const S_WALL_OFFLINE_TEXT: &str = "Wallpapers come from the meow store. Connect this TV to a network to choose one.";
const S_WALL_ERROR: &str = "Wallpapers are not available";
const S_WALL_ERROR_TEXT: &str = "The meow store could not be reached. Try again in a moment.";
const S_WALL_CURRENT: &str = "Current wallpaper";
const S_WALL_USE: &str = "Press OK to use this wallpaper";
const S_WALL_BUSY: &str = "Downloading…";

#[derive(Clone, Debug, PartialEq)]
pub enum RowId {
    Output,
    Volume,
    Depth,
    Dim,
    Zone,
    Clock24,
    Hud,
    Restart,
    WifiScan,
    WifiSetup,
    WifiPair,
    WifiForget,
    Unpair,
    FactoryReset,
    NetDebug,
    DebugLan,
    DebugCloud,
    DebugPair,
    DebugRevoke(String),
    /// An installed app in the list: opens its page.
    App(String),
    /// The "Run in background" switch of an app (in the list's overview and on its page).
    AppBg(String),
    AppUninstall(String),
    None,
}

enum Kind {
    Toggle(bool),
    Choice(String),
    Action(String),
    Info(String),
    /// A centred empty state: big icon, title and one line of text.
    Empty(Glyph, String, String),
    /// A permission of an app: label, a line of detail, and its state on the right
    /// (text, colour); the dot in front is blue for one that needs the user's consent.
    Perm(String, u32, bool),
}

struct Row {
    id: RowId,
    label: String,
    sub: String,
    kind: Kind,
}

impl Row {
    fn new(id: RowId, label: impl Into<String>, sub: impl Into<String>, kind: Kind) -> Row {
        Row { id, label: label.into(), sub: sub.into(), kind }
    }
    fn info(label: impl Into<String>, value: impl Into<String>) -> Row {
        Row::new(RowId::None, label, "", Kind::Info(value.into()))
    }
    fn focusable(&self) -> bool {
        matches!(self.kind, Kind::Toggle(_) | Kind::Choice(_) | Kind::Action(_))
    }
    fn height(&self) -> f32 {
        match self.kind {
            Kind::Info(_) => INFO_H,
            Kind::Empty(..) => EMPTY_H,
            Kind::Perm(..) => PERM_H,
            _ => ROW_H,
        }
    }
}

/// What a key press asks main.rs to do.
pub enum Cmd {
    Close,
    SetOutput(String),
    KeepOutput,
    RevertOutput,
    SetZone(String),
    SetDim(u32),
    /// Set the system volume (appd keeps it; apps follow it).
    SetVolume(u32),
    ToggleDepth,
    ToggleClock24,
    ToggleHud,
    Restart,
    WifiScan,
    WifiSetup,
    WifiPair,
    WifiForget,
    Unpair,
    ToggleNetDebug,
    ToggleDebugLan,
    ToggleDebugCloud,
    DebugPair,
    DebugRevoke(String),
    FactoryReset,
    Uninstall(String),
    /// Switch an app's "Run in background" on or off (appd keeps it and starts or stops
    /// the app's service).
    SetBackground(String, bool),
    /// Use this wallpaper asset (gallery.rs).
    SetWallpaper(String),
}

/// Everything the pages show that lives outside the view.
pub struct Ctx<'a> {
    pub settings: &'a Settings,
    pub info: &'a SysInfo,
    pub apps: &'a [AppInfo],
    pub appd: bool,
    /// The mode the display really runs (kms.rs name), and its refresh rate.
    pub output_now: &'a str,
    pub planes: bool,
    /// meow-netd's state (Wi-Fi, pilots, debug bridge).
    pub net: &'a crate::onboard::NetState,
    /// The store's wallpapers and their thumbnails.
    pub gallery: &'a Gallery,
    /// The chosen wallpaper is still being downloaded (its blurred preview is shown).
    pub wall_busy: bool,
    /// The system volume, 0-100 (appd's; None without appd).
    pub volume: Option<u32>,
    /// appd's details of the installed apps (version, permissions, background service).
    pub details: &'a HashMap<String, AppDetail>,
}

pub struct Picker {
    id: RowId,
    options: Vec<(String, String)>,
    current: usize,
    focus: usize,
    hl: Tween,
    scroll: Tween,
    shown: Instant,
    /// Logical y of the centre of the row it opened from.
    anchor: f32,
    /// Measured when it is first drawn.
    layout: Option<PickLayout>,
}

/// A picker's box, from its options (physical px): the box is as wide as the widest label
/// and the widest hint side by side need. Only when that is more than the panel has do the
/// two columns get less than they need, and their texts an ellipsis.
#[derive(Clone, Copy)]
struct PickLayout {
    w: f32,
    label: f32,
    hint: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DialogKind {
    KeepOutput,
    Uninstall(String),
    DebugRevoke(String),
    FactoryReset,
}

pub struct Dialog {
    pub kind: DialogKind,
    title: String,
    body: String,
    buttons: [&'static str; 2],
    focus: usize,
    shown: Instant,
    /// Keep-output countdown: when it reverts.
    pub deadline: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Cats,
    Rows,
}

/// CPU-side art (worker thread); `SettingsView::set_art` uploads it.
pub struct ArtImages {
    cats: Vec<Image>,
    check: Image,
    check_small: Image,
    chevron: Image,
    wifi_big: Image,
    pill: art::Slice,
    knob: Image,
    dot: Image,
}

struct SetArt {
    cats: Vec<Option<Region>>,
    check: Option<Region>,
    check_small: Option<Region>,
    chevron: Option<Region>,
    wifi_big: Option<Region>,
    pill: Option<(Region, u32)>,
    knob: Option<Region>,
    dot: Option<Region>,
}

pub fn render_art(scale: f32) -> ArtImages {
    let px = |v: f32| (v * scale).round().max(4.0) as u32;
    ArtImages {
        cats: Cat::ALL.iter().map(|c| art::line_icon(c.glyph(), px(32.0))).collect(),
        check: art::line_icon(Glyph::Check, px(28.0)),
        check_small: art::line_icon(Glyph::Check, px(20.0)),
        chevron: art::line_icon(Glyph::Chevron, px(22.0)),
        wifi_big: art::line_icon(Glyph::Wifi, px(96.0)),
        pill: art::slice_rounded(SWITCH_H / 2.0 * scale),
        knob: art::dot(px(KNOB_D)),
        dot: art::dot(px(10.0)),
    }
}

/// Every static string the view draws, with its face and physical size (rasterised on a
/// worker at start-up), plus the character sets of the live values.
pub fn text_runs(scale: f32) -> Vec<(Face, f32, String)> {
    // Few distinct sizes, shared with the home screen (29 = card titles, 21 = subtitles,
    // 24/26/28/46 = launch splash and consent dialog): every size and weight opens its own
    // atlas shelves, and half-empty shelves are what fill the atlas.
    let s = scale;
    let mut cats = String::new();
    let mut titles = [S_NO_WIFI, S_WALL_LOADING, S_WALL_OFFLINE, S_WALL_ERROR].concat();
    let mut blurbs = [S_NO_WIFI_TEXT, S_WALL_OFFLINE_TEXT, S_WALL_ERROR_TEXT].concat();
    for c in Cat::ALL {
        cats.push_str(c.title());
        titles.push_str(c.title());
        blurbs.push_str(c.blurb());
    }
    for b in [S_NO_WIFI_USB, S_WIFI_SETUP_SUB, S_WIFI_PAIR_SUB, S_NETDEBUG_SUB] {
        blurbs.push_str(b);
    }
    let mut labels: String = [S_RESOLUTION, S_VOLUME, S_DEPTH, S_DIM, S_ZONE, S_CLOCK24, S_HUD, S_RESTART, S_SEARCH, S_SEARCHING, S_UNINSTALL, S_WIFI_SETUP, S_WIFI_PAIR, S_WIFI_FORGET, S_UNPAIR, S_NETDEBUG, S_BACKGROUND, S_BG_ON].concat();
    for o in OUTPUTS {
        labels.push_str(o.1);
    }
    for d in DIMS {
        labels.push_str(d.1);
    }
    for z in ZONES {
        labels.push_str(z.0);
        labels.push_str(z.1);
    }
    // Live values (addresses, sizes, versions, temperatures, app names).
    labels.push_str("0123456789.:,-+()/%°×· abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ");
    vec![
        (Face::Light, 48.0 * s, S_SETTINGS.to_string()),
        (Face::Regular, 29.0 * s, cats),
        (Face::Light, 40.0 * s, titles),
        (Face::Regular, 24.0 * s, blurbs),
        (Face::Regular, 28.0 * s, labels),
        // (The alphabet: app summaries and permission reasons come from the apps.)
        (Face::Regular, 21.0 * s, [S_RESOLUTION_SUB, S_VOLUME_SUB, S_DEPTH_SUB, S_DIM_SUB, S_CLOCK24_SUB, S_HUD_SUB, S_RESTART_SUB, S_UNINSTALL_SUB, "0123456789.:,-*·…' abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"].concat()),
        (Face::Regular, 26.0 * s, "0123456789.:,-+()/%°×·—… abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".into()),
        (Face::Light, 46.0 * s, S_KEEP_TITLE.into()),
        (Face::Medium, 28.0 * s, [S_KEEP, S_REVERT, S_UNINSTALL, S_CANCEL].concat()),
    ]
}

pub struct SettingsView {
    s: f32,
    art: Option<SetArt>,
    /// 0 = closed, 1 = fully open.
    open: Tween,
    cat: usize,
    pane: Pane,
    /// Focused row, as an index among the focusable rows of the category.
    row: usize,
    hl_cat: Tween,
    /// Top of the row highlight (logical, relative to CONTENT_Y, unscrolled).
    hl_row: Tween,
    scroll: Tween,
    switched: Instant,
    knobs: HashMap<String, Tween>,
    pub picker: Option<Picker>,
    pub dialog: Option<Dialog>,
    searching: Option<Instant>,
    /// Wallpaper grid: the focused tile, the focus ring's (column, row), and when a
    /// wallpaper was last chosen (its badge pops in).
    wall: usize,
    wall_hl: (Tween, Tween),
    wall_set: Instant,
    /// The user went for the grid before the catalog arrived: the focus goes in when it does.
    wall_pending: bool,
    /// Apps: the app whose page is open (None: the list).
    app_page: Option<String>,
    /// Texts cut to their room with an ellipsis: measured once, not on every frame.
    fits: Fits,
}

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn since_ms(now: Instant, t: Instant) -> f32 {
    now.saturating_duration_since(t).as_secs_f32() * 1000.0
}

impl SettingsView {
    pub fn new(scale: f32) -> SettingsView {
        let past = Instant::now() - Duration::from_secs(5);
        SettingsView {
            s: scale,
            art: None,
            open: Tween::fixed(0.0),
            cat: 0,
            pane: Pane::Cats,
            row: 0,
            hl_cat: Tween::fixed(0.0),
            hl_row: Tween::fixed(0.0),
            scroll: Tween::fixed(0.0),
            switched: past,
            knobs: HashMap::new(),
            picker: None,
            dialog: None,
            searching: None,
            wall: 0,
            wall_hl: (Tween::fixed(0.0), Tween::fixed(0.0)),
            wall_set: past,
            wall_pending: false,
            app_page: None,
            fits: Fits::default(),
        }
    }

    pub fn set_art(&mut self, gpu: &Gpu, atlas: &mut Atlas, img: ArtImages) {
        let mut ins = |i: &Image| atlas.insert(gpu, i);
        let cats = img.cats.iter().map(&mut ins).collect();
        let check = ins(&img.check);
        let check_small = ins(&img.check_small);
        let chevron = ins(&img.chevron);
        let wifi_big = ins(&img.wifi_big);
        let knob = ins(&img.knob);
        let dot = ins(&img.dot);
        let pill = atlas.insert(gpu, &img.pill.img).map(|r| (r, img.pill.margin));
        self.art = Some(SetArt { cats, check, check_small, chevron, wifi_big, pill, knob, dot });
    }

    /// Shown at all (open, or still fading out).
    pub fn visible(&self, now: Instant) -> bool {
        self.open.target() > 0.0 || self.open.value(now) > 0.0
    }
    /// The selected category (Settings reopens on it).
    pub fn cat(&self) -> Cat {
        Cat::ALL[self.cat]
    }

    /// Open (takes the keys).
    pub fn is_open(&self) -> bool {
        self.open.target() > 0.0
    }
    /// Open on `cat`; `into` puts the focus straight into its settings.
    pub fn open_at(&mut self, cat: Cat, into: bool, now: Instant, ctx: &Ctx) {
        self.open.go(1.0, now, Duration::from_millis(OPEN_MS));
        self.wall_pending = false;
        self.select_cat(cat.index(), now, false);
        self.hl_cat = Tween::fixed(self.cat as f32);
        self.pane = Pane::Cats;
        if into && rows(Cat::ALL[self.cat], ctx, self.searching.is_some(), self.app_page.as_deref()).iter().any(|r| r.focusable()) {
            self.pane = Pane::Rows;
        }
        self.focus_row(0, ctx, now, false);
        if into && cat == Cat::Wallpaper {
            self.enter_walls(ctx, now);
        }
    }

    pub fn close(&mut self, now: Instant) {
        self.open.go(0.0, now, Duration::from_millis(OPEN_MS - 60));
        self.picker = None;
        self.wall_pending = false;
    }

    /// Close without the fade (an app takes the screen).
    pub fn close_now(&mut self) {
        self.open = Tween::fixed(0.0);
        self.picker = None;
        self.dialog = None;
        self.wall_pending = false;
    }

    /// Focus the row with this id (after a restart into a new resolution).
    pub fn focus_id(&mut self, id: &RowId, ctx: &Ctx, now: Instant) {
        let rs = rows(Cat::ALL[self.cat], ctx, self.searching.is_some(), self.app_page.as_deref());
        if let Some(i) = rs.iter().filter(|r| r.focusable()).position(|r| &r.id == id) {
            self.pane = Pane::Rows;
            self.focus_row(i, ctx, now, false);
        }
    }

    pub fn show_dialog(&mut self, kind: DialogKind, title: String, body: String, buttons: [&'static str; 2], deadline: Option<Instant>, now: Instant) {
        self.picker = None;
        // A destructive question starts on the safe answer.
        let focus = if matches!(kind, DialogKind::Uninstall(_) | DialogKind::DebugRevoke(_) | DialogKind::FactoryReset) { 1 } else { 0 };
        self.dialog = Some(Dialog { kind, title, body, buttons, focus, shown: now, deadline });
    }

    pub fn wifi_scan_started(&mut self, now: Instant) {
        self.searching = Some(now);
    }

    fn select_cat(&mut self, i: usize, now: Instant, animate: bool) {
        if i != self.cat || !animate {
            self.switched = if animate { now } else { now - Duration::from_secs(5) };
        }
        self.cat = i;
        self.app_page = None;
        self.hl_cat.go(i as f32, now, Duration::from_millis(MOVE_MS));
        self.row = 0;
        self.scroll = Tween::fixed(0.0);
        self.knobs.clear();
    }

    /// Row tops (relative to CONTENT_Y) of `rs`.
    fn tops(rs: &[Row]) -> Vec<f32> {
        let mut y = 0.0;
        rs.iter()
            .map(|r| {
                let t = y;
                y += r.height();
                t
            })
            .collect()
    }

    fn focus_row(&mut self, i: usize, ctx: &Ctx, now: Instant, animate: bool) {
        let rs = rows(Cat::ALL[self.cat], ctx, self.searching.is_some(), self.app_page.as_deref());
        let tops = Self::tops(&rs);
        let focusable: Vec<usize> = (0..rs.len()).filter(|&k| rs[k].focusable()).collect();
        if focusable.is_empty() {
            self.row = 0;
            return;
        }
        self.row = i.min(focusable.len() - 1);
        let k = focusable[self.row];
        let top = tops[k];
        let dur = Duration::from_millis(if animate { MOVE_MS } else { 0 });
        if animate {
            self.hl_row.go(top, now, dur);
        } else {
            self.hl_row = Tween::fixed(top);
        }
        // Keep the focused row (and the row above it, when there is one) in view.
        let vis = CONTENT_BOTTOM - CONTENT_Y;
        let mut sc = self.scroll.target();
        let want_top = if k > 0 { tops[k - 1].min(top) } else { 0.0 };
        if want_top < sc {
            sc = want_top;
        }
        if top + rs[k].height() - sc > vis {
            sc = top + rs[k].height() - vis;
        }
        let total = tops.last().copied().unwrap_or(0.0) + rs.last().map_or(0.0, |r| r.height());
        sc = sc.clamp(0.0, (total - vis).max(0.0));
        if animate {
            self.scroll.go(sc, now, Duration::from_millis(MOVE_MS + 60));
        } else {
            self.scroll = Tween::fixed(sc);
        }
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.open.active(now)
            || self.hl_cat.active(now)
            || self.hl_row.active(now)
            || self.scroll.active(now)
            || since_ms(now, self.switched) < FADE_MS + 20.0
            || self.knobs.values().any(|k| k.active(now))
            || self.picker.as_ref().is_some_and(|p| since_ms(now, p.shown) < POP_MS + 20.0 || p.hl.active(now) || p.scroll.active(now))
            || self.dialog.as_ref().is_some_and(|d| since_ms(now, d.shown) < POP_MS + 20.0)
            || self.searching.is_some()
            || self.wall_hl.0.active(now)
            || self.wall_hl.1.active(now)
            || since_ms(now, self.wall_set) < POP_MS + 20.0
    }

    /// Next time the view changes by itself (dialog countdown, Wi-Fi search end).
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        let mut w: Option<Instant> = None;
        if let Some(d) = self.dialog.as_ref().and_then(|d| d.deadline) {
            let left = d.saturating_duration_since(now);
            let frac = left.as_secs_f32().fract();
            w = Some(now + Duration::from_secs_f32(frac + 0.001));
        }
        if let Some(t) = self.searching {
            let end = t + Duration::from_millis(SEARCH_MS);
            w = Some(w.map_or(end, |x| x.min(end)));
        }
        w
    }

    /// Housekeeping each loop: the Wi-Fi search feedback ends; the countdown expires.
    pub fn tick(&mut self, now: Instant) -> Option<Cmd> {
        if self.searching.is_some_and(|t| now >= t + Duration::from_millis(SEARCH_MS)) {
            self.searching = None;
        }
        if let Some(d) = self.dialog.as_ref() {
            if d.kind == DialogKind::KeepOutput && d.deadline.is_some_and(|t| now >= t) {
                self.dialog = None;
                return Some(Cmd::RevertOutput);
            }
        }
        if !self.is_open() && self.open.value(now) <= 0.0 {
            self.knobs.clear();
        }
        None
    }

    pub fn key(&mut self, k: Key, now: Instant, ctx: &Ctx) -> Option<Cmd> {
        self.wall_pending = false;
        if let Some(d) = self.dialog.as_mut() {
            let choice = match k {
                Key::Left => {
                    d.focus = 0;
                    None
                }
                Key::Right => {
                    d.focus = 1;
                    None
                }
                Key::Enter => Some(d.focus == 0),
                Key::Back | Key::Home => Some(false),
                _ => None,
            };
            let kind = d.kind.clone();
            return choice.and_then(|yes| {
                self.dialog = None;
                match (kind, yes) {
                    (DialogKind::KeepOutput, true) => Some(Cmd::KeepOutput),
                    (DialogKind::KeepOutput, false) => Some(Cmd::RevertOutput),
                    (DialogKind::Uninstall(id), true) => Some(Cmd::Uninstall(id)),
                    (DialogKind::Uninstall(_), false) => None,
                    (DialogKind::DebugRevoke(id), true) => Some(Cmd::DebugRevoke(id)),
                    (DialogKind::DebugRevoke(_), false) => None,
                    (DialogKind::FactoryReset, true) => Some(Cmd::FactoryReset),
                    (DialogKind::FactoryReset, false) => None,
                }
            });
        }
        if let Some(p) = self.picker.as_mut() {
            let n = p.options.len();
            match k {
                Key::Up => p.focus = p.focus.saturating_sub(1),
                Key::Down => p.focus = (p.focus + 1).min(n - 1),
                Key::Back | Key::Left => {
                    self.picker = None;
                    return None;
                }
                Key::Home => {
                    self.picker = None;
                    return Some(Cmd::Close);
                }
                Key::Enter | Key::Right => {
                    let p = self.picker.take().unwrap();
                    let i = p.focus;
                    return match p.id {
                        RowId::Output if OUTPUTS[i].0 != ctx.settings.output => Some(Cmd::SetOutput(OUTPUTS[i].0.to_string())),
                        RowId::Zone => Some(Cmd::SetZone(ZONES[i].2.to_string())),
                        RowId::Dim => Some(Cmd::SetDim(DIMS[i].0)),
                        RowId::Volume => Some(Cmd::SetVolume(VOLUMES[i])),
                        _ => None,
                    };
                }
            }
            p.hl.go(p.focus as f32, now, Duration::from_millis(MOVE_MS));
            let first = p.scroll.target() as usize;
            let nfirst = if p.focus < first { p.focus } else if p.focus >= first + PICK_MAX { p.focus + 1 - PICK_MAX } else { first };
            p.scroll.go(nfirst as f32, now, Duration::from_millis(MOVE_MS + 40));
            return None;
        }
        let cat = Cat::ALL[self.cat];
        if cat == Cat::Wallpaper {
            match (self.pane, k) {
                (Pane::Cats, Key::Right | Key::Enter) => {
                    self.enter_walls(ctx, now);
                    return None;
                }
                (Pane::Rows, _) => return self.key_walls(k, now, ctx),
                _ => {}
            }
        }
        if let (Cat::Apps, Pane::Rows, Some(id), Key::Left | Key::Back) = (cat, self.pane, self.app_page.clone(), k) {
            // Back from an app's page to the list, onto that app.
            self.show_app_page(None, now);
            self.focus_id(&RowId::App(id), ctx, now);
            return None;
        }
        let rs = rows(cat, ctx, self.searching.is_some(), self.app_page.as_deref());
        let focusable: Vec<usize> = (0..rs.len()).filter(|&i| rs[i].focusable()).collect();
        match self.pane {
            Pane::Cats => match k {
                Key::Up if self.cat > 0 => self.select_cat(self.cat - 1, now, true),
                Key::Down if self.cat + 1 < Cat::ALL.len() => self.select_cat(self.cat + 1, now, true),
                Key::Right | Key::Enter if !focusable.is_empty() => {
                    self.pane = Pane::Rows;
                    self.focus_row(0, ctx, now, false);
                }
                Key::Back | Key::Home => return Some(Cmd::Close),
                _ => {}
            },
            Pane::Rows => match k {
                Key::Up if self.row > 0 => self.focus_row(self.row - 1, ctx, now, true),
                Key::Down if self.row + 1 < focusable.len() => self.focus_row(self.row + 1, ctx, now, true),
                Key::Left | Key::Back => self.pane = Pane::Cats,
                Key::Home => return Some(Cmd::Close),
                Key::Enter | Key::Right => {
                    let Some(&ri) = focusable.get(self.row) else { return None };
                    return self.activate(&rs, ri, now, ctx, k == Key::Right);
                }
                _ => {}
            },
        }
        None
    }

    /// Apps: open the page of app `id`, or go back to the list (None). The content fades in
    /// like a category change.
    fn show_app_page(&mut self, id: Option<String>, now: Instant) {
        self.app_page = id;
        self.switched = now;
        self.row = 0;
        self.scroll = Tween::fixed(0.0);
        self.hl_row = Tween::fixed(0.0);
        self.knobs.clear();
        self.fits.clear();
    }

    /// Put the focus into the wallpaper grid, on the wallpaper in use (if the store's
    /// catalog is here: until then there is nothing to choose).
    fn enter_walls(&mut self, ctx: &Ctx, now: Instant) {
        let Some(c) = ctx.gallery.catalog.as_ref() else {
            self.wall_pending = true;
            return;
        };
        self.wall_pending = false;
        self.pane = Pane::Rows;
        let i = c.walls.iter().position(|w| w.asset == ctx.settings.wallpaper).unwrap_or(0);
        self.focus_wall(i, now, false);
    }

    fn focus_wall(&mut self, i: usize, now: Instant, animate: bool) {
        self.wall = i;
        let (col, row) = ((i % WALL_COLS) as f32, (i / WALL_COLS) as f32);
        // Keep the focused row inside the three visible ones.
        let top = row * TILE_PITCH;
        let sc = self.scroll.target().min(top).max(top + TILE_H - GRID_H);
        if animate {
            let d = Duration::from_millis(MOVE_MS);
            self.wall_hl.0.go(col, now, d);
            self.wall_hl.1.go(row, now, d);
            self.scroll.go(sc, now, Duration::from_millis(MOVE_MS + 60));
        } else {
            self.wall_hl = (Tween::fixed(col), Tween::fixed(row));
            self.scroll = Tween::fixed(sc);
        }
    }

    /// Keys in the wallpaper grid: the D-pad moves in two dimensions (Left from the first
    /// column goes back to the categories), OK uses the focused wallpaper.
    fn key_walls(&mut self, k: Key, now: Instant, ctx: &Ctx) -> Option<Cmd> {
        let Some(c) = ctx.gallery.catalog.as_ref() else {
            self.pane = Pane::Cats;
            return None;
        };
        let n = c.walls.len();
        let i = self.wall.min(n - 1);
        let (col, row) = (i % WALL_COLS, i / WALL_COLS);
        let to = match k {
            Key::Left if col > 0 => i - 1,
            Key::Left | Key::Back => {
                self.pane = Pane::Cats;
                return None;
            }
            Key::Right if col + 1 < WALL_COLS && i + 1 < n => i + 1,
            Key::Up if row > 0 => i - WALL_COLS,
            Key::Down if i + WALL_COLS < n => i + WALL_COLS,
            // A shorter last row: Down from above its end goes to its last tile.
            Key::Down if row < (n - 1) / WALL_COLS => n - 1,
            Key::Home => return Some(Cmd::Close),
            Key::Enter if c.walls[i].asset != ctx.settings.wallpaper => {
                self.wall_set = now;
                return Some(Cmd::SetWallpaper(c.walls[i].asset.clone()));
            }
            _ => i,
        };
        self.focus_wall(to, now, true);
        None
    }

    fn activate(&mut self, rs: &[Row], ri: usize, now: Instant, ctx: &Ctx, right: bool) -> Option<Cmd> {
        let r = &rs[ri];
        let tops = Self::tops(rs);
        let anchor = CONTENT_Y + tops[ri] - self.scroll.target() + r.height() / 2.0;
        let st = ctx.settings;
        let picker = |id: RowId, options: Vec<(String, String)>, current: usize| {
            let focus = current;
            let first = focus.saturating_sub(PICK_MAX / 2).min(options.len().saturating_sub(PICK_MAX));
            Picker { id, options, current, focus, hl: Tween::fixed(focus as f32), scroll: Tween::fixed(first as f32), shown: now, anchor, layout: None }
        };
        match (&r.id, &r.kind) {
            (RowId::Output, _) => {
                let cur = OUTPUTS.iter().position(|o| o.0 == st.output).unwrap_or(0);
                self.picker = Some(picker(RowId::Output, OUTPUTS.iter().map(|o| (o.1.to_string(), o.2.to_string())).collect(), cur));
            }
            (RowId::Zone, _) => {
                let cur = st.zone_index().unwrap_or(ZONES.iter().position(|z| z.2 == "UTC0").unwrap_or(0));
                self.picker = Some(picker(RowId::Zone, ZONES.iter().map(|z| (z.0.to_string(), z.1.to_string())).collect(), cur));
            }
            (RowId::Volume, _) => {
                let now_at = ctx.volume.unwrap_or(100);
                let cur = VOLUMES.iter().enumerate().min_by_key(|(_, v)| v.abs_diff(now_at)).map_or(0, |(i, _)| i);
                self.picker = Some(picker(RowId::Volume, VOLUMES.iter().map(|v| (format!("{v}%"), String::new())).collect(), cur));
            }
            (RowId::Dim, _) => {
                let cur = DIMS.iter().position(|d| d.0 == st.dim_min).unwrap_or(0);
                self.picker = Some(picker(RowId::Dim, DIMS.iter().map(|d| (d.1.to_string(), String::new())).collect(), cur));
            }
            // Right on a switch only turns it on (Left goes back), Enter flips it.
            (RowId::Depth, Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::ToggleDepth),
            (RowId::Clock24, Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::ToggleClock24),
            (RowId::Hud, Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::ToggleHud),
            (RowId::Restart, _) if !right => return Some(Cmd::Restart),
            (RowId::WifiScan, _) if !right && self.searching.is_none() => return Some(Cmd::WifiScan),
            (RowId::WifiSetup, _) if !right => return Some(Cmd::WifiSetup),
            (RowId::WifiPair, _) if !right => return Some(Cmd::WifiPair),
            (RowId::WifiForget, _) if !right => return Some(Cmd::WifiForget),
            (RowId::Unpair, _) if !right => return Some(Cmd::Unpair),
            (RowId::NetDebug, Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::ToggleNetDebug),
            (RowId::DebugLan, Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::ToggleDebugLan),
            (RowId::DebugCloud, Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::ToggleDebugCloud),
            (RowId::DebugPair, _) if !right => return Some(Cmd::DebugPair),
            (RowId::FactoryReset, _) if !right => self.show_dialog(DialogKind::FactoryReset, S_RESET_TITLE.into(), S_RESET_BODY.into(), [S_RESET_YES, S_CANCEL], None, now),
            (RowId::DebugRevoke(id), _) if !right => self.show_dialog(
                DialogKind::DebugRevoke(id.clone()), "Revoke this computer?".into(),
                "Its SSH key and cloud access will be removed. All current debug sessions will end.".into(),
                ["Revoke", S_CANCEL], None, now),
            (RowId::App(id), _) => {
                self.show_app_page(Some(id.clone()), now);
                self.focus_row(0, ctx, now, false);
            }
            (RowId::AppBg(id), Kind::Toggle(on)) if !(right && *on) => return Some(Cmd::SetBackground(id.clone(), !*on)),
            (RowId::AppUninstall(id), _) if !right => {
                let name = ctx.apps.iter().find(|a| &a.id == id).map_or(id.clone(), |a| a.name.clone());
                self.show_dialog(
                    DialogKind::Uninstall(id.clone()),
                    format!("Uninstall {name}?"),
                    "The app and everything it saved on this device will be removed.".into(),
                    [S_UNINSTALL, S_CANCEL],
                    None,
                    now,
                );
            }
            _ => {}
        }
        None
    }

    /// Build the view into `f` (ids 2_000_000+).
    #[allow(clippy::too_many_arguments)]
    pub fn build(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, ctx: &Ctx, sl: &Slices, f: &mut Frame) {
        let o = self.open.value(now);
        if o <= 0.0 {
            return;
        }
        if self.wall_pending && self.is_open() && Cat::ALL[self.cat] == Cat::Wallpaper && self.pane == Pane::Cats && ctx.gallery.catalog.is_some() {
            self.enter_walls(ctx, now);
        }
        // The app whose page is open was uninstalled: back to the list.
        if self.app_page.as_ref().is_some_and(|id| !ctx.details.contains_key(id) || !ctx.apps.iter().any(|a| a.id == *id && a.state != "available")) {
            self.show_app_page(None, now);
            self.focus_row(0, ctx, now, false);
        }
        let Some(art) = self.art.as_ref() else { return };
        let s = self.s;
        let a = o;
        let dx = 48.0 * (1.0 - o);
        let px = |v: f32| (v * s).round();
        let dk = s / sl.raster;
        let (cr, cm) = sl.card;
        let (rr, rm) = sl.ring;
        let cat = Cat::ALL[self.cat];

        // Left column: header and categories.
        let st = f.begin();
        text.draw(gpu, atlas, &mut f.verts, Face::Light, 48.0 * s, px(LEFT_X + dx), px(HEADER_BASE), rgba(0xffffff, a), S_SETTINGS);
        f.end(ID + 1, st);
        let hy = CAT_Y + self.hl_cat.value(now) * CAT_H + 5.0;
        let st = f.begin();
        let hl = RectF::new((CAT_HL_X + dx) * s, hy * s, CAT_HL_W * s, (CAT_H - 10.0) * s);
        let (hc, ha) = if self.pane == Pane::Cats && self.picker.is_none() { (FOCUS_COLOR, 0.92) } else { (0xffffff, 0.10) };
        nine(&mut f.verts, atlas, &cr, cm, hl, cm as f32 * dk, rgba(hc, ha * a));
        f.end(ID_CAT_HL, st);
        for (i, c) in Cat::ALL.iter().enumerate() {
            let y = CAT_Y + i as f32 * CAT_H;
            let sel = i == self.cat;
            let st = f.begin();
            if let Some(Some(r)) = art.cats.get(i) {
                let (x0, y0) = (px(LEFT_X + 14.0 + dx), (px(y + CAT_H / 2.0) - (r.h / 2) as f32));
                let uv = atlas.uv(r, 0.0, 0.0, r.w as f32, r.h as f32);
                quad(&mut f.verts, x0, y0, x0 + r.w as f32, y0 + r.h as f32, uv, rgba(0xffffff, if sel { a } else { 0.72 * a }));
            }
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 29.0 * s, px(LEFT_X + 66.0 + dx), px(y + 49.0), rgba(0xffffff, if sel { a } else { 0.78 * a }), c.title());
            f.end(ID_CAT + i as u32, st);
        }

        // The panel.
        let panel = RectF::new((PANEL_X + dx) * s, PANEL_Y * s, PANEL_W * s, PANEL_H * s);
        // Frosted (the wallpaper behind it is the blurred layer); no shadow, which would show
        // through the translucent panel as a darker inner frame.
        let st = f.begin();
        nine(&mut f.verts, atlas, &cr, cm, panel, cm as f32 * dk, rgba(0x0b0e1c, 0.72 * a));
        f.end(ID, st);
        // Content fades in when the category changes.
        let ca = a * ease(since_ms(now, self.switched) / FADE_MS);
        let cdy = 14.0 * (1.0 - ease(since_ms(now, self.switched) / FADE_MS));
        let x0 = PANEL_X + PAD_X + dx;
        let x1 = PANEL_X + PANEL_W - PAD_X + dx;
        let st = f.begin();
        // An app's page has the app as its heading.
        let page = self.app_page.as_ref().and_then(|id| ctx.apps.iter().find(|a| a.id == *id));
        let (title, blurb) = match page {
            Some(app) => {
                let version = ctx.details.get(&app.id).map_or(String::new(), |d| format!("Version {}  ·  ", d.version));
                (app.name.clone(), format!("{version}{}", app.id))
            }
            None => (cat.title().to_string(), cat.blurb().to_string()),
        };
        // Every text of the panel stays between x0 and x1 (`self.fits` cuts what is longer).
        let room = (x1 - x0) * s;
        let title = self.fits.line(text, Face::Light, 40.0 * s, &title, room);
        text.draw(gpu, atlas, &mut f.verts, Face::Light, 40.0 * s, px(x0), px(PANEL_Y + 70.0 + cdy), rgba(0xffffff, ca), title);
        let blurb = self.fits.line(text, Face::Regular, 24.0 * s, &blurb, room);
        text.draw(gpu, atlas, &mut f.verts, Face::Regular, 24.0 * s, px(x0), px(PANEL_Y + 108.0 + cdy), rgba(0xffffff, 0.6 * ca), blurb);
        f.end(ID_HEAD + if page.is_some() { 50 } else { self.cat as u32 }, st);

        let rs = rows(cat, ctx, self.searching.is_some(), self.app_page.as_deref());
        let tops = Self::tops(&rs);
        let scroll = self.scroll.value(now);
        let focusable: Vec<usize> = (0..rs.len()).filter(|&i| rs[i].focusable()).collect();
        if self.row >= focusable.len() && !focusable.is_empty() {
            self.row = focusable.len() - 1;
        }
        // The window the rows scroll in. They are cut to it (`clip`), so a row on its way out
        // is never drawn over the heading above or past the panel's edge below.
        let content = RectF::new(panel.x, px(CONTENT_Y - 2.0), panel.w, px(CONTENT_BOTTOM + 2.0) - px(CONTENT_Y - 2.0));
        // Visible fraction of a row inside the panel's content window.
        let vis = |top: f32, h: f32| {
            let (y0, y1) = (CONTENT_Y + top - scroll, CONTENT_Y + top - scroll + h);
            // A row shows once a third of it is inside, and fades in from there.
            let inside = ((y1 - CONTENT_Y).min(h) / h).min((CONTENT_BOTTOM - y0) / h);
            ((inside - 0.34) / 0.66).clamp(0.0, 1.0)
        };
        if self.pane == Pane::Rows && !focusable.is_empty() {
            let top = self.hl_row.value(now);
            let va = vis(top, ROW_H);
            if va > 0.0 {
                let y = CONTENT_Y + top - scroll + cdy;
                let r = RectF::new((PANEL_X + 16.0 + dx) * s, (y + 6.0) * s, (PANEL_W - 32.0) * s, (ROW_H - 12.0) * s);
                let st = f.begin();
                nine(&mut f.verts, atlas, &cr, cm, r, cm as f32 * dk, rgba(0xffffff, 0.09 * ca * va));
                // Under an open picker or dialog only the fill marks the row (they have the ring).
                if self.picker.is_none() && self.dialog.is_none() {
                    nine(&mut f.verts, atlas, &rr, rm, r.expand(5.0 * s), rm as f32 * dk, rgba(FOCUS_COLOR, ca * va));
                }
                clip(&mut f.verts, st, content);
                f.end(ID_ROW_HL, st);
            }
        }
        // More rows than fit: a thumb on the panel's edge shows where the window is.
        let total = tops.last().copied().unwrap_or(0.0) + rs.last().map_or(0.0, |r| r.height());
        let window = CONTENT_BOTTOM - CONTENT_Y;
        if total > window + 1.0 {
            let st = f.begin();
            let bar = RectF::new((PANEL_X + PANEL_W - 9.0 + dx) * s, (CONTENT_Y + cdy) * s, 4.0 * s, window * s);
            scrollbar(&mut f.verts, atlas, sl, bar, scroll / total, window / total, s, ca);
            f.end(ID_ROW_HL - 1, st);
        }
        for (i, row) in rs.iter().enumerate() {
            let h = row.height();
            let va = vis(tops[i], h);
            if va <= 0.0 {
                continue;
            }
            let ra = ca * va;
            let top = CONTENT_Y + tops[i] - scroll + cdy;
            let st = f.begin();
            match &row.kind {
                Kind::Info(v) => {
                    // The value (an address, a network's name, an error) may be long: it has
                    // the line up to the label's column; a label without a value has it all.
                    let lroom = if v.is_empty() { room } else { INFO_LABEL_W * s };
                    let label = self.fits.line(text, Face::Regular, 26.0 * s, &row.label, lroom);
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, px(x0), px(top + 38.0), rgba(0xffffff, 0.66 * ra), label);
                    let v = self.fits.line(text, Face::Regular, 26.0 * s, v, room - (INFO_LABEL_W + ROW_GAP) * s);
                    let w = text.width(Face::Regular, 26.0 * s, v);
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, (x1 * s - w).round(), px(top + 38.0), rgba(0xffffff, ra), v);
                }
                Kind::Perm(state, tint, sensitive) => {
                    if let Some(d) = art.dot {
                        let (dx, dy) = (px(x0 + 2.0), px(top + 25.0) - (d.h / 2) as f32);
                        let uv = atlas.uv(&d, 0.0, 0.0, d.w as f32, d.h as f32);
                        quad(&mut f.verts, dx, dy, dx + d.w as f32, dy + d.h as f32, uv, rgba(if *sensitive { FOCUS_COLOR } else { 0x7d848b }, ra));
                    }
                    let w = text.width(Face::Regular, 26.0 * s, state);
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, (x1 * s - w).round(), px(top + 46.0), rgba(*tint, if *tint == 0xffffff { 0.72 * ra } else { ra }), state);
                    let room = (x1 - x0 - 30.0 - 36.0) * s - w;
                    for (line, size, base, alpha) in [(&row.label, 28.0 * s, 35.0, ra), (&row.sub, 21.0 * s, 64.0, 0.55 * ra)] {
                        let fitted = self.fits.line(text, Face::Regular, size, line, room);
                        text.draw(gpu, atlas, &mut f.verts, Face::Regular, size, px(x0 + 30.0), px(top + base), rgba(0xffffff, alpha), fitted);
                    }
                }
                Kind::Empty(g, title, line) => {
                    let cx = (x0 + x1) / 2.0;
                    if let (Glyph::Wifi, Some(r)) = (g, art.wifi_big) {
                        let (ix, iy) = (px(cx) - (r.w / 2) as f32, px(top + 20.0));
                        let uv = atlas.uv(&r, 0.0, 0.0, r.w as f32, r.h as f32);
                        quad(&mut f.verts, ix, iy, ix + r.w as f32, iy + r.h as f32, uv, rgba(0xffffff, 0.3 * ra));
                    }
                    let title = self.fits.line(text, Face::Light, 40.0 * s, title, room);
                    let w = text.width(Face::Light, 40.0 * s, title);
                    text.draw(gpu, atlas, &mut f.verts, Face::Light, 40.0 * s, (cx * s - w / 2.0).round(), px(top + 190.0), rgba(0xffffff, ra), title);
                    let line = self.fits.line(text, Face::Regular, 24.0 * s, line, room);
                    let w = text.width(Face::Regular, 24.0 * s, line);
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, 24.0 * s, (cx * s - w / 2.0).round(), px(top + 234.0), rgba(0xffffff, 0.6 * ra), line);
                }
                kind => {
                    let cy = top + ROW_H / 2.0;
                    // Where the row's right side (value, dots, switch) begins: the label
                    // and the line under it end before it.
                    let mut left = x1 * s;
                    match kind {
                        Kind::Toggle(on) => {
                            let key = format!("{:?}", row.id);
                            let kt = self.knobs.entry(key).or_insert_with(|| Tween::fixed(if *on { 1.0 } else { 0.0 }));
                            kt.go(if *on { 1.0 } else { 0.0 }, now, Duration::from_millis(SWITCH_MS));
                            let t = kt.value(now);
                            let track = RectF::new((x1 - SWITCH_W) * s, (cy - SWITCH_H / 2.0) * s, SWITCH_W * s, SWITCH_H * s);
                            left = track.x;
                            if let Some((pr, pm)) = art.pill {
                                let off = rgba(0xffffff, 0.22 * ra * (1.0 - t));
                                let onc = rgba(FOCUS_COLOR, ra * t);
                                nine(&mut f.verts, atlas, &pr, pm, track, pm as f32, off);
                                if t > 0.0 {
                                    nine(&mut f.verts, atlas, &pr, pm, track, pm as f32, onc);
                                }
                            }
                            if let Some(k) = art.knob {
                                let travel = (SWITCH_W - KNOB_D - 8.0) * s;
                                let kx = track.x + 4.0 * s + travel * t;
                                let ky = (cy * s - k.h as f32 / 2.0).round();
                                let kx = if t == 0.0 || t == 1.0 { kx.round() } else { kx };
                                let uv = atlas.uv(&k, 0.0, 0.0, k.w as f32, k.h as f32);
                                quad(&mut f.verts, kx, ky, kx + k.w as f32, ky + k.h as f32, uv, rgba(0xffffff, ra));
                            }
                        }
                        Kind::Choice(v) | Kind::Action(v) => {
                            let tint = if matches!(kind, Kind::Action(_)) { FOCUS_COLOR } else { 0xffffff };
                            let mut right = x1 * s;
                            if let Some(c) = art.chevron {
                                let (cx0, cy0) = ((x1 * s - c.w as f32).round(), (cy * s - c.h as f32 / 2.0).round());
                                let uv = atlas.uv(&c, 0.0, 0.0, c.w as f32, c.h as f32);
                                quad(&mut f.verts, cx0, cy0, cx0 + c.w as f32, cy0 + c.h as f32, uv, rgba(0xffffff, 0.55 * ra));
                                right = cx0 - 14.0 * s;
                            }
                            if self.searching.is_some() && row.id == RowId::WifiScan {
                                // Three dots pulse while it searches.
                                if let Some(d) = art.dot {
                                    let ph = since_ms(now, self.searching.unwrap()) / 400.0;
                                    let uv = atlas.uv(&d, 0.0, 0.0, d.w as f32, d.h as f32);
                                    for j in 0..3 {
                                        let al = 0.25 + 0.75 * (0.5 + 0.5 * ((ph - j as f32 * 0.25) * std::f32::consts::TAU).cos());
                                        let dxp = (right - (3 - j) as f32 * 20.0 * s).round();
                                        let dy = (cy * s - d.h as f32 / 2.0).round();
                                        quad(&mut f.verts, dxp, dy, dxp + d.w as f32, dy + d.h as f32, uv, rgba(FOCUS_COLOR, al * ra));
                                    }
                                }
                                left = right - 60.0 * s;
                            } else {
                                // The value has at most half the row.
                                let v = self.fits.line(text, Face::Regular, 28.0 * s, v, room * 0.5);
                                let w = text.width(Face::Regular, 28.0 * s, v);
                                text.draw(gpu, atlas, &mut f.verts, Face::Regular, 28.0 * s, (right - w).round(), px(top + 58.0), rgba(tint, if tint == 0xffffff { 0.72 * ra } else { ra }), v);
                                left = right - w;
                            }
                        }
                        _ => {}
                    }
                    let lroom = (left - (x0 + ROW_GAP) * s).round();
                    let has_sub = !row.sub.is_empty();
                    let ly = if has_sub { top + 44.0 } else { top + 58.0 };
                    let label = self.fits.line(text, Face::Regular, 28.0 * s, &row.label, lroom);
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, 28.0 * s, px(x0), px(ly), rgba(0xffffff, ra), label);
                    if has_sub {
                        let sub = self.fits.line(text, Face::Regular, 21.0 * s, &row.sub, lroom);
                        text.draw(gpu, atlas, &mut f.verts, Face::Regular, 21.0 * s, px(x0), px(top + 74.0), rgba(0xffffff, 0.55 * ra), sub);
                    }
                }
            }
            // Hairline under every row but the last.
            if i + 1 < rs.len() && !matches!(row.kind, Kind::Empty(..)) {
                let y = ((top + h) * s).round();
                solid(&mut f.verts, atlas, &cr, cm, x0 * s, y, x1 * s, y + s.max(1.0).round(), rgba(0xffffff, 0.07 * ra));
            }
            clip(&mut f.verts, st, content);
            f.end(ID_ROW + i as u32, st);
        }

        if cat == Cat::Wallpaper {
            let mut fits = std::mem::take(&mut self.fits);
            self.build_walls(gpu, atlas, text, now, ctx, art, sl, (x0, x1), cdy, ca, &mut fits, f);
            self.fits = fits;
        }
        if let Some(p) = self.picker.as_mut() {
            build_picker(gpu, atlas, text, now, p, &mut self.fits, art, sl, s, dx, a, f);
        }
        if let Some(d) = self.dialog.as_ref() {
            build_dialog(gpu, atlas, text, now, d, &mut self.fits, sl, s, a, f);
        }
    }

    /// The wallpaper page: a grid of thumbnails and a caption for the focused one.
    #[allow(clippy::too_many_arguments)]
    fn build_walls(&self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, ctx: &Ctx, art: &SetArt, sl: &Slices, (x0, x1): (f32, f32), cdy: f32, ca: f32, fits: &mut Fits, f: &mut Frame) {
        let s = self.s;
        let px = |v: f32| (v * s).round();
        let dk = s / sl.raster;
        let (cr, cm) = sl.card;
        let (rr, rm) = sl.ring;
        let room = (x1 - x0) * s;
        let Some(c) = ctx.gallery.catalog.as_ref() else {
            // Nothing to choose from yet: say why (the catalog comes from the network).
            let (title, line) = if !ctx.appd || ctx.gallery.error.is_some() {
                (S_WALL_ERROR, S_WALL_ERROR_TEXT)
            } else if ctx.info.ip.is_empty() {
                (S_WALL_OFFLINE, S_WALL_OFFLINE_TEXT)
            } else {
                (S_WALL_LOADING, "")
            };
            let (cx, top) = ((x0 + x1) / 2.0, GRID_Y + cdy + 150.0);
            let st = f.begin();
            let w = text.width(Face::Light, 40.0 * s, title);
            text.draw(gpu, atlas, &mut f.verts, Face::Light, 40.0 * s, (cx * s - w / 2.0).round(), px(top), rgba(0xffffff, ca), title);
            let line = fits.line(text, Face::Regular, 24.0 * s, line, room);
            let w = text.width(Face::Regular, 24.0 * s, line);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 24.0 * s, (cx * s - w / 2.0).round(), px(top + 44.0), rgba(0xffffff, 0.6 * ca), line);
            f.end(ID_WALL + 63, st);
            return;
        };
        let n = c.walls.len();
        let scroll = self.scroll.value(now);
        let pitch_x = TILE_W + (x1 - x0 - WALL_COLS as f32 * TILE_W) / (WALL_COLS - 1) as f32;
        // Where tile (col, row) is, and how much of it is inside the grid's window (tiles
        // fade at the window's edges while a longer grid scrolls).
        let tile = |col: f32, row: f32| {
            let top = row * TILE_PITCH - scroll;
            let va = ((top + TILE_H) / TILE_H).min((GRID_H - top) / TILE_H).clamp(0.0, 1.0);
            (RectF::new(px(x0 + col * pitch_x), px(GRID_Y + top + cdy), px(TILE_W), px(TILE_H)), va)
        };
        let at = |i: usize| tile((i % WALL_COLS) as f32, (i / WALL_COLS) as f32);
        // The grid's window (with room for the focus ring): a longer grid scrolls inside it.
        let window = RectF::new(px(x0) - 12.0 * s, px(GRID_Y + cdy) - 12.0 * s, room + 24.0 * s, px(GRID_H) + 24.0 * s);
        let ta = ease(ctx.gallery.thumbs_alpha(now));
        // Until the thumbnails are here (and under them while they fade in): each tile in
        // its wallpaper's mean colour.
        if ta < 1.0 {
            for (i, w) in c.walls.iter().enumerate() {
                let (r, va) = at(i);
                if va > 0.0 {
                    let st = f.begin();
                    nine(&mut f.verts, atlas, &cr, cm, r, cm as f32 * dk, rgba(w.color, ca * va));
                    clip(&mut f.verts, st, window);
                    f.end(ID_WALL + i as u32, st);
                }
            }
        }
        // The thumbnails: quads of the sheet texture, consecutive (one draw call).
        if let (Some((tex, _)), true) = (ctx.gallery.sheet, ta > 0.0) {
            for i in 0..n {
                let (r, va) = at(i);
                if va > 0.0 {
                    let st = f.begin();
                    quad(&mut f.verts, r.x, r.y, r.x + r.w, r.y + r.h, c.uv(i), rgba(0xffffff, ca * va * ta));
                    clip(&mut f.verts, st, window);
                    f.end_tex(ID_WALL + 30 + i as u32, st, Some(tex));
                }
            }
        }
        let focused = (self.pane == Pane::Rows).then(|| self.wall.min(n - 1));
        if focused.is_some() {
            let (r, va) = tile(self.wall_hl.0.value(now), self.wall_hl.1.value(now));
            if va > 0.0 {
                let st = f.begin();
                nine(&mut f.verts, atlas, &rr, rm, r.expand(5.0 * s), rm as f32 * dk, rgba(FOCUS_COLOR, ca * va));
                clip(&mut f.verts, st, window);
                f.end(ID_WALL + 60, st);
            }
        }
        // The wallpaper in use: a check badge in its corner (it pops in when just chosen).
        let current = c.walls.iter().position(|w| w.asset == ctx.settings.wallpaper);
        if let (Some(i), Some(k), Some(ck)) = (current, art.knob, art.check_small) {
            let (r, va) = at(i);
            if va > 0.0 {
                let pop = 0.5 + 0.5 * ease(since_ms(now, self.wall_set) / POP_MS);
                let (bx, by) = (r.x + r.w - 26.0 * s, r.y + 26.0 * s);
                let (kw, cw) = (k.w as f32 * pop / 2.0, ck.w as f32 * pop / 2.0);
                let st = f.begin();
                let uv = atlas.uv(&k, 0.0, 0.0, k.w as f32, k.h as f32);
                quad(&mut f.verts, bx - kw, by - kw, bx + kw, by + kw, uv, rgba(FOCUS_COLOR, ca * va));
                let uv = atlas.uv(&ck, 0.0, 0.0, ck.w as f32, ck.h as f32);
                quad(&mut f.verts, bx - cw, by - cw, bx + cw, by + cw, uv, rgba(0xffffff, ca * va));
                clip(&mut f.verts, st, window);
                f.end(ID_WALL + 61, st);
            }
        }
        // Caption: the focused wallpaper (from the categories: the one in use).
        if let Some(i) = focused.or(current) {
            let w = &c.walls[i];
            let y = GRID_Y + GRID_H + cdy;
            let st = f.begin();
            let (hint, tint, ha) = match (Some(i) == current, ctx.wall_busy) {
                (true, true) => (S_WALL_BUSY, FOCUS_COLOR, 1.0),
                (true, false) => (S_WALL_CURRENT, 0xffffff, 0.6),
                (false, _) => (S_WALL_USE, 0xffffff, 0.6),
            };
            let hw = text.width(Face::Regular, 26.0 * s, hint);
            // The store's title and author: the title ends before the hint on its line.
            let title = fits.line(text, Face::Regular, 28.0 * s, &w.title, room - hw - ROW_GAP * s);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 28.0 * s, px(x0), px(y + 54.0), rgba(0xffffff, ca), title);
            let by = format!("by {}", w.author);
            let by = fits.line(text, Face::Regular, 26.0 * s, &by, room);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, px(x0), px(y + 92.0), rgba(0xffffff, 0.6 * ca), by);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, (x1 * s - hw).round(), px(y + 54.0), rgba(tint, ha * ca), hint);
            f.end(ID_WALL + 62, st);
        }
    }

}

/// A scroll thumb in `bar`: `at` and `part` are the window's start and size as fractions of
/// the whole list.
#[allow(clippy::too_many_arguments)]
fn scrollbar(out: &mut Vec<crate::gpu::Vertex>, atlas: &Atlas, sl: &Slices, bar: RectF, at: f32, part: f32, s: f32, a: f32) {
    let (cr, cm) = sl.card;
    let h = (bar.h * part.clamp(0.0, 1.0)).max(24.0 * s).min(bar.h);
    let y = bar.y + (bar.h * at).clamp(0.0, bar.h - h);
    nine(out, atlas, &cr, cm, RectF::new(bar.x.round(), y.round(), bar.w.round().max(2.0), h.round()), bar.w / 2.0, rgba(0xffffff, 0.3 * a));
}

/// The open picker: an opaque box over the panel, right-aligned with it, as wide as its
/// options need (`PickLayout`). Its options scroll inside a window they are clipped to;
/// a thumb on the right shows that there are more than fit.
#[allow(clippy::too_many_arguments)]
fn build_picker(gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, p: &mut Picker, fits: &mut Fits, art: &SetArt, sl: &Slices, s: f32, dx: f32, a: f32, f: &mut Frame) {
    let dk = s / sl.raster;
    let (cr, cm) = sl.card;
    let (rr, rm) = sl.ring;
    let (shr, shm, shpad) = sl.shadow;
    let n = p.options.len().min(PICK_MAX);
    let scrolls = p.options.len() > n;
    // Right of the options: the check mark's column and, on a list that scrolls, the thumb.
    let bar = if scrolls { 12.0 * s } else { 0.0 };
    let check = art.check.map_or(0.0, |c| c.w as f32 + 18.0 * s);
    let size = 28.0 * s;
    let lay = match p.layout {
        Some(l) => l,
        None => {
            let widest = |side: fn(&(String, String)) -> &str| p.options.iter().map(|o| text.width(Face::Regular, size, side(o))).fold(0.0, f32::max).ceil();
            let (lw, hw) = (widest(|o| &o.0), widest(|o| &o.1));
            let fixed = (32.0 + 28.0) * s + check + bar + if hw > 0.0 { 40.0 * s } else { 0.0 };
            // More than the panel is wide: the hints give way first, down to 40 % of the room.
            let room = (PICK_MAX_W * s - fixed).max(0.0);
            let label = lw.min(room - hw.min(room * 0.4));
            let hint = hw.min(room - label);
            let l = PickLayout { w: (fixed + label + hint).max(PICK_MIN_W * s), label, hint };
            p.layout = Some(l);
            l
        }
    };
    let pw = lay.w / s;
    let h = n as f32 * PICK_ITEM + 28.0;
    let pe = ease(since_ms(now, p.shown) / POP_MS);
    let pa = a * pe;
    let x = PANEL_X + PANEL_W - 16.0 - pw + dx;
    // Centred on its row, but inside the panel and below its heading.
    let (y_min, y_max) = (CONTENT_Y - 8.0, PANEL_Y + PANEL_H - 8.0 - h);
    let y = (p.anchor - h / 2.0).clamp(y_min, y_max.max(y_min)) + 10.0 * (1.0 - pe);
    let box_ = RectF::new(x * s, y * s, lay.w, h * s);
    let st = f.begin();
    // The panel behind recedes, so the rows the box cuts across do not compete with it.
    let panel = RectF::new((PANEL_X + dx) * s, PANEL_Y * s, PANEL_W * s, PANEL_H * s);
    nine(&mut f.verts, atlas, &cr, cm, panel, cm as f32 * dk, rgba(0x0b0e1c, 0.5 * pa));
    nine_ex(&mut f.verts, atlas, &shr, shm, box_.expand(shpad as f32 * dk), shm as f32 * dk, rgba(0x000000, 0.6 * pa), false);
    nine(&mut f.verts, atlas, &cr, cm, box_, cm as f32 * dk, rgba(0x1d2233, pa));
    f.end(ID_PICK, st);
    let first = p.scroll.value(now);
    let top0 = y + 14.0;
    // The window the options scroll in: what leaves it is cut off, not drawn over the box's
    // edge or the rows behind.
    let window = RectF::new(box_.x, ((top0 - 2.0) * s).round(), lay.w, ((n as f32 * PICK_ITEM + 4.0) * s).round());
    // Focus highlight.
    let hy = top0 + (p.hl.value(now) - first) * PICK_ITEM;
    let r = RectF::new((x + 10.0) * s, (hy + 4.0) * s, lay.w - 20.0 * s - bar, (PICK_ITEM - 8.0) * s);
    let st = f.begin();
    nine(&mut f.verts, atlas, &cr, cm, r, cm as f32 * dk, rgba(0xffffff, 0.10 * pa));
    nine(&mut f.verts, atlas, &rr, rm, r.expand(4.0 * s), rm as f32 * dk, rgba(FOCUS_COLOR, pa));
    clip(&mut f.verts, st, window);
    f.end(ID_PICK + 1, st);
    if scrolls {
        let st = f.begin();
        let track = RectF::new(box_.x + lay.w - 13.0 * s, (top0 + 6.0) * s, 5.0 * s, (n as f32 * PICK_ITEM - 12.0) * s);
        let len = p.options.len() as f32;
        scrollbar(&mut f.verts, atlas, sl, track, first / len, n as f32 / len, s, pa);
        f.end(ID_PICK + 2, st);
    }
    for (i, (label, hint)) in p.options.iter().enumerate() {
        let iy = top0 + (i as f32 - first) * PICK_ITEM;
        if iy + PICK_ITEM <= top0 || iy >= top0 + n as f32 * PICK_ITEM {
            continue;
        }
        let st = f.begin();
        let label = fits.line(text, Face::Regular, size, label, lay.label + 1.0);
        text.draw(gpu, atlas, &mut f.verts, Face::Regular, size, ((x + 32.0) * s).round(), ((iy + 44.0) * s).round(), rgba(0xffffff, pa), label);
        let mut rx = box_.x + lay.w - 28.0 * s - bar;
        if let Some(c) = art.check {
            rx -= c.w as f32;
            if i == p.current {
                let (cx0, cy0) = (rx.round(), ((iy + PICK_ITEM / 2.0) * s - c.h as f32 / 2.0).round());
                let uv = atlas.uv(&c, 0.0, 0.0, c.w as f32, c.h as f32);
                quad(&mut f.verts, cx0, cy0, cx0 + c.w as f32, cy0 + c.h as f32, uv, rgba(FOCUS_COLOR, pa));
            }
            rx -= 18.0 * s;
        }
        if !hint.is_empty() {
            let hint = fits.line(text, Face::Regular, size, hint, lay.hint + 1.0);
            let w = text.width(Face::Regular, size, hint);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, size, (rx - w).round(), ((iy + 44.0) * s).round(), rgba(0xffffff, 0.5 * pa), hint);
        }
        clip(&mut f.verts, st, window);
        f.end(ID_PICK + 10 + i as u32, st);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_dialog(gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, d: &Dialog, fits: &mut Fits, sl: &Slices, s: f32, a: f32, f: &mut Frame) {
    let dk = s / sl.raster;
    let (cr, cm) = sl.card;
    let (rr, rm) = sl.ring;
    let (shr, shm, shpad) = sl.shadow;
    let pe = ease(since_ms(now, d.shown) / POP_MS);
    let pa = a * pe;
    let st = f.begin();
    solid(&mut f.verts, atlas, &cr, cm, 0.0, 0.0, 1920.0 * s, 1080.0 * s, rgba(0x05060c, 0.62 * pa));
    f.end(ID_DIALOG, st);
    let body = match d.deadline {
        Some(t) => {
            let left = t.saturating_duration_since(now).as_secs_f32().ceil() as u64;
            d.body.replace("{s}", &left.to_string())
        }
        None => d.body.clone(),
    };
    // The title on one line, the body on up to three (the panel grows with them).
    let w = 1040.0;
    let room = (w - 112.0) * s;
    let extra = (fits.lines(text, Face::Regular, 26.0 * s, &body, room, 3).len() - 1) as f32 * 36.0;
    let h = 330.0 + extra;
    let (x, y) = ((1920.0 - w) / 2.0, (1080.0 - h) / 2.0 + 20.0 * (1.0 - pe));
    let panel = RectF::new(x * s, y * s, w * s, h * s);
    let st = f.begin();
    nine_ex(&mut f.verts, atlas, &shr, shm, panel.expand(shpad as f32 * dk), shm as f32 * dk, rgba(0x000000, 0.6 * pa), false);
    nine(&mut f.verts, atlas, &cr, cm, panel, cm as f32 * dk, rgba(0x1d2233, pa));
    let title = fits.line(text, Face::Light, 46.0 * s, &d.title, room);
    text.draw(gpu, atlas, &mut f.verts, Face::Light, 46.0 * s, ((x + 56.0) * s).round(), ((y + 96.0) * s).round(), rgba(0xffffff, pa), title);
    f.end(ID_DIALOG + 1, st);
    // The body on its own item: a countdown repaints only these lines.
    let st = f.begin();
    for (i, line) in fits.lines(text, Face::Regular, 26.0 * s, &body, room, 3).iter().enumerate() {
        text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, ((x + 56.0) * s).round(), ((y + 150.0 + i as f32 * 36.0) * s).round(), rgba(0xffffff, 0.66 * pa), line);
    }
    f.end(ID_DIALOG + 2, st);
    let (bw, bh) = (280.0, 80.0);
    for (i, l) in d.buttons.iter().enumerate() {
        let bx = x + w - 56.0 - (2 - i) as f32 * bw - (1 - i) as f32 * 24.0;
        let by = y + h - 56.0 - bh;
        let r = RectF::new(bx * s, by * s, bw * s, bh * s);
        let focused = d.focus == i;
        let st = f.begin();
        nine(&mut f.verts, atlas, &cr, cm, r, cm as f32 * dk, rgba(if focused { FOCUS_COLOR } else { 0x2c3245 }, pa));
        if focused {
            nine(&mut f.verts, atlas, &rr, rm, r.expand(5.0 * s), rm as f32 * dk, rgba(0xffffff, 0.85 * pa));
        }
        let tw = text.width(Face::Medium, 28.0 * s, l);
        text.draw(gpu, atlas, &mut f.verts, Face::Medium, 28.0 * s, (r.x + (r.w - tw) / 2.0).round(), (r.y + 50.0 * s).round(), rgba(0xffffff, pa), l);
        f.end(ID_DIALOG + 10 + i as u32, st);
    }
}

/// The rows of a category, from the current settings and system state.
fn rows(cat: Cat, ctx: &Ctx, searching: bool, app_page: Option<&str>) -> Vec<Row> {
    let st = ctx.settings;
    let i = ctx.info;
    let dash = |v: &str| if v.is_empty() { "—".to_string() } else { v.to_string() };
    match cat {
        Cat::Wifi => {
            let n = ctx.net;
            let mut v = Vec::new();
            if !n.adapter() {
                let why = if n.get("usb") == "device" { S_NO_WIFI_USB } else { S_NO_WIFI_TEXT };
                let title = match i.wifi.first() {
                    Some(dev) => format!("Wi-Fi adapter {dev} found"),
                    None => S_NO_WIFI.into(),
                };
                v.push(Row::new(RowId::None, "", "", Kind::Empty(Glyph::Wifi, title, if n.running() { why.into() } else { "The network service is not running.".into() })));
                v.push(Row::new(RowId::WifiScan, S_SEARCH, "", Kind::Action(if searching { S_SEARCHING.into() } else { String::new() })));
                return v;
            }
            v.push(Row::info("Status", n.status()));
            if n.connected() {
                let sig = n.signal();
                let q = if sig >= -55 { "excellent" } else if sig >= -67 { "good" } else if sig >= -78 { "fair" } else { "weak" };
                if sig != 0 {
                    v.push(Row::info("Signal", format!("{sig} dBm ({q})")));
                }
                v.push(Row::info("IP address", dash(n.get("ip"))));
            }
            if !n.get("error").is_empty() && !n.connected() {
                v.push(Row::info("Problem", n.get("error")));
            }
            if n.setup() {
                v.push(Row::info("Setup network", format!("{} · password {}", n.get("setup_ssid"), n.get("setup_psk"))));
            }
            v.push(Row::new(RowId::WifiSetup, S_WIFI_SETUP, S_WIFI_SETUP_SUB, Kind::Action(String::new())));
            if n.connected() {
                v.push(Row::new(RowId::WifiPair, S_WIFI_PAIR, S_WIFI_PAIR_SUB, Kind::Action(String::new())));
            }
            let pilots = n.pilots();
            v.push(Row::info("Paired phones", if pilots == 0 { "None".to_string() } else { format!("{pilots} ({} connected now)", n.get("online")) }));
            if pilots > 0 {
                v.push(Row::new(RowId::Unpair, S_UNPAIR, "They need the TV's code to pair again", Kind::Action(String::new())));
            }
            if n.get("saved") != "0" && !n.get("saved").is_empty() {
                v.push(Row::new(RowId::WifiForget, S_WIFI_FORGET, "The TV offers its setup network again", Kind::Action(String::new())));
            }
            v
        }
        Cat::Network => vec![
            Row::info("Connection", if i.iface.starts_with("wlan") { "Wi-Fi" } else { "USB Ethernet" }),
            Row::info("Status", if !i.link_up { "Disconnected" } else if i.ip.is_empty() { "Connecting…" } else { "Connected" }),
            Row::info("IP address", dash(&i.ip)),
            Row::info("Gateway", dash(&i.gateway)),
            Row::info("DNS", dash(&i.dns)),
            Row::info("MAC address", dash(&i.mac)),
            Row::info("Device name", dash(&i.hostname)),
        ],
        Cat::Display => {
            let mut v = vec![
                Row::new(RowId::Output, S_RESOLUTION, S_RESOLUTION_SUB, Kind::Choice(st.output_label().into())),
                Row::new(RowId::Depth, S_DEPTH, S_DEPTH_SUB, Kind::Toggle(st.depth)),
                Row::new(RowId::Dim, S_DIM, S_DIM_SUB, Kind::Choice(st.dim_label())),
            ];
            if let Some(volume) = ctx.volume {
                v.insert(1, Row::new(RowId::Volume, S_VOLUME, S_VOLUME_SUB, Kind::Choice(format!("{volume}%"))));
            }
            v.push(Row::info("Output now", settings::output_label(ctx.output_now)));
            v.push(Row::info("Composition", if ctx.planes { "Two display planes (wallpaper + UI)" } else { "GPU composited" }));
            v
        }
        // Not rows: the thumbnail grid (build_walls).
        Cat::Wallpaper => Vec::new(),
        Cat::Time => vec![
            Row::new(RowId::Zone, S_ZONE, "", Kind::Choice(st.zone_label())),
            Row::new(RowId::Clock24, S_CLOCK24, S_CLOCK24_SUB, Kind::Toggle(st.clock24)),
            Row::info(
                "Time source",
                if i.time_synced { "Network time (NTP)" } else if i.ntp_running { "Waiting for network time" } else { "Not set (no network)" },
            ),
        ],
        Cat::Apps => {
            // "fetching" too: an installed app whose payload is being downloaded again.
            // (appd sends details for installed apps only.)
            let installed: Vec<&AppInfo> = ctx.apps.iter().filter(|a| a.state != "available" && ctx.details.contains_key(&a.id)).collect();
            if let Some(app) = app_page.and_then(|id| installed.iter().find(|a| a.id == id)) {
                return app_rows(app, ctx.details.get(&app.id));
            }
            let mut v: Vec<Row> = installed
                .iter()
                .map(|a| {
                    let sub = if a.state == "running" { "Open now".to_string() } else { a.summary.clone() };
                    let runs = ctx.details.get(&a.id).is_some_and(|d| d.background == Background::On);
                    Row::new(RowId::App(a.id.clone()), a.name.clone(), sub, Kind::Choice(if runs { S_BG_ON.into() } else { String::new() }))
                })
                .collect();
            if v.is_empty() {
                let why = if ctx.appd { "No apps installed yet" } else { "App manager not running" };
                v.push(Row::info(why, ""));
            }
            v.push(Row::info("Free app storage", if i.data_total_kib > 0 { settings::size_label(i.data_free_kib) } else { "—".into() }));
            // Everything that may run while it is not on screen, with its switch.
            let capable: Vec<(&AppInfo, &AppDetail)> = installed.iter().filter_map(|a| ctx.details.get(&a.id).filter(|d| d.background != Background::None).map(|d| (*a, d))).collect();
            if !capable.is_empty() {
                let on = capable.iter().filter(|(_, d)| d.background == Background::On).count();
                v.push(Row::info(S_BG_APPS, if on == 0 { "None running".to_string() } else { format!("{on} of {} on", capable.len()) }));
                for (a, d) in capable {
                    v.push(Row::new(RowId::AppBg(a.id.clone()), a.name.clone(), d.service_label(), Kind::Toggle(d.background == Background::On)));
                }
            }
            v
        }
        Cat::Developer => {
            let n = ctx.net;
            let mut v = vec![Row::new(RowId::NetDebug, S_NETDEBUG, S_NETDEBUG_SUB, Kind::Toggle(n.debug()))];
            if n.debug() {
                v.push(Row::info("Time remaining", format!("{} minutes", n.get("debug_minutes"))));
                v.push(Row::new(RowId::DebugLan, "Allow on local network", "Expose SSH to devices on this network", Kind::Toggle(n.get("debug_lan") == "1")));
                v.push(Row::new(RowId::DebugCloud, "Allow through cloud", "Enrolled computers can connect from any network", Kind::Toggle(n.get("debug_cloud") == "1")));
                v.push(Row::new(RowId::DebugPair, "Pair developer computer", "Open pairing for two minutes, then run meowdbg pair HOST", Kind::Action(String::new())));
                let ip = n.get("lan_ip");
                v.push(Row::info("Pairing address", if ip.is_empty() { "Connect USB or Wi-Fi".to_string() } else { format!("meowdbg pair {ip}") }));
                if !n.get("debug_usb").is_empty() {
                    v.push(Row::info("USB pairing", format!("meowdbg pair {}", n.get("debug_usb"))));
                }
                if !n.get("debug_code").is_empty() {
                    v.push(Row::info("Pairing code", n.get("debug_code")));
                    v.push(Row::info("On your computer", "Enter this code when meowdbg asks"));
                }
                if n.get("cloud") == "online" {
                    // meowdbg use-cloud <id>: the same bridge through the relay, from any network.
                    v.push(Row::info("Cloud ID", n.get("cloud_id")));
                }
                if n.get("debug_running") != "1" {
                    v.push(Row::info("Status", "Starting…"));
                }
            }
            for i in 0..8 {
                if let Some((id, label)) = n.get(&format!("developer_{i}")).split_once(':') {
                    v.push(Row::new(RowId::DebugRevoke(id.to_string()), label, format!("Developer computer · {id}"), Kind::Action("Revoke…".into())));
                }
            }
            v.extend(developer_rows(st));
            v
        }
        Cat::About => about_rows(i, dash),
    }
}

/// The page of one installed app: its background switch, its permissions, Uninstall.
fn app_rows(app: &AppInfo, detail: Option<&AppDetail>) -> Vec<Row> {
    let mut v = Vec::new();
    let Some(d) = detail else {
        v.push(Row::info("Details", "Not available"));
        v.push(Row::new(RowId::AppUninstall(app.id.clone()), S_UNINSTALL, S_UNINSTALL_SUB, Kind::Action(String::new())));
        return v;
    };
    if d.background != Background::None {
        v.push(Row::new(RowId::AppBg(app.id.clone()), S_BACKGROUND, d.service_label(), Kind::Toggle(d.background == Background::On)));
        let service = match (d.background, d.service.as_str()) {
            (_, "running") => "Running",
            (Background::On, "failing") => "Keeps stopping",
            (Background::On, _) => "Starting…",
            _ => "Stopped",
        };
        v.push(Row::info("Background service", service));
    }
    v.push(Row::info(S_PERMISSIONS, if d.perms.is_empty() { "None" } else { "" }));
    for p in &d.perms {
        let mut sub: Vec<&str> = vec![if p.sensitive { "Needs your consent" } else { "Standard" }];
        if !p.required {
            sub.push("optional");
        }
        // The facts first (hosts, memory), then the app's own reason; a quota is in the label.
        sub.extend([p.extra.as_str(), p.reason.as_str()].into_iter().filter(|t| !t.is_empty() && !p.label.contains(*t)));
        let (state, tint) = match p.state.as_str() {
            "granted" => ("Allowed", 0xffffff),
            "denied" => ("Not allowed", AMBER),
            _ => ("Not asked yet", AMBER),
        };
        v.push(Row::new(RowId::None, p.label.clone(), sub.join("  ·  "), Kind::Perm(state.into(), tint, p.sensitive)));
    }
    if d.background == Background::None {
        v.push(Row::info("Background", d.service_label()));
    }
    v.push(Row::new(RowId::AppUninstall(app.id.clone()), S_UNINSTALL, S_UNINSTALL_SUB, Kind::Action(String::new())));
    v
}

fn developer_rows(st: &Settings) -> Vec<Row> {
    vec![
            Row::new(RowId::Hud, S_HUD, S_HUD_SUB, Kind::Toggle(st.hud)),
            Row::new(RowId::Restart, S_RESTART, S_RESTART_SUB, Kind::Action(String::new())),
            Row::info("Launcher", concat!("miracast-ui ", env!("CARGO_PKG_VERSION"))),
        ]
}

fn about_rows(i: &SysInfo, dash: impl Fn(&str) -> String) -> Vec<Row> {
    vec![
            Row::info("Device", "RK3228A TV dongle"),
            Row::info("Processor", format!("{} × Cortex-A7, up to {} MHz", i.cpus, i.cpu_max_mhz)),
            Row::info("Graphics", format!("Mali-400 MP2 · {} MHz now", i.gpu_mhz)),
            Row::info("Memory", format!("{} MiB free of {} MiB", i.mem_avail_mib, i.mem_total_mib)),
            Row::info(
                "Storage",
                if i.data_volatile {
                    "Not saved (no data partition)".to_string()
                } else {
                    format!("{} free of {}", settings::size_label(i.data_free_kib), settings::size_label(i.data_total_kib))
                },
            ),
            Row::info("Temperature", format!("{:.0} °C", i.temp_c)),
            Row::info("Linux", dash(&i.kernel)),
            Row::info("Up for", settings::uptime_label(i.uptime_s)),
            Row::new(RowId::FactoryReset, S_RESET, S_RESET_SUB, Kind::Action(String::new())),
    ]
}
