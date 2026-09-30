//! Constant-frame-rate pacing: turning irregular source frames into a fixed slot grid.
//!
//! Screen sources are *damage driven* (`PipeWire`, WGC deliver a frame only when something
//! changed) or jittery (a polling loop wakes up a little early or late). Encoders and
//! players want a constant rate. [`CfrPacer`] maps source frames onto slots
//! `0, 1, 2, ...` at `slot_time(k) = k / fps`:
//!
//! * a frame whose timestamp is nearest to slot `n` is emitted *as* slot `n`;
//! * slots that received no frame are filled with a **duplicate** of the previous frame
//!   (the screen did not change, so repeating it is exact);
//! * a frame that maps to a slot that has already been filled (source faster than the
//!   target rate, or timestamp jitter) is **dropped as surplus**, but it still replaces
//!   the "current picture" so that the next duplicate shows the newest content.
//!
//! The pacer is pure logic (no clock, no threads) so it is unit- and property-tested.
//! [`CfrPacer::tick`] advances time without a frame: a static screen produces no frames
//! at all, so the caller ticks with the current time to make duplicates appear.

use std::time::Duration;

use crate::time::Fps;

/// One output slot of the constant-rate timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paced<T> {
    /// Slot index; its presentation time is [`Fps::slot_time`].
    pub slot: u64,
    /// The picture for this slot.
    pub item: T,
    /// `true` if this slot repeats the previous picture (no new source frame).
    pub duplicate: bool,
}

/// Counters kept by the pacer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacerStats {
    /// Frames pushed by the source.
    pub received: u64,
    /// Slots filled with a repeat of the previous frame.
    pub duplicated: u64,
    /// Source frames discarded because their slot was already filled.
    pub surplus_dropped: u64,
    /// Slots emitted in total (real + duplicate).
    pub emitted: u64,
}

/// Maps timestamped frames onto a constant-rate slot grid. See the module docs.
#[derive(Debug)]
pub struct CfrPacer<T> {
    fps: Fps,
    next_slot: u64,
    last: Option<T>,
    stats: PacerStats,
}

impl<T: Clone> CfrPacer<T> {
    /// A pacer for `fps`. Timestamps passed in are *recording time* (origin 0).
    pub fn new(fps: Fps) -> Self {
        Self { fps, next_slot: 0, last: None, stats: PacerStats::default() }
    }

    /// The target frame rate.
    pub fn fps(&self) -> Fps {
        self.fps
    }

    /// Index of the next slot that has not been emitted yet.
    pub fn next_slot(&self) -> u64 {
        self.next_slot
    }

    /// Counters.
    pub fn stats(&self) -> PacerStats {
        self.stats
    }

    /// `true` once at least one frame has been pushed.
    pub fn started(&self) -> bool {
        self.last.is_some()
    }

    /// A source frame captured at recording time `ts`. Appends the resulting slots
    /// (duplicates for skipped slots, then the frame itself) to `out`.
    pub fn push(&mut self, ts: Duration, item: T, out: &mut Vec<Paced<T>>) {
        self.stats.received += 1;
        let target = self.fps.slot_for(ts);
        if self.last.is_none() {
            // The first frame defines where the timeline starts.
            self.next_slot = target;
        }
        if target < self.next_slot {
            self.stats.surplus_dropped += 1;
            self.last = Some(item);
            return;
        }
        if let Some(prev) = &self.last {
            for slot in self.next_slot..target {
                self.stats.duplicated += 1;
                self.stats.emitted += 1;
                out.push(Paced { slot, item: prev.clone(), duplicate: true });
            }
        }
        self.stats.emitted += 1;
        out.push(Paced { slot: target, item: item.clone(), duplicate: false });
        self.next_slot = target + 1;
        self.last = Some(item);
    }

    /// Time has advanced to `now` (recording time) with no new frame: emits duplicates
    /// for every slot that can no longer receive a real frame, i.e. slots whose time is
    /// more than half a frame in the past.
    pub fn tick(&mut self, now: Duration, out: &mut Vec<Paced<T>>) {
        let Some(prev) = &self.last else { return };
        let half = self.fps.frame_duration() / 2;
        while self.fps.slot_time(self.next_slot) + half <= now {
            self.stats.duplicated += 1;
            self.stats.emitted += 1;
            out.push(Paced { slot: self.next_slot, item: prev.clone(), duplicate: true });
            self.next_slot += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn run(fps: Fps, frames: &[(u64, u32)], end_ms: u64) -> (Vec<Paced<u32>>, PacerStats) {
        let mut p = CfrPacer::new(fps);
        let mut out = Vec::new();
        for &(t, id) in frames {
            p.push(ms(t), id, &mut out);
        }
        p.tick(ms(end_ms), &mut out);
        (out, p.stats())
    }

    #[test]
    fn perfect_source_passes_through() {
        let frames: Vec<(u64, u32)> = (0..30).map(|i| (i * 1000 / 30, i as u32)).collect();
        let (out, stats) = run(Fps::FPS_30, &frames, 1000);
        assert_eq!(out.len(), 30);
        assert!(out.iter().all(|p| !p.duplicate));
        assert_eq!(out.iter().map(|p| p.slot).collect::<Vec<_>>(), (0..30).collect::<Vec<_>>());
        assert_eq!(stats.surplus_dropped, 0);
        assert_eq!(stats.duplicated, 0);
    }

    #[test]
    fn idle_screen_is_filled_with_duplicates() {
        // One frame, then nothing for a second (damage-driven source).
        let (out, stats) = run(Fps::FPS_30, &[(0, 7)], 1000);
        assert_eq!(out.len(), 30, "{:?}", out.iter().map(|p| p.slot).collect::<Vec<_>>());
        assert!(!out[0].duplicate);
        assert!(out[1..].iter().all(|p| p.duplicate && p.item == 7));
        assert_eq!(stats.duplicated, 29);
    }

    #[test]
    fn gap_between_frames_repeats_the_old_picture() {
        let (out, _) = run(Fps::FPS_30, &[(0, 1), (200, 2)], 200);
        // Slots 0..=5 (0..166 ms) show picture 1, slot 6 (200 ms) shows picture 2.
        assert_eq!(out.len(), 7);
        assert!(out[..6].iter().all(|p| p.item == 1));
        assert_eq!(out[6].item, 2);
        assert!(!out[6].duplicate);
    }

    #[test]
    fn faster_source_is_decimated_and_counted() {
        // 60 fps source into a 30 fps grid: every second frame is surplus.
        let frames: Vec<(u64, u32)> = (0..60).map(|i| (i * 1000 / 60, i as u32)).collect();
        let (out, stats) = run(Fps::FPS_30, &frames, 1000);
        assert_eq!(out.len(), 30);
        assert_eq!(stats.received, 60);
        assert_eq!(stats.surplus_dropped, 30);
        assert!(out.windows(2).all(|w| w[1].slot == w[0].slot + 1));
    }

    #[test]
    fn duplicate_shows_newest_dropped_content() {
        // Frame 2 is surplus for slot 0 but is newer than frame 1, so the following
        // duplicate must show it.
        let mut p = CfrPacer::new(Fps::FPS_30);
        let mut out = Vec::new();
        p.push(ms(0), 1, &mut out);
        p.push(ms(5), 2, &mut out);
        p.tick(ms(100), &mut out);
        assert_eq!(out[0].item, 1);
        assert!(out[1..].iter().all(|x| x.item == 2));
    }

    #[test]
    fn first_frame_defines_timeline_start() {
        let mut p = CfrPacer::new(Fps::FPS_30);
        let mut out = Vec::new();
        p.push(ms(100), 1u32, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].slot, 3);
    }

    #[test]
    fn tick_before_first_frame_emits_nothing() {
        let mut p: CfrPacer<u32> = CfrPacer::new(Fps::FPS_30);
        let mut out = Vec::new();
        p.tick(ms(5000), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn slots_are_strictly_increasing_for_any_input() {
        // Property test with a small deterministic LCG: random timestamps (monotonic with
        // jitter, including bursts and stalls) always yield strictly increasing slots
        // and every emitted slot is accounted for.
        let mut seed = 0x1234_5678_9abc_def0_u64;
        let mut rnd = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) as u32
        };
        for fps in [Fps::FPS_24, Fps::FPS_30, Fps::FPS_60, Fps::new(30_000, 1001)] {
            let mut p = CfrPacer::new(fps);
            let mut out = Vec::new();
            let mut t = 0u64;
            let mut id = 0u32;
            for _ in 0..5000 {
                t += u64::from(rnd() % 90); // 0..89 ms steps, sometimes 0 (burst)
                if rnd() % 50 == 0 {
                    t += 500; // stall
                }
                if rnd() % 4 == 0 {
                    p.tick(ms(t), &mut out);
                } else {
                    id += 1;
                    p.push(ms(t), id, &mut out);
                }
            }
            assert!(out.windows(2).all(|w| w[1].slot == w[0].slot + 1), "{fps}: gap or reorder");
            let s = p.stats();
            assert_eq!(s.emitted as usize, out.len());
            assert_eq!(s.emitted, s.duplicated + (s.received - s.surplus_dropped));
        }
    }

    #[test]
    fn ntsc_grid_has_no_drift_over_an_hour() {
        let fps = Fps::new(30_000, 1001);
        let mut p = CfrPacer::new(fps);
        let mut out = Vec::new();
        for k in 0..107_892u64 {
            p.push(fps.slot_time(k), k, &mut out);
        }
        assert_eq!(out.len(), 107_892);
        assert_eq!(p.stats().duplicated + p.stats().surplus_dropped, 0);
    }
}
