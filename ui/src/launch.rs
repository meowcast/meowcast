//! App launch / return transition: the focused card grows into a full-screen panel in the
//! app's tint (its icon gliding to the centre over a soft glow, the name rising in), waits
//! for appd with a progress bar or spinner, fades its content out to the plain tint and only
//! then hands the screen to the app (which can fade in from the same colour: MEOW_SPLASH).
//! Coming back, the plain panel shrinks onto the card. A failed launch reverses the same way.
//!
//! Cost model (Mali-400, see PERF-FINDINGS.md): one opaque 9-slice panel plus a few sprites.
//! The panel is reported as an occluder, so cards fully behind it are not drawn and the
//! wallpaper is drawn only around it; while waiting only the spinner/progress box repaints.

use crate::art::{Badge, Glyph};
use crate::util::RectF;
use std::time::Instant;

pub const OPEN_MS: f32 = 420.0;
pub const CLOSE_MS: f32 = 360.0;
pub const FADE_MS: f32 = 150.0;
/// Waiting shorter than this shows no status (no flash of a spinner for fast starts).
pub const STATUS_DELAY_MS: f32 = 300.0;
pub const STATUS_FADE_MS: f32 = 200.0;
/// Item ids of the launch layer (excluded from occlusion culling).
pub const ID_BASE: u32 = 980_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Card -> full screen.
    Opening,
    /// Full screen, waiting for appd (download, sandbox start).
    Waiting,
    /// Icon and name fade out; then the screen goes to the app.
    Fading,
    /// The app has the screen (launcher suspended).
    Released,
    /// Full screen (or wherever opening was interrupted) -> card.
    Closing,
}

pub struct Launch {
    pub id: String,
    pub name: String,
    pub tint: u32,
    pub glyph: Glyph,
    pub grad: (u32, u32),
    pub shape: Badge,
    /// The card (physical px), its badge width and its 9-slice corner size, at e = 0.
    pub card: RectF,
    pub card_badge_w: f32,
    pub card_corner: f32,
    pub phase: Phase,
    pub t0: Instant,
    /// Expansion a Closing starts from (1 = full screen).
    pub e0: f32,
    /// appd asked for the screen while opening: hand it over right after the fade.
    pub release_pending: bool,
    pub status: String,
    pub progress: Option<f32>,
    /// Set when a frame with the content fully faded out was built (then release).
    pub faded_frame_built: bool,
}

fn ease_out_quart(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(4)
}

fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}

pub enum Tick {
    None,
    /// The closing animation ended: drop the launch layer.
    Finished,
}

impl Launch {
    pub fn elapsed(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.t0).as_secs_f32() * 1000.0
    }

    pub fn set_phase(&mut self, p: Phase, now: Instant) {
        self.phase = p;
        self.t0 = now;
        self.faded_frame_built = false;
    }

    /// Start closing from wherever the panel is now, towards `card` (re-measured: the home
    /// screen may have scrolled).
    pub fn close(&mut self, now: Instant, card: Option<(RectF, f32, f32)>) {
        self.e0 = self.expansion(now);
        if let Some((r, bw, c)) = card {
            (self.card, self.card_badge_w, self.card_corner) = (r, bw, c);
        }
        self.set_phase(Phase::Closing, now);
    }

    /// 0 = the card, 1 = full screen.
    pub fn expansion(&self, now: Instant) -> f32 {
        let el = self.elapsed(now);
        match self.phase {
            Phase::Opening => ease_out_quart(el / OPEN_MS),
            Phase::Waiting | Phase::Fading | Phase::Released => 1.0,
            Phase::Closing => self.e0 * (1.0 - ease_in_out_cubic(el / CLOSE_MS)),
        }
    }

    /// Opacity of icon, glow and name.
    pub fn content_alpha(&self, now: Instant) -> f32 {
        let el = self.elapsed(now);
        match self.phase {
            Phase::Opening | Phase::Waiting => 1.0,
            Phase::Fading => 1.0 - (el / FADE_MS).min(1.0),
            Phase::Released => 0.0,
            // Back from the app: the plain panel first, the icon fades in as it shrinks.
            Phase::Closing if self.e0 >= 1.0 => (el / (CLOSE_MS * 0.45)).min(1.0),
            Phase::Closing => 1.0,
        }
    }

    /// Opacity of the status line and the progress bar / spinner.
    pub fn status_alpha(&self, now: Instant) -> f32 {
        let el = self.elapsed(now);
        match self.phase {
            Phase::Waiting => ((el - STATUS_DELAY_MS) / STATUS_FADE_MS).clamp(0.0, 1.0),
            Phase::Fading => 0.0,
            _ => 0.0,
        }
    }

    pub fn animating(&self, now: Instant) -> bool {
        match self.phase {
            Phase::Opening | Phase::Fading | Phase::Closing => true,
            // Fade-in of the status, then only the indeterminate spinner moves.
            Phase::Waiting => self.elapsed(now) < STATUS_DELAY_MS + STATUS_FADE_MS || self.progress.is_none(),
            Phase::Released => false,
        }
    }

    /// When the next state change is due if nothing animates (poll timeout).
    pub fn wake_at(&self) -> Option<Instant> {
        (self.phase == Phase::Waiting).then(|| self.t0 + std::time::Duration::from_millis(STATUS_DELAY_MS as u64))
    }

    pub fn tick(&mut self, now: Instant) -> Tick {
        let el = self.elapsed(now);
        match self.phase {
            Phase::Opening if el >= OPEN_MS => {
                let next = if self.release_pending { Phase::Fading } else { Phase::Waiting };
                self.set_phase(next, now);
            }
            Phase::Closing if el >= CLOSE_MS => return Tick::Finished,
            _ => {}
        }
        Tick::None
    }

    /// The spinner's phase (0..1, one turn per second).
    pub fn spin(&self, now: Instant) -> f32 {
        (self.elapsed(now) / 1000.0).fract()
    }
}

/// Transition boost (like Android's input boost): while the card zooms, raise the minimum
/// CPU and GPU clocks so the first frames after idle are not rendered at 408 MHz CPU /
/// 200 MHz GPU (measured: 2 late frames on a cold launch). The maximum, and therefore the
/// thermal caps, are untouched; the minimums go back as soon as the motion ends.
pub struct Boost {
    on: bool,
    saved: Option<(String, String)>,
}

const GPU_MIN: &str = "/sys/class/devfreq/20000000.gpu/min_freq";
const GPU_MAX: &str = "/sys/class/devfreq/20000000.gpu/max_freq";
const CPU_MIN: &str = "/sys/devices/system/cpu/cpufreq/policy0/scaling_min_freq";
const CPU_MAX: &str = "/sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq";

impl Boost {
    pub fn new() -> Boost {
        Boost { on: false, saved: None }
    }

    pub fn set(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        let read = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_string()).ok();
        if on {
            let (Some(g), Some(c)) = (read(GPU_MIN), read(CPU_MIN)) else { return };
            self.saved = Some((g, c));
            if let Some(m) = read(GPU_MAX) {
                let _ = std::fs::write(GPU_MIN, m);
            }
            if let Some(m) = read(CPU_MAX) {
                let _ = std::fs::write(CPU_MIN, m);
            }
        } else if let Some((g, c)) = self.saved.take() {
            let _ = std::fs::write(GPU_MIN, g);
            let _ = std::fs::write(CPU_MIN, c);
        }
    }
}
