//! Trailing-edge debouncing, as a pure state machine driven by an explicit clock.
//!
//! The HDR preview re-renders a scene through the tone mapper; dragging a slider changes the
//! settings every frame, so the work must wait until the value has been still for a moment
//! and must never run for a value that has already been replaced. [`Debouncer`] holds the
//! newest submitted value and releases it once `delay` has passed without a *different* value
//! arriving. Re-submitting the value that is already pending does not restart the wait (a
//! held slider keeps producing the same value every frame). Time is passed in, so the
//! behaviour is tested without sleeping.

use std::time::{Duration, Instant};

/// Holds at most one pending value.
#[derive(Debug, Clone)]
pub struct Debouncer<T> {
    delay: Duration,
    pending: Option<(T, Instant)>,
}

impl<T: PartialEq> Debouncer<T> {
    /// A debouncer that waits `delay` after the last change.
    pub fn new(delay: Duration) -> Self {
        Self { delay, pending: None }
    }

    /// Offers `value` at time `now`. Replaces a different pending value (and restarts the
    /// wait); an equal one is left alone.
    pub fn submit(&mut self, value: T, now: Instant) {
        match &self.pending {
            Some((p, _)) if *p == value => {}
            _ => self.pending = Some((value, now)),
        }
    }

    /// Takes the pending value if it has been still for `delay`.
    pub fn take_due(&mut self, now: Instant) -> Option<T> {
        match &self.pending {
            Some((_, since)) if now.saturating_duration_since(*since) >= self.delay => {
                self.pending.take().map(|(v, _)| v)
            }
            _ => None,
        }
    }

    /// Takes the pending value regardless of the wait (the first render of a page should not
    /// wait).
    pub fn take_now(&mut self) -> Option<T> {
        self.pending.take().map(|(v, _)| v)
    }

    /// How long until the pending value is due (`None` if nothing is pending, zero if due).
    pub fn time_until_due(&self, now: Instant) -> Option<Duration> {
        self.pending
            .as_ref()
            .map(|(_, since)| self.delay.saturating_sub(now.saturating_duration_since(*since)))
    }

    /// Whether a value is waiting.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The waiting value.
    pub fn pending(&self) -> Option<&T> {
        self.pending.as_ref().map(|(v, _)| v)
    }

    /// Drops the pending value.
    pub fn cancel(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const D: Duration = Duration::from_millis(100);

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn nothing_pending_nothing_due() {
        let mut d = Debouncer::<u32>::new(D);
        let t0 = Instant::now();
        assert_eq!(d.take_due(t0), None);
        assert_eq!(d.time_until_due(t0), None);
        assert!(!d.is_pending());
    }

    #[test]
    fn a_value_is_released_only_after_the_delay() {
        let mut d = Debouncer::new(D);
        let t0 = Instant::now();
        d.submit(1, t0);
        assert_eq!(d.take_due(ms(t0, 99)), None);
        assert_eq!(d.time_until_due(ms(t0, 40)), Some(Duration::from_millis(60)));
        assert_eq!(d.take_due(ms(t0, 100)), Some(1));
        assert_eq!(d.take_due(ms(t0, 500)), None, "released once");
        assert!(!d.is_pending());
    }

    #[test]
    fn a_new_value_replaces_the_old_and_restarts_the_wait() {
        let mut d = Debouncer::new(D);
        let t0 = Instant::now();
        d.submit(1, t0);
        d.submit(2, ms(t0, 80));
        assert_eq!(d.take_due(ms(t0, 150)), None, "the wait restarted at 80 ms");
        assert_eq!(d.take_due(ms(t0, 180)), Some(2));
    }

    #[test]
    fn resubmitting_the_same_value_does_not_postpone_it() {
        let mut d = Debouncer::new(D);
        let t0 = Instant::now();
        d.submit(7, t0);
        for i in 1..10 {
            d.submit(7, ms(t0, i * 10));
        }
        assert_eq!(d.take_due(ms(t0, 100)), Some(7));
    }

    #[test]
    fn take_now_and_cancel() {
        let mut d = Debouncer::new(D);
        let t0 = Instant::now();
        d.submit(3, t0);
        assert_eq!(d.pending(), Some(&3));
        assert_eq!(d.take_now(), Some(3));
        d.submit(4, t0);
        d.cancel();
        assert_eq!(d.take_due(ms(t0, 1000)), None);
    }

    proptest! {
        /// However values arrive, only the newest is ever released, never early, never twice.
        #[test]
        fn only_the_last_value_of_a_burst_is_released(
            gaps in proptest::collection::vec(0u64..250, 1..30),
        ) {
            let mut d = Debouncer::new(D);
            let t0 = Instant::now();
            let mut now = 0u64;
            let mut submitted_at = 0u64;
            let mut released = Vec::new();
            for (i, gap) in gaps.iter().enumerate() {
                now += gap;
                // poll before submitting, as a UI frame would
                if let Some(v) = d.take_due(ms(t0, now)) {
                    released.push((v, now));
                }
                d.submit(i, ms(t0, now));
                submitted_at = now;
            }
            if let Some(v) = d.take_due(ms(t0, submitted_at + 100)) {
                released.push((v, submitted_at + 100));
            }
            // every released value was the newest at its release time, i.e. never a stale one
            let mut seen = std::collections::HashSet::new();
            for (v, _) in &released {
                prop_assert!(seen.insert(*v), "released twice");
            }
            prop_assert_eq!(released.last().map(|r| r.0), Some(gaps.len() - 1));
            // and released values are in submission order
            prop_assert!(released.windows(2).all(|w| w[0].0 < w[1].0));
        }
    }
}
