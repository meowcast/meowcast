//! The Plasma-Bigscreen-style home screen: top bar (clock, date, 5 line icons: settings, the
//! volume as it is, Wi-Fi, the phone remote's state, sleep) and horizontally scrolling rows of
//! rounded cards: the apps opened last ("Recent", only when there are any), all apps, and the
//! Settings categories. Layout is in 1920x1080 logical units
//! (measured from Bigscreen_plasma.png) and multiplied by the output scale.
//!
//! Each frame is rebuilt as a list of items (stable id + quads). main.rs diffs consecutive
//! lists to find the damaged area, so only changed regions are redrawn.

use crate::art::{self, Badge, Glyph};
use crate::atlas::{Atlas, Region};
use crate::gpu::{Gpu, Vertex};
use crate::text::{Face, Fits, Text};
use crate::util::{Rect, RectF, Tween, rgba};
use std::time::{Duration, Instant};

// Layout (logical px).
const MARGIN_X: f32 = 110.0;
const CLOCK_BASE: f32 = 132.0;
const ICON_CY: f32 = 113.0;
const ICON_SIZE: f32 = 38.0;
const ICON_PITCH: f32 = 88.0;
const ICON_LAST_CX: f32 = 1779.0;
const HEADER_BASE: f32 = 597.0;
const SECTION_PITCH: f32 = 413.0;
const CARD_DY: f32 = 24.0; // header baseline -> card top
const CARD_W: f32 = 350.0;
const CARD_H: f32 = 219.0;
const CARD_PITCH: f32 = 378.0;
const CARD_R: f32 = 8.0;
const FOCUS_SCALE: f32 = 0.05;
const RING_GAP: f32 = 5.0;
const RING_W: f32 = 4.0;
const BADGE_D: f32 = 108.0;
// Card text: the title on one line and the description on up to two, both as wide as the
// card (cut with an ellipsis), so no text reaches the next card.
const TITLE_PX: f32 = 29.0;
const SUB_PX: f32 = 21.0;
const SUB_LINES: usize = 2;
/// Title baseline -> first description line, and between the two lines: the second line of
/// the focused card (it sits lower) has to stay clear of the next row's header.
const SUB_DY: f32 = 31.0;
const SUB_PITCH: f32 = 25.0;
const FULLY_VISIBLE: usize = 4; // the 5th card is cut by the screen edge
pub const FOCUS_COLOR: u32 = 0x3daee9;

const FOCUS_MS: u64 = 150;
pub const NOTICE_SECS: f32 = 4.0;
const SCROLL_X_MS: u64 = 250;
const SCROLL_Y_MS: u64 = 300;
const WALL_DEPTH_MS: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    Enter,
    Back,
    /// System key: always returns to the launcher (closes the foreground app).
    Home,
}

impl Key {
    /// Name on the appd protocol (keys forwarded to the foreground app).
    pub fn name(self) -> &'static str {
        match self {
            Key::Left => "left",
            Key::Right => "right",
            Key::Up => "up",
            Key::Down => "down",
            Key::Enter => "enter",
            Key::Back => "back",
            Key::Home => "home",
        }
    }
}

/// What a key press on the home screen asks the launcher to do.
pub enum Action {
    Launch(String),
    /// Open Settings (on this category, or the last one; `true` = focus into its rows).
    Settings(Option<crate::setview::Cat>, bool),
    /// Open the volume pop-up for the D-pad (the top bar's speaker).
    Volume,
    /// Blank the screen until the next key (the top bar's power button).
    Sleep,
}

/// Install consent dialog (appd CONSENT): the permissions, Allow / Don't allow.
pub struct Consent {
    pub id: String,
    pub name: String,
    pub perms: Vec<crate::apps::Perm>,
    /// The app is installed; an update asks for these (optional) permissions.
    pub update: bool,
    /// 0 = Allow/Install, 1 = Don't allow/Cancel.
    pub focus: usize,
    pub shown_at: Instant,
}

impl Consent {
    pub fn sensitive(&self) -> bool {
        self.perms.iter().any(|p| p.sensitive)
    }
    /// Some(true) = allow, Some(false) = deny.
    pub fn key(&mut self, k: Key) -> Option<bool> {
        match k {
            Key::Left => self.focus = 0,
            Key::Right => self.focus = 1,
            Key::Enter => return Some(self.focus == 0),
            Key::Back | Key::Home => return Some(false),
            _ => {}
        }
        None
    }
}

/// What a row of cards is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecKind {
    /// The apps opened last, most recent first. The row exists only while there are any.
    Recent,
    /// appd's apps.
    Apps,
    /// One card per Settings category (in setview::Cat::ALL order).
    Settings,
}

/// The phone remote ("pilot"), as the top bar's phone icon shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pilot {
    /// No phone is paired.
    #[default]
    None,
    /// Paired, but no phone is connected right now.
    Paired,
    /// A paired phone is connected now.
    Connected,
}

impl Pilot {
    /// From meow-netd's state: `pilots` paired phones, of which `online` are connected now
    /// (`pilot_connected=1` says the same, when the daemon publishes it).
    pub fn of(pilots: i64, online: i64, connected_flag: &str) -> Pilot {
        match (pilots > 0, online > 0 || connected_flag == "1") {
            (false, _) => Pilot::None,
            (true, false) => Pilot::Paired,
            (true, true) => Pilot::Connected,
        }
    }
}

struct Card {
    title: String,
    subtitle: String,
    /// appd app id (None: built-in placeholder card).
    app_id: Option<String>,
    tint: u32,
    glyph: Glyph,
    badge: Badge,
    grad: (u32, u32),
    focus: Tween,
    icon: Option<Option<Region>>, // lazily rendered on first visibility
    text_warm: bool,
}

struct Section {
    kind: SecKind,
    /// Index in CATALOG (start-up badges are rendered by it); None for the Recent row.
    catalog: Option<usize>,
    title: &'static str,
    cards: Vec<Card>,
    col: usize,
    first: Tween, // first fully visible card (in cards)
    title_warm: bool,
}

struct Art {
    card: (Region, u32),
    ring: (Region, u32),
    shadow: (Region, u32, u32),
    icons: Vec<Option<Region>>,
    /// The speaker by `osd::Speaker` (muted, low, medium, high), the crossed-out phone, and
    /// the "connected" dot.
    speakers: Vec<Option<Region>>,
    phone_off: Option<Region>,
    status_dot: Option<Region>,
    disc: Option<Region>,
    /// Scale at which the art was rasterised (output scale x focused size, so the
    /// focused card is drawn 1:1).
    raster: f32,
}

pub struct Item {
    pub id: u32,
    pub start: usize,
    pub len: usize,
    pub bounds: Rect,
    pub hash: u64,
    /// Texture of its quads: None = the UI atlas (nearly everything). Consecutive items of
    /// one texture are one draw call (main.rs).
    pub tex: Option<glow::Texture>,
}

#[derive(Default)]
pub struct Frame {
    pub verts: Vec<Vertex>,
    pub items: Vec<Item>,
}

impl Frame {
    pub(crate) fn begin(&self) -> usize {
        self.verts.len()
    }
    pub(crate) fn end(&mut self, id: u32, start: usize) {
        self.end_tex(id, start, None)
    }
    /// `end` for quads of another texture than the atlas.
    pub(crate) fn end_tex(&mut self, id: u32, start: usize, tex: Option<glow::Texture>) {
        let len = self.verts.len() - start;
        if len == 0 {
            return;
        }
        let vs = &self.verts[start..];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        let mut h: u64 = 0xcbf29ce484222325;
        for v in vs {
            x0 = x0.min(v.x);
            y0 = y0.min(v.y);
            x1 = x1.max(v.x);
            y1 = y1.max(v.y);
            for word in [v.x.to_bits(), v.y.to_bits(), v.u.to_bits(), v.v.to_bits(), u32::from_ne_bytes(v.rgba)] {
                h = (h ^ word as u64).wrapping_mul(0x100000001b3);
            }
        }
        let bounds = Rect::new(x0.floor() as i32 - 1, y0.floor() as i32 - 1, x1.ceil() as i32 + 1, y1.ceil() as i32 + 1);
        self.items.push(Item { id, start, len, bounds, hash: h, tex });
    }
}

pub fn quad(out: &mut Vec<Vertex>, x0: f32, y0: f32, x1: f32, y1: f32, uv: [f32; 4], c: [u8; 4]) {
    out.push(Vertex { x: x0, y: y0, u: uv[0], v: uv[1], rgba: c });
    out.push(Vertex { x: x1, y: y0, u: uv[2], v: uv[1], rgba: c });
    out.push(Vertex { x: x0, y: y1, u: uv[0], v: uv[3], rgba: c });
    out.push(Vertex { x: x1, y: y1, u: uv[2], v: uv[3], rgba: c });
}

/// 9-slice: `margin` texels of `r` map to `dm` pixels of `dst`; the centre stretches.
pub(crate) fn nine(out: &mut Vec<Vertex>, atlas: &Atlas, r: &Region, margin: u32, dst: RectF, dm: f32, c: [u8; 4]) {
    nine_ex(out, atlas, r, margin, dst, dm, c, true)
}

/// 9-slice without the centre piece when `centre` is false (shadows: the centre is hidden
/// under the opaque card, so drawing it would only cost fill rate).
#[allow(clippy::too_many_arguments)]
pub(crate) fn nine_ex(out: &mut Vec<Vertex>, atlas: &Atlas, r: &Region, margin: u32, dst: RectF, dm: f32, c: [u8; 4], centre: bool) {
    let dm = dm.min(dst.w / 2.0).min(dst.h / 2.0);
    let m = margin as f32;
    let xs = [dst.x, dst.x + dm, dst.x + dst.w - dm, dst.x + dst.w];
    let ys = [dst.y, dst.y + dm, dst.y + dst.h - dm, dst.y + dst.h];
    let us = [0.0, m, r.w as f32 - m, r.w as f32];
    let vs = [0.0, m, r.h as f32 - m, r.h as f32];
    for j in 0..3 {
        for i in 0..3 {
            if !centre && i == 1 && j == 1 {
                continue;
            }
            let uv = atlas.uv(r, us[i], vs[j], us[i + 1] - us[i], vs[j + 1] - vs[j]);
            quad(out, xs[i], ys[j], xs[i + 1], ys[j + 1], uv, c);
        }
    }
}

/// Cut the quads of `verts[start..]` (as `quad` pushes them) to `r`; their texture
/// coordinates follow, and quads left with nothing are removed. For content that scrolls
/// inside a window (the Settings pickers): call it before `Frame::end`.
pub(crate) fn clip(verts: &mut Vec<Vertex>, start: usize, r: RectF) {
    let (cx0, cy0, cx1, cy1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    let mut to = start;
    for i in (start..verts.len().saturating_sub(3)).step_by(4) {
        let (a, d) = (verts[i], verts[i + 3]);
        let (x0, y0, x1, y1) = (a.x.max(cx0), a.y.max(cy0), d.x.min(cx1), d.y.min(cy1));
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        if (x0, y0, x1, y1) == (a.x, a.y, d.x, d.y) {
            verts.copy_within(i..i + 4, to);
        } else {
            let u = |x: f32| a.u + (d.u - a.u) * (x - a.x) / (d.x - a.x);
            let v = |y: f32| a.v + (d.v - a.v) * (y - a.y) / (d.y - a.y);
            let (u0, v0, u1, v1) = (u(x0), v(y0), u(x1), v(y1));
            verts[to] = Vertex { x: x0, y: y0, u: u0, v: v0, rgba: a.rgba };
            verts[to + 1] = Vertex { x: x1, y: y0, u: u1, v: v0, rgba: a.rgba };
            verts[to + 2] = Vertex { x: x0, y: y1, u: u0, v: v1, rgba: a.rgba };
            verts[to + 3] = Vertex { x: x1, y: y1, u: u1, v: v1, rgba: a.rgba };
        }
        to += 4;
    }
    verts.truncate(to);
}

const TOP_N: usize = 5;
const TOP_ICONS: [Glyph; TOP_N] = [Glyph::Sliders, Glyph::Volume, Glyph::Wifi, Glyph::Device, Glyph::Power];
const TOP_SETTINGS: usize = 0;
const TOP_VOLUME: usize = 1;
const TOP_WIFI: usize = 2;
const TOP_PHONE: usize = 3;
/// Every tooltip a top-bar button can show (rasterised at start-up).
const TOP_LABEL_TEXT: &str = "Settings Volume 0123456789 Muted Wi-Fi Phone remote connected paired, not No phone Sleep";
const STATUS_OK: u32 = 0x4ad06a;
const TOP_DISC: f32 = 64.0;
const TOP_LABEL_PX: f32 = 21.0;
/// Home content slides down this far while Settings opens over it.
const AWAY_DY: f32 = 36.0;

/// Static card catalogue: (section title, [(title, subtitle, tint, glyph, badge gradient,
/// badge shape)]). Pure data, so worker threads can pre-render badges and glyphs from it.
type CardDef = (&'static str, &'static str, u32, Glyph, (u32, u32), Badge);
const CATALOG_KINDS: [SecKind; 2] = [SecKind::Apps, SecKind::Settings];
const CATALOG: &[(&str, &[CardDef])] = {
    use Badge::{Circle, Square};
    &[
        // Placeholders until appd's list arrives (and without appd).
        ("Applications", &[
            ("Photos", "Your pictures", 0x4a3f2c, Glyph::Photo, (0xf6b84a, 0xe0782e), Circle),
            ("Files", "Browse local storage", 0x4a2633, Glyph::Folder, (0x56a8f8, 0x2e6bd4), Circle),
            ("Archives", "Pack and unpack files", 0x23262b, Glyph::Archive, (0xa0a6ac, 0x5c6066), Square),
            ("Podcasts", "Shows and episodes", 0x2f3a5a, Glyph::Headphones, (0xb862ea, 0x6a34c8), Circle),
            ("Weather", "Forecast for today", 0x1f3b3a, Glyph::Sun, (0xffd24e, 0xf2952a), Circle),
            ("Games", "Play with a controller", 0x51385e, Glyph::Gamepad, (0xec5470, 0xb1244a), Circle),
            ("Web Browser", "Browse the web", 0x1b7e73, Glyph::Globe, (0x2b8c7c, 0x0f4f47), Circle),
            ("Music", "Listen to your music collection", 0x384263, Glyph::Music, (0x6a9af5, 0x3c4ed0), Circle),
            ("Media Player", "Play your media files", 0x673937, Glyph::Play, (0xd04ad8, 0x7a2fd0), Circle),
            ("Terminal", "Use the command line", 0x2a2e32, Glyph::Terminal, (0x50565c, 0x2f3338), Square),
        ]),
        // One card per Settings category, in setview::Cat::ALL order (Enter opens it).
        ("Settings", &[
            ("Wi-Fi", "Wireless networks", 0x384263, Glyph::Wifi, (0x4fb3e8, 0x2a72c4), Circle),
            ("Network", "USB Ethernet link", 0x1f3b3a, Glyph::Lan, (0x3cc0a0, 0x16806c), Circle),
            ("Display", "Resolution and dimming", 0x1b7e73, Glyph::Monitor, (0x2b8c7c, 0x0f4f47), Circle),
            ("Wallpaper", "Choose a background", 0x4a2633, Glyph::Photo, (0xec5470, 0xb1244a), Circle),
            ("Date & time", "Time zone and clock", 0x51385e, Glyph::Clock, (0xb862ea, 0x6a34c8), Circle),
            ("Apps", "Manage installed apps", 0x4a3f2c, Glyph::Apps, (0xf6b84a, 0xe0782e), Circle),
            ("Developer", "Performance overlay", 0x2a2e32, Glyph::Terminal, (0x50565c, 0x2f3338), Square),
            ("About", "This device", 0x673937, Glyph::Info, (0xf39a4c, 0xd5473b), Circle),
        ]),
    ]
};

/// CPU-side home artwork (any thread); `Home::new` uploads it.
pub struct ArtImages {
    card: art::Slice,
    ring: art::Slice,
    shadow: (art::Slice, u32),
    icons: Vec<crate::atlas::Image>,
    speakers: Vec<crate::atlas::Image>,
    phone_off: crate::atlas::Image,
    status_dot: crate::atlas::Image,
    disc: crate::atlas::Image,
}

/// Scale the card art is rasterised at (output scale x focused size, so the focused card
/// is drawn 1:1).
fn raster_scale(scale: f32) -> f32 {
    scale * (1.0 + FOCUS_SCALE)
}

pub fn render_art(scale: f32) -> ArtImages {
    let raster = raster_scale(scale);
    let isz = (ICON_SIZE * scale).round() as u32;
    ArtImages {
        card: art::slice_rounded(CARD_R * raster),
        ring: art::slice_ring((CARD_R + RING_GAP) * raster, RING_W * raster),
        shadow: art::slice_shadow(CARD_R * raster, 16.0 * raster),
        icons: TOP_ICONS.iter().map(|g| art::line_icon(*g, isz)).collect(),
        speakers: [Glyph::VolumeMute, Glyph::VolumeLow, Glyph::VolumeMid, Glyph::Volume].iter().map(|g| art::line_icon(*g, isz)).collect(),
        phone_off: art::line_icon(Glyph::DeviceOff, isz),
        status_dot: art::dot((12.0 * scale).round().max(4.0) as u32),
        disc: art::dot((TOP_DISC * scale).round() as u32),
    }
}

/// Render every card badge, in on-screen order (first rows and columns first).
pub fn render_badges(scale: f32, mut out: impl FnMut(usize, usize, crate::atlas::Image)) {
    let d = (BADGE_D * raster_scale(scale)).round() as u32;
    let mut order: Vec<(usize, usize)> =
        CATALOG.iter().enumerate().flat_map(|(si, (_, cards))| (0..cards.len()).map(move |ci| (si, ci))).collect();
    order.sort_by_key(|&(si, ci)| (ci.min(FULLY_VISIBLE + 1) + si * 2, si, ci));
    for (si, ci) in order {
        let c = &CATALOG[si].1[ci];
        out(si, ci, art::app_icon(c.3, d, c.4.0, c.4.1, c.5));
    }
}

/// Every string the home screen draws, with its face and physical size, so its glyphs can
/// be rasterised off the render thread before the first frame.
pub fn text_runs(scale: f32, date: &str) -> Vec<(Face, f32, String)> {
    let s = scale;
    let mut runs = vec![
        (Face::Light, 50.0 * s, "0123456789: AMP".to_string()),
        (Face::Regular, 36.0 * s, date.to_string()),
    ];
    for (title, cards) in CATALOG {
        runs.push((Face::Light, 48.0 * s, title.to_string()));
        for c in *cards {
            runs.push((Face::Regular, TITLE_PX * s, c.0.to_string()));
            runs.push((Face::Regular, SUB_PX * s, c.1.to_string()));
        }
    }
    // The ellipsis of text cut to its room (text::Fits).
    for px in [TITLE_PX, SUB_PX, 22.0, 24.0, 26.0, 28.0] {
        runs.push((Face::Regular, px * s, "…".to_string()));
    }
    runs.push((Face::Light, 46.0 * s, "…".to_string()));
    runs.push((Face::Regular, TOP_LABEL_PX * s, TOP_LABEL_TEXT.to_string()));
    runs.push((Face::Light, 48.0 * s, "Recent".to_string()));
    // Every weekday and month, so the date never rasterises at midnight.
    runs.push((Face::Regular, 36.0 * s, "SundayMondayTuesdayWednesdayThursdayFridaySaturday,0123456789".into()));
    runs.push((Face::Regular, 36.0 * s, "JanuaryFebruaryMarchAprilMayJuneJulyAugustSeptemberOctoberNovemberDecember".into()));
    runs
}

// Entrance choreography (ms after the entrance starts): the top bar drops in, section
// headers and cards rise in a left-to-right, top-to-bottom cascade.
const ENT_CARD_DELAY: f32 = 250.0;
const ENT_CARD_STAGGER: f32 = 60.0;
const ENT_ROW_STAGGER: f32 = 120.0;
const ENT_CARD_MS: f32 = 700.0;
const ENT_CARD_RISE: f32 = 80.0;
const ENT_TOP_DELAY: f32 = 300.0;
const ENT_TOP_MS: f32 = 700.0;
pub const ENTRANCE_MS: u64 = 1400;
/// When the first card has fully risen (the intro's focus ring lands then).
pub const ENTRANCE_FOCUS_MS: f32 = ENT_CARD_DELAY + ENT_CARD_MS;

fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Geometry of one card at a point in time (physical px unless noted).
struct CardGeom {
    r: RectF,
    t: f32,      // focus 0..1
    k: f32,      // focus scale
    draw_k: f32, // art texels -> screen px
    a: f32,      // opacity
    x: f32,      // logical left edge (unscaled position)
    top: f32,    // logical card top incl. entrance offset
    ent: f32,    // entrance progress 0..1
}

pub struct Home {
    s: f32,
    sections: Vec<Section>,
    row: usize,
    scroll_y: Tween,
    art: Art,
    pub icons_rendered: u32,
    digits_warm: bool,
    entrance: Option<Instant>,
    /// The intro draws the focus ring itself while it morphs into place.
    pub ring_hidden: bool,
    /// Launch splash art: 2x app icons by app id (None = atlas full), glow and dot sprites.
    big_badges: std::collections::HashMap<String, Option<Region>>,
    fx: Option<(Option<Region>, Option<Region>)>,
    /// Wallpaper depth: 0 = sharp layer (top row focused), 1 = soft, blurred layer.
    blur: Tween,
    /// The depth dissolve runs together with the vertical scroll (cheap with the wallpaper on
    /// its own display plane) instead of after it (GPU-composited wallpaper).
    pub blur_with_scroll: bool,
    /// Focused top-bar button (None: the focus is on a card).
    top: Option<usize>,
    top_last: usize,
    top_focus: [Tween; TOP_N],
    /// 0 = home content shown, 1 = hidden behind Settings.
    away: Tween,
    /// The system volume (level, muted) for the speaker icon; None without appd.
    volume: Option<(u32, bool)>,
    pilot: Pilot,
    /// The volume pop-up is open under the speaker: its tooltip stays away.
    pub volume_popup: bool,
    /// The user has moved the focus (a Recent row arriving later then keeps it where it is).
    touched: bool,
    /// Text cut or wrapped to its room (card titles and descriptions, dialogs, notices).
    fits: Fits,
}

/// The shared 9-slice art, for other views drawn in the same style (setview.rs).
pub struct Slices {
    pub card: (Region, u32),
    pub ring: (Region, u32),
    pub shadow: (Region, u32, u32),
    /// Scale the slices were rasterised at.
    pub raster: f32,
}

/// A solid rectangle from the opaque centre of the rounded-card slice `r` (margin `m`).
#[allow(clippy::too_many_arguments)]
pub fn solid(out: &mut Vec<Vertex>, atlas: &Atlas, r: &Region, m: u32, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 4]) {
    let uv = atlas.uv(r, m as f32, m as f32, 2.0, 2.0);
    quad(out, x0, y0, x1, y1, uv, c);
}

/// Where a card is drawn now (for the launch transition to start/end exactly on it).
pub struct CardVisual {
    pub rect: RectF,
    pub badge_w: f32,
    pub corner: f32,
    pub tint: u32,
    pub glyph: Glyph,
    pub grad: (u32, u32),
    pub shape: Badge,
    pub title: String,
}

/// The card of app `a`: one of `old` when its look is unchanged (keeps the badge and the
/// rasterised text), else a new one.
fn app_card(a: &crate::apps::AppInfo, old: &mut Vec<Card>) -> Card {
    let reuse = old.iter().position(|c| c.app_id.as_deref() == Some(a.id.as_str()) && c.glyph == a.glyph && c.grad == a.badge && c.badge == a.shape && c.tint == a.tint);
    let mut c = match reuse {
        Some(i) => old.swap_remove(i),
        None => Card {
            title: String::new(),
            subtitle: String::new(),
            app_id: Some(a.id.clone()),
            tint: a.tint,
            glyph: a.glyph,
            badge: a.shape,
            grad: a.badge,
            focus: Tween::fixed(0.0),
            icon: None,
            text_warm: false,
        },
    };
    let sub = a.subtitle();
    if c.title != a.name || c.subtitle != sub {
        c.text_warm = false;
    }
    c.title = a.name.clone();
    c.subtitle = sub;
    c
}

const SPLASH_BADGE: f32 = 216.0;
const SPLASH_CY: f32 = 455.0;
/// Room for the app's name and the status line under the splash icon.
const SPLASH_TEXT_W: f32 = 1920.0 - 2.0 * MARGIN_X;

impl Home {
    pub fn new(gpu: &Gpu, atlas: &mut Atlas, scale: f32, img: ArtImages) -> Home {
        let sections = CATALOG
            .iter()
            .enumerate()
            .map(|(i, (title, cards))| Section {
                kind: CATALOG_KINDS[i],
                catalog: Some(i),
                title,
                cards: cards
                    .iter()
                    .map(|c| Card {
                        title: c.0.to_string(),
                        subtitle: c.1.to_string(),
                        app_id: None,
                        tint: c.2,
                        glyph: c.3,
                        badge: c.5,
                        grad: c.4,
                        focus: Tween::fixed(0.0),
                        icon: None,
                        text_warm: false,
                    })
                    .collect(),
                col: 0,
                first: Tween::fixed(0.0),
                title_warm: false,
            })
            .collect();
        let ins = |atlas: &mut Atlas, img: &crate::atlas::Image| atlas.insert(gpu, img).expect("atlas space for base art");
        let card_r = ins(atlas, &img.card.img);
        let ring_r = ins(atlas, &img.ring.img);
        let sh_r = ins(atlas, &img.shadow.0.img);
        let icons = img.icons.iter().map(|i| atlas.insert(gpu, i)).collect();
        let speakers = img.speakers.iter().map(|i| atlas.insert(gpu, i)).collect();
        let mut home = Home {
            s: scale,
            sections,
            row: 0,
            scroll_y: Tween::fixed(0.0),
            art: Art {
                card: (card_r, img.card.margin),
                ring: (ring_r, img.ring.margin),
                shadow: (sh_r, img.shadow.0.margin, img.shadow.1),
                icons,
                speakers,
                phone_off: atlas.insert(gpu, &img.phone_off),
                status_dot: atlas.insert(gpu, &img.status_dot),
                disc: atlas.insert(gpu, &img.disc),
                raster: raster_scale(scale),
            },
            icons_rendered: 0,
            digits_warm: false,
            entrance: None,
            ring_hidden: false,
            big_badges: std::collections::HashMap::new(),
            fx: None,
            blur: Tween::fixed(0.0),
            blur_with_scroll: false,
            top: None,
            top_last: TOP_SETTINGS,
            top_focus: [Tween::fixed(0.0); TOP_N],
            away: Tween::fixed(0.0),
            volume: None,
            pilot: Pilot::None,
            volume_popup: false,
            touched: false,
            fits: Fits::default(),
        };
        home.sections[0].cards[0].focus = Tween::fixed(1.0);
        home
    }

    /// Upload a badge rendered by `render_badges` (no-op if it is already there).
    pub fn set_badge(&mut self, gpu: &Gpu, atlas: &mut Atlas, si: usize, ci: usize, img: &crate::atlas::Image) {
        // Start-up badges belong to the built-in catalogue; app cards render their own.
        let Some(c) = self.sections.iter_mut().find(|s| s.catalog == Some(si)).and_then(|s| s.cards.get_mut(ci)) else { return };
        if c.icon.is_none() && c.app_id.is_none() {
            c.icon = Some(atlas.insert(gpu, img));
            self.icons_rendered += 1;
        }
    }

    /// Replace the Applications row with appd's apps (keeps focus on the same app, reuses
    /// badges and text of cards whose look did not change).
    pub fn set_apps(&mut self, apps: &[crate::apps::AppInfo], now: Instant) {
        let Some(si) = self.sec(SecKind::Apps) else { return };
        let sec = &mut self.sections[si];
        let focused_id = sec.cards.get(sec.col).and_then(|c| c.app_id.clone());
        let mut old: Vec<Card> = std::mem::take(&mut sec.cards);
        let mut cards: Vec<Card> = apps.iter().map(|a| app_card(a, &mut old)).collect();
        if cards.is_empty() {
            cards.push(Card {
                title: "No apps yet".into(),
                subtitle: "Waiting for the app store".into(),
                app_id: None,
                tint: 0x2a2e32,
                glyph: Glyph::Apps,
                badge: Badge::Circle,
                grad: (0x7d848b, 0x464b51),
                focus: Tween::fixed(0.0),
                icon: None,
                text_warm: false,
            });
        }
        let col = focused_id.and_then(|id| cards.iter().position(|c| c.app_id.as_deref() == Some(id.as_str()))).unwrap_or(sec.col.min(cards.len() - 1));
        for (i, c) in cards.iter_mut().enumerate() {
            let on = self.row == si && i == col && self.top.is_none();
            c.focus = Tween::fixed(if on { 1.0 } else { 0.0 });
        }
        sec.cards = cards;
        sec.col = col;
        let max_first = sec.cards.len().saturating_sub(FULLY_VISIBLE) as f32;
        if sec.first.target() > max_first {
            sec.first.go(max_first, now, Duration::from_millis(SCROLL_X_MS));
        }
        self.share_icons();
    }

    fn sec(&self, kind: SecKind) -> Option<usize> {
        self.sections.iter().position(|s| s.kind == kind)
    }

    /// A Recent card shows the same badge as the app's card in Applications: one atlas
    /// entry for both.
    fn share_icons(&mut self) {
        let known: Vec<(String, Glyph, (u32, u32), Badge, Option<Region>)> = self
            .sections
            .iter()
            .flat_map(|s| s.cards.iter())
            .filter_map(|c| Some((c.app_id.clone()?, c.glyph, c.grad, c.badge, c.icon?)))
            .collect();
        for c in self.sections.iter_mut().flat_map(|s| s.cards.iter_mut()).filter(|c| c.icon.is_none()) {
            if let Some(k) = known.iter().find(|k| c.app_id.as_deref() == Some(k.0.as_str()) && (k.1, k.2, k.3) == (c.glyph, c.grad, c.badge)) {
                c.icon = Some(k.4);
            }
        }
    }

    /// The "Recent" row: `recents` (app ids, most recent first, from appd) that are still
    /// installed. Without any the row is not there at all. When it appears or goes, the
    /// focus stays on the card it was on (the rows shift under it, nothing moves on screen);
    /// only before the user has touched anything does a new Recent row take the focus, so
    /// the home screen opens on it. Inside the row the focus follows the app it is on: the
    /// app just opened is first when the user comes back from it.
    pub fn set_recents(&mut self, apps: &[crate::apps::AppInfo], recents: &[String], now: Instant) {
        let list: Vec<&crate::apps::AppInfo> = recents.iter().filter_map(|id| apps.iter().find(|a| a.id == *id && a.state != "available")).collect();
        let on_top = self.top.is_some();
        match (self.sec(SecKind::Recent), list.is_empty()) {
            (None, true) => {}
            (Some(si), true) => {
                self.sections.remove(si);
                if self.row > si {
                    self.row -= 1;
                } else if !on_top {
                    // The focus was in the row that went: on to the first card of the next.
                    let col = self.sections[self.row].col.min(self.sections[self.row].cards.len() - 1);
                    self.sections[self.row].col = col;
                    self.sections[self.row].cards[col].focus = Tween::fixed(1.0);
                }
                self.scroll_y = Tween::fixed(self.row as f32);
                self.blur.go(if self.row == 0 && self.away.target() == 0.0 { 0.0 } else { 1.0 }, now, Duration::from_millis(WALL_DEPTH_MS));
            }
            (found, false) => {
                let si = found.unwrap_or(0);
                let mut old: Vec<Card> = match found {
                    Some(si) => std::mem::take(&mut self.sections[si].cards),
                    None => {
                        self.sections.insert(0, Section { kind: SecKind::Recent, catalog: None, title: "Recent", cards: Vec::new(), col: 0, first: Tween::fixed(0.0), title_warm: false });
                        Vec::new()
                    }
                };
                let focused_id = (found.is_some() && self.row == si).then(|| old.get(self.sections[si].col).and_then(|c| c.app_id.clone())).flatten();
                let mut cards: Vec<Card> = list.iter().map(|a| app_card(a, &mut old)).collect();
                let mut col = self.sections[si].col.min(cards.len() - 1);
                if found.is_none() {
                    if self.touched || self.row != 0 {
                        // The rows move down by one under the focus.
                        self.row += 1;
                        self.scroll_y = Tween::fixed(self.row as f32);
                        self.blur.go(1.0, now, Duration::from_millis(WALL_DEPTH_MS));
                    } else {
                        // Untouched home screen: it opens on the Recent row.
                        let below = &mut self.sections[1];
                        let c = below.col.min(below.cards.len() - 1);
                        below.cards[c].focus = Tween::fixed(0.0);
                        col = 0;
                    }
                } else if let Some(i) = focused_id.and_then(|id| cards.iter().position(|c| c.app_id.as_deref() == Some(id.as_str()))) {
                    col = i;
                }
                for (i, c) in cards.iter_mut().enumerate() {
                    c.focus = Tween::fixed(if self.row == si && i == col && !on_top { 1.0 } else { 0.0 });
                }
                let sec = &mut self.sections[si];
                sec.cards = cards;
                sec.col = col;
                // Keep the focused card among the fully visible ones.
                let first = sec.first.target() as usize;
                let max_first = sec.cards.len().saturating_sub(FULLY_VISIBLE);
                let nfirst = if col < first { col } else if col >= first + FULLY_VISIBLE { col + 1 - FULLY_VISIBLE } else { first };
                sec.first = Tween::fixed(nfirst.min(max_first) as f32);
            }
        }
        self.share_icons();
    }

    /// The system volume for the speaker icon (None: no app service). True if the icon or
    /// its tooltip changed.
    pub fn set_volume(&mut self, volume: Option<(u32, bool)>) -> bool {
        std::mem::replace(&mut self.volume, volume) != volume
    }

    pub fn set_pilot(&mut self, pilot: Pilot) -> bool {
        std::mem::replace(&mut self.pilot, pilot) != pilot
    }

    fn top_label(&self, i: usize) -> String {
        match i {
            TOP_SETTINGS => "Settings".into(),
            TOP_VOLUME => match self.volume {
                Some((_, true)) => "Muted".into(),
                Some((v, false)) => format!("Volume {v}"),
                None => "Volume".into(),
            },
            TOP_WIFI => "Wi-Fi".into(),
            TOP_PHONE => match self.pilot {
                Pilot::None => "No phone paired".into(),
                Pilot::Paired => "Phone remote paired, not connected".into(),
                Pilot::Connected => "Phone remote connected".into(),
            },
            _ => "Sleep".into(),
        }
    }

    /// Start the entrance cascade at `now` (the screen fades in from the intro).
    pub fn start_entrance(&mut self, now: Instant) {
        self.entrance = Some(now);
    }

    /// Entrance progress of an element that starts `delay` ms in and takes `dur` ms.
    fn ent(&self, now: Instant, delay: f32, dur: f32) -> f32 {
        match self.entrance {
            None => 1.0,
            Some(t0) => ease_out((now.saturating_duration_since(t0).as_secs_f32() * 1000.0 - delay) / dur),
        }
    }

    fn entrance_active(&self, now: Instant) -> bool {
        self.entrance.is_some_and(|t0| now < t0 + Duration::from_millis(ENTRANCE_MS))
    }

    fn card_geom(&self, si: usize, ci: usize, now: Instant) -> CardGeom {
        let s = self.s;
        let sy = self.scroll_y.value(now) * SECTION_PITCH;
        let sx = self.sections[si].first.value(now) * CARD_PITCH;
        let x = MARGIN_X + ci as f32 * CARD_PITCH - sx;
        let vis = ((x - MARGIN_X) / CARD_PITCH).max(0.0);
        let ent = self.ent(now, ENT_CARD_DELAY + si as f32 * ENT_ROW_STAGGER + vis * ENT_CARD_STAGGER, ENT_CARD_MS);
        let away = self.away.value(now);
        let top = HEADER_BASE + si as f32 * SECTION_PITCH - sy + CARD_DY + ENT_CARD_RISE * (1.0 - ent) + AWAY_DY * away;
        let t = self.sections[si].cards[ci].focus.value(now);
        // Content fades out as it scrolls up into the top bar (and behind Settings).
        let a = ((top - 60.0) / 80.0).clamp(0.0, 1.0) * (ent * 1.5).min(1.0) * (1.0 - away);
        let k = 1.0 + FOCUS_SCALE * t;
        let mut r = RectF::new(x * s, top * s, CARD_W * s, CARD_H * s).scaled_about_center(k);
        if (t == 0.0 || t == 1.0) && ent >= 1.0 && (away == 0.0 || away == 1.0) {
            r = r.snapped();
        }
        CardGeom { r, t, k, draw_k: k * s / self.art.raster, a, x, top, ent }
    }

    /// The focused card's ring: (outer rectangle, outer corner radius) in physical px.
    pub fn focus_ring(&self, now: Instant) -> (RectF, f32) {
        let g = self.card_geom(self.row, self.sections[self.row].col, now);
        (g.r.expand(RING_GAP * self.s * g.k), (CARD_R + RING_GAP) * self.s * g.k)
    }

    pub fn animating(&self, now: Instant) -> bool {
        self.scroll_y.active(now)
            || self.blur.active(now)
            || self.away.active(now)
            || self.top_focus.iter().any(|t| t.active(now))
            || self.entrance_active(now)
            || self.sections.iter().any(|s| s.first.active(now) || s.cards.iter().any(|c| c.focus.active(now)))
    }

    /// Settings opens over the home screen (`on`) or closes: the cards fade and slide out,
    /// and the wallpaper recedes like it does below the first row.
    pub fn set_away(&mut self, on: bool, now: Instant) {
        let dur = Duration::from_millis(if on { 280 } else { 240 });
        self.away.go(if on { 1.0 } else { 0.0 }, now, dur);
        let depth = if on || self.row > 0 { 1.0 } else { 0.0 };
        self.blur.go(depth, now, Duration::from_millis(SCROLL_Y_MS + 100));
    }

    /// `set_away` without the animation (an app takes the screen).
    pub fn set_away_now(&mut self, on: bool) {
        self.away = Tween::fixed(if on { 1.0 } else { 0.0 });
        self.blur = Tween::fixed(if on || self.row > 0 { 1.0 } else { 0.0 });
    }

    pub fn away(&self, now: Instant) -> f32 {
        self.away.value(now)
    }

    fn focus_top(&mut self, i: Option<usize>, now: Instant) {
        let d = Duration::from_millis(FOCUS_MS);
        if let Some(old) = self.top {
            self.top_focus[old].go(0.0, now, d);
        }
        let (row, col) = (self.row, self.sections[self.row].col);
        match i {
            Some(n) => {
                self.top_focus[n].go(1.0, now, d);
                self.top_last = n;
                if self.top.is_none() {
                    self.sections[row].cards[col].focus.go(0.0, now, d);
                }
            }
            None => self.sections[row].cards[col].focus.go(1.0, now, d),
        }
        self.top = i;
    }

    /// What Enter on top-bar button `i` does.
    fn top_action(&mut self, i: usize, now: Instant) -> Option<Action> {
        use crate::setview::Cat;
        let _ = now;
        match i {
            TOP_SETTINGS => Some(Action::Settings(None, false)),
            TOP_VOLUME => Some(Action::Volume),
            // Wi-Fi and the phone remote live on the same page (pairing is there).
            TOP_WIFI | TOP_PHONE => Some(Action::Settings(Some(Cat::Wifi), true)),
            _ => Some(Action::Sleep),
        }
    }

    pub fn key(&mut self, k: Key, now: Instant) -> Option<Action> {
        self.touched = true;
        if let Some(i) = self.top {
            match k {
                Key::Left if i > 0 => self.focus_top(Some(i - 1), now),
                Key::Right if i + 1 < TOP_ICONS.len() => self.focus_top(Some(i + 1), now),
                Key::Down | Key::Back => self.focus_top(None, now),
                Key::Enter => return self.top_action(i, now),
                _ => {}
            }
            return None;
        }
        let (row, col) = (self.row, self.sections[self.row].col);
        let (mut nrow, mut ncol) = (row, col);
        match k {
            Key::Left => ncol = col.saturating_sub(1),
            Key::Right => ncol = (col + 1).min(self.sections[row].cards.len() - 1),
            Key::Up if row == 0 => {
                // Up from the first row goes to the top bar.
                self.focus_top(Some(self.top_last), now);
                return None;
            }
            Key::Up => nrow = row - 1,
            Key::Down => nrow = (row + 1).min(self.sections.len() - 1),
            Key::Enter => {
                let c = &self.sections[row].cards[col];
                if let Some(id) = &c.app_id {
                    return Some(Action::Launch(id.clone()));
                }
                if self.sections[row].kind == SecKind::Settings {
                    return crate::setview::Cat::ALL.get(col).map(|c| Action::Settings(Some(*c), true));
                }
                return None;
            }
            Key::Back | Key::Home => return None,
        }
        if nrow != row {
            ncol = self.sections[nrow].col.min(self.sections[nrow].cards.len() - 1);
        }
        if (nrow, ncol) == (row, col) {
            return None;
        }
        self.move_focus(nrow, ncol, now, false);
        None
    }

    /// Move the card focus to (nrow, ncol): focus tweens, row scroll, section scroll and the
    /// wallpaper depth. `from_top`: no card had the focus (it was in the top bar).
    fn move_focus(&mut self, nrow: usize, ncol: usize, now: Instant, from_top: bool) {
        let (row, col) = (self.row, self.sections[self.row].col);
        let focus = Duration::from_millis(FOCUS_MS);
        if !from_top {
            self.sections[row].cards[col].focus.go(0.0, now, focus);
        }
        self.sections[nrow].cards[ncol].focus.go(1.0, now, focus);
        self.sections[nrow].col = ncol;
        self.row = nrow;
        // Scroll the row only when the focus leaves the fully visible slots.
        let sec = &mut self.sections[nrow];
        let first = sec.first.target() as usize;
        let max_first = sec.cards.len().saturating_sub(FULLY_VISIBLE);
        let nfirst = if ncol < first { ncol } else if ncol >= first + FULLY_VISIBLE { ncol + 1 - FULLY_VISIBLE } else { first };
        sec.first.go(nfirst.min(max_first) as f32, now, Duration::from_millis(SCROLL_X_MS));
        self.scroll_y.go(nrow as f32, now, Duration::from_millis(SCROLL_Y_MS));
        // Like Bigscreen: the wallpaper is crisp behind the top row and recedes (blurs and
        // darkens) once the focus goes down into the other rows. With the wallpaper on its own
        // display plane the dissolve runs with the scroll (a quarter-size blend per frame);
        // GPU-composited, it starts when the scroll has landed (a full-screen cross-fade under
        // moving cards measured ~20 ms GPU per frame on the Mali-400).
        let target = if nrow == 0 { 0.0 } else { 1.0 };
        if nrow != row {
            let delay = if self.blur_with_scroll { 0 } else { SCROLL_Y_MS };
            let dur = if self.blur_with_scroll { SCROLL_Y_MS + 100 } else { WALL_DEPTH_MS };
            self.blur.go_at(target, now, now + Duration::from_millis(delay), Duration::from_millis(dur));
        }
    }

    pub fn wall_blur(&self, now: Instant) -> f32 {
        self.blur.value(now)
    }

    pub fn slices(&self) -> Slices {
        Slices { card: self.art.card, ring: self.art.ring, shadow: self.art.shadow, raster: self.art.raster }
    }

    fn badge(&mut self, gpu: &Gpu, atlas: &mut Atlas, si: usize, ci: usize) -> Option<Region> {
        let d = (BADGE_D * self.art.raster).round() as u32;
        if self.sections[si].cards[ci].icon.is_none() {
            // The app's other card (Recent / Applications) may have the badge already.
            self.share_icons();
        }
        let c = &mut self.sections[si].cards[ci];
        if c.icon.is_none() {
            c.icon = Some(atlas.insert(gpu, &art::app_icon(c.glyph, d, c.grad.0, c.grad.1, c.badge)));
            self.icons_rendered += 1;
        }
        c.icon.unwrap()
    }

    /// Idle-time prefetch: render one missing badge or one card's text glyphs (nearest rows
    /// first). Returns false when everything is cached. Keeps navigation free of CPU
    /// rasterisation spikes while still loading lazily (nothing before the first frame
    /// that is not on screen).
    pub fn prefetch_one(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text) -> bool {
        let s = self.s;
        let mut order: Vec<usize> = (0..self.sections.len()).collect();
        order.sort_by_key(|&si| (si as i64 - self.row as i64).abs());
        for si in order {
            for ci in 0..self.sections[si].cards.len() {
                if self.sections[si].cards[ci].icon.is_none() {
                    self.badge(gpu, atlas, si, ci);
                    return true;
                }
                if !self.sections[si].cards[ci].text_warm {
                    // The fitting too (text::Fits), so the first frame with this card only looks it up.
                    let c = &self.sections[si].cards[ci];
                    let room = CARD_W * s;
                    let mut scratch = Vec::new();
                    let title = self.fits.line(text, Face::Regular, TITLE_PX * s, &c.title, room);
                    text.draw(gpu, atlas, &mut scratch, Face::Regular, TITLE_PX * s, 0.0, 0.0, [0; 4], title);
                    for line in self.fits.lines(text, Face::Regular, SUB_PX * s, &c.subtitle, room, SUB_LINES) {
                        text.draw(gpu, atlas, &mut scratch, Face::Regular, SUB_PX * s, 0.0, 0.0, [0; 4], line);
                    }
                    self.sections[si].cards[ci].text_warm = true;
                    return true;
                }
            }
            if !self.sections[si].title_warm {
                let mut scratch = Vec::new();
                text.draw(gpu, atlas, &mut scratch, Face::Light, 48.0 * s, 0.0, 0.0, [0; 4], self.sections[si].title);
                self.sections[si].title_warm = true;
                return true;
            }
        }
        if !self.digits_warm {
            // Clock digits for every future minute.
            let mut scratch = Vec::new();
            text.draw(gpu, atlas, &mut scratch, Face::Light, 50.0 * s, 0.0, 0.0, [0; 4], "0123456789:APM");
            self.digits_warm = true;
            return true;
        }
        false
    }

    /// Build the full item list for time `now`.
    pub fn build(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, clock: &str, date: &str) -> Frame {
        let s = self.s;
        let mut f = Frame::default();

        // Top bar.
        let e = self.ent(now, ENT_TOP_DELAY, ENT_TOP_MS);
        let dy = -16.0 * s * (1.0 - e);
        let st = f.begin();
        let cw = text.draw(gpu, atlas, &mut f.verts, Face::Light, 50.0 * s, MARGIN_X * s, (CLOCK_BASE * s + dy).round(), rgba(0xffffff, e), clock);
        f.end(1, st);
        let e = self.ent(now, ENT_TOP_DELAY + 70.0, ENT_TOP_MS);
        let dy = -16.0 * s * (1.0 - e);
        let st = f.begin();
        text.draw(gpu, atlas, &mut f.verts, Face::Regular, 36.0 * s, (MARGIN_X * s + cw + 30.0 * s).round(), (CLOCK_BASE * s + dy).round(), rgba(0xffffff, 0.86 * e), date);
        f.end(2, st);
        // Behind Settings the focused button stays marked, dimmer ("you came from here").
        let away = self.away.value(now);
        let mark = 1.0 - 0.6 * away;
        for i in 0..self.art.icons.len() {
            let Some(r) = self.art.icons[i] else { continue };
            // The speaker shows the volume as it is; the phone whether a remote is paired.
            let (r, dim) = match i {
                TOP_VOLUME => {
                    let kind = self.volume.map_or(meow_app::osd::Speaker::High, |(v, m)| meow_app::osd::Speaker::of(v.min(100) as u8, m));
                    (self.art.speakers.get(kind.waves()).copied().flatten().unwrap_or(r), 1.0)
                }
                TOP_PHONE if self.pilot == Pilot::None => (self.art.phone_off.unwrap_or(r), 0.6),
                _ => (r, 1.0),
            };
            let e = self.ent(now, ENT_TOP_DELAY + 40.0 * (TOP_N - 1 - i) as f32, ENT_TOP_MS - 100.0);
            if e <= 0.0 {
                continue;
            }
            let cx = (ICON_LAST_CX - ICON_PITCH * (TOP_N - 1 - i) as f32) * s;
            let ft = self.top_focus[i].value(now);
            if ft > 0.004 {
                // Focus: a Breeze-blue disc grows in behind the icon, the tooltip rises below.
                if let Some(d) = self.art.disc {
                    let ds = d.w as f32 * (0.82 + 0.18 * ft);
                    let (x0, y0) = (cx - ds / 2.0, ICON_CY * s - ds / 2.0);
                    let (x0, y0) = if ft >= 1.0 { (x0.round(), y0.round()) } else { (x0, y0) };
                    let st = f.begin();
                    let uv = atlas.uv(&d, 0.0, 0.0, d.w as f32, d.h as f32);
                    quad(&mut f.verts, x0, y0, x0 + ds, y0 + ds, uv, rgba(FOCUS_COLOR, ft * e * mark));
                    f.end(20 + i as u32, st);
                }
                let label = self.top_label(i);
                let px = TOP_LABEL_PX * s;
                let tw = text.width(Face::Regular, px, &label);
                let tx = (cx - tw / 2.0).min(1900.0 * s - tw).round();
                let ty = ((ICON_CY + 70.0 - 8.0 * (1.0 - ft)) * s).round();
                // The volume pop-up opens right here: no tooltip under it.
                if !self.volume_popup {
                    let st = f.begin();
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, px, tx, ty, rgba(0xffffff, 0.9 * ft * e * (1.0 - away)), &label);
                    f.end(30 + i as u32, st);
                }
            }
            let st = f.begin();
            let (x0, y0) = ((cx - r.w as f32 / 2.0).round(), (ICON_CY * s - r.h as f32 / 2.0 - 16.0 * s * (1.0 - e)).round());
            let uv = atlas.uv(&r, 0.0, 0.0, r.w as f32, r.h as f32);
            quad(&mut f.verts, x0, y0, x0 + r.w as f32, y0 + r.h as f32, uv, rgba(0xffffff, 0.95 * e * dim));
            if let (TOP_PHONE, Pilot::Connected, Some(d)) = (i, self.pilot, self.art.status_dot) {
                // Connected now: a green dot on the phone's corner, on a dark rim so it
                // reads on any wallpaper and on the focus disc.
                let (dx, dy) = ((cx + 13.0 * s).round(), y0 + (r.h as f32 * 0.08).round());
                let uv = atlas.uv(&d, 0.0, 0.0, d.w as f32, d.h as f32);
                let (half, rim) = (d.w as f32 / 2.0, d.w as f32 / 2.0 + (2.5 * s).round());
                quad(&mut f.verts, dx - rim, dy - rim, dx + rim, dy + rim, uv, rgba(0x10131c, 0.92 * e));
                quad(&mut f.verts, dx - half, dy - half, dx + half, dy + half, uv, rgba(STATUS_OK, e));
            }
            f.end(10 + i as u32, st);
        }
        if away >= 1.0 {
            return f; // Settings covers the rows: nothing below the top bar is visible
        }

        // Sections.
        let sy = self.scroll_y.value(now) * SECTION_PITCH;
        let fade = |top: f32| ((top - 60.0) / 80.0).clamp(0.0, 1.0);
        for si in 0..self.sections.len() {
            let header = HEADER_BASE + si as f32 * SECTION_PITCH - sy + AWAY_DY * away;
            if header + CARD_DY > 1080.0 + 20.0 {
                break;
            }
            // Section headers fade out before reaching the clock row (their cards may stay).
            let e = self.ent(now, ENT_CARD_DELAY - 50.0 + si as f32 * ENT_ROW_STAGGER, ENT_CARD_MS);
            let a = ((header - AWAY_DY * away - 40.0 - 150.0) / 60.0).clamp(0.0, 1.0) * e * (1.0 - away);
            if a > 0.0 {
                let st = f.begin();
                let hy = header + 28.0 * (1.0 - e);
                text.draw(gpu, atlas, &mut f.verts, Face::Light, 48.0 * s, MARGIN_X * s, (hy * s).round(), rgba(0xffffff, a), self.sections[si].title);
                f.end(1000 + si as u32, st);
            }
            for ci in 0..self.sections[si].cards.len() {
                let g = self.card_geom(si, ci, now);
                // One card of look-ahead on each side (badges render lazily).
                if g.x > 1920.0 + CARD_PITCH || g.x + CARD_W < -CARD_PITCH {
                    continue;
                }
                let on_screen = g.x < 1920.0 && g.x + CARD_W > 0.0 && g.top < 1080.0;
                let badge = self.badge(gpu, atlas, si, ci);
                if !on_screen || g.a <= 0.0 {
                    continue;
                }
                let (r, t, k, draw_k, a) = (g.r, g.t, g.k, g.draw_k, g.a);
                let crisp = (t == 0.0 || t == 1.0) && g.ent >= 1.0;
                let st = f.begin();
                let (shr, shm, shpad) = self.art.shadow;
                let sh = RectF::new(r.x, r.y + 6.0 * s * k, r.w, r.h).expand(shpad as f32 * draw_k);
                nine_ex(&mut f.verts, atlas, &shr, shm, sh, shm as f32 * draw_k, rgba(0x000000, 0.55 * a), false);
                let (cr, cm) = self.art.card;
                let tint = self.sections[si].cards[ci].tint;
                nine(&mut f.verts, atlas, &cr, cm, r, cm as f32 * draw_k, rgba(tint, a));
                if let Some(b) = badge {
                    let (bw, bh) = (b.w as f32 * draw_k, b.h as f32 * draw_k);
                    let (bx, by) = (r.x + (r.w - bw) / 2.0, r.y + (r.h - bh) / 2.0);
                    let (bx, by) = if crisp { (bx.round(), by.round()) } else { (bx, by) };
                    let uv = atlas.uv(&b, 0.0, 0.0, b.w as f32, b.h as f32);
                    quad(&mut f.verts, bx, by, bx + bw, by + bh, uv, rgba(0xffffff, a));
                }
                let focused = si == self.row && ci == self.sections[si].col;
                if t > 0.004 && !(focused && self.ring_hidden) {
                    let (rr, rm) = self.art.ring;
                    let ring = r.expand(RING_GAP * s * k);
                    nine(&mut f.verts, atlas, &rr, rm, ring, rm as f32 * draw_k, rgba(FOCUS_COLOR, t * a));
                }
                f.end(100_000 + (si * 1000 + ci * 2) as u32, st);

                let te = self.ent(now, ENT_CARD_DELAY + 60.0 + si as f32 * ENT_ROW_STAGGER + ((g.x - MARGIN_X) / CARD_PITCH).max(0.0) * ENT_CARD_STAGGER, ENT_CARD_MS);
                let ty = g.top - ENT_CARD_RISE * (1.0 - g.ent) + ENT_CARD_RISE * 0.6 * (1.0 - te) + CARD_H + 42.0 + 11.0 * t;
                let ta = fade(ty - AWAY_DY * away - 30.0) * te * (1.0 - away);
                if ta > 0.0 {
                    let st = f.begin();
                    let (tx, ty) = ((g.x * s).round(), (ty * s).round());
                    let c = &self.sections[si].cards[ci];
                    let room = CARD_W * s;
                    let title = self.fits.line(text, Face::Regular, TITLE_PX * s, &c.title, room);
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, TITLE_PX * s, tx, ty, rgba(0xffffff, ta), title);
                    for (i, line) in self.fits.lines(text, Face::Regular, SUB_PX * s, &c.subtitle, room, SUB_LINES).iter().enumerate() {
                        let ly = ty + ((SUB_DY + i as f32 * SUB_PITCH) * s).round();
                        text.draw(gpu, atlas, &mut f.verts, Face::Regular, SUB_PX * s, tx, ly, rgba(0xffffff, 0.66 * ta), line);
                    }
                    f.end(100_001 + (si * 1000 + ci * 2) as u32, st);
                }
            }
        }
        f
    }

    /// Idle-time preparation of the launch splash for the focused app card (2x icon, glow
    /// and dot sprites, name and status glyphs), so pressing Enter never rasterises.
    /// Returns true if it did any work.
    pub fn prewarm_launch(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text) -> bool {
        let s = self.s;
        let sec = &self.sections[self.row];
        let Some(c) = sec.cards.get(sec.col) else { return false };
        let Some(id) = c.app_id.clone() else { return false };
        if self.big_badges.contains_key(&id) && self.fx.is_some() {
            return false;
        }
        let (glyph, grad, shape, title) = (c.glyph, c.grad, c.badge, c.title.clone());
        if self.fx.is_none() {
            let glow = atlas.insert(gpu, &art::glow(64));
            let dot = atlas.insert(gpu, &art::dot((8.0 * s).round().max(4.0) as u32));
            self.fx = Some((glow, dot));
        }
        let d = (SPLASH_BADGE * s).round() as u32;
        self.big_badges.insert(id, atlas.insert(gpu, &art::app_icon(glyph, d, grad.0, grad.1, shape)));
        let mut scratch = Vec::new();
        let name = self.fits.line(text, Face::Light, 46.0 * s, &title, SPLASH_TEXT_W * s);
        text.draw(gpu, atlas, &mut scratch, Face::Light, 46.0 * s, 0.0, 0.0, [0; 4], name);
        text.draw(gpu, atlas, &mut scratch, Face::Regular, 24.0 * s, 0.0, 0.0, [0; 4], "Downloading 0123456789% Starting\u{2026}");
        true
    }

    pub fn card_visual(&self, id: &str, now: Instant) -> Option<CardVisual> {
        // An app can have two cards (Recent and Applications): the focused one first.
        let mut order: Vec<usize> = (0..self.sections.len()).collect();
        order.sort_by_key(|&si| si != self.row);
        for si in order {
            let sec = &self.sections[si];
            let mut cols: Vec<usize> = (0..sec.cards.len()).collect();
            cols.sort_by_key(|&ci| ci != sec.col);
            for ci in cols {
                let c = &sec.cards[ci];
                if c.app_id.as_deref() != Some(id) {
                    continue;
                }
                let g = self.card_geom(si, ci, now);
                let badge_w = c.icon.flatten().map_or(BADGE_D * self.s * g.k * 1.16, |b| b.w as f32 * g.draw_k);
                return Some(CardVisual {
                    rect: g.r,
                    badge_w,
                    corner: self.art.card.1 as f32 * g.draw_k,
                    tint: c.tint,
                    glyph: c.glyph,
                    grad: c.grad,
                    shape: c.badge,
                    title: c.title.clone(),
                });
            }
        }
        None
    }

    /// Draw the launch layer (item ids from launch::ID_BASE) and return the area it covers
    /// opaquely, so the caller can skip what lies fully behind it.
    #[allow(clippy::too_many_arguments)]
    pub fn build_launch(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, l: &mut crate::launch::Launch, w: u32, h: u32, f: &mut Frame) -> Rect {
        use crate::launch::ID_BASE;
        let s = self.s;
        if self.fx.is_none() {
            let glow = atlas.insert(gpu, &art::glow(64));
            let dot = atlas.insert(gpu, &art::dot((8.0 * s).round().max(4.0) as u32));
            self.fx = Some((glow, dot));
        }
        let (glow, dot) = self.fx.unwrap();
        let big = *self.big_badges.entry(l.id.clone()).or_insert_with(|| {
            let d = (SPLASH_BADGE * s).round() as u32;
            atlas.insert(gpu, &art::app_icon(l.glyph, d, l.grad.0, l.grad.1, l.shape))
        });
        let (fw, fh) = (w as f32, h as f32);
        let e = l.expansion(now);
        let lerp = |a: f32, b: f32| a + (b - a) * e;
        let panel = RectF::new(lerp(l.card.x, 0.0), lerp(l.card.y, 0.0), lerp(l.card.w, fw), lerp(l.card.h, fh));
        let corner = l.card_corner * (1.0 - e);
        let (cr, cm) = self.art.card;
        let st = f.begin();
        nine(&mut f.verts, atlas, &cr, cm, panel, corner, rgba(l.tint, 1.0));
        f.end(ID_BASE, st);

        let ca = l.content_alpha(now);
        let (cx, cy) = (lerp(l.card.x + l.card.w / 2.0, fw / 2.0), lerp(l.card.y + l.card.h / 2.0, SPLASH_CY * s));
        if ca > 0.0 {
            if let Some(g) = glow {
                // Only at the end of the zoom (a 1000 px blended quad would cost half a
                // screen pass on every zoom frame); then static.
                let gs = 1000.0 * s;
                // Not while closing: at low alpha over the dark tint it shows 8-bit banding rings.
                let closing = l.phase == crate::launch::Phase::Closing;
                let a = if closing { 0.0 } else { 0.32 * ca * ((e - 0.8) / 0.2).clamp(0.0, 1.0) };
                if a > 0.004 {
                    let st = f.begin();
                    let uv = atlas.uv(&g, 0.0, 0.0, g.w as f32, g.h as f32);
                    quad(&mut f.verts, cx - gs / 2.0, cy - gs / 2.0, cx + gs / 2.0, cy + gs / 2.0, uv, rgba(l.grad.0, a));
                    f.end(ID_BASE + 1, st);
                }
            }
            let mut icon_bottom = cy + l.card_badge_w / 2.0;
            if let Some(b) = big {
                let bw = lerp(l.card_badge_w, b.w as f32);
                let k = bw / b.w as f32;
                let bh = b.h as f32 * k;
                let (x0, y0) = (cx - bw / 2.0, cy - bh / 2.0);
                let (x0, y0) = if e >= 1.0 { (x0.round(), y0.round()) } else { (x0, y0) };
                let st = f.begin();
                let uv = atlas.uv(&b, 0.0, 0.0, b.w as f32, b.h as f32);
                quad(&mut f.verts, x0, y0, x0 + bw, y0 + bh, uv, rgba(0xffffff, ca));
                f.end(ID_BASE + 2, st);
                icon_bottom = cy + bh / 2.0;
            }
            // The name rises in during the second half of the zoom.
            let na = ca * ((e - 0.5) / 0.5).clamp(0.0, 1.0);
            if na > 0.0 {
                let px = 46.0 * s;
                let name = self.fits.line(text, Face::Light, px, &l.name, SPLASH_TEXT_W * s);
                let tw = text.width(Face::Light, px, name);
                // Anchored to the icon, so it travels with it (opening and closing).
                let y = icon_bottom + (62.0 * e + 18.0 * (1.0 - na)) * s;
                let st = f.begin();
                text.draw(gpu, atlas, &mut f.verts, Face::Light, px, (cx - tw / 2.0).round(), y.round(), rgba(0xffffff, na), name);
                f.end(ID_BASE + 3, st);
            }
        }
        let sa = l.status_alpha(now);
        if sa > 0.0 {
            let y = (SPLASH_CY + SPLASH_BADGE * 0.58 + 140.0) * s;
            if !l.status.is_empty() {
                let px = 24.0 * s;
                let status = self.fits.line(text, Face::Regular, px, &l.status, SPLASH_TEXT_W * s);
                let tw = text.width(Face::Regular, px, status);
                let st = f.begin();
                text.draw(gpu, atlas, &mut f.verts, Face::Regular, px, ((fw - tw) / 2.0).round(), y.round(), rgba(0xffffff, 0.72 * sa), status);
                f.end(ID_BASE + 4, st);
            }
            let by = y + 34.0 * s;
            match l.progress {
                Some(p) => {
                    let (bw, bh) = ((360.0 * s).round(), (6.0 * s).round().max(2.0));
                    let bx = ((fw - bw) / 2.0).round();
                    let st = f.begin();
                    nine(&mut f.verts, atlas, &cr, cm, RectF::new(bx, by, bw, bh), bh / 2.0, rgba(0xffffff, 0.18 * sa));
                    let fill = (bw * p.clamp(0.0, 1.0)).max(bh);
                    nine(&mut f.verts, atlas, &cr, cm, RectF::new(bx, by, fill, bh), bh / 2.0, rgba(FOCUS_COLOR, sa));
                    f.end(ID_BASE + 5, st);
                }
                None => {
                    if let Some(d) = dot {
                        // 10 dots on a ring; a bright head runs round once per second.
                        let (r, ds) = (20.0 * s, d.w as f32);
                        let (scx, scy) = (fw / 2.0, by + r + ds);
                        let head = l.spin(now) * 10.0;
                        let st = f.begin();
                        let uv = atlas.uv(&d, 0.0, 0.0, d.w as f32, d.h as f32);
                        for i in 0..10 {
                            let a = i as f32 / 10.0 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                            let lag = (head - i as f32).rem_euclid(10.0) / 10.0;
                            let al = sa * (0.18 + 0.82 * (1.0 - lag).powi(3));
                            let (x, y) = ((scx + r * a.cos() - ds / 2.0).round(), (scy + r * a.sin() - ds / 2.0).round());
                            quad(&mut f.verts, x, y, x + ds, y + ds, uv, rgba(0xffffff, al));
                        }
                        f.end(ID_BASE + 6, st);
                    }
                }
            }
        }
        if l.phase == crate::launch::Phase::Fading && ca <= 0.0 {
            l.faded_frame_built = true;
        }
        // Opaque core of the panel (inside its rounded corners).
        let inset = corner.ceil() + 1.0;
        Rect::new((panel.x + inset).ceil() as i32, (panel.y + inset).ceil() as i32, (panel.x + panel.w - inset).floor() as i32, (panel.y + panel.h - inset).floor() as i32)
    }

    /// Overlays above the home screen: the install consent dialog and a short notice.
    /// Logical layout in 1920x1080 units like the rest; item ids 900_000+.
    #[allow(clippy::too_many_arguments)]
    pub fn build_overlay(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, consent: Option<&Consent>, notice: Option<&(String, Instant)>, f: &mut Frame) {
        let s = self.s;
        let (cr, cm) = self.art.card;
        let (rr, rm) = self.art.ring;
        let dk = s / self.art.raster;
        if let Some(n) = notice {
            let age = now.saturating_duration_since(n.1).as_secs_f32();
            let a = (age / 0.2).min(1.0) * ((NOTICE_SECS - age) / 0.4).clamp(0.0, 1.0);
            if a > 0.0 {
                // One line: a long one (an app's error) is cut at the screen margins.
                let msg = self.fits.line(text, Face::Regular, 26.0 * s, &n.0, (1920.0 - 2.0 * MARGIN_X - 80.0) * s);
                let tw = text.width(Face::Regular, 26.0 * s, msg) / s;
                let (w, h) = (tw + 80.0, 76.0);
                let (x, y) = ((1920.0 - w) / 2.0, 1080.0 - 150.0);
                let st = f.begin();
                nine(&mut f.verts, atlas, &cr, cm, RectF::new(x * s, y * s, w * s, h * s), cm as f32 * dk, rgba(0x2c3245, 0.96 * a));
                text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, ((x + 40.0) * s).round(), ((y + 48.0) * s).round(), rgba(0xffffff, a), msg);
                f.end(950_000, st);
            }
        }
        let Some(d) = consent else { return };
        let a = (now.saturating_duration_since(d.shown_at).as_secs_f32() / 0.15).min(1.0);
        let st = f.begin();
        nine(&mut f.verts, atlas, &cr, cm, RectF::new(-20.0 * s, -20.0 * s, 1960.0 * s, 1120.0 * s), cm as f32 * dk, rgba(0x05060c, 0.72 * a));
        f.end(900_000, st);
        // Everything stays inside the panel: the name and the permissions on one line each
        // (cut with an ellipsis), the lead and each app-written reason on up to two.
        let w = 1080.0;
        let (room, perm_room) = ((w - 112.0) * s, (w - 112.0 - 30.0) * s);
        let lead = if d.update { "The new version would also like to (you can change this in Settings > Apps):" } else { "It will run in its own sandbox and be able to:" };
        let lead_extra = (self.fits.lines(text, Face::Regular, 26.0 * s, lead, room, 2).len() - 1) as f32 * 36.0;
        let mut lines = lead_extra;
        for p in &d.perms {
            lines += 50.0;
            if !p.reason.is_empty() {
                lines += 34.0 * self.fits.lines(text, Face::Regular, 22.0 * s, &p.reason, perm_room, 2).len() as f32;
            }
        }
        let h = 56.0 + 60.0 + 50.0 + lines + 40.0 + 80.0 + 56.0;
        let (x, y) = ((1920.0 - w) / 2.0, ((1080.0 - h) / 2.0).round());
        let rise = 24.0 * (1.0 - a);
        let st = f.begin();
        let (shr, shm, shpad) = self.art.shadow;
        let panel = RectF::new(x * s, (y + rise) * s, w * s, h * s);
        nine_ex(&mut f.verts, atlas, &shr, shm, panel.expand(shpad as f32 * dk), shm as f32 * dk, rgba(0x000000, 0.6 * a), false);
        nine(&mut f.verts, atlas, &cr, cm, panel, cm as f32 * dk, rgba(0x1d2233, a));
        let (x0, mut by) = ((x + 56.0) * s, y + rise + 56.0 + 44.0);
        let sensitive = d.sensitive();
        let title = if d.update { format!("{} was updated", d.name) } else { format!("Install {}?", d.name) };
        let title = self.fits.line(text, Face::Light, 46.0 * s, &title, room);
        text.draw(gpu, atlas, &mut f.verts, Face::Light, 46.0 * s, x0.round(), (by * s).round(), rgba(0xffffff, a), title);
        by += 56.0;
        for (i, line) in self.fits.lines(text, Face::Regular, 26.0 * s, lead, room, 2).iter().enumerate() {
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, x0.round(), ((by + i as f32 * 36.0) * s).round(), rgba(0xffffff, 0.62 * a), line);
        }
        by += 58.0 + lead_extra;
        for p in &d.perms {
            let dot = if p.sensitive { FOCUS_COLOR } else { 0x7d848b };
            nine(&mut f.verts, atlas, &cr, cm, RectF::new(x0, (by - 17.0) * s, 12.0 * s, 12.0 * s), 2.0 * s, rgba(dot, a));
            let mut label = p.label.clone();
            if p.sensitive && !p.required {
                label.push_str("  (optional)");
            }
            let label = self.fits.line(text, Face::Regular, 28.0 * s, &label, perm_room);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 28.0 * s, (x0 + 30.0 * s).round(), (by * s).round(), rgba(0xffffff, a), label);
            if !p.reason.is_empty() {
                for line in self.fits.lines(text, Face::Regular, 22.0 * s, &p.reason, perm_room, 2) {
                    by += 34.0;
                    text.draw(gpu, atlas, &mut f.verts, Face::Regular, 22.0 * s, (x0 + 30.0 * s).round(), (by * s).round(), rgba(0xffffff, 0.55 * a), line);
                }
            }
            by += 50.0;
        }
        f.end(900_001, st);
        let labels = if sensitive { ["Allow", "Don't allow"] } else { ["Install", "Cancel"] };
        let (bw, bh) = (300.0, 80.0);
        for (i, l) in labels.iter().enumerate() {
            let bx = x + w - 56.0 - (2 - i) as f32 * bw - (1 - i) as f32 * 24.0;
            let byy = y + rise + h - 56.0 - bh;
            let r = RectF::new(bx * s, byy * s, bw * s, bh * s);
            let focused = d.focus == i;
            let st = f.begin();
            nine(&mut f.verts, atlas, &cr, cm, r, cm as f32 * dk, rgba(if focused { FOCUS_COLOR } else { 0x2c3245 }, a));
            if focused {
                nine(&mut f.verts, atlas, &rr, rm, r.expand(RING_GAP * s), rm as f32 * dk, rgba(0xffffff, 0.85 * a));
            }
            let tw = text.width(Face::Medium, 28.0 * s, l);
            text.draw(gpu, atlas, &mut f.verts, Face::Medium, 28.0 * s, (r.x + (r.w - tw) / 2.0).round(), (r.y + 50.0 * s).round(), rgba(0xffffff, a), l);
            f.end(900_010 + i as u32, st);
        }
    }

    pub fn overlay_animating(&self, now: Instant, consent: Option<&Consent>, notice: Option<&(String, Instant)>) -> bool {
        consent.is_some_and(|d| now < d.shown_at + Duration::from_millis(160))
            || notice.is_some_and(|n| now < n.1 + Duration::from_secs_f32(NOTICE_SECS + 0.1))
    }
}

#[cfg(test)]
mod tests {
    use super::{Pilot, clip, quad};
    use crate::util::RectF;

    #[test]
    fn clip_cuts_quads_and_their_texture_coordinates() {
        let mut v = Vec::new();
        quad(&mut v, 0.0, 0.0, 10.0, 10.0, [0.0, 0.0, 1.0, 1.0], [1; 4]); // before `start`: untouched
        quad(&mut v, 0.0, 0.0, 10.0, 10.0, [0.0, 0.0, 1.0, 1.0], [2; 4]); // outside: removed
        quad(&mut v, 30.0, 30.0, 40.0, 40.0, [0.0, 0.0, 1.0, 1.0], [3; 4]); // inside: as it is
        quad(&mut v, 10.0, 40.0, 30.0, 60.0, [0.2, 0.4, 0.6, 0.8], [4; 4]); // across a corner: cut
        clip(&mut v, 4, RectF::new(20.0, 20.0, 30.0, 30.0));
        assert_eq!(v.len(), 12);
        assert_eq!((v[0].rgba, v[3].x), ([1; 4], 10.0));
        assert_eq!((v[4].rgba, v[4].x, v[7].y, v[7].u), ([3; 4], 30.0, 40.0, 1.0));
        let (a, d) = (v[8], v[11]);
        assert_eq!((a.rgba, a.x, a.y, d.x, d.y), ([4; 4], 20.0, 40.0, 30.0, 50.0));
        let near = |x: f32, y: f32| (x - y).abs() < 1e-6;
        assert!(near(a.u, 0.4) && near(a.v, 0.4) && near(d.u, 0.6) && near(d.v, 0.6), "{a:?} {d:?}");
        assert_eq!((v[9].x, v[9].y, v[10].x, v[10].y), (30.0, 40.0, 20.0, 50.0));
    }

    #[test]
    fn the_phone_icon_has_three_states() {
        assert_eq!(Pilot::of(0, 0, ""), Pilot::None);
        assert_eq!(Pilot::of(0, 1, "1"), Pilot::None, "nothing paired: nothing can be connected");
        assert_eq!(Pilot::of(2, 0, ""), Pilot::Paired);
        assert_eq!(Pilot::of(2, 0, "0"), Pilot::Paired);
        assert_eq!(Pilot::of(1, 1, ""), Pilot::Connected, "meow-netd's online count");
        assert_eq!(Pilot::of(1, 0, "1"), Pilot::Connected, "or its pilot_connected flag");
    }
}
