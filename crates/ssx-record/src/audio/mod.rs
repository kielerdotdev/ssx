//! Audio capture, conversion and mixing for recordings.
//!
//! ```text
//!  system loopback ──┐   per source: convert to 48 kHz stereo, follow the shared clock
//!                    ├─► [resample + drift control] ─┐
//!  microphone ───────┘                               ├─► [mixer] ─► f32 blocks ─► encoder
//!  synthetic (tests) ────────────────────────────────┘
//! ```
//!
//! Design rules:
//!
//! * **One clock.** Every chunk carries the [`Clock`] time of its first sample; the
//!   pipeline places it on the recording timeline through the shared
//!   [`Timeline`](crate::timeline::Timeline) (pauses compacted) and the video's first-frame
//!   origin, so audio and video meet at the same instant by construction.
//! * **Sample counts, not jitter.** Device callbacks arrive with scheduling jitter of
//!   several milliseconds. A source's audio position is its *sample count*; timestamps
//!   only feed a slow controller ([`drift::DriftController`]) that nudges the resampling
//!   ratio when the device clock runs measurably fast or slow relative to the shared
//!   clock (real sound cards are 50-500 ppm off, which is 0.2-2 s per hour), and resyncs
//!   hard after glitches.
//! * **Failure is not fatal.** A missing device, a device that disappears mid-recording or
//!   a resampler error costs that source (with a warning), never the video. The mixed
//!   stream is padded with silence to the end of the video so the file stays aligned.

use std::time::Duration;

use crate::{error::AudioError, time::Clock};

pub mod drift;
pub mod mixer;
pub mod pipeline;
pub mod resample;
pub mod synth;

#[cfg(feature = "audio")]
pub mod device;

/// Sample rate and channel count of an audio stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    /// Samples per second per channel.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
}

impl AudioFormat {
    /// 48 kHz stereo: what the recording pipeline mixes to.
    pub const STEREO_48K: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };
}

/// A block of interleaved `f32` samples in `-1.0..=1.0`.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Interleaved samples (`frames * channels` values).
    pub samples: Vec<f32>,
    /// [`Clock`] time at which the first sample was captured.
    pub timestamp: Duration,
}

/// What an audio source produced on one poll.
#[derive(Debug)]
pub enum AudioEvent {
    /// New samples.
    Chunk(AudioChunk),
    /// Nothing within the timeout.
    Timeout,
    /// The source ended by itself.
    Ended,
}

/// A microphone, a system-audio loopback or a synthetic signal.
pub trait AudioSource: Send {
    /// Human-readable name for logs and warnings (`"microphone: Blue Yeti"`).
    fn name(&self) -> String;

    /// Opens the device and starts capturing. Returns the device's native format.
    fn start(&mut self, clock: Clock) -> Result<AudioFormat, AudioError>;

    /// Waits up to `timeout` for the next chunk. An `Err` ends this source only.
    fn read(&mut self, timeout: Duration) -> Result<AudioEvent, AudioError>;

    /// Stops capturing. Idempotent.
    fn stop(&mut self);
}

impl<T: AudioSource + ?Sized> AudioSource for Box<T> {
    fn name(&self) -> String {
        (**self).name()
    }
    fn start(&mut self, clock: Clock) -> Result<AudioFormat, AudioError> {
        (**self).start(clock)
    }
    fn read(&mut self, timeout: Duration) -> Result<AudioEvent, AudioError> {
        (**self).read(timeout)
    }
    fn stop(&mut self) {
        (**self).stop();
    }
}

/// Converts `frames` interleaved samples between channel counts: mono is duplicated to
/// stereo, stereo is averaged to mono, anything else keeps the first two channels (for
/// stereo output) or averages all channels (for mono output).
pub fn convert_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    let from = from.max(1);
    if from == to {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(from) {
        match to {
            1 => out.push(frame.iter().sum::<f32>() / from as f32),
            2 if from == 1 => out.extend_from_slice(&[frame[0], frame[0]]),
            2 => out.extend_from_slice(&[frame[0], frame[1]]),
            _ => out.extend(std::iter::repeat_n(frame.iter().sum::<f32>() / from as f32, to)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_conversion() {
        let mut out = Vec::new();
        convert_channels(&[0.5, -0.5], 1, 2, &mut out);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        out.clear();
        convert_channels(&[1.0, 0.0, 0.5, 0.5], 2, 1, &mut out);
        assert_eq!(out, [0.5, 0.5]);
        out.clear();
        convert_channels(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3, 2, &mut out);
        assert_eq!(out, [1.0, 2.0, 4.0, 5.0]);
        out.clear();
        convert_channels(&[1.0, 2.0], 2, 2, &mut out);
        assert_eq!(out, [1.0, 2.0]);
    }
}
