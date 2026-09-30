//! Mixing several audio sources on the recording timeline.
//!
//! Each source (lane) delivers samples tagged with their **timeline position** in frames
//! (already drift-corrected and resampled, see [`super::pipeline`]). The mixer keeps one
//! buffer per lane and emits the sum for every timeline range that all *live* lanes have
//! reached, so two sources are added sample-accurately whatever order and burstiness they
//! arrive in.
//!
//! Waiting for the slowest lane must not let one broken device freeze the recording, so a
//! lane that has not delivered anything for `stall` (wall time) stops holding the output
//! back: it is treated as silent until it produces data again (data that arrives after
//! its slot was already emitted is discarded). Lanes that ended are simply silent. The
//! rules are deterministic given the push/pop sequence, which is what the tests exploit.

use std::time::{Duration, Instant};

#[derive(Debug)]
struct Lane {
    /// Timeline frame index of `buf[0]`.
    base: u64,
    buf: Vec<f32>,
    ended: bool,
    created: Instant,
    last_push: Option<Instant>,
}

impl Lane {
    fn end(&self, ch: usize) -> u64 {
        self.base + (self.buf.len() / ch) as u64
    }
}

/// See the module docs.
#[derive(Debug)]
pub struct Mixer {
    ch: usize,
    next_pos: u64,
    lanes: Vec<Lane>,
    stall: Duration,
    /// Frames discarded because they arrived after their slot was emitted.
    pub late_frames: u64,
    /// Frames of silence inserted inside lanes to bridge gaps.
    pub gap_frames: u64,
}

/// Soft limiter: transparent below 0.8, smoothly saturating above, never beyond 1.0.
fn limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.8 { x } else { x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()) }
}

impl Mixer {
    /// A mixer for `lanes` sources of `channels` interleaved channels. `now` starts the
    /// stall clock of every lane.
    pub fn new(lanes: usize, channels: usize, stall: Duration, now: Instant) -> Self {
        Self {
            ch: channels.max(1),
            next_pos: 0,
            lanes: (0..lanes)
                .map(|_| Lane {
                    base: 0,
                    buf: Vec::new(),
                    ended: false,
                    created: now,
                    last_push: None,
                })
                .collect(),
            stall,
            late_frames: 0,
            gap_frames: 0,
        }
    }

    /// Timeline frame index of the next frame to be emitted.
    pub fn next_pos(&self) -> u64 {
        self.next_pos
    }

    /// Adds `samples` (interleaved) to `lane`, starting at timeline frame `pos`.
    pub fn push(&mut self, lane: usize, pos: u64, samples: &[f32], now: Instant) {
        let ch = self.ch;
        let next_pos = self.next_pos;
        let Some(l) = self.lanes.get_mut(lane) else { return };
        l.last_push = Some(now);
        let frames = (samples.len() / ch) as u64;
        if frames == 0 {
            return;
        }
        let mut pos = pos;
        let mut samples = samples;
        // Already emitted: drop the late part.
        if pos < next_pos {
            let late = (next_pos - pos).min(frames);
            self.late_frames += late;
            samples = &samples[late as usize * ch..];
            pos += late;
            if samples.is_empty() {
                return;
            }
        }
        if l.buf.is_empty() {
            l.base = pos.max(next_pos);
        }
        let end = l.end(ch);
        if pos > end {
            let gap = pos - end;
            self.gap_frames += gap;
            l.buf.resize(l.buf.len() + gap as usize * ch, 0.0);
        } else if pos < end {
            // Overlap with what the lane already holds: keep the existing samples.
            let overlap = (end - pos).min((samples.len() / ch) as u64);
            self.late_frames += overlap;
            samples = &samples[overlap as usize * ch..];
        }
        l.buf.extend_from_slice(samples);
    }

    /// Marks `lane` finished: it no longer holds the output back.
    pub fn end_lane(&mut self, lane: usize) {
        if let Some(l) = self.lanes.get_mut(lane) {
            l.ended = true;
        }
    }

    fn is_live(&self, l: &Lane, now: Instant) -> bool {
        if l.ended {
            return false;
        }
        let since = l.last_push.unwrap_or(l.created);
        now.saturating_duration_since(since) <= self.stall
    }

    /// The timeline position up to which output can be emitted now.
    fn watermark(&self, now: Instant) -> u64 {
        let ch = self.ch;
        let live_end = self
            .lanes
            .iter()
            .filter(|l| self.is_live(l, now))
            .map(|l| l.end(ch).max(self.next_pos))
            .min();
        match live_end {
            Some(w) => w,
            // Nothing live: flush whatever any lane holds.
            None => self
                .lanes
                .iter()
                .map(|l| l.end(ch))
                .max()
                .unwrap_or(self.next_pos)
                .max(self.next_pos),
        }
    }

    /// Emits everything that is ready (see the module docs) into `out`; returns the
    /// number of frames appended.
    pub fn pop_ready(&mut self, now: Instant, out: &mut Vec<f32>) -> usize {
        let w = self.watermark(now);
        self.emit_to(w, out)
    }

    /// Emits up to timeline frame `up_to` unconditionally, padding with silence where
    /// lanes have no data (used at the end of a recording).
    pub fn flush(&mut self, up_to: u64, out: &mut Vec<f32>) -> usize {
        self.emit_to(up_to, out)
    }

    fn emit_to(&mut self, to: u64, out: &mut Vec<f32>) -> usize {
        if to <= self.next_pos {
            return 0;
        }
        let ch = self.ch;
        let n = (to - self.next_pos) as usize;
        let start = out.len();
        out.resize(start + n * ch, 0.0);
        for l in &mut self.lanes {
            if l.buf.is_empty() {
                l.base = l.base.max(to);
                continue;
            }
            let lane_end = l.end(ch);
            // Where this lane's data overlaps [next_pos, to).
            let from = l.base.max(self.next_pos);
            let upto = lane_end.min(to);
            if upto > from {
                let dst = &mut out[start + (from - self.next_pos) as usize * ch..];
                let src = &l.buf[(from - l.base) as usize * ch..(upto - l.base) as usize * ch];
                for (d, s) in dst.iter_mut().zip(src) {
                    *d += *s;
                }
            }
            // Consume.
            if lane_end <= to {
                l.buf.clear();
                l.base = to;
            } else if l.base < to {
                l.buf.drain(..(to - l.base) as usize * ch);
                l.base = to;
            }
        }
        for s in &mut out[start..] {
            *s = limit(*s);
        }
        self.next_pos = to;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STALL: Duration = Duration::from_millis(500);

    fn mono(v: f32, n: usize) -> Vec<f32> {
        vec![v; n]
    }

    fn mixer(lanes: usize) -> (Mixer, Instant) {
        let t0 = Instant::now();
        (Mixer::new(lanes, 1, STALL, t0), t0)
    }

    #[test]
    fn two_lanes_add_sample_accurately() {
        let (mut m, t0) = mixer(2);
        m.push(0, 0, &mono(0.2, 100), t0);
        // Lane 1 hasn't arrived: nothing may be emitted yet.
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0, &mut out), 0);
        m.push(1, 0, &mono(0.3, 60), t0);
        assert_eq!(m.pop_ready(t0, &mut out), 60);
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6));
        // The rest of lane 0 waits for lane 1.
        m.push(1, 60, &mono(0.1, 40), t0);
        out.clear();
        assert_eq!(m.pop_ready(t0, &mut out), 40);
        assert!(out.iter().all(|s| (s - 0.3).abs() < 1e-6));
        assert_eq!(m.next_pos(), 100);
    }

    #[test]
    fn lanes_are_aligned_by_position_not_arrival_order() {
        let (mut m, t0) = mixer(2);
        // Lane 1 starts 50 frames later than lane 0.
        m.push(1, 50, &mono(0.4, 50), t0);
        m.push(0, 0, &mono(0.1, 100), t0);
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0, &mut out), 100);
        assert!(out[..50].iter().all(|s| (s - 0.1).abs() < 1e-6));
        assert!(out[50..].iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn a_stalled_lane_stops_blocking_and_late_data_is_dropped() {
        let (mut m, t0) = mixer(2);
        m.push(0, 0, &mono(0.2, 100), t0);
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0 + STALL / 2, &mut out), 0, "lane 1 still within its grace time");
        // Both lanes are stalled by now, so nothing waits: what lane 0 holds is emitted.
        assert_eq!(m.pop_ready(t0 + STALL * 2, &mut out), 100);
        // Lane 0 keeps delivering, lane 1 never does: output follows lane 0 alone.
        let t1 = t0 + STALL * 2;
        m.push(0, 100, &mono(0.2, 50), t1);
        assert_eq!(m.pop_ready(t1, &mut out), 50);
        assert_eq!(out.len(), 150);
        assert!(out.iter().all(|s| (s - 0.2).abs() < 1e-6));
        // Lane 1 wakes up with data for an already emitted range.
        m.push(1, 0, &mono(0.3, 200), t1);
        assert_eq!(m.late_frames, 150);
        out.clear();
        m.push(0, 150, &mono(0.2, 50), t1);
        assert_eq!(m.pop_ready(t1, &mut out), 50);
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6), "the surviving 50 frames are mixed");
    }

    #[test]
    fn ended_lanes_do_not_hold_output_back() {
        let (mut m, t0) = mixer(2);
        m.push(0, 0, &mono(0.25, 80), t0);
        m.end_lane(1);
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0, &mut out), 80);
        m.end_lane(0);
        assert_eq!(m.pop_ready(t0, &mut out), 0);
    }

    #[test]
    fn gaps_inside_a_lane_become_silence() {
        let (mut m, t0) = mixer(1);
        m.push(0, 0, &mono(0.5, 10), t0);
        m.push(0, 30, &mono(0.5, 10), t0);
        assert_eq!(m.gap_frames, 20);
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0, &mut out), 40);
        assert!(out[10..30].iter().all(|s| *s == 0.0));
        assert!(out[30..].iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn overlapping_pushes_keep_the_first_samples() {
        let (mut m, t0) = mixer(1);
        m.push(0, 0, &mono(0.1, 20), t0);
        m.push(0, 10, &mono(0.6, 20), t0);
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0, &mut out), 30);
        assert!(out[..20].iter().all(|s| (s - 0.1).abs() < 1e-6));
        assert!(out[20..].iter().all(|s| (s - 0.6).abs() < 1e-6));
        assert_eq!(m.late_frames, 10);
    }

    #[test]
    fn flush_pads_with_silence_to_the_requested_end() {
        let (mut m, t0) = mixer(1);
        m.push(0, 0, &mono(0.5, 10), t0);
        let mut out = Vec::new();
        assert_eq!(m.flush(25, &mut out), 25);
        assert!(out[..10].iter().all(|s| (s - 0.5).abs() < 1e-6));
        assert!(out[10..].iter().all(|s| *s == 0.0));
        assert_eq!(m.flush(25, &mut out), 0);
    }

    #[test]
    fn loud_mixes_are_limited_not_wrapped() {
        let t0 = Instant::now();
        let mut m = Mixer::new(2, 1, STALL, t0);
        m.push(0, 0, &mono(0.9, 10), t0);
        m.push(1, 0, &mono(0.9, 10), t0);
        let mut out = Vec::new();
        m.pop_ready(t0, &mut out);
        assert!(out.iter().all(|s| *s > 0.9 && *s <= 1.0), "{out:?}");
        assert_eq!(limit(0.5), 0.5);
        assert!(limit(-3.0) >= -1.0 && limit(-3.0) < -0.99);
    }

    #[test]
    fn stereo_frames_stay_interleaved() {
        let t0 = Instant::now();
        let mut m = Mixer::new(2, 2, STALL, t0);
        m.push(0, 0, &[0.1, 0.2, 0.1, 0.2], t0);
        m.push(1, 0, &[0.3, 0.0, 0.3, 0.0], t0);
        let mut out = Vec::new();
        assert_eq!(m.pop_ready(t0, &mut out), 2);
        assert_eq!(out.len(), 4);
        assert!((out[0] - 0.4).abs() < 1e-6 && (out[1] - 0.2).abs() < 1e-6);
    }
}
