//! Clock-drift control for one audio source.
//!
//! A sound card's sample clock is never exactly the system clock: 50-500 ppm is normal,
//! which is 0.2-2 seconds over an hour. Left alone, audio and video would slide apart.
//! [`DriftController`] compares how many output frames a source has produced against how
//! many the *shared clock* says should exist by now, and returns a small ratio correction
//! for the resampler (a PI controller: the proportional part closes an error in a few
//! seconds, the integral part learns the device's steady skew so no residual offset is
//! left).
//! Large errors (an xrun, a stalled device) are not smoothed away but reported for a hard
//! resync: silence is inserted, or samples are dropped.
//!
//! Pure logic; the tests simulate skewed and jittery devices for a minute of stream time.

use std::time::Duration;

/// What the pipeline should do after an observation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DriftAction {
    /// Resample with this relative ratio (output rate / input rate, relative to nominal;
    /// `1.0` is no correction, `1.001` produces 0.1 % more output frames).
    Ratio(f64),
    /// The source is late by this many output frames: insert silence.
    InsertSilence(u64),
    /// The source is ahead by this many output frames: drop this many input frames.
    DropInput(u64),
}

/// Tunables of the controller.
#[derive(Debug, Clone, Copy)]
pub struct DriftConfig {
    /// Output sample rate.
    pub rate_out: u32,
    /// Time constant of the error low-pass filter (removes callback jitter).
    pub filter: Duration,
    /// Time in which a steady error is closed by the ratio correction.
    pub settle: Duration,
    /// Largest ratio correction (0.005 = +-0.5 %, inaudible).
    pub max_correction: f64,
    /// Errors below this are ignored (avoids chatter).
    pub dead_band: Duration,
    /// Errors above this trigger a hard resync instead of ratio control.
    pub resync_threshold: Duration,
}

impl DriftConfig {
    /// Defaults for a 48 kHz output.
    pub fn new(rate_out: u32) -> Self {
        Self {
            rate_out,
            filter: Duration::from_secs(1),
            settle: Duration::from_secs(6),
            max_correction: 0.005,
            dead_band: Duration::from_millis(4),
            resync_threshold: Duration::from_millis(80),
        }
    }
}

/// See the module docs.
#[derive(Debug)]
pub struct DriftController {
    cfg: DriftConfig,
    /// Timeline position (seconds) of the first chunk, the reference of the ideal count.
    start: Option<f64>,
    /// Output frames produced so far (resampler latency included by the caller).
    produced: f64,
    last_t: f64,
    smoothed: f64,
    /// Integral term: the learned steady skew of the device.
    integ: f64,
    ratio: f64,
    /// Number of hard resyncs so far (diagnostics).
    pub resyncs: u32,
}

impl DriftController {
    /// A controller with the given configuration.
    pub fn new(cfg: DriftConfig) -> Self {
        Self {
            cfg,
            start: None,
            produced: 0.0,
            last_t: 0.0,
            smoothed: 0.0,
            integ: 0.0,
            ratio: 1.0,
            resyncs: 0,
        }
    }

    /// The current correction ratio.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Smoothed error in seconds (positive: more output produced than the clock allows).
    pub fn error_seconds(&self) -> f64 {
        self.smoothed
    }

    /// Reports that a chunk starting at timeline time `t` arrived. `produced_total` is
    /// the total number of output frames produced from all earlier chunks, plus the
    /// resampler's pending latency. Returns what to do with this chunk.
    pub fn observe(&mut self, t: Duration, produced_total: u64) -> DriftAction {
        let t = t.as_secs_f64();
        let rate = f64::from(self.cfg.rate_out);
        let start = *self.start.get_or_insert(t);
        self.produced = produced_total as f64;
        let ideal = (t - start) * rate;
        let err_frames = self.produced - ideal;
        let err = err_frames / rate;

        if err.abs() > self.cfg.resync_threshold.as_secs_f64() {
            // Too large to be clock skew: a glitch. Fix it right away and start over.
            self.resyncs += 1;
            self.smoothed = 0.0;
            self.ratio = 1.0 + self.integ;
            self.last_t = t;
            let frames = err_frames.abs().round() as u64;
            return if err_frames < 0.0 {
                DriftAction::InsertSilence(frames)
            } else {
                DriftAction::DropInput(frames)
            };
        }

        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let alpha = 1.0 - (-dt / self.cfg.filter.as_secs_f64()).exp();
        self.smoothed += alpha * (err - self.smoothed);

        let dead = self.cfg.dead_band.as_secs_f64();
        let effective = if self.smoothed.abs() <= dead {
            0.0
        } else {
            self.smoothed - dead.copysign(self.smoothed)
        };
        // Second-order loop, critically damped: natural frequency 1.5 / settle.
        // Too many frames produced -> slow the output down (ratio below 1).
        let w = 1.5 / self.cfg.settle.as_secs_f64();
        let max = self.cfg.max_correction;
        self.integ = (self.integ - w * w * effective * dt).clamp(-max, max);
        let corr = (self.integ - 2.0 * w * effective).clamp(-max, max);
        self.ratio = 1.0 + corr;
        DriftAction::Ratio(self.ratio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sim {
        /// Final smoothed error in seconds.
        err: f64,
        /// Mean ratio over the last 10 s.
        mean_ratio: f64,
        /// Largest deviation of the ratio from the ideal one after the first 20 s.
        peak_dev: f64,
        /// Largest deviation over the whole run.
        peak_dev_all: f64,
        ctl: DriftController,
    }

    /// Simulates a source whose device clock runs `skew` fast (0.001 = 0.1 %), delivering
    /// 10 ms chunks, for `seconds` of shared-clock time. Timestamps carry uniform jitter
    /// of `+-jitter_ms`; the first chunk is stamped `first_early_ms` too early (a real
    /// effect: device latency is unknown when the stream starts).
    fn simulate(skew: f64, jitter_ms: f64, first_early_ms: f64, seconds: f64) -> Sim {
        let rate_in = 48_000.0;
        let mut c = DriftController::new(DriftConfig::new(48_000));
        let chunk_in = 480.0; // frames
        let ideal_ratio = 1.0 / (1.0 + skew);
        let mut produced = 0.0f64; // output frames so far (fractional, ideal resampler)
        let mut t_dev = 0.0f64; // device time = sample count / rate
        let mut seed = 1u64;
        let mut rnd = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) as f64 / f64::from(1u32 << 31)) * 2.0 - 1.0
        };
        let (mut ratio_sum, mut ratio_n) = (0.0, 0);
        let mut ratio = 1.0f64;
        let (mut peak, mut peak_all) = (0.0f64, 0.0f64);
        let mut first = true;
        // The true start of the stream is 1 s into the clock so an early stamp stays positive.
        let base = 1.0;
        loop {
            // Shared-clock time of the chunk's first sample: the device produces samples
            // (1 + skew) times faster than real time.
            let mut t_clock = base + t_dev / (1.0 + skew) + rnd() * jitter_ms / 1000.0;
            if first {
                t_clock -= first_early_ms / 1000.0;
                first = false;
            }
            if t_clock > base + seconds {
                break;
            }
            match c.observe(Duration::from_secs_f64(t_clock), produced.round() as u64) {
                DriftAction::Ratio(r) => ratio = r,
                DriftAction::InsertSilence(n) => produced += n as f64,
                DriftAction::DropInput(n) => produced -= n as f64,
            }
            produced += chunk_in * ratio;
            t_dev += chunk_in / rate_in;
            let dev = (ratio - ideal_ratio).abs();
            peak_all = peak_all.max(dev);
            if t_clock > base + 20.0 {
                peak = peak.max(dev);
            }
            if t_clock > base + seconds - 10.0 {
                ratio_sum += ratio;
                ratio_n += 1;
            }
        }
        Sim {
            err: c.error_seconds(),
            mean_ratio: ratio_sum / f64::from(ratio_n.max(1)),
            peak_dev: peak,
            peak_dev_all: peak_all,
            ctl: c,
        }
    }

    #[test]
    fn perfect_clock_needs_no_correction() {
        let s = simulate(0.0, 0.0, 0.0, 60.0);
        assert!(s.err.abs() < 0.001, "{}", s.err);
        assert!((s.mean_ratio - 1.0).abs() < 1e-4, "{}", s.mean_ratio);
        assert!(s.peak_dev_all < 1e-6, "an exact clock must never be touched: {}", s.peak_dev_all);
        assert_eq!(s.ctl.resyncs, 0);
    }

    #[test]
    fn fast_and_slow_devices_are_tracked_to_sample_accuracy() {
        for skew in [0.0002, -0.0002, 0.001, -0.001, 0.003, -0.003] {
            let s = simulate(skew, 0.0, 0.0, 120.0);
            // A device running `skew` fast delivers more samples per real second, so
            // the output ratio must go *below* 1 by the same amount.
            let ideal = 1.0 / (1.0 + skew);
            assert!((s.mean_ratio - ideal).abs() < 3e-4, "skew {skew}: ratio {}", s.mean_ratio);
            assert!(s.err.abs() < 0.008, "skew {skew}: residual error {} s", s.err);
            assert_eq!(s.ctl.resyncs, 0, "skew {skew}");
        }
    }

    #[test]
    fn callback_jitter_does_not_wobble_the_pitch() {
        for skew in [0.0, 0.001, -0.001] {
            let s = simulate(skew, 4.0, 0.0, 120.0);
            assert!(s.err.abs() < 0.008, "skew {skew}: {}", s.err);
            assert_eq!(s.ctl.resyncs, 0, "jitter alone must not cause a resync");
            // After settling, the ratio stays within 0.1 % of the ideal despite +-4 ms
            // timestamp noise (0.1 % is 1.7 cents of pitch, inaudible).
            assert!(s.peak_dev < 0.001, "skew {skew}: peak deviation {}", s.peak_dev);
        }
    }

    #[test]
    fn a_wrong_first_timestamp_is_absorbed_gently() {
        // Device latency is unknown at stream start, so the first chunk may be stamped
        // several milliseconds off. For a few milliseconds the controller must not let that
        // show as a pitch shift beyond 0.3 % (a larger error may use the full 0.5 %), and
        // must have removed the offset a minute later.
        for early in [7.0, -7.0, 20.0] {
            let s = simulate(0.0, 2.0, early, 90.0);
            let limit = if early.abs() <= 8.0 { 0.003 } else { 0.0051 };
            assert!(s.peak_dev_all < limit, "first stamp {early} ms off: peak {}", s.peak_dev_all);
            assert!(s.err.abs() < 0.006, "residual {} after {early} ms", s.err);
            assert_eq!(s.ctl.resyncs, 0);
        }
    }

    #[test]
    fn a_gap_triggers_a_hard_resync_with_the_right_length() {
        let mut c = DriftController::new(DriftConfig::new(48_000));
        // Steady chunks...
        let mut produced = 0u64;
        for k in 0..100u64 {
            let t = Duration::from_millis(k * 10);
            assert!(matches!(c.observe(t, produced), DriftAction::Ratio(_)));
            produced += 480;
        }
        // ...then the device stalls for 250 ms: the next chunk is 250 ms late.
        let t = Duration::from_millis(100 * 10 + 250);
        match c.observe(t, produced) {
            DriftAction::InsertSilence(n) => {
                assert!((11_800..=12_200).contains(&n), "asked for {n} frames of silence");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.resyncs, 1);
    }

    #[test]
    fn a_burst_ahead_of_the_clock_drops_input() {
        let mut c = DriftController::new(DriftConfig::new(48_000));
        let mut produced = 0u64;
        for k in 0..50u64 {
            c.observe(Duration::from_millis(k * 10), produced);
            produced += 480;
        }
        // 300 ms of samples arrive stamped 100 ms later than they should.
        let t = Duration::from_millis(500 - 300);
        match c.observe(t, produced) {
            DriftAction::DropInput(n) => assert!(n > 4000, "{n}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn correction_is_capped() {
        let cfg =
            DriftConfig { resync_threshold: Duration::from_secs(10), ..DriftConfig::new(48_000) };
        let mut c = DriftController::new(cfg);
        c.observe(Duration::ZERO, 0);
        // 2 s behind: ratio would be huge, must be clamped to +0.5 %.
        for k in 1..200u64 {
            c.observe(Duration::from_millis(k * 10), 0);
        }
        assert!((c.ratio() - 1.005).abs() < 1e-9, "{}", c.ratio());
    }
}
