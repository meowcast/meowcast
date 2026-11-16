//! The factory-reset screens. Two ways lead here:
//!   - the hardware button (meow-buttond, os/buttond.c): held for 3 s it starts a
//!     warning with a countdown, at 10 s it is armed, and letting go within the next 5 s erases.
//!     The daemon owns the timing and runs the reset; this module only draws what it reports
//!     (one byte per step on the key FIFO, `Input::Button`), so the reset works the same with
//!     no launcher and no picture.
//!   - Settings > About > Factory reset: a dialog (setview.rs), then `erasing` here while
//!     /bin/factory-reset stops the launcher.
//! While a screen is up the remote's keys do nothing: the only way on is the button.

use crate::atlas::Atlas;
use crate::gpu::Gpu;
use crate::home::{Frame, Slices, nine, nine_ex};
use crate::text::{Face, Fits, Text};
use crate::util::{RectF, Tween, rgba};
use std::time::{Duration, Instant};

pub const ID_RESET: u32 = 970_000;
/// The daemon's defaults: warning from 3 s to 10 s of holding, armed until 15 s.
const WARN_MS: u64 = 7000;
const ARMED_MS: u64 = 5000;
/// A step the daemon never followed up (it died, a byte was lost): the screen goes by itself.
const GRACE_MS: u64 = 1500;
const ERASING_MAX_MS: u64 = 40_000;
const DANGER: u32 = 0xe5484d;
const AMBER: u32 = 0xf3b04c;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Warn,
    Armed,
    Erasing,
}

pub struct Reset {
    /// What is (or was last) shown, and since when; it stays while the panel fades out.
    phase: Option<(Phase, Instant)>,
    alpha: Tween,
    fits: Fits,
}

impl Reset {
    pub fn new() -> Reset {
        Reset { phase: None, alpha: Tween::fixed(0.0), fits: Fits::default() }
    }

    /// A screen is up (keys are held back, nothing else may cover it).
    pub fn active(&self) -> bool {
        self.alpha.target() > 0.0
    }

    /// Needs frames: fading, or a countdown bar is moving.
    pub fn animating(&self, now: Instant) -> bool {
        self.alpha.active(now) || (self.active() && matches!(self.phase, Some((Phase::Warn | Phase::Armed, _))))
    }

    fn show(&mut self, p: Phase, now: Instant) {
        self.phase = Some((p, now));
        self.alpha.go(1.0, now, Duration::from_millis(220));
    }

    /// The button has been held long enough: the warning and its countdown.
    pub fn warn(&mut self, now: Instant) {
        self.show(Phase::Warn, now);
    }

    /// Letting go now erases.
    pub fn armed(&mut self, now: Instant) {
        self.show(Phase::Armed, now);
    }

    /// The reset has started (button or Settings).
    pub fn erasing(&mut self, now: Instant) {
        self.show(Phase::Erasing, now);
    }

    /// Let go early or held too long: nothing happens.
    pub fn cancel(&mut self, now: Instant) {
        self.alpha.go(0.0, now, Duration::from_millis(260));
    }

    /// Closes a screen whose next step never came; true when it did (worth a notice).
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some((p, t)) = self.phase else { return false };
        if !self.active() {
            if self.alpha.value(now) <= 0.0 {
                self.phase = None;
            }
            return false;
        }
        let limit = match p {
            Phase::Warn => WARN_MS + GRACE_MS,
            Phase::Armed => ARMED_MS + GRACE_MS,
            Phase::Erasing => ERASING_MAX_MS,
        };
        if now >= t + Duration::from_millis(limit) {
            self.cancel(now);
            return true;
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(&mut self, gpu: &Gpu, atlas: &mut Atlas, text: &mut Text, now: Instant, sl: &Slices, s: f32, f: &mut Frame) {
        let a = self.alpha.value(now);
        let Some((phase, since)) = self.phase else { return };
        if a <= 0.0 {
            return;
        }
        let (cr, cm) = sl.card;
        let (shr, shm, shpad) = sl.shadow;
        let dk = s / sl.raster;
        let px = |v: f32| (v * s).round();
        let st = f.begin();
        nine(&mut f.verts, atlas, &cr, cm, RectF::new(-20.0 * s, -20.0 * s, 1960.0 * s, 1120.0 * s), cm as f32 * dk, rgba(0x05060c, 0.9 * a));
        f.end(ID_RESET, st);

        let elapsed = now.saturating_duration_since(since).as_millis() as f32;
        let left = |total: u64| ((total as f32 - elapsed) / 1000.0).ceil().max(0.0) as u64;
        let (title, lines, bar): (&str, [String; 2], Option<(f32, u32)>) = match phase {
            Phase::Warn => (
                "Factory reset",
                [
                    "Keep holding the button to erase everything on this device:".into(),
                    "Wi-Fi networks, paired phones, apps and all settings.".into(),
                ],
                Some(((elapsed / WARN_MS as f32).min(1.0), AMBER)),
            ),
            Phase::Armed => (
                "Let go of the button now to erase everything",
                ["This cannot be undone.".into(), "To cancel, keep holding until this message goes away.".into()],
                Some(((1.0 - elapsed / ARMED_MS as f32).max(0.0), DANGER)),
            ),
            Phase::Erasing => ("Erasing…", ["Everything on this device is being erased.".into(), "It starts again as new in a few seconds.".into()], None),
        };
        let hint = match phase {
            Phase::Warn => format!("Let go now to cancel  ·  {} s", left(WARN_MS)),
            Phase::Armed => format!("Cancels by itself in {} s", left(ARMED_MS)),
            Phase::Erasing => String::new(),
        };

        let (w, h): (f32, f32) = (1240.0, if bar.is_some() { 470.0 } else { 330.0 });
        let (x, y) = ((1920.0 - w) / 2.0, ((1080.0 - h) / 2.0).round() + 18.0 * (1.0 - a));
        let room = (w - 144.0) * s;
        let panel = RectF::new(x * s, y * s, w * s, h * s);
        let st = f.begin();
        nine_ex(&mut f.verts, atlas, &shr, shm, panel.expand(shpad as f32 * dk), shm as f32 * dk, rgba(0x000000, 0.6 * a), false);
        nine(&mut f.verts, atlas, &cr, cm, panel, cm as f32 * dk, rgba(0x1d2233, a));
        // A coloured edge: this is not an ordinary dialog.
        let edge = if phase == Phase::Warn { AMBER } else { DANGER };
        nine(&mut f.verts, atlas, &cr, cm, RectF::new(px(x + 72.0), px(y + 56.0), px(72.0), px(8.0)), 4.0 * s, rgba(edge, a));
        let t = self.fits.line(text, Face::Light, 46.0 * s, title, room);
        text.draw(gpu, atlas, &mut f.verts, Face::Light, 46.0 * s, px(x + 72.0), px(y + 136.0), rgba(0xffffff, a), t);
        for (i, l) in lines.iter().enumerate() {
            let l = self.fits.line(text, Face::Regular, 26.0 * s, l, room);
            text.draw(gpu, atlas, &mut f.verts, Face::Regular, 26.0 * s, px(x + 72.0), px(y + 200.0 + i as f32 * 40.0), rgba(0xffffff, if i == 0 { 0.92 } else { 0.62 } * a), l);
        }
        f.end(ID_RESET + 1, st);

        if let Some((frac, color)) = bar {
            // The bar and the seconds on their own items: only they repaint during a countdown.
            let (bx, by, bw, bh) = (x + 72.0, y + 316.0, w - 144.0, 16.0);
            let st = f.begin();
            nine(&mut f.verts, atlas, &cr, cm, RectF::new(px(bx), px(by), px(bw), px(bh)), 8.0 * s, rgba(0xffffff, 0.12 * a));
            let fill = (bw * frac).max(bh);
            if frac > 0.0 {
                nine(&mut f.verts, atlas, &cr, cm, RectF::new(px(bx), px(by), px(fill), px(bh)), 8.0 * s, rgba(color, a));
            }
            f.end(ID_RESET + 2, st);
            let st = f.begin();
            let hint = self.fits.line(text, Face::Medium, 28.0 * s, &hint, room);
            text.draw(gpu, atlas, &mut f.verts, Face::Medium, 28.0 * s, px(bx), px(by + 76.0), rgba(0xffffff, a), hint);
            f.end(ID_RESET + 3, st);
        }
    }
}

/// Start /bin/factory-reset (UI_FACTORY_RESET: another program, for tests) in its own process
/// group: it stops this launcher and must outlive it. False when it could not be started.
pub fn run(from: &str) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let cmd = std::env::var("UI_FACTORY_RESET").unwrap_or_else(|_| "/bin/factory-reset".into());
    match Command::new(&cmd).arg(from).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("UI_WARN factory reset: {cmd}: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_expire_and_cancel() {
        let t0 = Instant::now();
        let mut r = Reset::new();
        assert!(!r.active());
        r.warn(t0);
        assert!(r.active() && r.animating(t0));
        // The daemon's "armed" is due at 7 s; without it the warning closes by itself.
        assert!(!r.tick(t0 + Duration::from_millis(WARN_MS)));
        assert!(r.tick(t0 + Duration::from_millis(WARN_MS + GRACE_MS)));
        assert!(!r.active());
        let t1 = t0 + Duration::from_secs(20);
        assert!(!r.tick(t1));
        assert!(r.phase.is_none());
        r.warn(t1);
        r.armed(t1 + Duration::from_secs(7));
        r.cancel(t1 + Duration::from_secs(8));
        assert!(!r.active());
        r.erasing(t1 + Duration::from_secs(9));
        assert!(r.active() && !r.tick(t1 + Duration::from_secs(30)));
    }
}
