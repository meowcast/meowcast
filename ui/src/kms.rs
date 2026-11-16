//! KMS output: named mode selection, GBM scanout surface, modeset, pipelined page flips and
//! CRTC restore on exit. Ported from os/gpu-test.c (legacy path, which works on the
//! RK3228 VOP), using the `drm` and `gbm` crates.

use drm::control::{
    AtomicCommitFlags, Device as _, Mode, ModeFlags, PageFlipFlags, atomic::AtomicModeReq, connector, crtc,
    framebuffer, plane, property,
};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

pub struct Card(std::fs::File);
impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// Named output modes: (name, width, height, refresh Hz). The UI is laid out in
/// 1920x1080 logical units; scale = height / 1080.
const NAMED: &[(&str, u16, u16, u32)] = &[
    ("1080p60", 1920, 1080, 60),
    ("4k30", 3840, 2160, 30),
    ("720p60", 1280, 720, 60),
];
const DEFAULT: &str = "1080p60";
/// Last resort if the TV lacks 1080p60 too; never the TV's preferred mode (4K60 on the
/// test TV gives no signal on this board).
const SAFE: &str = "720p60";

pub type Bo = gbm::BufferObject<framebuffer::Handle>;

pub struct Kms {
    pub gbm: gbm::Device<Card>,
    conn: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    saved: Option<crtc::Info>,
    #[allow(dead_code)]
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    modeset: bool,
    /// The console's mode was kept: the HDMI link has been up since the kernel set it.
    pub kept_mode: bool,
    pending: bool,
    shown: Option<Bo>,
    queued: Option<Bo>,
    pub flips: u64,
    /// Scanout format is RGB565 instead of XRGB8888.
    pub rgb565: bool,
    /// Just took the screen back from an app: try a page flip (no blocking SetCrtc) first.
    reclaimed: bool,
    /// Two-plane composition (see `enable_planes`): wallpaper on the primary plane (VOP win0,
    /// scaled up by the VOP), UI with per-pixel alpha on the overlay plane (win1).
    pub planes: Option<Planes>,
    wall_shown: Option<Bo>,
    wall_queued: Option<Bo>,
    /// Every framebuffer made for our scanout buffers (removed in `free_buffers`).
    fbs: Vec<framebuffer::Handle>,
    /// Small solid-colour buffer shown (scaled by the VOP) while an app starts: see `release`.
    solid: Option<(drm::control::dumbbuffer::DumbBuffer, framebuffer::Handle)>,
    /// The connector's DPMS property (None: the driver has none).
    dpms: Option<property::Handle>,
    /// Video output switched off by `set_output(false)` (sleep).
    off: bool,
}

/// Atomic plane state: handles and the property ids we set.
pub struct Planes {
    primary: plane::Handle,
    overlay: plane::Handle,
    /// Property ids, same names on both planes: FB_ID CRTC_ID SRC_X SRC_Y SRC_W SRC_H CRTC_X
    /// CRTC_Y CRTC_W CRTC_H.
    prim: [property::Handle; 10],
    over: [property::Handle; 10],
    /// The UI overlay is on screen (off while an app has the display).
    overlay_on: bool,
    pub info: String,
}

const PLANE_PROPS: [&str; 10] = ["FB_ID", "CRTC_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H"];

fn find_mode(conn: &connector::Info, w: u16, h: u16, hz: u32) -> Option<Mode> {
    conn.modes()
        .iter()
        .filter(|m| m.size() == (w, h) && m.vrefresh() == hz && !m.flags().contains(ModeFlags::INTERLACE))
        // Prefer the exact rate (148.5 MHz 60.00 Hz over 148.35 MHz 59.94 Hz).
        .max_by_key(|m| m.clock())
        .copied()
}

impl Kms {
    /// Open the KMS card with a connected connector and resolve `wanted` (e.g. "4k30").
    pub fn open(wanted: &str) -> Result<(Kms, String), String> {
        let dir = std::fs::read_dir("/dev/dri/").map_err(|e| format!("/dev/dri: {e}"))?;
        let mut paths: Vec<_> = dir.flatten().map(|e| e.path()).collect();
        paths.sort();
        let t = Instant::now();
        for path in paths {
            let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            let card = Card(file);
            // With the aspect-ratio cap, modes keep their CEA picture aspect (16:9), so the mode
            // we set compares equal to the console's: no full modeset, no HDMI re-sync.
            let _ = drm::Device::set_client_capability(&card, drm::ClientCapability::AspectRatio, true);
            let Ok(res) = card.resource_handles() else { continue };
            // The cached connector state is enough when it already offers the wanted mode
            // (kernel booted with the same video= mode). Otherwise force a probe (EDID read,
            // ~200 ms): after an HDMI hot-plug the cached list held only <=720p modes.
            let want = NAMED.iter().find(|n| n.0 == wanted).copied();
            let connected = |force: bool| {
                res.connectors().iter().find_map(|h| {
                    let c = card.get_connector(*h, force).ok()?;
                    (c.state() == connector::State::Connected).then_some(c)
                })
            };
            let cached = connected(false).filter(|c| want.is_some_and(|(_, w, h, hz)| find_mode(c, w, h, hz).is_some()));
            let cached_used = cached.is_some();
            let Some(conn) = cached.or_else(|| connected(true)) else {
                continue;
            };
            let conn_name = format!("{}-{}", conn.interface().as_str(), conn.interface_id());

            let mut notes = String::new();
            let mut chosen = None;
            let mut tried: Vec<&str> = Vec::new();
            for name in [wanted, DEFAULT, SAFE] {
                if tried.contains(&name) {
                    continue;
                }
                if !tried.is_empty() {
                    notes.push_str(&format!(" fallback={name}"));
                }
                tried.push(name);
                let Some(&(n, w, h, hz)) = NAMED.iter().find(|n| n.0 == name) else {
                    notes.push_str(&format!(" unknown={name}"));
                    continue;
                };
                match find_mode(&conn, w, h, hz) {
                    Some(m) => {
                        chosen = Some((n, m));
                        break;
                    }
                    None => notes.push_str(&format!(" missing={name}")),
                }
            }
            let Some((name, mode)) = chosen else {
                let modes: Vec<String> = conn
                    .modes()
                    .iter()
                    .map(|m| format!("{}x{}@{}", m.size().0, m.size().1, m.vrefresh()))
                    .collect();
                return Err(format!("UI_OUTPUT error: no usable mode on {conn_name}{notes}; modes: {}", modes.join(" ")));
            };

            let crtc = conn
                .current_encoder()
                .and_then(|e| card.get_encoder(e).ok())
                .and_then(|e| e.crtc())
                .or_else(|| {
                    conn.encoders()
                        .iter()
                        .filter_map(|e| card.get_encoder(*e).ok())
                        .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
                        .next()
                })
                .ok_or_else(|| format!("no CRTC for {conn_name}"))?;
            let saved = card.get_crtc(crtc).ok();
            // Keep the CRTC's current mode (the console's, set from the kernel's video=) when
            // it is the requested one: the EDID can list the same WxH@Hz twice (different
            // sync polarity), and any difference makes the commit a full modeset, which
            // blanks the TV for seconds (and with it the boot intro).
            let mut mode = mode;
            let mut notes = notes;
            let mut kept_mode = false;
            if let Some(cur) = saved.as_ref().and_then(|c| c.mode()) {
                if cur.size() == mode.size() && cur.vrefresh() == mode.vrefresh() && !cur.flags().contains(ModeFlags::INTERLACE) {
                    mode = cur;
                    kept_mode = true;
                    notes.push_str(" keep_mode=1");
                }
            }
            let (w, h) = mode.size();
            let refresh_hz = mode.clock() as f32 * 1000.0 / (mode.hsync().2 as f32 * mode.vsync().2 as f32);
            let probe_ms = t.elapsed().as_millis();
            let gbm = gbm::Device::new(card).map_err(|e| format!("GBM device: {e}"))?;
            let summary = format!(
                "UI_OUTPUT name={name} mode={w}x{h}@{refresh_hz:.2} scale={} connector={conn_name} dev={}{notes} probe={} probe_ms={probe_ms} gbm_ms={}",
                h as f32 / 1080.0,
                path.display(),
                if cached_used { "cached" } else { "forced" },
                t.elapsed().as_millis() - probe_ms
            );
            return Ok((
                Kms {
                    gbm,
                    conn: conn.handle(),
                    crtc,
                    mode,
                    saved,
                    name,
                    width: w as u32,
                    height: h as u32,
                    refresh_hz,
                    modeset: false,
                    kept_mode,
                    pending: false,
                    shown: None,
                    queued: None,
                    flips: 0,
                    rgb565: false,
                    reclaimed: false,
                    planes: None,
                    wall_shown: None,
                    wall_queued: None,
                    fbs: Vec::new(),
                    solid: None,
                    dpms: None,
                    off: false,
                },
                summary,
            ));
        }
        Err("UI_OUTPUT error: no connected DRM connector".into())
    }

    pub fn fd(&self) -> RawFd {
        self.gbm.as_fd().as_raw_fd()
    }

    fn framebuffer(&mut self, bo: &mut Bo) -> io::Result<framebuffer::Handle> {
        if let Some(fb) = bo.userdata() {
            return Ok(*fb);
        }
        // Linear (GBM_BO_USE_LINEAR) buffers: plain AddFB, XRGB8888 = depth 24 / 32 bpp,
        // RGB565 = depth 16 / 16 bpp.
        // ARGB8888 (plane mode, both planes) = depth 32: the overlay blends per pixel.
        let (depth, bpp) = if self.rgb565 { (16, 16) } else if self.planes.is_some() { (32, 32) } else { (24, 32) };
        let fb = self.gbm.add_framebuffer(&*bo, depth, bpp)?;
        bo.set_userdata(fb);
        self.fbs.push(fb);
        Ok(fb)
    }

    /// Present the frame just swapped into `surface`. Like gpu-test's pipelined loop, this
    /// only waits for the *previous* flip after the new frame has been rendered.
    pub fn present(&mut self, surface: &gbm::Surface<framebuffer::Handle>) -> io::Result<()> {
        let mut bo = unsafe { surface.lock_front_buffer() }
            .map_err(|e| io::Error::other(format!("lock_front_buffer: {e}")))?;
        let fb = self.framebuffer(&mut bo)?;
        if !self.modeset {
            self.gbm.set_crtc(self.crtc, Some(fb), (0, 0), &[self.conn], Some(self.mode))?;
            self.modeset = true;
            self.shown = Some(bo);
            self.flips += 1;
            return Ok(());
        }
        if self.pending {
            self.wait_flip(Duration::from_secs(3))?;
        }
        if let Err(e) = self.gbm.page_flip(self.crtc, fb, PageFlipFlags::EVENT, None) {
            if !self.reclaimed {
                return Err(e);
            }
            // The app left another format or mode on the CRTC: full SetCrtc instead.
            eprintln!("UI_KMS reclaim flip refused ({e}); SetCrtc");
            self.reclaimed = false;
            self.gbm.set_crtc(self.crtc, Some(fb), (0, 0), &[self.conn], Some(self.mode))?;
            self.shown = Some(bo);
            self.flips += 1;
            return Ok(());
        }
        self.reclaimed = false;
        self.pending = true;
        self.queued = Some(bo);
        Ok(())
    }

    /// Switch to two-plane composition: find the CRTC's primary plane and an overlay that
    /// takes ARGB8888 (on the RK3228 VOP: win0 and win1, both with a scaler; win1 is exposed
    /// as the "cursor" plane but is a full window with per-pixel premultiplied alpha).
    pub fn enable_planes(&mut self) -> Result<(), String> {
        use drm::ClientCapability;
        let dev = &self.gbm;
        drm::Device::set_client_capability(dev, ClientCapability::UniversalPlanes, true).map_err(|e| format!("universal planes: {e}"))?;
        drm::Device::set_client_capability(dev, ClientCapability::Atomic, true).map_err(|e| format!("atomic: {e}"))?;
        let res = dev.resource_handles().map_err(|e| e.to_string())?;
        let crtc_bit = res.crtcs().iter().position(|c| *c == self.crtc).ok_or("crtc index")?;
        let (mut primary, mut overlay) = (None, None);
        let mut info = String::new();
        for h in dev.plane_handles().map_err(|e| e.to_string())? {
            let Ok(p) = dev.get_plane(h) else { continue };
            if !res.filter_crtcs(p.possible_crtcs()).contains(&self.crtc) {
                continue;
            }
            let props = dev.get_properties(h).map_err(|e| e.to_string())?;
            let map = props.as_hashmap(dev).map_err(|e| e.to_string())?;
            let kind = map.get("type").and_then(|t| props.iter().find(|(id, _)| **id == t.handle()).map(|(_, v)| *v)).unwrap_or(99);
            let argb = p.formats().contains(&(drm::buffer::DrmFourcc::Argb8888 as u32));
            let ids: Option<Vec<property::Handle>> = PLANE_PROPS.iter().map(|n| map.get(*n).map(|i| i.handle())).collect();
            info.push_str(&format!(" plane{}:type={kind},argb={argb}", u32::from(h)));
            let Some(ids) = ids else { continue };
            let ids: [property::Handle; 10] = ids.try_into().unwrap();
            // DRM plane types: 0 overlay, 1 primary, 2 cursor.
            match kind {
                1 if primary.is_none() => primary = Some((h, ids)),
                0 | 2 if argb && overlay.is_none() => overlay = Some((h, ids)),
                _ => {}
            }
        }
        let _ = crtc_bit;
        let (Some((ph, pp)), Some((oh, op))) = (primary, overlay) else {
            return Err(format!("no primary + ARGB overlay plane for the CRTC:{info}"));
        };
        self.planes = Some(Planes { primary: ph, overlay: oh, prim: pp, over: op, overlay_on: false, info: info.trim().to_string() });
        Ok(())
    }

    fn plane_props(req: &mut AtomicModeReq, plane: plane::Handle, ids: &[property::Handle; 10], crtc: Option<crtc::Handle>, fb: Option<framebuffer::Handle>, src: (u32, u32), dst: (u32, u32)) {
        let v: [u64; 10] = match (crtc, fb) {
            (Some(c), Some(f)) => [
                u32::from(f) as u64, u32::from(c) as u64, 0, 0, (src.0 as u64) << 16, (src.1 as u64) << 16, 0, 0, dst.0 as u64, dst.1 as u64,
            ],
            _ => [0; 10],
        };
        for (id, val) in ids.iter().zip(v) {
            req.add_raw_property(plane.into(), *id, val);
        }
    }

    /// Plane mode present: one non-blocking atomic commit with the new UI frame (overlay)
    /// and/or the new wallpaper frame (primary, `wall_size` scaled to the mode). The first
    /// frame, and a reclaim the app left in another mode, go through SetCrtc first.
    pub fn present_planes(
        &mut self,
        ui: Option<&gbm::Surface<framebuffer::Handle>>,
        wall: Option<&gbm::Surface<framebuffer::Handle>>,
        wall_size: (u32, u32),
    ) -> io::Result<()> {
        let mut ui_bo = match ui {
            Some(s) => Some(unsafe { s.lock_front_buffer() }.map_err(|e| io::Error::other(format!("lock_front_buffer: {e}")))?),
            None => None,
        };
        let mut wall_bo = match wall {
            Some(s) => Some(unsafe { s.lock_front_buffer() }.map_err(|e| io::Error::other(format!("lock_front_buffer (wall): {e}")))?),
            None => None,
        };
        let ui_fb = match ui_bo.as_mut() { Some(b) => Some(self.framebuffer(b)?), None => None };
        let wall_fb = match wall_bo.as_mut() { Some(b) => Some(self.framebuffer(b)?), None => None };
        if self.pending {
            self.wait_flip(Duration::from_secs(3))?;
        }
        if !self.modeset {
            // First frame: make the CRTC ours (keeps the console's mode: no HDMI re-sync).
            let fb = ui_fb.or(wall_fb).ok_or_else(|| io::Error::other("first frame needs a buffer"))?;
            self.gbm.set_crtc(self.crtc, Some(fb), (0, 0), &[self.conn], Some(self.mode))?;
            self.modeset = true;
        }
        let (w, h) = (self.width, self.height);
        let p = self.planes.as_mut().unwrap();
        let mut req = AtomicModeReq::new();
        // The overlay needs the UI frame on screen; the primary needs a wallpaper frame.
        let cur_ui = ui_fb.or_else(|| self.shown.as_ref().and_then(|b| b.userdata().copied()));
        let cur_wall = wall_fb.or_else(|| self.wall_shown.as_ref().and_then(|b| b.userdata().copied()));
        if let Some(f) = ui_fb.or(if p.overlay_on { None } else { cur_ui }) {
            Self::plane_props(&mut req, p.overlay, &p.over, Some(self.crtc), Some(f), (w, h), (w, h));
            p.overlay_on = true;
        }
        if let Some(f) = wall_fb.or(if self.reclaimed { cur_wall } else { None }) {
            Self::plane_props(&mut req, p.primary, &p.prim, Some(self.crtc), Some(f), wall_size, (w, h));
        }
        let flags = AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT;
        if let Err(e) = self.gbm.atomic_commit(flags, req) {
            if !self.reclaimed {
                return Err(e);
            }
            // The app left another mode on the CRTC: SetCrtc, then planes from the next frame.
            eprintln!("UI_KMS reclaim commit refused ({e}); SetCrtc");
            self.reclaimed = false;
            if let Some(fb) = cur_ui {
                self.gbm.set_crtc(self.crtc, Some(fb), (0, 0), &[self.conn], Some(self.mode))?;
            }
            if let Some(p) = self.planes.as_mut() {
                p.overlay_on = false;
            }
            if ui_bo.is_some() {
                self.shown = ui_bo;
            }
            if wall_bo.is_some() {
                self.wall_shown = wall_bo;
            }
            self.flips += 1;
            return Ok(());
        }
        self.reclaimed = false;
        self.pending = true;
        self.queued = ui_bo;
        self.wall_queued = wall_bo;
        Ok(())
    }

    /// Plane mode: blocking commit that shows `fb` full-screen on the primary plane and turns
    /// the overlay off (before giving the display away, and before restoring the console).
    fn planes_to_primary(&mut self, fb: Option<framebuffer::Handle>, src: (u32, u32)) -> io::Result<()> {
        let (w, h) = (self.width, self.height);
        let Some(p) = self.planes.as_mut() else { return Ok(()) };
        let mut req = AtomicModeReq::new();
        Self::plane_props(&mut req, p.overlay, &p.over, None, None, (0, 0), (0, 0));
        if let Some(f) = fb {
            Self::plane_props(&mut req, p.primary, &p.prim, Some(self.crtc), Some(f), src, (w, h));
        }
        p.overlay_on = false;
        self.gbm.atomic_commit(AtomicCommitFlags::empty(), req)
    }

    /// Handle pending DRM events (call when the fd is readable).
    pub fn dispatch(&mut self) -> io::Result<()> {
        for ev in self.gbm.receive_events()? {
            if let drm::control::Event::PageFlip(_) = ev {
                self.pending = false;
                self.flips += 1;
                // Dropping the old front buffer returns it to the GBM surface.
                if let Some(b) = self.queued.take() {
                    self.shown = Some(b);
                }
                if let Some(b) = self.wall_queued.take() {
                    self.wall_shown = Some(b);
                }
            }
        }
        Ok(())
    }

    pub fn wait_flip(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while self.pending {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "page flip timeout"));
            }
            let mut p = libc::pollfd { fd: self.fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut p, 1, left.as_millis().min(200) as i32) };
            if n > 0 {
                self.dispatch()?;
            } else if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// The video output is off (`set_output(false)`): nothing may be presented.
    pub fn is_off(&self) -> bool {
        self.off
    }

    /// Sleep: switch the video output off or back on through the connector's DPMS property.
    /// Off disables the CRTC: the VOP stops scanning out and suspends, the HDMI PHY and its
    /// PLL power down (the TV reports "no signal"). That is most of the idle heat of the
    /// board. Mode, planes and framebuffers stay as they are, so "on" brings the same picture
    /// back (the TV needs a few seconds to lock on again). Needs DRM master.
    pub fn set_output(&mut self, on: bool) -> io::Result<()> {
        if on != self.off {
            return Ok(());
        }
        if !on && self.pending {
            let _ = self.wait_flip(Duration::from_secs(1));
        }
        let dpms = match self.dpms {
            Some(p) => p,
            None => {
                let props = self.gbm.get_properties(self.conn)?;
                let map = props.as_hashmap(&self.gbm)?;
                let p = map.get("DPMS").map(|i| i.handle()).ok_or_else(|| io::Error::other("connector has no DPMS property"))?;
                self.dpms = Some(p);
                p
            }
        };
        // DRM_MODE_DPMS_ON = 0, DRM_MODE_DPMS_OFF = 3.
        self.gbm.set_property(self.conn, dpms, if on { 0 } else { 3 })?;
        self.off = !on;
        Ok(())
    }

    /// Give the screen to an app (appd RELEASE_DISPLAY): finish the pending flip, drop DRM
    /// master. Our buffers stay allocated; nothing is drawn until `reacquire`.
    ///
    /// `solid` = the last UI frame is this plain colour (0xRRGGBB, the launch tint): show it
    /// from a 320x180 buffer scaled up by the VOP instead of from the full-size UI frame, so
    /// that none of our scanout buffers stays on screen and `free_buffers` can run (an app
    /// may draw only on the overlay plane and leave the primary plane as it finds it).
    pub fn release(&mut self, solid: Option<u32>) -> io::Result<()> {
        // An app only flips planes: it must find the output on.
        if let Err(e) = self.set_output(true) {
            eprintln!("UI_WARN output on before release: {e}");
        }
        if self.pending {
            let _ = self.wait_flip(Duration::from_secs(1));
        }
        if self.planes.is_some() {
            // The app sets up the primary plane only: the overlay must not cover it. The last
            // UI frame (the opaque launch tint) moves onto the primary plane meanwhile.
            let fb = self.shown.as_ref().and_then(|b| b.userdata().copied());
            let done = match solid.map(|rgb| self.solid_to_primary(rgb)) {
                Some(Ok(())) => true,
                Some(Err(e)) => {
                    eprintln!("UI_WARN solid hand-over frame: {e}");
                    false
                }
                None => false,
            };
            if !done {
                if let Err(e) = self.planes_to_primary(fb, (self.width, self.height)) {
                    eprintln!("UI_WARN overlay off before release: {e}");
                }
            }
        }
        drm::Device::release_master_lock(&self.gbm)
    }

    const SOLID: (u32, u32) = (320, 180);

    fn solid_to_primary(&mut self, rgb: u32) -> io::Result<()> {
        if self.solid.is_none() {
            let db = self.gbm.create_dumb_buffer(Self::SOLID, drm::buffer::DrmFourcc::Xrgb8888, 32)?;
            let fb = match self.gbm.add_framebuffer(&db, 24, 32) {
                Ok(fb) => fb,
                Err(e) => {
                    let _ = self.gbm.destroy_dumb_buffer(db);
                    return Err(e);
                }
            };
            self.solid = Some((db, fb));
        }
        let (db, fb) = self.solid.as_mut().unwrap();
        let fb = *fb;
        {
            let mut map = self.gbm.map_dumb_buffer(db)?;
            let px = (0xff00_0000u32 | (rgb & 0xff_ffff)).to_le_bytes();
            for c in map.as_mut().chunks_exact_mut(4) {
                c.copy_from_slice(&px);
            }
        }
        self.planes_to_primary(Some(fb), Self::SOLID)
    }

    /// An app has the screen and has put its own framebuffer on the CRTC: none of ours is
    /// being scanned out any more.
    pub fn foreign_scanout(&self) -> bool {
        match self.gbm.get_crtc(self.crtc).ok().and_then(|c| c.framebuffer()) {
            Some(fb) => !self.fbs.contains(&fb),
            None => false,
        }
    }

    /// Drop every scanout buffer reference and remove the framebuffers (a framebuffer keeps
    /// its pixels alive even after the GBM surface is gone). Only while an app has the screen
    /// and `foreign_scanout()`; the caller then destroys the GBM surfaces. Returns the count.
    pub fn free_buffers(&mut self) -> usize {
        self.shown = None;
        self.queued = None;
        self.wall_shown = None;
        self.wall_queued = None;
        self.pending = false;
        let n = self.fbs.len();
        for fb in self.fbs.drain(..) {
            if let Err(e) = self.gbm.destroy_framebuffer(fb) {
                eprintln!("UI_WARN rmfb: {e}");
            }
        }
        n
    }

    /// Take the screen back (appd RECLAIM_DISPLAY): become master again; the next present
    /// page-flips over the app's framebuffer (SetCrtc if that is refused).
    pub fn reacquire(&mut self) -> io::Result<()> {
        drm::Device::acquire_master_lock(&self.gbm)?;
        // The app normally leaves our mode and format on the CRTC: a page flip replaces its
        // framebuffer without the blocking legacy SetCrtc (one dropped frame otherwise).
        self.modeset = true;
        self.reclaimed = true;
        self.pending = false;
        self.queued = None;
        self.wall_queued = None;
        Ok(())
    }

    /// Wait for the last flip, then put the console's CRTC configuration back.
    pub fn restore(&mut self) -> String {
        if let Err(e) = self.set_output(true) {
            eprintln!("UI_WARN output on before restore: {e}");
        }
        if self.pending {
            let _ = self.wait_flip(Duration::from_secs(1));
        }
        if self.planes.is_some() && self.modeset {
            let _ = self.planes_to_primary(None, (0, 0));
        }
        let r = match &self.saved {
            Some(s) if self.modeset && s.mode().is_some() => self.gbm.set_crtc(
                self.crtc,
                s.framebuffer(),
                s.position(),
                &[self.conn],
                s.mode(),
            ),
            _ if self.modeset => self.gbm.set_crtc(self.crtc, None, (0, 0), &[], None),
            _ => Ok(()),
        };
        self.shown = None;
        self.queued = None;
        self.wall_shown = None;
        self.wall_queued = None;
        match r {
            Ok(()) => "UI_RESTORE_CRTC ok".into(),
            Err(e) => format!("UI_RESTORE_CRTC error {e}"),
        }
    }
}
