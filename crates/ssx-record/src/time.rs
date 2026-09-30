//! Frame rates and the shared monotonic clock.
//!
//! Every timestamp in a recording is a [`Duration`] on **one** [`Clock`] that is created
//! when the session starts and handed to the video and the audio sources. Using a single
//! monotonic clock (not wall time, which can jump) is what makes A/V sync a matter of
//! arithmetic instead of luck. Frame rates are exact rationals ([`Fps`]) so 29.97 fps does
//! not accumulate rounding error over an hour.

use std::time::{Duration, Instant};

const NANOS: u128 = 1_000_000_000;

/// An exact frame rate `num / den` (frames per second).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fps {
    num: u32,
    den: u32,
}

impl Fps {
    /// 24 fps.
    pub const FPS_24: Fps = Fps { num: 24, den: 1 };
    /// 30 fps.
    pub const FPS_30: Fps = Fps { num: 30, den: 1 };
    /// 60 fps.
    pub const FPS_60: Fps = Fps { num: 60, den: 1 };

    /// `num / den` frames per second. A zero numerator or denominator is replaced by 1
    /// so the value is always usable.
    pub const fn new(num: u32, den: u32) -> Self {
        Self { num: if num == 0 { 1 } else { num }, den: if den == 0 { 1 } else { den } }
    }

    /// Whole frames per second.
    pub const fn from_int(fps: u32) -> Self {
        Self::new(fps, 1)
    }

    /// Converts a float, recognising the NTSC rates (29.97, 59.94, 23.976) exactly.
    /// Returns `None` for non-finite, non-positive or absurd (> 1000) values.
    pub fn from_f64(fps: f64) -> Option<Self> {
        if !fps.is_finite() || fps <= 0.0 || fps > 1000.0 {
            return None;
        }
        for (nominal, ntsc_num) in [(23.976, 24_000), (29.97, 30_000), (59.94, 60_000)] {
            if (fps - nominal).abs() < 0.005 {
                return Some(Self::new(ntsc_num, 1001));
            }
        }
        if (fps - fps.round()).abs() < 1e-6 {
            return Some(Self::from_int(fps.round() as u32));
        }
        // Milli-fps resolution is plenty for anything a user types.
        Some(Self::new((fps * 1000.0).round() as u32, 1000))
    }

    /// Numerator.
    pub const fn num(self) -> u32 {
        self.num
    }

    /// Denominator.
    pub const fn den(self) -> u32 {
        self.den
    }

    /// As a float.
    pub fn as_f64(self) -> f64 {
        f64::from(self.num) / f64::from(self.den)
    }

    /// Duration of one frame (rounded to the nearest nanosecond).
    pub fn frame_duration(self) -> Duration {
        self.slot_time(1)
    }

    /// Presentation time of frame slot `k` (`k * den / num` seconds), exact to 1 ns.
    pub fn slot_time(self, k: u64) -> Duration {
        let ns = (u128::from(k) * u128::from(self.den) * NANOS + u128::from(self.num) / 2)
            / u128::from(self.num);
        Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
    }

    /// The slot whose presentation time is nearest to `t`.
    pub fn slot_for(self, t: Duration) -> u64 {
        let d = u128::from(self.den) * NANOS;
        let k = (t.as_nanos() * u128::from(self.num) + d / 2) / d;
        u64::try_from(k).unwrap_or(u64::MAX)
    }
}

impl Default for Fps {
    fn default() -> Self {
        Self::FPS_30
    }
}

impl std::fmt::Display for Fps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.den == 1 { write!(f, "{}", self.num) } else { write!(f, "{:.3}", self.as_f64()) }
    }
}

/// The monotonic clock shared by all sources of one recording. Cheap to clone.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    origin: Instant,
}

impl Clock {
    /// Starts a new clock at "now".
    pub fn start() -> Self {
        Self { origin: Instant::now() }
    }

    /// Time elapsed since the clock started.
    pub fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    /// The clock time of an [`Instant`] (saturating at zero for instants before start).
    pub fn at(&self, instant: Instant) -> Duration {
        instant.saturating_duration_since(self.origin)
    }

    /// Sleeps until `deadline` (clock time). Returns immediately if it has passed.
    ///
    /// Sleeps in one call for long waits and spins the last 200 microseconds, which is
    /// what keeps a 60 fps polling loop within about a millisecond of its grid on both
    /// Linux and Windows (whose timers are coarser).
    pub fn sleep_until(&self, deadline: Duration) {
        const SPIN: Duration = Duration::from_micros(200);
        let now = self.now();
        if let Some(wait) = deadline.checked_sub(now)
            && wait > SPIN
        {
            std::thread::sleep(wait.checked_sub(SPIN).unwrap());
        }
        while self.now() < deadline {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntsc_rates_are_exact() {
        assert_eq!(Fps::from_f64(29.97), Some(Fps::new(30_000, 1001)));
        assert_eq!(Fps::from_f64(59.94), Some(Fps::new(60_000, 1001)));
        assert_eq!(Fps::from_f64(30.0), Some(Fps::FPS_30));
        assert_eq!(Fps::from_f64(12.5), Some(Fps::new(12_500, 1000)));
        assert_eq!(Fps::from_f64(0.0), None);
        assert_eq!(Fps::from_f64(f64::NAN), None);
        assert_eq!(Fps::from_f64(5000.0), None);
    }

    #[test]
    fn slots_round_trip() {
        for fps in [Fps::FPS_24, Fps::FPS_30, Fps::FPS_60, Fps::new(30_000, 1001), Fps::new(7, 3)] {
            for k in [0u64, 1, 2, 29, 30, 1234, 1_000_000] {
                assert_eq!(fps.slot_for(fps.slot_time(k)), k, "{fps} slot {k}");
            }
        }
    }

    #[test]
    fn slot_time_has_no_drift() {
        // One hour of 29.97 fps: slot n is placed by exact rational arithmetic, so the
        // error is at most the 1 ns rounding, not n times a rounded frame duration.
        let fps = Fps::new(30_000, 1001);
        let k = 30_000 * 3600 / 1001 * 1001 / 1000;
        let exact = k as f64 * 1001.0 / 30_000.0;
        let got = fps.slot_time(k).as_secs_f64();
        assert!((got - exact).abs() < 1e-8, "{got} vs {exact}");
    }

    #[test]
    fn slot_for_rounds_to_nearest() {
        let fps = Fps::FPS_30; // 33.33 ms
        assert_eq!(fps.slot_for(Duration::from_millis(16)), 0);
        assert_eq!(fps.slot_for(Duration::from_millis(17)), 1);
        assert_eq!(fps.slot_for(Duration::from_millis(50)), 2);
    }

    #[test]
    fn zero_components_are_sanitised() {
        let f = Fps::new(0, 0);
        assert_eq!((f.num(), f.den()), (1, 1));
    }

    #[test]
    fn clock_sleep_until_is_accurate() {
        let c = Clock::start();
        let target = Duration::from_millis(20);
        c.sleep_until(target);
        let now = c.now();
        assert!(now >= target && now < target + Duration::from_millis(15), "{now:?}");
        // A deadline in the past returns immediately.
        let t = Instant::now();
        c.sleep_until(Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(5));
    }
}
