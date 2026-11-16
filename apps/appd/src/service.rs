//! Background services: when appd starts an app's small background instance, and how long it
//! waits before starting it again. Pure state (no processes), so it can be tested: the daemon
//! asks `due`, reports `started` and `exited`, and does the rest.
//!
//! A service that keeps dying is started again after 2 s, 4 s, 8 s ... up to 5 minutes; one
//! that ran for a minute counts as healthy again. A service that appd stopped itself (the app
//! opened, an update) comes back quickly.

use std::time::{Duration, Instant};

pub const BACKOFF_MIN: Duration = Duration::from_secs(2);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Running this long clears the failure count.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// After appd stopped it on purpose (the app was open, a new version): start again this soon.
pub const RESTART_AFTER_STOP: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not running; started again when `Supervisor::due`.
    Down,
    Running,
}

#[derive(Clone, Debug)]
pub struct Supervisor {
    pub phase: Phase,
    /// Do not start before this.
    pub not_before: Option<Instant>,
    /// Exits in a row that came too soon.
    pub failures: u32,
    pub started_at: Option<Instant>,
    /// Starts so far (`appctl background`).
    pub starts: u32,
}

impl Default for Supervisor {
    fn default() -> Self {
        Supervisor { phase: Phase::Down, not_before: None, failures: 0, started_at: None, starts: 0 }
    }
}

/// The wait after `failures` early exits in a row (1 -> 2 s, 2 -> 4 s, ... capped).
pub fn backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    (BACKOFF_MIN * (1u32 << shift)).min(BACKOFF_MAX)
}

impl Supervisor {
    /// May it be started now?
    pub fn due(&self, now: Instant) -> bool {
        self.phase == Phase::Down && self.not_before.is_none_or(|t| now >= t)
    }

    pub fn started(&mut self, now: Instant) {
        self.phase = Phase::Running;
        self.started_at = Some(now);
        self.not_before = None;
        self.starts += 1;
    }

    /// It could not be started at all (payload not here, spawn failed): like an early exit.
    pub fn start_failed(&mut self, now: Instant) -> Duration {
        self.failures += 1;
        let wait = backoff(self.failures);
        self.phase = Phase::Down;
        self.not_before = Some(now + wait);
        wait
    }

    /// The instance exited. `stopped`: appd asked it to (not a failure). Returns the wait
    /// before the next start.
    pub fn exited(&mut self, now: Instant, stopped: bool) -> Duration {
        let ran = self.started_at.take().map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.phase = Phase::Down;
        let wait = if stopped {
            RESTART_AFTER_STOP
        } else {
            if ran >= HEALTHY_AFTER {
                self.failures = 0;
            }
            self.failures += 1;
            backoff(self.failures)
        };
        if stopped && ran >= HEALTHY_AFTER {
            self.failures = 0;
        }
        self.not_before = Some(now + wait);
        wait
    }

    /// The user switched it on (again), or granted the permission: no waiting.
    pub fn reset(&mut self) {
        self.failures = 0;
        if self.phase == Phase::Down {
            self.not_before = None;
        }
    }
}

/// A launch payload as it goes into the foreground instance's environment: one line of
/// printable text, at most `PAYLOAD_MAX` bytes. None if it cannot be passed.
pub const PAYLOAD_MAX: usize = 4096;
pub fn clean_payload(p: &str) -> Option<String> {
    (p.len() <= PAYLOAD_MAX && !p.chars().any(|c| c.is_control())).then(|| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_a_cap() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(5), Duration::from_secs(32));
        assert_eq!(backoff(9), BACKOFF_MAX);
        assert_eq!(backoff(1000), BACKOFF_MAX);
        assert_eq!(backoff(0), Duration::from_secs(2));
    }

    #[test]
    fn a_crashing_service_is_started_less_and_less_often() {
        let t0 = Instant::now();
        let mut s = Supervisor::default();
        assert!(s.due(t0), "a new service starts at once");
        s.started(t0);
        assert!(!s.due(t0 + Duration::from_secs(1)), "not while it runs");
        // Dies after 1 s, three times.
        let mut now = t0 + Duration::from_secs(1);
        for expected in [2, 4, 8] {
            let wait = s.exited(now, false);
            assert_eq!(wait, Duration::from_secs(expected));
            assert!(!s.due(now + wait - Duration::from_millis(1)));
            assert!(s.due(now + wait));
            now += wait;
            s.started(now);
            now += Duration::from_secs(1);
        }
        assert_eq!(s.failures, 3);
        assert_eq!(s.starts, 4);
    }

    #[test]
    fn a_minute_of_running_is_health() {
        let t0 = Instant::now();
        let mut s = Supervisor { failures: 6, ..Default::default() };
        s.started(t0);
        // After a long run a crash is the first failure again.
        assert_eq!(s.exited(t0 + Duration::from_secs(3600), false), Duration::from_secs(2));
        assert_eq!(s.failures, 1);
    }

    #[test]
    fn a_stop_by_appd_is_not_a_failure() {
        let t0 = Instant::now();
        let mut s = Supervisor::default();
        s.started(t0);
        let now = t0 + Duration::from_secs(5);
        assert_eq!(s.exited(now, true), RESTART_AFTER_STOP);
        assert_eq!(s.failures, 0);
        assert!(s.due(now + RESTART_AFTER_STOP));
        // Earlier failures are kept across a short, stopped run (no reset by flapping) ...
        let mut s = Supervisor { failures: 4, ..Default::default() };
        s.started(t0);
        s.exited(now, true);
        assert_eq!(s.failures, 4);
        // ... and cleared by a long one.
        s.started(now);
        s.exited(now + Duration::from_secs(120), true);
        assert_eq!(s.failures, 0);
    }

    #[test]
    fn failing_to_start_backs_off_and_the_user_can_reset() {
        let t0 = Instant::now();
        let mut s = Supervisor::default();
        assert_eq!(s.start_failed(t0), Duration::from_secs(2));
        assert_eq!(s.start_failed(t0), Duration::from_secs(4));
        assert!(!s.due(t0 + Duration::from_secs(3)));
        s.reset();
        assert!(s.due(t0));
        assert_eq!(s.failures, 0);
        // A reset never touches a running service.
        s.started(t0);
        s.reset();
        assert_eq!(s.phase, Phase::Running);
    }

    #[test]
    fn payloads_are_one_printable_line() {
        assert_eq!(clean_payload("{\"v\":\"dQw4w9WgXcQ\",\"t\":12}").as_deref(), Some("{\"v\":\"dQw4w9WgXcQ\",\"t\":12}"));
        assert_eq!(clean_payload("").as_deref(), Some(""));
        assert!(clean_payload("a\nb").is_none());
        assert!(clean_payload("a\0b").is_none());
        assert!(clean_payload("a\tb").is_none(), "TAB separates fields on the sockets");
        assert!(clean_payload(&"x".repeat(PAYLOAD_MAX + 1)).is_none());
        assert!(clean_payload(&"x".repeat(PAYLOAD_MAX)).is_some());
    }
}
