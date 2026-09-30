//! Pause / resume timeline compaction.
//!
//! Sources stamp frames and audio with the shared [`Clock`](crate::time::Clock), which
//! keeps running while a recording is paused. The [`Timeline`] remembers every pause
//! interval and converts a clock time into *recording time* by subtracting the paused
//! time before it, so a resumed recording continues seamlessly (no frozen video, no
//! silence) and video and audio, which use the same mapping, stay in sync. Times that
//! fall inside a pause map to `None` and the frame or sample is discarded.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

/// The list of pause intervals of one recording, in clock time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Timeline {
    /// `(start, end)`; `end` is `None` while the recording is paused.
    pauses: Vec<(Duration, Option<Duration>)>,
}

impl Timeline {
    /// A timeline with no pauses.
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` while paused.
    pub fn is_paused(&self) -> bool {
        matches!(self.pauses.last(), Some((_, None)))
    }

    /// Starts a pause at clock time `at`. No effect when already paused.
    pub fn pause(&mut self, at: Duration) {
        if !self.is_paused() {
            let at = self.pauses.last().and_then(|p| p.1).map_or(at, |end| at.max(end));
            self.pauses.push((at, None));
        }
    }

    /// Ends the current pause at clock time `at`. No effect when not paused.
    pub fn resume(&mut self, at: Duration) {
        if let Some((start, end @ None)) = self.pauses.last_mut() {
            *end = Some(at.max(*start));
        }
    }

    /// Recording time of clock time `t`, or `None` if `t` lies inside a pause.
    pub fn map(&self, t: Duration) -> Option<Duration> {
        let mut paused = Duration::ZERO;
        for &(start, end) in &self.pauses {
            if t < start {
                break;
            }
            match end {
                Some(end) if t >= end => paused += end - start,
                _ => return None,
            }
        }
        Some(t - paused)
    }

    /// Total time spent paused before clock time `t` (an open pause counts up to `t`).
    pub fn paused_before(&self, t: Duration) -> Duration {
        let mut paused = Duration::ZERO;
        for &(start, end) in &self.pauses {
            if t <= start {
                break;
            }
            paused += end.unwrap_or(t).min(t) - start;
        }
        paused
    }

    /// The parts of the clock interval `[start, end)` that are *not* paused, as
    /// `(clock_start, clock_end)` pairs. Used to trim audio chunks that straddle a pause.
    pub fn active_segments(&self, start: Duration, end: Duration) -> Vec<(Duration, Duration)> {
        let mut out = Vec::new();
        let mut cursor = start;
        for &(ps, pe) in &self.pauses {
            if cursor >= end {
                break;
            }
            let pe = pe.unwrap_or(Duration::MAX);
            if pe <= cursor {
                continue;
            }
            if ps >= end {
                break;
            }
            if ps > cursor {
                out.push((cursor, ps));
            }
            cursor = pe;
        }
        if cursor < end {
            out.push((cursor, end));
        }
        out
    }
}

/// A [`Timeline`] shared between the capture and audio threads.
#[derive(Debug, Clone, Default)]
pub struct SharedTimeline(Arc<Mutex<Timeline>>);

impl SharedTimeline {
    /// An empty shared timeline.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` with exclusive access.
    pub fn with<T>(&self, f: impl FnOnce(&mut Timeline) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// See [`Timeline::map`].
    pub fn map(&self, t: Duration) -> Option<Duration> {
        self.with(|tl| tl.map(t))
    }

    /// See [`Timeline::is_paused`].
    pub fn is_paused(&self) -> bool {
        self.with(|tl| tl.is_paused())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn no_pauses_is_identity() {
        let t = Timeline::new();
        assert_eq!(t.map(ms(1234)), Some(ms(1234)));
        assert!(!t.is_paused());
    }

    #[test]
    fn pause_is_compacted() {
        let mut t = Timeline::new();
        t.pause(ms(1000));
        assert!(t.is_paused());
        assert_eq!(t.map(ms(999)), Some(ms(999)));
        assert_eq!(t.map(ms(1000)), None);
        assert_eq!(t.map(ms(5000)), None);
        t.resume(ms(3000));
        assert_eq!(t.map(ms(2999)), None);
        assert_eq!(t.map(ms(3000)), Some(ms(1000)));
        assert_eq!(t.map(ms(3500)), Some(ms(1500)));
        t.pause(ms(4000));
        t.resume(ms(4500));
        assert_eq!(t.map(ms(5000)), Some(ms(2500)));
        assert_eq!(t.paused_before(ms(5000)), ms(2500));
    }

    #[test]
    fn redundant_pause_and_resume_are_ignored() {
        let mut t = Timeline::new();
        t.resume(ms(10));
        assert_eq!(t, Timeline::new());
        t.pause(ms(100));
        t.pause(ms(200));
        t.resume(ms(300));
        t.resume(ms(400));
        assert_eq!(t.map(ms(300)), Some(ms(100)));
        assert_eq!(t.paused_before(ms(1000)), ms(200));
    }

    #[test]
    fn open_pause_counts_up_to_query_time() {
        let mut t = Timeline::new();
        t.pause(ms(100));
        assert_eq!(t.paused_before(ms(600)), ms(500));
    }

    #[test]
    fn segments_split_around_pauses() {
        let mut t = Timeline::new();
        t.pause(ms(100));
        t.resume(ms(200));
        t.pause(ms(300));
        t.resume(ms(350));
        assert_eq!(t.active_segments(ms(0), ms(50)), vec![(ms(0), ms(50))]);
        assert_eq!(t.active_segments(ms(50), ms(150)), vec![(ms(50), ms(100))]);
        assert_eq!(t.active_segments(ms(120), ms(180)), vec![]);
        assert_eq!(
            t.active_segments(ms(0), ms(400)),
            vec![(ms(0), ms(100)), (ms(200), ms(300)), (ms(350), ms(400))]
        );
        // An open pause swallows everything after it.
        t.pause(ms(500));
        assert_eq!(t.active_segments(ms(450), ms(900)), vec![(ms(450), ms(500))]);
    }

    #[test]
    fn segment_lengths_match_mapping() {
        // Property: the summed length of active segments equals the mapped length.
        let mut t = Timeline::new();
        for (p, r) in [(100u64, 180u64), (400, 410), (900, 1500)] {
            t.pause(ms(p));
            t.resume(ms(r));
        }
        for (a, b) in [(0u64, 2000u64), (50, 950), (1500, 1800), (0, 100)] {
            let total: Duration =
                t.active_segments(ms(a), ms(b)).iter().map(|(s, e)| *e - *s).sum();
            let expected = (ms(b) - t.paused_before(ms(b))) - (ms(a) - t.paused_before(ms(a)));
            assert_eq!(total, expected, "[{a},{b})");
        }
    }
}
