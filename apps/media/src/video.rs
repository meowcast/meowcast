//! The display: decoded pictures on the display controller's primary plane (VOP win0), the UI
//! on the overlay plane above it (win1, blended per pixel).
//!
//! Pictures go straight from the decoder's buffers: each NV12 dmabuf becomes a KMS framebuffer
//! once, and showing one is an atomic commit that swaps the plane's framebuffer at the next
//! vertical blank. The VOP converts YUV to RGB, crops the coded size to the visible size and
//! scales to the screen; the CPU and GPU never touch a picture.
//!
//! The UI is drawn by the CPU into one of two full-screen ARGB buffers and published as a
//! request. While a video plays, the player's presenter is the only one that commits: it adds
//! the UI change to its next commit and slides the playback bar in and out by moving the
//! plane, which costs nothing. Without a video, the UI's own thread commits.
//!
//! The mode on the CRTC is left as it is (no TV re-sync). Needs DRM master: on the shell, stop
//! the launcher first (`ui-session stop`); as an app, the brokered display fd will be used.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use drm::buffer::{DrmFourcc, DrmModifier, PlanarBuffer};
use drm::control::atomic::AtomicModeReq;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{AtomicCommitFlags, Device as _, FbCmd2Flags, connector, crtc, framebuffer, plane, property};

use crate::canvas::Canvas;

struct Card(File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

const PLANE_PROPERTIES: [&str; 10] = ["FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H"];
type PlaneProperties = [property::Handle; 10];
/// How long the playback bar takes to slide in or out.
const SLIDE: Duration = Duration::from_millis(280);

struct Nv12 {
    handle: drm::buffer::Handle,
    width: u32,
    height: u32,
    stride: u32,
}

impl PlanarBuffer for Nv12 {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Nv12
    }
    fn modifier(&self) -> Option<DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.stride, self.stride, 0, 0]
    }
    fn handles(&self) -> [Option<drm::buffer::Handle>; 4] {
        [Some(self.handle), Some(self.handle), None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        // One memory plane: chroma follows the luma rows.
        [0, self.stride * self.height, 0, 0]
    }
}

/// A rectangle of screen pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The part of both, or an empty rectangle.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        let (right, bottom) = ((self.x + self.width).min(other.x + other.width), (self.y + self.height).min(other.y + other.height));
        Rect { x, y, width: right.saturating_sub(x), height: bottom.saturating_sub(y) }
    }

    /// The smallest rectangle around both (an empty one counts for nothing).
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        let (right, bottom) = ((self.x + self.width).max(other.x + other.width), (self.y + self.height).max(other.y + other.height));
        Rect { x, y, width: right - x, height: bottom - y }
    }
}

/// The screen edge a part of the UI slides in from (and out to). The plane is moved and
/// cropped by the display controller: a slide costs no drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// It appears and goes at once.
    None,
    Bottom,
    Left,
    Right,
}

/// Where the UI is on the screen and how it comes and goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// The part of the screen the UI has something in (the plane covers only this).
    pub area: Rect,
    pub from: Edge,
    /// How far it travels, in pixels (the width of a side panel, the height of a bar).
    pub travel: u32,
    pub duration: Duration,
}

impl Placement {
    /// As `Display::publish` places things: rows from `top` down, sliding up from the bottom
    /// edge; a full-screen view (`top` 0) just appears.
    fn rows_from(top: u32, width: u32, height: u32) -> Placement {
        Placement { area: Rect::new(0, top, width, height.saturating_sub(top)), from: if top == 0 { Edge::None } else { Edge::Bottom }, travel: height.saturating_sub(top), duration: SLIDE }
    }

    fn moves_like(&self, other: &Placement) -> bool {
        self.from == other.from && self.travel == other.travel
    }

    /// The source rectangle and where its corner goes, `fraction` (0 to 1) of the way in;
    /// None while nothing of it shows.
    fn shown(&self, fraction: f32) -> Option<(Rect, u32, u32)> {
        let area = self.area;
        if area.is_empty() {
            return None;
        }
        // Whole even pixels, so the plane's position and size stay valid for the hardware.
        let hidden = (self.travel as f32 * (1.0 - fraction.clamp(0.0, 1.0))) as u32 & !1;
        match self.from {
            Edge::None => (fraction > 0.0).then_some((area, area.x, area.y)),
            Edge::Bottom => (hidden + 2 <= area.height).then(|| (Rect::new(area.x, area.y, area.width, area.height - hidden), area.x, area.y + hidden)),
            // The panel's far part first: it comes out of the edge.
            Edge::Left => (hidden + 16 <= area.width).then(|| (Rect::new(area.x + hidden, area.y, area.width - hidden, area.height), area.x, area.y)),
            Edge::Right => (hidden + 16 <= area.width).then(|| (Rect::new(area.x, area.y, area.width - hidden, area.height), area.x + hidden, area.y)),
        }
    }
}

/// What the UI wants on the overlay plane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiRequest {
    /// Which of the two UI buffers holds it.
    pub buffer: usize,
    /// The UI occupies the rows from here down; 0 is a full-screen view.
    pub top: u32,
    /// False: slide the bar away (a full-screen view just goes).
    pub visible: bool,
    /// When `visible` last changed: the slide starts here.
    pub since: Instant,
    pub generation: u64,
    pub placement: Placement,
    /// Published by an app that calls `Display::animate` while nothing plays: slides are
    /// shown then too. Otherwise the UI is simply there (or gone) without a player.
    pub animated: bool,
}

impl UiRequest {
    /// How far in the UI is at `now` (0 to 1), eased, and whether it is still moving.
    fn progress(&self, now: Instant) -> (f32, bool) {
        let moved = match self.placement.from {
            Edge::None => 1.0,
            _ => (now.saturating_duration_since(self.since).as_secs_f32() / self.placement.duration.as_secs_f32().max(0.001)).min(1.0),
        };
        let eased = 1.0 - (1.0 - moved).powi(3);
        (if self.visible { eased } else { 1.0 - eased }, moved < 1.0)
    }
}

/// The overlay plane as last committed.
/// See `Display::reached`.
#[derive(Default)]
struct Reached {
    /// The newest request generation whose commit has been shown.
    shown: u64,
    /// The generation in the commit that is queued for the next vertical blank.
    queued: u64,
    /// The UI is in the middle of an animation until then: the presenter waits a little for
    /// its next frame before it commits a picture.
    busy_until: Option<Instant>,
    /// When the newest request was published, and how long after that it was committed and
    /// reached the screen (for `Pacing`).
    published: Option<Instant>,
    committed_after: Option<Duration>,
    shown_after: Option<Duration>,
}

/// Where the time of one `publish_painted` went, and how the frame before it fared.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pacing {
    /// Waiting for the frame before to be on screen, then painting this one.
    pub wait: Duration,
    pub paint: Duration,
    /// The frame before: from its publishing to its commit, and to the screen.
    pub to_commit: Duration,
    pub to_screen: Duration,
    /// Pixels written to the display's buffer for this frame.
    pub pixels: usize,
}

#[derive(Clone, Copy, PartialEq)]
struct Overlay {
    buffer: usize,
    /// The part of the buffer shown and where on the screen its corner goes; None: plane off.
    shown: Option<(Rect, u32, u32)>,
}

impl Overlay {
    /// A plane that is off is off whichever buffer it would show: committing "off" again
    /// for another buffer would be a commit that touches no CRTC, which the kernel refuses
    /// when a flip event is asked for (EINVAL: seen when a panel began to slide in at a
    /// vertical blank without a new picture).
    fn of(buffer: usize, shown: Option<(Rect, u32, u32)>) -> Overlay {
        Overlay { buffer: if shown.is_some() { buffer } else { 0 }, shown }
    }
}

/// One of the UI buffers, mapped, for `Display::publish_painted`'s painter.
pub struct Frame<'a> {
    bytes: &'a mut [u8],
    stride: usize,
    pub width: usize,
    pub height: usize,
    /// Which of the two buffers this is: a painter that keeps track of what each holds
    /// (to write only what differs) tells them apart by it.
    pub buffer: usize,
    /// Pixels handed out by `row` so far: what the painter wrote.
    pub written: usize,
}

impl<'a> Frame<'a> {
    /// A frame over memory of the caller's (tests, pictures of the screen): rows of `stride`
    /// bytes.
    pub fn over(bytes: &'a mut [u8], stride: usize, width: usize, height: usize, buffer: usize) -> Frame<'a> {
        Frame { bytes, stride, width, height, buffer, written: 0 }
    }

    /// The pixels `from..to` of row `y` (premultiplied 0xAARRGGBB). Write-only in spirit:
    /// reading this memory back is slow.
    pub fn row(&mut self, y: usize, from: usize, to: usize) -> &mut [u32] {
        let (from, to) = (from.min(self.width), to.min(self.width));
        self.written += to.saturating_sub(from);
        let bytes = &mut self.bytes[y * self.stride + from * 4..y * self.stride + to.max(from) * 4];
        // SAFETY: the mapping is page-aligned and rows are whole pixels, so the bytes are
        // aligned u32s; the target is little-endian ARGB8888.
        unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<u32>(), bytes.len() / 4) }
    }

    /// Copy `rect` from a canvas of the screen's size.
    pub fn copy(&mut self, canvas: &Canvas, rect: Rect) {
        let (x0, x1) = (rect.x as usize, ((rect.x + rect.width) as usize).min(canvas.width));
        for y in rect.y as usize..((rect.y + rect.height) as usize).min(canvas.height) {
            self.row(y, x0, x1).copy_from_slice(&canvas.pixels[y * canvas.width + x0..y * canvas.width + x1]);
        }
    }
}

/// What a UI buffer still lacks of the newest picture.
#[derive(Clone, Default)]
struct Stale {
    /// Nothing in it can be relied on.
    whole: bool,
    parts: Vec<Rect>,
}

impl Stale {
    /// One more part; parts that overlap become one (a highlight on its way damages nearly
    /// the same rows frame after frame: they are painted once).
    fn add(&mut self, mut part: Rect) {
        while let Some(at) = self.parts.iter().position(|other| !other.intersect(&part).is_empty()) {
            part = part.union(&self.parts.swap_remove(at));
        }
        self.parts.push(part);
        // Many small parts: one rectangle around them is as good.
        if self.parts.len() > 12 {
            let around = self.parts.iter().fold(Rect::default(), |all, part| all.union(part));
            self.parts = vec![around];
        }
    }
}

pub struct Screen {
    card: Arc<Card>,
    crtc: crtc::Handle,
    primary: (plane::Handle, PlaneProperties),
    overlay: (plane::Handle, PlaneProperties),
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    ui_framebuffers: [framebuffer::Handle; 2],
    /// The attached decoder's pictures.
    pictures: Vec<(framebuffer::Handle, drm::buffer::Handle)>,
    /// Which of them is on the plane.
    current: Option<usize>,
    /// The last picture of a playback that ended for a seek: it stays on screen (its memory
    /// outlives the decoder that made it) until the next playback shows its first picture.
    retired: Option<(framebuffer::Handle, drm::buffer::Handle)>,
    /// Visible part of a picture, and where it goes on the screen (x, y, width, height).
    source: (u32, u32),
    target: (u32, u32, u32, u32),
    committed: Option<Overlay>,
    picture_shown: bool,
}

/// The display as the app's threads share it.
pub struct Display {
    card: Arc<Card>,
    pub screen: Mutex<Screen>,
    ui_buffers: Mutex<[DumbBuffer; 2]>,
    request: Mutex<Option<UiRequest>>,
    /// The generation of the UI request last put on screen.
    committed: AtomicU64,
    /// The generation that has reached the screen (its vertical blank has passed), and a
    /// wake-up for whoever waits for that or for a new request.
    reached: Mutex<Reached>,
    turn: Condvar,
    /// What each UI buffer lacks (see `publish_painted`).
    stale: Mutex<[Stale; 2]>,
    /// The part of the screen `publish_painted` keeps up to date in the buffers (the
    /// placement's area: the plane shows nothing else).
    kept_area: Mutex<Rect>,
    /// The UI buffers, mapped once for `publish_painted` (address, length).
    mapped: Mutex<[Option<(usize, usize)>; 2]>,
    /// See `Pacing`: of the last `publish_painted`.
    pub pacing: Mutex<Pacing>,
    /// A debugging switch: false makes `hold_for_ui` return at once.
    pub hold: AtomicBool,
    /// The display controller refused a plane cropped at its side (a panel half-way in from
    /// the left or right): such panels appear at once from then on instead of sliding.
    no_side_slides: AtomicBool,
    /// A player is attached and commits; the UI thread must not.
    playing: AtomicBool,
    pub width: u32,
    pub height: u32,
}

impl Display {
    /// `card`: the KMS fd the system brokered to the app; None opens the card directly (a
    /// shell test with the launcher stopped).
    pub fn open(card: Option<std::os::fd::OwnedFd>) -> Result<Display, String> {
        let file = match card {
            Some(fd) => File::from(fd),
            None => OpenOptions::new().read(true).write(true).open("/dev/dri/card0").map_err(|e| format!("/dev/dri/card0: {e}"))?,
        };
        let card = Arc::new(Card(file));
        let capability = |c, name: &str| drm::Device::set_client_capability(&*card, c, true).map_err(|e| format!("{name}: {e}"));
        capability(drm::ClientCapability::UniversalPlanes, "universal planes")?;
        capability(drm::ClientCapability::Atomic, "atomic mode setting")?;
        let resources = card.resource_handles().map_err(|e| format!("KMS resources: {e}"))?;
        let connector = resources
            .connectors()
            .iter()
            .filter_map(|h| card.get_connector(*h, false).ok())
            .find(|c| c.state() == connector::State::Connected)
            .ok_or("no connected display")?;
        let crtc = connector
            .current_encoder()
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc())
            .ok_or("the display has no active CRTC")?;
        let mode = card.get_crtc(crtc).ok().and_then(|c| c.mode()).ok_or("the CRTC has no mode")?;

        let (mut primary, mut overlay) = (None, None);
        for handle in card.plane_handles().map_err(|e| format!("planes: {e}"))? {
            let Ok(info) = card.get_plane(handle) else { continue };
            if !resources.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
                continue;
            }
            let values = card.get_properties(handle).map_err(|e| format!("plane properties: {e}"))?;
            let named = values.as_hashmap(&*card).map_err(|e| format!("plane properties: {e}"))?;
            let kind = named.get("type").and_then(|t| values.iter().find(|(id, _)| **id == t.handle()).map(|(_, v)| *v));
            let ids: Option<Vec<property::Handle>> = PLANE_PROPERTIES.iter().map(|n| named.get(*n).map(|i| i.handle())).collect();
            let Some(Ok(ids)) = ids.map(PlaneProperties::try_from) else { continue };
            let takes = |format: DrmFourcc| info.formats().contains(&(format as u32));
            // DRM plane type 1 is the primary plane; the VOP's second window registers as the
            // cursor plane but is a full window with per-pixel alpha.
            if kind == Some(1) && primary.is_none() && takes(DrmFourcc::Nv12) {
                primary = Some((handle, ids));
            } else if kind != Some(1) && overlay.is_none() && takes(DrmFourcc::Argb8888) {
                overlay = Some((handle, ids));
            }
        }
        let (width, height) = (mode.size().0 as u32, mode.size().1 as u32);
        let buffer = || card.create_dumb_buffer((width, height), DrmFourcc::Argb8888, 32).map_err(|e| format!("UI buffer: {e}"));
        let ui_buffers = [buffer()?, buffer()?];
        let framebuffer = |b: &DumbBuffer| card.add_framebuffer(b, 32, 32).map_err(|e| format!("UI framebuffer: {e}"));
        let ui_framebuffers = [framebuffer(&ui_buffers[0])?, framebuffer(&ui_buffers[1])?];
        let screen = Screen {
            card: card.clone(),
            crtc,
            primary: primary.ok_or("no primary plane that takes NV12")?,
            overlay: overlay.ok_or("no overlay plane with per-pixel alpha")?,
            width,
            height,
            refresh_hz: mode.vrefresh(),
            ui_framebuffers,
            pictures: Vec::new(),
            current: None,
            retired: None,
            source: (0, 0),
            target: (0, 0, 0, 0),
            committed: None,
            picture_shown: false,
        };
        Ok(Display { card, screen: Mutex::new(screen), ui_buffers: Mutex::new(ui_buffers), request: Mutex::new(None), committed: AtomicU64::new(0), reached: Mutex::new(Reached::default()), turn: Condvar::new(), stale: Mutex::new([Stale { whole: true, parts: Vec::new() }, Stale { whole: true, parts: Vec::new() }]), kept_area: Mutex::new(Rect::default()), mapped: Mutex::new([None, None]), pacing: Mutex::new(Pacing::default()), hold: AtomicBool::new(true), no_side_slides: AtomicBool::new(false), playing: AtomicBool::new(false), width, height })
    }

    /// The UI thread: copy the canvas's rows from `top` down into the UI buffer that is not on
    /// screen, and ask for it to be shown. Returns once it is safe to draw the next one.
    pub fn publish(&self, canvas: &Canvas, top: u32, visible: bool) -> Result<(), String> {
        let previous = *self.request.lock().unwrap();
        // The other buffer may still be scanned out until the previous request is on screen.
        if let Some(previous) = previous {
            let deadline = Instant::now() + Duration::from_millis(300);
            while self.committed.load(Ordering::Acquire) < previous.generation && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(4));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let buffer = previous.map_or(0, |p| 1 - p.buffer);
        {
            let mut buffers = self.ui_buffers.lock().unwrap();
            let mut mapping = self.card.map_dumb_buffer(&mut buffers[buffer]).map_err(|e| format!("map the UI buffer: {e}"))?;
            let width = canvas.width * 4;
            // SAFETY: plain u32 pixels viewed as bytes; the target is little-endian ARGB8888.
            let bytes = unsafe { std::slice::from_raw_parts(canvas.pixels.as_ptr().cast::<u8>(), canvas.pixels.len() * 4) };
            let stride = mapping.len() / canvas.height;
            for row in top as usize..canvas.height {
                mapping[row * stride..row * stride + width].copy_from_slice(&bytes[row * width..(row + 1) * width]);
            }
        }
        // (Should the app mix this with `publish_painted`: that starts from nothing again.)
        *self.stale.lock().unwrap() = [Stale { whole: true, parts: Vec::new() }, Stale { whole: true, parts: Vec::new() }];
        let since = match previous {
            Some(p) if p.visible == visible && p.top == top => p.since,
            _ => Instant::now(),
        };
        let placement = Placement::rows_from(top, self.width, self.height);
        let request = UiRequest { buffer, top, visible, since, generation: previous.map_or(1, |p| p.generation + 1), placement, animated: false };
        *self.request.lock().unwrap() = Some(request);
        self.turn.notify_all();
        self.commit_idle()
    }

    /// The UI thread, for a UI that animates: bring the buffer that is not on screen up to
    /// date and ask for it to be shown at the next vertical blank.
    ///
    /// The two buffers take turns, so each is two frames behind: `damage` (what changed since
    /// the last call) is remembered for both, and `paint` is called with every part the buffer
    /// in turn still lacks (the whole of the placement's area, the first times). It writes the
    /// pixels of the rectangle it is given; the rest of the buffer is as it left it two calls
    /// ago. Last it is called once with None, for a painter that knows by itself what else
    /// this buffer (`Frame::buffer`) lacks. Writing to these buffers is slow (about 100 MB a
    /// second on the RK3228): what is painted per frame decides the frame rate.
    ///
    /// Blocks until the frame before this one is on screen, so calling it in a loop draws one
    /// frame per vertical blank. `more`: another frame follows shortly (an animation), so a
    /// running player holds its next commit for it and both go up together.
    /// Without a player, call `animate` to show slides.
    pub fn publish_painted(&self, placement: Placement, damage: &[Rect], more: bool, paint: &mut dyn FnMut(&mut Frame, Option<Rect>)) -> Result<(), String> {
        let previous = *self.request.lock().unwrap();
        let began = Instant::now();
        if let Some(previous) = previous {
            self.wait_reached(previous.generation, Duration::from_millis(300));
        }
        let mut pacing = Pacing { wait: began.elapsed(), ..Pacing::default() };
        {
            let reached = self.reached.lock().unwrap();
            pacing.to_commit = reached.committed_after.unwrap_or_default();
            pacing.to_screen = reached.shown_after.unwrap_or_default();
        }
        let painting = Instant::now();
        let buffer = previous.map_or(0, |p| 1 - p.buffer);
        let screen = Rect::new(0, 0, self.width, self.height);
        let area = placement.area.intersect(&screen);
        let parts = {
            let mut stale = self.stale.lock().unwrap();
            // Outside the area nothing is kept up to date: when it grows, nothing is known.
            let mut kept = self.kept_area.lock().unwrap();
            if kept.union(&area) != *kept {
                stale.iter_mut().for_each(|side| *side = Stale { whole: true, parts: Vec::new() });
            }
            *kept = area;
            for side in stale.iter_mut().filter(|side| !side.whole) {
                for part in damage.iter().map(|part| part.intersect(&area)).filter(|part| !part.is_empty()) {
                    side.add(part);
                }
            }
            let mine = std::mem::take(&mut stale[buffer]);
            if mine.whole { vec![area] } else { mine.parts.iter().map(|part| part.intersect(&area)).filter(|part| !part.is_empty()).collect() }
        };
        {
            let mut buffers = self.ui_buffers.lock().unwrap();
            // Mapped once and kept (mapping and unmapping 8 MB per frame is not free).
            let kept = self.mapped.lock().unwrap()[buffer];
            let (address, length) = match kept {
                Some(kept) => kept,
                None => {
                    let mapping = self.card.map_dumb_buffer(&mut buffers[buffer]).map_err(|e| format!("map the UI buffer: {e}"))?;
                    let kept = (mapping.as_ptr() as usize, mapping.len());
                    std::mem::forget(mapping);
                    self.mapped.lock().unwrap()[buffer] = Some(kept);
                    kept
                }
            };
            // SAFETY: the mapping of this buffer, made above and never unmapped; only this
            // function writes through it, under the lock on the buffers.
            let bytes = unsafe { std::slice::from_raw_parts_mut(address as *mut u8, length) };
            let stride = length / self.height as usize;
            let mut frame = Frame { bytes, stride, width: self.width as usize, height: self.height as usize, buffer, written: 0 };
            for part in parts {
                paint(&mut frame, Some(part));
            }
            paint(&mut frame, None);
            pacing.pixels = frame.written;
        }
        pacing.paint = painting.elapsed();
        *self.pacing.lock().unwrap() = pacing;
        let since = match previous {
            Some(p) if p.visible && p.placement.moves_like(&placement) => p.since,
            _ => Instant::now(),
        };
        let request = UiRequest { buffer, top: placement.area.y, visible: true, since, generation: previous.map_or(1, |p| p.generation + 1), placement, animated: true };
        *self.request.lock().unwrap() = Some(request);
        {
            let mut reached = self.reached.lock().unwrap();
            reached.busy_until = more.then(|| Instant::now() + Duration::from_millis(60));
            (reached.published, reached.committed_after, reached.shown_after) = (Some(Instant::now()), None, None);
        }
        self.turn.notify_all();
        self.commit_idle()
    }

    /// Wait until request `generation` is on screen (or `limit` has passed).
    fn wait_reached(&self, generation: u64, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut reached = self.reached.lock().unwrap();
        while reached.shown < generation {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            reached = self.turn.wait_timeout(reached, left).unwrap().0;
        }
    }

    /// A slide of the UI is under way (see `animate`).
    pub fn sliding(&self) -> bool {
        self.request.lock().unwrap().is_some_and(|request| request.animated && request.progress(Instant::now()).1)
    }

    /// A slide of the UI is under way. Without a player this also moves it on: call it once
    /// per frame while it returns true (each call takes until the next vertical blank).
    pub fn animate(&self) -> Result<bool, String> {
        let Some(request) = *self.request.lock().unwrap() else { return Ok(false) };
        let moving = request.animated && request.progress(Instant::now()).1;
        if self.playing.load(Ordering::Acquire) {
            if moving {
                // The presenter moves it; come back after the next blank.
                self.wait_reached(u64::MAX, Duration::from_millis(16));
            }
            return Ok(moving);
        }
        let before = Instant::now();
        self.commit_idle()?;
        if moving && before.elapsed() < Duration::from_millis(2) {
            // Not a pixel further than at the last call: no commit, no vertical blank waited for.
            std::thread::sleep(Duration::from_millis(4));
        }
        Ok(moving)
    }

    /// The player's presenter, before it commits for the vertical blank at `blank`: while the
    /// UI is animating, give its next frame until shortly before then to arrive, so that
    /// picture and UI go up in the same commit (a commit made right after the last blank
    /// would leave the UI's frame to the blank after).
    /// `idle`: the presenter has no picture for this blank and would only sleep: it waits
    /// for a UI frame whether or not one was announced.
    pub fn hold_for_ui(&self, blank: Instant, idle: bool) {
        const MARGIN: Duration = Duration::from_millis(4);
        if !self.hold.load(Ordering::Relaxed) {
            return;
        }
        let mut reached = self.reached.lock().unwrap();
        loop {
            let now = Instant::now();
            let waiting = (idle || reached.busy_until.is_some_and(|until| now < until)) && self.request.lock().unwrap().is_some_and(|request| request.generation <= reached.queued);
            let left = blank.checked_sub(MARGIN).map_or(Duration::ZERO, |limit| limit.saturating_duration_since(now));
            if !waiting || left.is_zero() {
                return;
            }
            reached = self.turn.wait_timeout(reached, left).unwrap().0;
        }
    }

    /// Show or slide away what is already published (no redraw).
    pub fn set_visible(&self, visible: bool) -> Result<(), String> {
        {
            let mut request = self.request.lock().unwrap();
            match request.as_mut() {
                Some(r) if r.visible != visible => {
                    r.visible = visible;
                    r.since = Instant::now();
                    r.generation += 1;
                }
                _ => return Ok(()),
            }
        }
        self.turn.notify_all();
        self.commit_idle()
    }

    /// The part of the UI that shows `fraction` of the way in (see `Placement::shown`).
    fn part_shown(&self, request: &UiRequest, fraction: f32) -> Option<(Rect, u32, u32)> {
        let mut placement = request.placement;
        if matches!(placement.from, Edge::Left | Edge::Right) && self.no_side_slides.load(Ordering::Relaxed) {
            placement.from = Edge::None;
            // (Gone when the slide out begins, there when the slide in does.)
            return placement.shown(if request.visible { 1.0 } else { 0.0 });
        }
        placement.shown(fraction)
    }

    /// A commit was refused. If it showed a panel part of the way in from a side, that is
    /// taken as the reason: side slides are given up (true: try again, without).
    fn gave_up_side_slides(&self, request: &UiRequest, refused: &Overlay) -> bool {
        let partial = refused.shown.is_some_and(|(part, _, _)| part.width != request.placement.area.width);
        partial && matches!(request.placement.from, Edge::Left | Edge::Right) && !self.no_side_slides.swap(true, Ordering::Relaxed)
    }

    /// The request is on screen (a blocking commit returned, or nothing visible changed).
    fn mark_shown(&self, generation: u64) {
        let mut reached = self.reached.lock().unwrap();
        reached.queued = reached.queued.max(generation);
        reached.shown = reached.shown.max(generation);
        self.turn.notify_all();
    }

    /// Without a player attached nobody else commits: put the request on screen from here.
    fn commit_idle(&self) -> Result<(), String> {
        let mut screen = self.screen.lock().unwrap();
        if self.playing.load(Ordering::Acquire) {
            return Ok(());
        }
        let request = *self.request.lock().unwrap();
        if let Some(request) = request {
            // Nobody else would move a slide on: only an app that calls `animate` gets one.
            let fraction = if request.animated { request.progress(Instant::now()).0 } else if request.visible { 1.0 } else { 0.0 };
            let mut overlay = Overlay::of(request.buffer, self.part_shown(&request, fraction));
            if Some(overlay) != screen.committed {
                // (Blocks until the vertical blank that shows it.)
                if let Err(error) = screen.commit(None, Some(overlay), false) {
                    if !self.gave_up_side_slides(&request, &overlay) {
                        return Err(error);
                    }
                    overlay = Overlay::of(request.buffer, self.part_shown(&request, fraction));
                    screen.commit(None, Some(overlay), false)?;
                }
            }
            self.committed.store(request.generation, Ordering::Release);
            drop(screen);
            self.mark_shown(request.generation);
        }
        Ok(())
    }

    /// A player takes over committing: its decoder's pictures become framebuffers. `coded` is
    /// the buffer layout, `shown` the visible part, which is scaled to fit the screen, centred.
    pub fn attach(&self, dmabufs: &[BorrowedFd<'_>], coded: (u32, u32), stride: u32, shown: (u32, u32)) -> Result<(), String> {
        let mut screen = self.screen.lock().unwrap();
        for dmabuf in dmabufs {
            let handle = screen.card.prime_fd_to_buffer(*dmabuf).map_err(|e| format!("import a picture buffer: {e}"))?;
            let buffer = Nv12 { handle, width: coded.0, height: coded.1, stride };
            let framebuffer = screen.card.add_planar_framebuffer(&buffer, FbCmd2Flags::empty()).map_err(|e| format!("NV12 framebuffer: {e}"))?;
            screen.pictures.push((framebuffer, handle));
        }
        screen.source = shown;
        screen.target = fit(shown, (screen.width, screen.height));
        self.playing.store(true, Ordering::Release);
        Ok(())
    }

    /// The player lets go, and its decoder's buffers are free to be released. With
    /// `keep_picture` the picture on screen stays there (a seek: the next playback replaces
    /// it); otherwise the video plane goes dark. Whatever the UI shows stays.
    pub fn detach(&self, keep_picture: bool) {
        let mut screen = self.screen.lock().unwrap();
        screen.drop_retired();
        let kept = screen.current.take().filter(|_| keep_picture).map(|index| screen.pictures[index]);
        if kept.is_none() && screen.picture_shown {
            let mut request = AtomicModeReq::new();
            Screen::plane(&mut request, &screen.primary, [0; 10]);
            let _ = screen.card.atomic_commit(AtomicCommitFlags::empty(), request);
            screen.picture_shown = false;
        }
        for picture in std::mem::take(&mut screen.pictures) {
            if Some(picture) != kept {
                let _ = screen.card.destroy_framebuffer(picture.0);
                let _ = screen.card.close_buffer(picture.1);
            }
        }
        screen.retired = kept;
        self.playing.store(false, Ordering::Release);
        drop(screen);
        // A UI change that arrived while the player was going is still to be shown.
        let _ = self.commit_idle();
    }

    /// Let go of a picture kept across a seek (the playback that should have replaced it did
    /// not come, or a full-screen view covers it now).
    pub fn clear_kept_picture(&self) {
        let mut screen = self.screen.lock().unwrap();
        if screen.retired.is_some() {
            // Removing a framebuffer that a plane still shows turns that plane off.
            screen.drop_retired();
            screen.picture_shown = false;
        }
    }

    /// The presenter, once per vertical blank: queue picture `picture` (None: keep the one on
    /// screen) together with whatever the UI wants now. Returns whether anything was queued;
    /// if so, `wait_shown` must return before the next call.
    pub fn present(&self, picture: Option<usize>) -> Result<bool, String> {
        let mut screen = self.screen.lock().unwrap();
        let request = *self.request.lock().unwrap();
        // A bar or a panel slides: the visible part of it grows from its edge.
        let part = |request: &UiRequest| Overlay::of(request.buffer, self.part_shown(request, request.progress(Instant::now()).0));
        let mut overlay = request.as_ref().map(part);
        let changed = overlay.is_some() && overlay != screen.committed;
        if picture.is_none() && !changed {
            // Nothing new; a request that changes nothing visible still counts as shown.
            if let Some(request) = request {
                self.committed.store(request.generation, Ordering::Release);
                drop(screen);
                self.mark_shown(request.generation);
            }
            return Ok(false);
        }
        if let Err(error) = screen.commit(picture, overlay.filter(|_| changed), true) {
            match (&request, &overlay) {
                (Some(request), Some(refused)) if changed && self.gave_up_side_slides(request, refused) => {
                    overlay = Some(part(request));
                    screen.commit(picture, overlay, true)?;
                }
                _ => return Err(error),
            }
        }
        if picture.is_some() {
            // The plane has its new picture queued: the one kept across a seek can go.
            screen.drop_retired();
        }
        if let Some(request) = request {
            self.committed.store(request.generation, Ordering::Release);
            let mut reached = self.reached.lock().unwrap();
            if reached.queued < request.generation && reached.committed_after.is_none() {
                reached.committed_after = reached.published.map(|at| at.elapsed());
            }
            reached.queued = request.generation;
        }
        Ok(true)
    }

    /// Wait until the queued commit is on screen; returns when that was noticed.
    pub fn wait_shown(&self, timeout: Duration) -> Result<Instant, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("the display did not show a picture in time".to_string());
            }
            let mut poll = libc::pollfd { fd: self.card.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&mut poll, 1, left.as_millis().min(100) as i32) } > 0 {
                let now = Instant::now();
                for event in self.card.receive_events().map_err(|e| format!("display events: {e}"))? {
                    if let drm::control::Event::PageFlip(_) = event {
                        // What was queued is on screen: the UI may draw its next frame.
                        let mut reached = self.reached.lock().unwrap();
                        if reached.shown < reached.queued && reached.shown_after.is_none() {
                            reached.shown_after = reached.published.map(|at| at.elapsed());
                        }
                        reached.shown = reached.shown.max(reached.queued);
                        self.turn.notify_all();
                        return Ok(now);
                    }
                }
            }
        }
    }

    /// Turn both planes off (the app is leaving the screen to the launcher).
    pub fn blank(&self) {
        let screen = self.screen.lock().unwrap();
        let mut request = AtomicModeReq::new();
        Screen::plane(&mut request, &screen.primary, [0; 10]);
        Screen::plane(&mut request, &screen.overlay, [0; 10]);
        let _ = screen.card.atomic_commit(AtomicCommitFlags::empty(), request);
    }

    pub fn refresh_hz(&self) -> u32 {
        self.screen.lock().unwrap().refresh_hz
    }
}

impl Screen {
    fn drop_retired(&mut self) {
        if let Some((framebuffer, handle)) = self.retired.take() {
            let _ = self.card.destroy_framebuffer(framebuffer);
            let _ = self.card.close_buffer(handle);
        }
    }

    fn plane(request: &mut AtomicModeReq, plane: &(plane::Handle, PlaneProperties), values: [u64; 10]) {
        for (id, value) in plane.1.iter().zip(values) {
            request.add_raw_property(plane.0.into(), *id, value);
        }
    }

    /// One atomic commit: a new picture and/or a new overlay state. `flip`: do not block, and
    /// deliver an event at the vertical blank.
    fn commit(&mut self, picture: Option<usize>, overlay: Option<Overlay>, flip: bool) -> Result<(), String> {
        let mut request = AtomicModeReq::new();
        let crtc = u32::from(self.crtc) as u64;
        if let Some(index) = picture {
            let (x, y, width, height) = self.target;
            let framebuffer = u32::from(self.pictures[index].0) as u64;
            Screen::plane(&mut request, &self.primary, [framebuffer, crtc, 0, 0, (self.source.0 as u64) << 16, (self.source.1 as u64) << 16, x as u64, y as u64, width as u64, height as u64]);
        }
        if let Some(overlay) = overlay {
            let values = match overlay.shown {
                Some((part, x, y)) => {
                    let framebuffer = u32::from(self.ui_framebuffers[overlay.buffer]) as u64;
                    [framebuffer, crtc, (part.x as u64) << 16, (part.y as u64) << 16, (part.width as u64) << 16, (part.height as u64) << 16, x as u64, y as u64, part.width as u64, part.height as u64]
                }
                None => [0; 10],
            };
            Screen::plane(&mut request, &self.overlay, values);
        }
        let flags = if flip { AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT } else { AtomicCommitFlags::empty() };
        self.card.atomic_commit(flags, request).map_err(|e| format!("display commit (is another program the DRM master?): {e}"))?;
        if picture.is_some() {
            self.picture_shown = true;
            self.current = picture;
        }
        if overlay.is_some() {
            self.committed = overlay;
        }
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.drop_retired();
        // Leave both planes off for the next master.
        let mut request = AtomicModeReq::new();
        Screen::plane(&mut request, &self.primary, [0; 10]);
        Screen::plane(&mut request, &self.overlay, [0; 10]);
        let _ = self.card.atomic_commit(AtomicCommitFlags::empty(), request);
        for framebuffer in self.ui_framebuffers {
            let _ = self.card.destroy_framebuffer(framebuffer);
        }
    }
}

/// The largest rectangle of `shown`'s aspect ratio inside `screen`, centred, on even pixels.
fn fit(shown: (u32, u32), screen: (u32, u32)) -> (u32, u32, u32, u32) {
    let (sw, sh, dw, dh) = (shown.0.max(1) as u64, shown.1.max(1) as u64, screen.0 as u64, screen.1 as u64);
    let (width, height) = if sw * dh >= sh * dw { (dw, (dw * sh / sw).max(2)) } else { ((dh * sw / sh).max(2), dh) };
    let (width, height) = ((width & !1) as u32, (height & !1) as u32);
    (((screen.0 - width) / 2) & !1, ((screen.1 - height) / 2) & !1, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_slides_up_and_panels_slide_out_of_their_edges() {
        // The bar of `publish`: rows from 700 down, as before.
        let bar = Placement::rows_from(700, 1920, 1080);
        assert_eq!(bar.shown(1.0), Some((Rect::new(0, 700, 1920, 380), 0, 700)));
        assert_eq!(bar.shown(0.5), Some((Rect::new(0, 700, 1920, 190), 0, 890)));
        assert_eq!(bar.shown(0.0), None);
        // A full-screen view is there or not.
        let view = Placement::rows_from(0, 1920, 1080);
        assert_eq!((view.shown(1.0), view.shown(0.0)), (Some((Rect::new(0, 0, 1920, 1080), 0, 0)), None));
        // A panel on the left shows its right part first, at the screen's edge.
        let left = Placement { area: Rect::new(0, 0, 1320, 1080), from: Edge::Left, travel: 1320, duration: Duration::from_millis(200) };
        assert_eq!(left.shown(0.25), Some((Rect::new(990, 0, 330, 1080), 0, 0)));
        assert_eq!(left.shown(1.0), Some((Rect::new(0, 0, 1320, 1080), 0, 0)));
        assert_eq!(left.shown(0.0), None);
        // One on the right comes in from the right edge.
        let right = Placement { area: Rect::new(640, 0, 1280, 1080), from: Edge::Right, travel: 1280, duration: Duration::from_millis(200) };
        assert_eq!(right.shown(0.5), Some((Rect::new(640, 0, 640, 1080), 1280, 0)));
        assert_eq!(right.shown(1.0), Some((Rect::new(640, 0, 1280, 1080), 640, 0)));
    }

    #[test]
    fn a_slide_is_eased_and_runs_both_ways() {
        let start = Instant::now();
        let placement = Placement { area: Rect::new(0, 0, 100, 100), from: Edge::Left, travel: 100, duration: Duration::from_millis(200) };
        let mut request = UiRequest { buffer: 0, top: 0, visible: true, since: start, generation: 1, placement, animated: true };
        assert_eq!(request.progress(start), (0.0, true));
        let (half, moving) = request.progress(start + Duration::from_millis(100));
        assert!(moving && (half - 0.875).abs() < 0.01, "{half}");
        assert_eq!(request.progress(start + Duration::from_millis(250)), (1.0, false));
        request.visible = false;
        assert_eq!(request.progress(start + Duration::from_millis(250)), (0.0, false));
        request.placement.from = Edge::None;
        assert_eq!(request.progress(start), (0.0, false));
    }

    #[test]
    fn rectangles_meet_and_join() {
        let (a, b) = (Rect::new(0, 0, 100, 50), Rect::new(60, 40, 100, 50));
        assert_eq!(a.intersect(&b), Rect::new(60, 40, 40, 10));
        assert_eq!(a.union(&b), Rect::new(0, 0, 160, 90));
        assert!(a.intersect(&Rect::new(200, 0, 10, 10)).is_empty());
        assert_eq!(a.union(&Rect::default()), a);
        // Damage that overlaps is painted once.
        let mut stale = Stale::default();
        for part in [Rect::new(0, 100, 800, 92), Rect::new(0, 108, 800, 92), Rect::new(0, 400, 800, 10), Rect::new(0, 120, 800, 92)] {
            stale.add(part);
        }
        stale.parts.sort_by_key(|part| part.y);
        assert_eq!(stale.parts, [Rect::new(0, 100, 800, 112), Rect::new(0, 400, 800, 10)]);
    }

    #[test]
    fn pictures_fit_the_screen_keeping_their_shape() {
        assert_eq!(fit((1920, 1080), (1920, 1080)), (0, 0, 1920, 1080));
        assert_eq!(fit((1280, 720), (3840, 2160)), (0, 0, 3840, 2160));
        assert_eq!(fit((320, 240), (1920, 1080)), (240, 0, 1440, 1080)); // 4:3: bars left and right
        assert_eq!(fit((1920, 800), (1920, 1080)), (0, 140, 1920, 800)); // wide: bars above and below
        assert_eq!(fit((1080, 1920), (1920, 1080)), (656, 0, 606, 1080)); // portrait
    }
}
