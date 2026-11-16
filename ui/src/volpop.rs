//! The volume pop-up on the launcher: the system's one pop-up (meow_app::osd draws the same
//! pixels for the apps), shown whenever something sets the volume, and usable with the D-pad
//! when opened from the top bar's speaker: Left/Down and Right/Up step the volume, OK mutes,
//! Back closes; it goes away by itself a few seconds after the last key.
//!
//! The sprite is a small texture of its own (one extra draw call while it shows). appd owns
//! the volume: a key only sends a request, and the pop-up follows what appd reports back.

use crate::atlas::Atlas;
use crate::gpu::Gpu;
use crate::home::{FOCUS_COLOR, Frame, Key, Slices, nine, quad};
use crate::util::{RectF, rgba};
use meow_app::osd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const ID_VOLUME: u32 = 970_000;
/// How long it waits for the next key when opened from the top bar.
const HOLD_KEYS: Duration = Duration::from_millis(3500);

/// What a key on the open pop-up asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum PopKey {
    /// Send this `VOLUME` request to appd ("+5", "-5", "mute"): one press is 5 steps.
    Request(&'static str),
    Closed,
    /// Not a key for the pop-up: it closes and the key goes on to the home screen.
    Pass,
}

pub struct VolPopup {
    renderer: Option<osd::Renderer>,
    loading: Option<mpsc::Receiver<Result<osd::Renderer, String>>>,
    tex: Option<(glow::Texture, u32, u32)>,
    uploaded: Option<(u8, bool)>,
    level: u8,
    muted: bool,
    /// When this showing began (None: hidden), and the last change or key.
    shown: Option<Instant>,
    last: Instant,
    /// Opened from the top bar: it has the keys.
    keys: bool,
}

/// Opacity at `now` of a pop-up that began at `shown`, was last touched at `last` and
/// holds for `hold` after that. None once it has faded out.
fn alpha(now: Instant, shown: Instant, last: Instant, hold: Duration) -> Option<f32> {
    let leaving = now.saturating_duration_since(last + hold);
    if leaving >= osd::FADE_OUT {
        return None;
    }
    let fade_in = (now.saturating_duration_since(shown).as_secs_f32() / osd::FADE_IN.as_secs_f32()).min(1.0);
    let out = 1.0 - leaving.as_secs_f32() / osd::FADE_OUT.as_secs_f32();
    Some((1.0 - (1.0 - fade_in).powi(3)) * out * out * (3.0 - 2.0 * out))
}

impl VolPopup {
    /// The renderer (font, panel, icons) is prepared on a worker thread: ~10 ms of CPU work
    /// that the first volume change should not pay.
    pub fn new(fonts_dir: &str, screen_h: u32) -> VolPopup {
        let (tx, rx) = mpsc::channel();
        let dir = fonts_dir.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(osd::Renderer::from_font_dir(&dir, screen_h));
        });
        VolPopup { renderer: None, loading: Some(rx), tex: None, uploaded: None, level: 100, muted: false, shown: None, last: Instant::now(), keys: false }
    }

    fn hold(&self) -> Duration {
        if self.keys { HOLD_KEYS } else { osd::HOLD }
    }

    fn alpha(&self, now: Instant) -> Option<f32> {
        alpha(now, self.shown?, self.last, self.hold())
    }

    pub fn visible(&self, now: Instant) -> bool {
        self.alpha(now).is_some()
    }

    /// It has the keys (opened from the top bar and not closed yet).
    pub fn has_keys(&self, now: Instant) -> bool {
        self.keys && self.shown.is_some() && now < self.last + self.hold()
    }

    /// Fading in or out (a frame is due); while it just holds nothing is drawn.
    pub fn animating(&self, now: Instant) -> bool {
        self.shown.is_some_and(|s| now < s + osd::FADE_IN || (now >= self.last + self.hold() && self.visible(now)))
    }

    /// When the fade-out starts (wake the loop then).
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        let end = self.last + self.hold();
        (self.shown.is_some() && now < end).then_some(end)
    }

    /// Housekeeping each loop: forget a pop-up that has faded out. True when it just went.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.shown.is_some() && self.alpha(now).is_none() {
            self.shown = None;
            self.keys = false;
            return true;
        }
        false
    }

    /// The volume was set (by anything): show it.
    pub fn show(&mut self, level: u32, muted: bool, now: Instant) {
        (self.level, self.muted) = (level.min(100) as u8, muted);
        // Coming back while it fades out: continue from the opacity it has.
        self.shown = Some(match self.alpha(now) {
            Some(a) if now >= self.last + self.hold() => now - osd::FADE_IN.mul_f32(a.clamp(0.0, 1.0)),
            Some(_) => self.shown.unwrap_or(now),
            None => now,
        });
        self.last = now;
    }

    /// Open it for the D-pad (the top bar's speaker).
    pub fn open(&mut self, level: u32, muted: bool, now: Instant) {
        self.show(level, muted, now);
        self.keys = true;
    }

    /// Fade out now.
    pub fn close(&mut self, now: Instant) {
        let was_holding = self.shown.is_some() && now < self.last + self.hold();
        self.keys = false;
        if was_holding {
            self.last = now.checked_sub(self.hold()).unwrap_or(now);
        }
    }

    /// Gone at once (an app takes the screen and shows its own).
    pub fn hide(&mut self) {
        self.shown = None;
        self.keys = false;
    }

    pub fn key(&mut self, k: Key, now: Instant) -> PopKey {
        match k {
            Key::Left | Key::Down => {
                self.last = now;
                PopKey::Request("-5")
            }
            Key::Right | Key::Up => {
                self.last = now;
                PopKey::Request("+5")
            }
            Key::Enter => {
                self.last = now;
                PopKey::Request("mute")
            }
            Key::Back => {
                self.close(now);
                PopKey::Closed
            }
            Key::Home => {
                self.close(now);
                PopKey::Pass
            }
        }
    }

    /// Take the renderer from the start-up thread (waiting up to `wait` for it) and bring
    /// the texture up to the volume to show. False while there is no renderer.
    fn sync(&mut self, gpu: &Gpu, wait: Duration) -> bool {
        if let Some(rx) = self.loading.as_ref() {
            match rx.recv_timeout(wait) {
                Ok(Ok(r)) => self.renderer = Some(r),
                Ok(Err(e)) => eprintln!("UI_WARN volume pop-up: {e}"),
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
            self.loading = None;
        }
        let Some(renderer) = self.renderer.as_mut() else { return false };
        if self.uploaded != Some((self.level, self.muted)) || self.tex.is_none() {
            let sprite = renderer.render(self.level, self.muted);
            let bytes = sprite.rgba_bytes();
            match self.tex {
                Some((t, tw, th)) if (tw, th) == (sprite.width, sprite.height) => gpu.upload(t, 0, 0, tw, th, &bytes),
                _ => self.tex = Some((gpu.texture(sprite.width, sprite.height, Some(&bytes)), sprite.width, sprite.height)),
            }
            self.uploaded = Some((self.level, self.muted));
        }
        true
    }

    /// Idle-time preparation: create the texture with the volume as it is, so the first
    /// pop-up only updates it. Returns true if it did any work.
    pub fn prewarm(&mut self, gpu: &Gpu, volume: Option<(u32, bool)>) -> bool {
        if self.tex.is_some() || self.shown.is_some() {
            return false;
        }
        if let Some((level, muted)) = volume {
            (self.level, self.muted) = (level.min(100) as u8, muted);
        }
        self.sync(gpu, Duration::ZERO) && self.tex.is_some()
    }

    /// Draw it into `f` (the sprite's own texture; the focus ring from the atlas).
    #[allow(clippy::too_many_arguments)]
    pub fn build(&mut self, gpu: &Gpu, atlas: &Atlas, now: Instant, (w, h): (u32, u32), sl: &Slices, f: &mut Frame) {
        let Some(a) = self.alpha(now) else { return };
        // (Prepared at start-up; at most a short wait if a change comes this early.)
        if !self.sync(gpu, Duration::from_millis(30)) {
            return;
        }
        let Some((tex, tw, th)) = self.tex else { return };
        let s = h as f32 / 1080.0;
        let (x, y, _, _) = osd::place(w, h);
        // It rises a few pixels while it fades in (like the top bar's tooltips).
        let rise = if a >= 1.0 { 0.0 } else { (8.0 * s * (1.0 - a)).round() };
        let (x0, y0) = (x as f32, y as f32 + rise);
        let st = f.begin();
        quad(&mut f.verts, x0, y0, x0 + tw as f32, y0 + th as f32, [0.0, 0.0, 1.0, 1.0], rgba(0xffffff, a));
        f.end_tex(ID_VOLUME, st, Some(tex));
        if self.keys {
            // It has the keys: the launcher's focus ring around the panel.
            let (rr, rm) = sl.ring;
            let panel = RectF::new(x0 + osd::PAD * s, y0 + osd::PAD * s, osd::PANEL_W * s, osd::PANEL_H * s);
            let st = f.begin();
            nine(&mut f.verts, atlas, &rr, rm, panel.expand(5.0 * s), rm as f32 * s / sl.raster, rgba(FOCUS_COLOR, a));
            f.end(ID_VOLUME + 1, st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn popup() -> VolPopup {
        VolPopup { renderer: None, loading: None, tex: None, uploaded: None, level: 100, muted: false, shown: None, last: Instant::now(), keys: false }
    }

    #[test]
    fn it_shows_on_a_change_and_goes_by_itself() {
        let t0 = Instant::now();
        let mut p = popup();
        assert!(!p.visible(t0) && !p.animating(t0) && p.wake_at(t0).is_none());
        p.show(40, false, t0);
        assert!(p.visible(t0) && p.animating(t0), "fading in");
        let held = t0 + Duration::from_millis(500);
        assert_eq!(p.alpha(held), Some(1.0));
        assert!(!p.animating(held), "nothing to draw while it holds");
        assert_eq!(p.wake_at(held), Some(t0 + osd::HOLD));
        let leaving = t0 + osd::HOLD + Duration::from_millis(100);
        assert!(p.animating(leaving) && p.alpha(leaving).unwrap() < 1.0);
        let gone = t0 + osd::HOLD + osd::FADE_OUT;
        assert!(!p.visible(gone));
        assert!(p.tick(gone) && !p.tick(gone));
        assert!(p.shown.is_none());
    }

    #[test]
    fn another_change_keeps_it_up_without_a_blink() {
        let t0 = Instant::now();
        let mut p = popup();
        p.show(40, false, t0);
        // A change while it holds: no new fade-in, a new hold.
        let t1 = t0 + Duration::from_millis(1500);
        p.show(45, false, t1);
        assert_eq!(p.alpha(t1), Some(1.0));
        assert_eq!(p.wake_at(t1), Some(t1 + osd::HOLD));
        assert_eq!((p.level, p.muted), (45, false));
        // A change while it fades out: it comes back from about where it was.
        let t2 = t1 + osd::HOLD + osd::FADE_OUT / 2;
        let before = p.alpha(t2).unwrap();
        p.show(50, true, t2);
        let after = p.alpha(t2).unwrap();
        assert!(after >= before * 0.5 && after < 1.0, "{before} -> {after}");
        assert_eq!(p.alpha(t2 + osd::FADE_IN), Some(1.0));
    }

    #[test]
    fn opened_from_the_top_bar_it_takes_the_keys() {
        let t0 = Instant::now();
        let mut p = popup();
        p.show(40, false, t0);
        assert!(!p.has_keys(t0), "a pop-up from a phone's volume change never takes the keys");
        p.open(40, false, t0);
        assert!(p.has_keys(t0));
        assert_eq!(p.key(Key::Right, t0), PopKey::Request("+5"));
        assert_eq!(p.key(Key::Up, t0), PopKey::Request("+5"));
        assert_eq!(p.key(Key::Left, t0), PopKey::Request("-5"));
        assert_eq!(p.key(Key::Down, t0), PopKey::Request("-5"));
        assert_eq!(p.key(Key::Enter, t0), PopKey::Request("mute"));
        // Each key restarts the wait; without keys it lets go.
        let t1 = t0 + Duration::from_secs(3);
        assert!(p.has_keys(t1));
        p.key(Key::Right, t1);
        assert!(p.has_keys(t1 + Duration::from_secs(3)));
        assert!(!p.has_keys(t1 + HOLD_KEYS));
        // Back closes it: the keys are the home screen's again at once, and it fades out.
        p.open(40, false, t1);
        assert_eq!(p.key(Key::Back, t1), PopKey::Closed);
        assert!(!p.has_keys(t1));
        assert!(p.visible(t1) && p.animating(t1));
        assert!(!p.visible(t1 + osd::FADE_OUT));
        p.open(40, false, t1);
        assert_eq!(p.key(Key::Home, t1), PopKey::Pass);
        p.hide();
        assert!(!p.visible(t1));
    }
}
