//! Sample-rate and channel conversion with a ratio that can be trimmed while running.
//!
//! Two modes, chosen automatically:
//!
//! * **Passthrough** while the source already has the output rate and no drift correction
//!   is requested: samples are only channel-converted (bit-exact, zero latency). The very
//!   common 48 kHz device costs nothing.
//! * **Sinc** (`rubato::Async`, band-limited interpolation, the same quality class as an
//!   FFT resampler) when the rates differ (44.1 kHz devices) or once the drift controller
//!   asks for a ratio other than 1.0. The ratio can change every chunk (`ramp = true`
//!   smooths the step), which is exactly what tracking a device clock needs.

use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
    audioadapter_buffers::direct::InterleavedSlice,
};

use super::{AudioFormat, convert_channels};
use crate::error::AudioError;

/// Input frames per resampler chunk (10 ms at 48 kHz).
const CHUNK: usize = 480;
/// Largest relative ratio change the resampler is built to support.
const MAX_RELATIVE: f64 = 1.05;

enum Mode {
    Passthrough,
    Sinc(Box<Async<f32>>),
}

/// See the module docs.
pub struct StreamResampler {
    input: AudioFormat,
    rate_out: u32,
    channels_out: usize,
    mode: Mode,
    /// Channel-converted input waiting for a full chunk (Sinc mode).
    fifo: Vec<f32>,
    out_buf: Vec<f32>,
    scratch: Vec<f32>,
    produced: u64,
    last_ratio: f64,
    /// Output frames of filter warm-up still to discard, so output sample `j` lines up
    /// with input sample `j` (a resampler delays its output by its filter length).
    skip: usize,
}

impl std::fmt::Debug for StreamResampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamResampler")
            .field("input", &self.input)
            .field("rate_out", &self.rate_out)
            .field("sinc", &matches!(self.mode, Mode::Sinc(_)))
            .field("produced", &self.produced)
            .finish_non_exhaustive()
    }
}

fn err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Resample(e.to_string())
}

impl StreamResampler {
    /// Converts from `input` to `rate_out` Hz with `channels_out` channels.
    pub fn new(input: AudioFormat, rate_out: u32, channels_out: u16) -> Result<Self, AudioError> {
        if input.sample_rate == 0 || rate_out == 0 || input.channels == 0 || channels_out == 0 {
            return Err(AudioError::UnsupportedFormat(format!(
                "{input:?} -> {rate_out} Hz x{channels_out}"
            )));
        }
        let mut s = Self {
            input,
            rate_out,
            channels_out: usize::from(channels_out),
            mode: Mode::Passthrough,
            fifo: Vec::new(),
            out_buf: Vec::new(),
            scratch: Vec::new(),
            produced: 0,
            last_ratio: 1.0,
            skip: 0,
        };
        if input.sample_rate != rate_out {
            s.switch_to_sinc()?;
        }
        Ok(s)
    }

    fn switch_to_sinc(&mut self) -> Result<(), AudioError> {
        let ratio = f64::from(self.rate_out) / f64::from(self.input.sample_rate);
        let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2);
        let rs = Async::<f32>::new_sinc(
            ratio,
            MAX_RELATIVE,
            &params,
            CHUNK,
            self.channels_out,
            FixedAsync::Input,
        )
        .map_err(err)?;
        self.out_buf = vec![0.0; rs.output_frames_max() * self.channels_out];
        self.skip = rs.output_delay();
        self.mode = Mode::Sinc(Box::new(rs));
        Ok(())
    }

    /// `true` while no filtering is applied.
    pub fn is_passthrough(&self) -> bool {
        matches!(self.mode, Mode::Passthrough)
    }

    /// Output frames produced so far.
    pub fn produced(&self) -> u64 {
        self.produced
    }

    /// Output frames that are consumed but not yet emitted (the partial input chunk), for
    /// the drift controller's position accounting.
    pub fn latency_frames(&self) -> u64 {
        match &self.mode {
            Mode::Passthrough => 0,
            Mode::Sinc(_) => {
                let pending_in = self.fifo.len() / self.channels_out;
                let ratio = f64::from(self.rate_out) / f64::from(self.input.sample_rate);
                (pending_in as f64 * ratio) as u64
            }
        }
    }

    /// Converts `samples` (interleaved, input format) and appends the result to `out`.
    /// `rel_ratio` is the drift controller's correction (`1.0` = none).
    pub fn process(
        &mut self,
        samples: &[f32],
        rel_ratio: f64,
        out: &mut Vec<f32>,
    ) -> Result<(), AudioError> {
        let rel_ratio = rel_ratio.clamp(1.0 / MAX_RELATIVE, MAX_RELATIVE);
        if self.is_passthrough() && (rel_ratio - 1.0).abs() > 1e-7 {
            self.switch_to_sinc()?;
        }
        self.scratch.clear();
        convert_channels(
            samples,
            usize::from(self.input.channels),
            self.channels_out,
            &mut self.scratch,
        );
        match &mut self.mode {
            Mode::Passthrough => {
                self.produced += (self.scratch.len() / self.channels_out) as u64;
                out.extend_from_slice(&self.scratch);
                Ok(())
            }
            Mode::Sinc(rs) => {
                self.fifo.extend_from_slice(&self.scratch);
                let ch = self.channels_out;
                if (rel_ratio - self.last_ratio).abs() > 1e-9 {
                    rs.set_resample_ratio_relative(rel_ratio, true).map_err(err)?;
                    self.last_ratio = rel_ratio;
                }
                while self.fifo.len() / ch >= rs.input_frames_next() {
                    let need = rs.input_frames_next();
                    let input =
                        InterleavedSlice::new(&self.fifo[..need * ch], ch, need).map_err(err)?;
                    let max_out = self.out_buf.len() / ch;
                    let mut output =
                        InterleavedSlice::new_mut(&mut self.out_buf, ch, max_out).map_err(err)?;
                    let (used, made) =
                        rs.process_into_buffer(&input, &mut output, None).map_err(err)?;
                    let drop = self.skip.min(made);
                    self.skip -= drop;
                    out.extend_from_slice(&self.out_buf[drop * ch..made * ch]);
                    self.produced += (made - drop) as u64;
                    self.fifo.drain(..used * ch);
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, ch: usize, hz: f32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        let mut v = Vec::with_capacity(n * ch);
        for i in 0..n {
            let s = (i as f32 / rate as f32 * hz * std::f32::consts::TAU).sin() * 0.5;
            for _ in 0..ch {
                v.push(s);
            }
        }
        v
    }

    fn zero_crossings(x: &[f32], ch: usize) -> usize {
        x.chunks(ch)
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count()
    }

    #[test]
    fn same_rate_is_bit_exact_passthrough() {
        let mut r =
            StreamResampler::new(AudioFormat { sample_rate: 48_000, channels: 2 }, 48_000, 2)
                .unwrap();
        assert!(r.is_passthrough());
        let input = sine(48_000, 2, 440.0, 0.1);
        let mut out = Vec::new();
        r.process(&input, 1.0, &mut out).unwrap();
        assert_eq!(out, input);
        assert_eq!(r.produced(), 4800);
        assert_eq!(r.latency_frames(), 0);
    }

    #[test]
    fn mono_44k_to_stereo_48k_keeps_pitch_length_and_level() {
        let mut r =
            StreamResampler::new(AudioFormat { sample_rate: 44_100, channels: 1 }, 48_000, 2)
                .unwrap();
        assert!(!r.is_passthrough());
        let input = sine(44_100, 1, 1000.0, 2.0);
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, 1.0, &mut out).unwrap();
        }
        let frames = out.len() / 2;
        // 2 s of input yields 2 s of output (minus at most one partial chunk).
        let expected = 96_000.0;
        assert!((frames as f64 - expected).abs() < 700.0, "{frames} frames");
        assert_eq!(r.produced() as usize, frames);
        // Pitch: one crossing per millisecond of output.
        let zc = zero_crossings(&out, 2);
        let expected_zc = frames as f64 / 48.0;
        assert!(
            (zc as f64 - expected_zc).abs() <= 3.0,
            "zero crossings {zc}, expected {expected_zc}"
        );
        // Level (skip the startup transient).
        let mid = &out[20_000..80_000];
        let rms = (mid.iter().map(|s| s * s).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.5 / std::f32::consts::SQRT_2).abs() < 0.01, "rms {rms}");
        // Both channels identical (mono duplicated).
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
    }

    #[test]
    fn ratio_trim_changes_the_output_length() {
        let fmt = AudioFormat { sample_rate: 48_000, channels: 2 };
        let input = sine(48_000, 2, 440.0, 5.0);
        let run = |ratio: f64| {
            let mut r = StreamResampler::new(fmt, 48_000, 2).unwrap();
            let mut out = Vec::new();
            for chunk in input.chunks(960) {
                r.process(chunk, ratio, &mut out).unwrap();
            }
            out.len() / 2
        };
        let base = run(1.0) as f64;
        let more = run(1.002) as f64;
        let less = run(0.998) as f64;
        assert!((more / base - 1.002).abs() < 0.0006, "{}", more / base);
        assert!((less / base - 0.998).abs() < 0.0006, "{}", less / base);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let mut r =
            StreamResampler::new(AudioFormat { sample_rate: 48_000, channels: 2 }, 48_000, 1)
                .unwrap();
        let mut out = Vec::new();
        r.process(&[1.0, 0.0, 0.5, 0.5], 1.0, &mut out).unwrap();
        assert_eq!(out, [0.5, 0.5]);
    }

    #[test]
    fn invalid_formats_are_rejected() {
        assert!(
            StreamResampler::new(AudioFormat { sample_rate: 0, channels: 2 }, 48_000, 2).is_err()
        );
        assert!(
            StreamResampler::new(AudioFormat { sample_rate: 48_000, channels: 0 }, 48_000, 2)
                .is_err()
        );
    }
}
