//! Synthetic audio sources for tests and demos: a test tone, silence, and fault injection.
//!
//! Like the synthetic video source it has a [`TimeMode::Virtual`] mode (chunk `k` is
//! stamped exactly `k * chunk` and produced instantly) and a [`TimeMode::Realtime`] mode
//! (paced on the session clock). Realtime mode can simulate a device whose sample clock
//! runs fast or slow (`skew`) and a device that dies mid-recording (`fail_after`), the
//! two situations the drift controller and the degrade-to-video-only path exist for.

use std::time::Duration;

use super::{AudioChunk, AudioEvent, AudioFormat, AudioSource};
use crate::{error::AudioError, source::synthetic::TimeMode, time::Clock};

/// What the source plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// All zeros.
    Silence,
    /// A sine tone that starts `start` after the beginning of the stream.
    Tone {
        /// Frequency in Hz.
        hz: f32,
        /// Peak amplitude (0-1).
        amplitude: f32,
        /// Stream time at which the tone starts (silence before).
        start: Duration,
    },
}

/// Configuration of a [`SyntheticAudio`] source.
#[derive(Debug, Clone)]
pub struct SyntheticAudioConfig {
    /// Native format.
    pub format: AudioFormat,
    /// The signal.
    pub signal: Signal,
    /// Time mode.
    pub time: TimeMode,
    /// Stop after this much audio (`None`: endless).
    pub total: Option<Duration>,
    /// Device clock error: `0.001` produces samples 0.1 % faster than real time.
    pub skew: f64,
    /// Chunk length.
    pub chunk: Duration,
    /// Fail with an error once this much audio has been produced.
    pub fail_after: Option<Duration>,
    /// Fail in `start` (no device).
    pub fail_on_start: bool,
    /// Shared-clock time of the first sample (a source that starts late).
    pub start_at: Duration,
}

impl SyntheticAudioConfig {
    /// A 48 kHz stereo source playing `signal` in `time` mode.
    pub fn new(signal: Signal, time: TimeMode) -> Self {
        Self {
            format: AudioFormat::STEREO_48K,
            signal,
            time,
            total: None,
            skew: 0.0,
            chunk: Duration::from_millis(10),
            fail_after: None,
            fail_on_start: false,
            start_at: Duration::ZERO,
        }
    }
}

/// The synthetic [`AudioSource`].
#[derive(Debug)]
pub struct SyntheticAudio {
    cfg: SyntheticAudioConfig,
    clock: Option<Clock>,
    origin: Duration,
    produced_frames: u64,
    started: bool,
}

impl SyntheticAudio {
    /// Creates the source; nothing runs until `start`.
    pub fn new(cfg: SyntheticAudioConfig) -> Self {
        Self { cfg, clock: None, origin: Duration::ZERO, produced_frames: 0, started: false }
    }

    fn chunk_frames(&self) -> u64 {
        ((self.cfg.chunk.as_secs_f64() * f64::from(self.cfg.format.sample_rate)) as u64).max(1)
    }

    fn render(&self, first_frame: u64, frames: u64) -> Vec<f32> {
        let ch = usize::from(self.cfg.format.channels);
        let rate = f64::from(self.cfg.format.sample_rate);
        let mut out = Vec::with_capacity(frames as usize * ch);
        for i in 0..frames {
            let idx = first_frame + i;
            let t = idx as f64 / rate;
            let s = match self.cfg.signal {
                Signal::Silence => 0.0,
                Signal::Tone { hz, amplitude, start } => {
                    if t >= start.as_secs_f64() {
                        (t * f64::from(hz) * std::f64::consts::TAU).sin() as f32 * amplitude
                    } else {
                        0.0
                    }
                }
            };
            out.extend(std::iter::repeat_n(s, ch));
        }
        out
    }
}

impl AudioSource for SyntheticAudio {
    fn name(&self) -> String {
        "synthetic audio".to_owned()
    }

    fn start(&mut self, clock: Clock) -> Result<AudioFormat, AudioError> {
        if self.cfg.fail_on_start {
            return Err(AudioError::NoDevice("synthetic device configured to fail".into()));
        }
        self.origin = if self.cfg.time == TimeMode::Realtime {
            clock.now() + self.cfg.start_at
        } else {
            self.cfg.start_at
        };
        self.clock = Some(clock);
        self.produced_frames = 0;
        self.started = true;
        Ok(self.cfg.format)
    }

    fn read(&mut self, timeout: Duration) -> Result<AudioEvent, AudioError> {
        let Some(clock) = self.clock else {
            return Err(AudioError::Backend {
                backend: "synthetic",
                message: "read before start".into(),
            });
        };
        if !self.started {
            return Ok(AudioEvent::Ended);
        }
        let rate = f64::from(self.cfg.format.sample_rate);
        let done_secs = self.produced_frames as f64 / rate;
        if self.cfg.fail_after.is_some_and(|f| done_secs >= f.as_secs_f64()) {
            return Err(AudioError::Backend {
                backend: "synthetic",
                message: "device removed (injected fault)".into(),
            });
        }
        if self.cfg.total.is_some_and(|t| done_secs >= t.as_secs_f64()) {
            return Ok(AudioEvent::Ended);
        }
        let frames = self.chunk_frames();
        // Shared-clock time of the first sample of this chunk: a device running `skew`
        // fast delivers its k-th sample earlier on the shared clock.
        let ts = self.origin + Duration::from_secs_f64(done_secs / (1.0 + self.cfg.skew));
        if self.cfg.time == TimeMode::Realtime {
            // A chunk is available once its last sample has been captured.
            let ready = ts + Duration::from_secs_f64(frames as f64 / rate / (1.0 + self.cfg.skew));
            let deadline = clock.now() + timeout;
            if ready > deadline {
                clock.sleep_until(deadline);
                return Ok(AudioEvent::Timeout);
            }
            clock.sleep_until(ready);
        }
        let samples = self.render(self.produced_frames, frames);
        self.produced_frames += frames;
        Ok(AudioEvent::Chunk(AudioChunk { samples, timestamp: ts }))
    }

    fn stop(&mut self) {
        self.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_chunks_are_stamped_exactly_and_end() {
        let mut cfg = SyntheticAudioConfig::new(Signal::Silence, TimeMode::Virtual);
        cfg.total = Some(Duration::from_millis(50));
        let mut s = SyntheticAudio::new(cfg);
        s.start(Clock::start()).unwrap();
        let mut stamps = Vec::new();
        let mut frames = 0;
        while let AudioEvent::Chunk(c) = s.read(Duration::from_secs(1)).unwrap() {
            stamps.push(c.timestamp);
            frames += c.samples.len() / 2;
        }
        assert_eq!(stamps.len(), 5);
        assert_eq!(stamps[3], Duration::from_millis(30));
        assert_eq!(frames, 2400);
    }

    #[test]
    fn tone_starts_at_the_requested_sample() {
        let cfg = SyntheticAudioConfig::new(
            Signal::Tone { hz: 1000.0, amplitude: 0.5, start: Duration::from_millis(20) },
            TimeMode::Virtual,
        );
        let s = SyntheticAudio::new(cfg);
        let x = s.render(0, 2400);
        // 20 ms = 960 frames of silence, then the tone.
        assert!(x[..960 * 2].iter().all(|v| *v == 0.0));
        assert!(x[960 * 2..].iter().any(|v| v.abs() > 0.4));
    }

    #[test]
    fn realtime_mode_paces_to_the_clock() {
        let mut s =
            SyntheticAudio::new(SyntheticAudioConfig::new(Signal::Silence, TimeMode::Realtime));
        let clock = Clock::start();
        s.start(clock).unwrap();
        let mut n = 0;
        while n < 10 {
            if let AudioEvent::Chunk(_) = s.read(Duration::from_millis(50)).unwrap() {
                n += 1;
            }
        }
        let t = clock.now();
        assert!(t >= Duration::from_millis(95) && t < Duration::from_millis(140), "{t:?}");
    }

    #[test]
    fn fault_injection() {
        let mut cfg = SyntheticAudioConfig::new(Signal::Silence, TimeMode::Virtual);
        cfg.fail_after = Some(Duration::from_millis(30));
        let mut s = SyntheticAudio::new(cfg);
        s.start(Clock::start()).unwrap();
        for _ in 0..3 {
            assert!(matches!(s.read(Duration::ZERO), Ok(AudioEvent::Chunk(_))));
        }
        assert!(s.read(Duration::ZERO).is_err());
        let mut cfg = SyntheticAudioConfig::new(Signal::Silence, TimeMode::Virtual);
        cfg.fail_on_start = true;
        assert!(SyntheticAudio::new(cfg).start(Clock::start()).is_err());
    }
}
