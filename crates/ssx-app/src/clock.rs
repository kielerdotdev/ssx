//! Time sources. Everything in the daemon that reads "now" takes a [`Clock`], so the
//! coalescer, the recording timer and the supervisor can be tested without sleeping.
//!
//! Instants are plain [`Duration`]s since an arbitrary origin: monotonic, cheap to compare,
//! and trivially fakeable.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// A monotonic clock.
pub trait Clock: Send + Sync + 'static {
    /// Time since this clock's origin. Never goes backwards.
    fn now(&self) -> Duration;
}

/// The real clock ([`Instant`]-based).
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose origin is now.
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that only moves when told to. Shareable (`&self` methods), so a test can hold one
/// handle and give another to the code under test.
#[derive(Debug, Default)]
pub struct FakeClock {
    nanos: AtomicU64,
}

impl FakeClock {
    /// A clock at time zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves time forward.
    pub fn advance(&self, by: Duration) {
        let n = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(n, Ordering::SeqCst);
    }

    /// Sets the time (must not go backwards; tests own that).
    pub fn set(&self, to: Duration) {
        self.nanos.store(u64::try_from(to.as_nanos()).unwrap_or(u64::MAX), Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
}

/// Formats a duration as `m:ss` (or `h:mm:ss` from an hour on), for the tray.
pub fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_clock_moves_only_when_told() {
        let c = FakeClock::new();
        assert_eq!(c.now(), Duration::ZERO);
        c.advance(Duration::from_millis(400));
        c.advance(Duration::from_millis(1));
        assert_eq!(c.now(), Duration::from_millis(401));
        c.set(Duration::from_secs(5));
        assert_eq!(c.now(), Duration::from_secs(5));
    }

    #[test]
    fn the_system_clock_is_monotonic() {
        let c = SystemClock::new();
        let a = c.now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.now() >= a + Duration::from_millis(4));
    }

    #[test]
    fn elapsed_time_formats() {
        assert_eq!(format_elapsed(Duration::ZERO), "0:00");
        assert_eq!(format_elapsed(Duration::from_secs(9)), "0:09");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1:15");
        assert_eq!(format_elapsed(Duration::from_secs(3600 + 62)), "1:01:02");
        assert_eq!(format_elapsed(Duration::from_millis(59_999)), "0:59");
    }
}
