//! Small shared types: integer pixel rectangles (damage), tweens, colours.

use std::time::{Duration, Instant};

/// Half-open pixel rectangle [x0, x1) x [y0, y1). Empty when x0 >= x1 or y0 >= y1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x0: 0, y0: 0, x1: 0, y1: 0 };

    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect::new(self.x0.min(o.x0), self.y0.min(o.y0), self.x1.max(o.x1), self.y1.max(o.y1))
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let r = Rect::new(self.x0.max(o.x0), self.y0.max(o.y0), self.x1.min(o.x1), self.y1.min(o.y1));
        if r.is_empty() { Rect::EMPTY } else { r }
    }
    /// `o` lies entirely inside self (an empty `o` counts as inside).
    pub fn contains(&self, o: &Rect) -> bool {
        o.is_empty() || (o.x0 >= self.x0 && o.y0 >= self.y0 && o.x1 <= self.x1 && o.y1 <= self.y1)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        !self.intersect(o).is_empty()
    }
    pub fn area(&self) -> i64 {
        if self.is_empty() { 0 } else { (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64 }
    }
    /// Grow outwards to multiples of `n` (Lima renders in 16x16 tiles; coarser alignment
    /// also lets Mesa reuse cached PP streams for recurring damage rectangles).
    pub fn align_out(&self, n: i32) -> Rect {
        if self.is_empty() {
            return *self;
        }
        Rect::new(
            self.x0.div_euclid(n) * n,
            self.y0.div_euclid(n) * n,
            (self.x1 + n - 1).div_euclid(n) * n,
            (self.y1 + n - 1).div_euclid(n) * n,
        )
    }
}

/// Float rectangle in physical pixels.
#[derive(Clone, Copy, Debug)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> RectF {
        RectF { x, y, w, h }
    }
    pub fn scaled_about_center(&self, s: f32) -> RectF {
        let (cx, cy) = (self.x + self.w * 0.5, self.y + self.h * 0.5);
        RectF::new(cx - self.w * s * 0.5, cy - self.h * s * 0.5, self.w * s, self.h * s)
    }
    pub fn expand(&self, d: f32) -> RectF {
        RectF::new(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }
    /// Snap edges to whole pixels (crisp edges when not animating).
    pub fn snapped(&self) -> RectF {
        let (x0, y0) = (self.x.round(), self.y.round());
        RectF::new(x0, y0, (self.x + self.w).round() - x0, (self.y + self.h).round() - y0)
    }
}

/// A value animated from `from` to `to` with an ease-out cubic curve.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    dur: Duration,
}

impl Tween {
    pub fn fixed(v: f32) -> Tween {
        Tween { from: v, to: v, start: Instant::now(), dur: Duration::ZERO }
    }
    pub fn target(&self) -> f32 {
        self.to
    }
    /// Retarget from the current value, so interrupted animations stay continuous.
    pub fn go(&mut self, to: f32, now: Instant, dur: Duration) {
        if (to - self.to).abs() < f32::EPSILON {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = now;
        self.dur = dur;
    }
    /// Like `go`, but the change starts at `start` (holds the current value until then).
    pub fn go_at(&mut self, to: f32, now: Instant, start: Instant, dur: Duration) {
        if (to - self.to).abs() < f32::EPSILON {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = start;
        self.dur = dur;
    }
    pub fn value(&self, now: Instant) -> f32 {
        let t = self.progress(now);
        let e = 1.0 - (1.0 - t).powi(3);
        self.from + (self.to - self.from) * e
    }
    pub fn active(&self, now: Instant) -> bool {
        self.progress(now) < 1.0
    }
    fn progress(&self, now: Instant) -> f32 {
        if self.dur.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f32() / self.dur.as_secs_f32()).min(1.0)
    }
}

/// Premultiplied RGBA8 from a 0xRRGGBB colour and an opacity.
pub fn rgba(rgb: u32, alpha: f32) -> [u8; 4] {
    let a = alpha.clamp(0.0, 1.0);
    let c = |s: u32| ((((rgb >> s) & 0xff) as f32) * a + 0.5) as u8;
    [c(16), c(8), c(0), (a * 255.0 + 0.5) as u8]
}
