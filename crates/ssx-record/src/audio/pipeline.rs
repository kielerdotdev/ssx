//! The audio pipeline: sources in, one continuous 48 kHz stereo stream on the recording
//! timeline out.
//!
//! Per source (lane): chunk timestamp -> timeline (pauses removed) -> origin (the first
//! video frame) -> drift controller -> resampler -> mixer. See [`super`] for the overall
//! design. The pipeline is a plain object driven by `poll`/`finish` calls from the
//! session's audio thread, which keeps it testable without threads.

use std::time::{Duration, Instant};

use super::{
    AudioChunk, AudioEvent, AudioFormat, AudioSource,
    drift::{DriftAction, DriftConfig, DriftController},
    mixer::Mixer,
    resample::StreamResampler,
};
use crate::{error::AudioError, timeline::SharedTimeline};

/// Audio that arrives before the video origin is known is kept for at most this long.
const MAX_PRE_ORIGIN: Duration = Duration::from_secs(5);

struct Lane {
    src: Box<dyn AudioSource>,
    name: String,
    fmt: AudioFormat,
    rs: StreamResampler,
    drift: DriftController,
    alive: bool,
    /// Timeline frame at which the next output frame of this lane goes.
    next_pos: Option<u64>,
    /// Segments received before the origin was known: `(recording time, samples)`.
    pending: Vec<(Duration, Vec<f32>)>,
    pending_frames: usize,
    tmp: Vec<f32>,
    /// Input frames still to be discarded (after a `DropInput` action).
    drop_debt: usize,
}

/// See the module docs.
pub struct AudioPipeline {
    lanes: Vec<Lane>,
    mixer: Mixer,
    timeline: SharedTimeline,
    target: AudioFormat,
    warnings: Vec<String>,
    /// Frames emitted so far.
    emitted: u64,
}

impl std::fmt::Debug for AudioPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPipeline")
            .field("lanes", &self.lanes.len())
            .field("emitted", &self.emitted)
            .finish_non_exhaustive()
    }
}

/// Counters for diagnostics and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineStats {
    /// Frames emitted to the encoder.
    pub emitted: u64,
    /// Frames dropped because they arrived after their slot was emitted.
    pub late_frames: u64,
    /// Frames of silence inserted to bridge gaps.
    pub gap_frames: u64,
    /// Hard resyncs across all lanes.
    pub resyncs: u32,
}

impl AudioPipeline {
    /// Builds the pipeline from already-started sources and their native formats. A
    /// source whose resampler cannot be built is dropped with a warning.
    pub fn new(
        sources: Vec<(Box<dyn AudioSource>, AudioFormat)>,
        target: AudioFormat,
        timeline: SharedTimeline,
        stall: Duration,
    ) -> Self {
        let mut lanes = Vec::new();
        let mut warnings = Vec::new();
        for (mut src, fmt) in sources {
            let name = src.name();
            match StreamResampler::new(fmt, target.sample_rate, target.channels) {
                Ok(rs) => lanes.push(Lane {
                    src,
                    name,
                    fmt,
                    rs,
                    drift: DriftController::new(DriftConfig::new(target.sample_rate)),
                    alive: true,
                    next_pos: None,
                    pending: Vec::new(),
                    pending_frames: 0,
                    tmp: Vec::new(),
                    drop_debt: 0,
                }),
                Err(e) => {
                    src.stop();
                    warnings.push(format!("audio source `{name}` disabled: {e}"));
                }
            }
        }
        let mixer = Mixer::new(lanes.len(), usize::from(target.channels), stall, Instant::now());
        Self { lanes, mixer, timeline, target, warnings, emitted: 0 }
    }

    /// Number of sources still delivering.
    pub fn live_sources(&self) -> usize {
        self.lanes.iter().filter(|l| l.alive).count()
    }

    /// Takes the warnings collected since the last call.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Counters.
    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            emitted: self.emitted,
            late_frames: self.mixer.late_frames,
            gap_frames: self.mixer.gap_frames,
            resyncs: self.lanes.iter().map(|l| l.drift.resyncs).sum(),
        }
    }

    /// Reads every live source once (waiting up to `timeout` in total), moves the audio
    /// through the pipeline and appends whatever mixed output is ready to `out`.
    /// `origin` is the recording time of the first video frame (`None` until it is known:
    /// audio is buffered meanwhile).
    pub fn poll(&mut self, origin: Option<Duration>, timeout: Duration, out: &mut Vec<f32>) {
        let per_lane = timeout / u32::try_from(self.lanes.len().max(1)).unwrap_or(1);
        for i in 0..self.lanes.len() {
            if !self.lanes[i].alive {
                continue;
            }
            match self.lanes[i].src.read(per_lane) {
                Ok(AudioEvent::Chunk(c)) => self.ingest(i, &c, origin),
                Ok(AudioEvent::Timeout) => {}
                Ok(AudioEvent::Ended) => self.retire(i, None),
                Err(e) => self.retire(i, Some(e)),
            }
        }
        if let Some(o) = origin {
            self.flush_pending(o);
        }
        let n = self.mixer.pop_ready(Instant::now(), out);
        self.emitted += n as u64;
    }

    /// Drains the sources, stops them and pads the mix with silence up to `end`
    /// (recording time relative to the origin).
    pub fn finish(&mut self, origin: Option<Duration>, end: Duration, out: &mut Vec<f32>) {
        for round in 0..64 {
            let mut any = false;
            for i in 0..self.lanes.len() {
                if !self.lanes[i].alive {
                    continue;
                }
                match self.lanes[i].src.read(Duration::ZERO) {
                    Ok(AudioEvent::Chunk(c)) => {
                        self.ingest(i, &c, origin);
                        any = true;
                    }
                    Ok(AudioEvent::Timeout) => {}
                    Ok(AudioEvent::Ended) => self.retire(i, None),
                    Err(e) => self.retire(i, Some(e)),
                }
            }
            if let Some(o) = origin {
                self.flush_pending(o);
            }
            if !any || round == 63 {
                break;
            }
        }
        for l in &mut self.lanes {
            l.src.stop();
            l.alive = false;
        }
        let end_frames = (end.as_secs_f64() * f64::from(self.target.sample_rate)).round() as u64;
        let n = self.mixer.flush(end_frames, out);
        self.emitted += n as u64;
    }

    fn retire(&mut self, i: usize, err: Option<AudioError>) {
        let lane = &mut self.lanes[i];
        if !lane.alive {
            return;
        }
        lane.alive = false;
        lane.src.stop();
        self.mixer.end_lane(i);
        match err {
            Some(e) => {
                tracing::warn!(source = %lane.name, error = %e, "audio source failed; continuing without it");
                self.warnings.push(format!("audio source `{}` stopped: {e}", lane.name));
            }
            None => tracing::debug!(source = %lane.name, "audio source ended"),
        }
    }

    /// Splits `chunk` around pauses and feeds the pieces to the lane.
    fn ingest(&mut self, i: usize, chunk: &AudioChunk, origin: Option<Duration>) {
        let (rate, ch) = {
            let l = &self.lanes[i];
            (f64::from(l.fmt.sample_rate), usize::from(l.fmt.channels))
        };
        let frames = chunk.samples.len() / ch;
        if frames == 0 {
            return;
        }
        let ts = chunk.timestamp;
        let dur = Duration::from_secs_f64(frames as f64 / rate);
        let segments = self.timeline.with(|tl| tl.active_segments(ts, ts + dur));
        for (s, e) in segments {
            let a = ((s - ts).as_secs_f64() * rate).round() as usize;
            let b = (((e - ts).as_secs_f64() * rate).round() as usize).min(frames);
            if b <= a {
                continue;
            }
            let rec = self.timeline.map(s).unwrap_or(s);
            let samples = &chunk.samples[a * ch..b * ch];
            match origin {
                Some(o) => self.process_segment(i, rec, samples, o),
                None => {
                    let l = &mut self.lanes[i];
                    l.pending.push((rec, samples.to_vec()));
                    l.pending_frames += b - a;
                    // Bound the memory held while waiting for the first video frame.
                    while l.pending_frames as f64 / rate > MAX_PRE_ORIGIN.as_secs_f64() {
                        let (_, old) = l.pending.remove(0);
                        l.pending_frames -= old.len() / ch;
                    }
                }
            }
        }
    }

    fn flush_pending(&mut self, origin: Duration) {
        for i in 0..self.lanes.len() {
            if self.lanes[i].pending.is_empty() {
                continue;
            }
            let pending = std::mem::take(&mut self.lanes[i].pending);
            self.lanes[i].pending_frames = 0;
            for (rec, samples) in pending {
                self.process_segment(i, rec, &samples, origin);
            }
        }
    }

    /// One contiguous piece of audio at recording time `rec` (clock time with pauses
    /// removed), relative to the video `origin`.
    fn process_segment(&mut self, i: usize, rec: Duration, samples: &[f32], origin: Duration) {
        let target_rate = self.target.sample_rate;
        let now = Instant::now();
        let lane = &mut self.lanes[i];
        let ch = usize::from(lane.fmt.channels);
        let rate_in = f64::from(lane.fmt.sample_rate);

        // Trim what precedes the video's first frame.
        let (t, mut samples) = if rec >= origin {
            (rec - origin, samples)
        } else {
            let skip = ((origin - rec).as_secs_f64() * rate_in).round() as usize;
            if skip * ch >= samples.len() {
                return;
            }
            let t = Duration::from_secs_f64(skip as f64 / rate_in).saturating_sub(origin - rec);
            (t, &samples[skip * ch..])
        };

        // Pay off input frames owed from an earlier "source is ahead" resync.
        if lane.drop_debt > 0 {
            let n = lane.drop_debt.min(samples.len() / ch);
            samples = &samples[n * ch..];
            lane.drop_debt -= n;
            if samples.is_empty() {
                return;
            }
        }

        let produced = lane.rs.produced() + lane.rs.latency_frames();
        let action = lane.drift.observe(t, produced);
        let mut ratio = lane.drift.ratio();
        let first = lane.next_pos.is_none();
        let start_pos = *lane
            .next_pos
            .get_or_insert_with(|| (t.as_secs_f64() * f64::from(target_rate)).round() as u64);
        match action {
            DriftAction::Ratio(r) => ratio = r,
            DriftAction::InsertSilence(n) if !first => {
                tracing::debug!(source = %lane.name, frames = n, "audio source late; inserting silence");
                lane.next_pos = Some(start_pos + n);
            }
            DriftAction::DropInput(n) if !first => {
                tracing::debug!(source = %lane.name, frames = n, "audio source ahead; dropping input");
                let owed = (n as f64 * rate_in / f64::from(target_rate)).round() as usize;
                let n_in = owed.min(samples.len() / ch);
                lane.drop_debt = owed - n_in;
                samples = &samples[n_in * ch..];
            }
            _ => {}
        }
        if samples.is_empty() {
            return;
        }
        lane.tmp.clear();
        let mut tmp = std::mem::take(&mut lane.tmp);
        match lane.rs.process(samples, ratio, &mut tmp) {
            Ok(()) => {
                let pos = lane.next_pos.unwrap_or(start_pos);
                let frames = (tmp.len() / usize::from(self.target.channels)) as u64;
                self.mixer.push(i, pos, &tmp, now);
                lane.next_pos = Some(pos + frames);
            }
            Err(e) => {
                lane.tmp = tmp;
                self.retire(i, Some(e));
                return;
            }
        }
        tmp.clear();
        lane.tmp = tmp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        audio::synth::{Signal, SyntheticAudio, SyntheticAudioConfig},
        source::synthetic::TimeMode,
    };

    fn tone_source(hz: f32, amp: f32, total_ms: u64) -> (Box<dyn AudioSource>, AudioFormat) {
        let mut cfg = SyntheticAudioConfig::new(
            Signal::Tone { hz, amplitude: amp, start: Duration::ZERO },
            TimeMode::Virtual,
        );
        cfg.total = Some(Duration::from_millis(total_ms));
        let mut s = SyntheticAudio::new(cfg);
        let fmt = s.start(crate::time::Clock::start()).unwrap();
        (Box::new(s), fmt)
    }

    fn run(pipe: &mut AudioPipeline, origin: Option<Duration>) -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..2000 {
            if pipe.live_sources() == 0 {
                break;
            }
            pipe.poll(origin, Duration::from_millis(1), &mut out);
        }
        out
    }

    #[test]
    fn single_source_passes_through_unchanged() {
        let src = tone_source(440.0, 0.5, 1000);
        let mut pipe = AudioPipeline::new(
            vec![src],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        pipe.finish(Some(Duration::ZERO), Duration::from_millis(1000), &mut out);
        assert_eq!(out.len(), 48_000 * 2);
        // Bit-exact against the generator (same rate, no drift, no resampler).
        let mut reference = SyntheticAudio::new(SyntheticAudioConfig::new(
            Signal::Tone { hz: 440.0, amplitude: 0.5, start: Duration::ZERO },
            TimeMode::Virtual,
        ));
        reference.start(crate::time::Clock::start()).unwrap();
        let mut expected = Vec::new();
        while expected.len() < out.len() {
            if let Ok(AudioEvent::Chunk(c)) = reference.read(Duration::ZERO) {
                expected.extend(c.samples);
            }
        }
        assert_eq!(out, expected[..out.len()]);
    }

    #[test]
    fn two_sources_are_summed() {
        let a = tone_source(300.0, 0.25, 500);
        let b = tone_source(300.0, 0.25, 500);
        let mut pipe = AudioPipeline::new(
            vec![a, b],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        pipe.finish(Some(Duration::ZERO), Duration::from_millis(500), &mut out);
        assert_eq!(out.len(), 24_000 * 2);
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "two 0.25 tones sum to 0.5, got {peak}");
    }

    #[test]
    fn audio_before_the_video_origin_is_trimmed_and_buffered_until_known() {
        let src = tone_source(440.0, 0.5, 1000);
        let mut pipe = AudioPipeline::new(
            vec![src],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        // Video's first frame arrives 300 ms into the recording clock: origin unknown at
        // first, everything is buffered.
        let mut out = Vec::new();
        for _ in 0..30 {
            pipe.poll(None, Duration::from_millis(1), &mut out);
        }
        assert!(out.is_empty());
        let origin = Some(Duration::from_millis(300));
        for _ in 0..200 {
            pipe.poll(origin, Duration::from_millis(1), &mut out);
        }
        pipe.finish(origin, Duration::from_millis(700), &mut out);
        // 1000 ms of audio minus the 300 ms before the origin = 700 ms.
        assert_eq!(out.len(), 33_600 * 2);
    }

    #[test]
    fn a_pause_removes_audio_and_keeps_the_rest_continuous() {
        let timeline = SharedTimeline::new();
        timeline.with(|tl| {
            tl.pause(Duration::from_millis(200));
            tl.resume(Duration::from_millis(500));
        });
        let src = tone_source(440.0, 0.5, 1000);
        let mut pipe = AudioPipeline::new(
            vec![src],
            AudioFormat::STEREO_48K,
            timeline,
            Duration::from_millis(500),
        );
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        pipe.finish(Some(Duration::ZERO), Duration::from_millis(700), &mut out);
        // 1000 ms - 300 ms paused = 700 ms, no gaps and no silence.
        assert_eq!(out.len(), 33_600 * 2);
        assert_eq!(pipe.stats().gap_frames, 0);
        // The signal is continuous through the join: no sample-to-sample jump beyond
        // what a 440 Hz sine allows (~0.03) apart from the phase jump at the cut.
        let jumps = out
            .chunks(2)
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| (w[1] - w[0]).abs() > 0.2)
            .count();
        assert!(jumps <= 1, "{jumps} discontinuities");
    }

    #[test]
    fn a_failing_source_is_dropped_with_a_warning_and_the_rest_continues() {
        let good = tone_source(440.0, 0.4, 600);
        let mut cfg = SyntheticAudioConfig::new(Signal::Silence, TimeMode::Virtual);
        cfg.fail_after = Some(Duration::from_millis(100));
        let mut bad = SyntheticAudio::new(cfg);
        let fmt = bad.start(crate::time::Clock::start()).unwrap();
        let mut pipe = AudioPipeline::new(
            vec![good, (Box::new(bad), fmt)],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        pipe.finish(Some(Duration::ZERO), Duration::from_millis(600), &mut out);
        let warnings = pipe.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("stopped"), "{warnings:?}");
        assert_eq!(out.len(), 28_800 * 2, "the healthy source still delivers its full length");
        assert!(out.iter().any(|s| s.abs() > 0.3));
    }

    #[test]
    fn audio_ending_early_is_padded_with_silence_to_the_video_end() {
        let src = tone_source(440.0, 0.5, 300);
        let mut pipe = AudioPipeline::new(
            vec![src],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        pipe.finish(Some(Duration::ZERO), Duration::from_millis(1000), &mut out);
        assert_eq!(out.len(), 48_000 * 2);
        assert!(out[14_400 * 2..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_44k_mono_source_is_converted_to_48k_stereo() {
        let mut cfg = SyntheticAudioConfig::new(
            Signal::Tone { hz: 1000.0, amplitude: 0.5, start: Duration::ZERO },
            TimeMode::Virtual,
        );
        cfg.format = AudioFormat { sample_rate: 44_100, channels: 1 };
        cfg.total = Some(Duration::from_secs(2));
        let mut s = SyntheticAudio::new(cfg);
        let fmt = s.start(crate::time::Clock::start()).unwrap();
        let mut pipe = AudioPipeline::new(
            vec![(Box::new(s), fmt)],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        pipe.finish(Some(Duration::ZERO), Duration::from_secs(2), &mut out);
        assert_eq!(out.len(), 96_000 * 2);
        let rms = (out[20_000..100_000].iter().map(|s| s * s).sum::<f32>() / 80_000.0).sqrt();
        assert!((rms - 0.354).abs() < 0.02, "{rms}");
    }

    #[test]
    fn a_skewed_device_clock_is_followed_without_a_growing_offset() {
        // The device produces samples 0.2 % faster than the clock. In virtual time the
        // timestamps encode that skew, so the pipeline must shrink the stream to keep the
        // output length locked to the clock (10 s of clock time = 10 s of audio).
        let mut cfg = SyntheticAudioConfig::new(
            Signal::Tone { hz: 440.0, amplitude: 0.3, start: Duration::ZERO },
            TimeMode::Virtual,
        );
        cfg.skew = 0.002;
        cfg.total = Some(Duration::from_millis(10_020));
        let mut s = SyntheticAudio::new(cfg);
        let fmt = s.start(crate::time::Clock::start()).unwrap();
        let mut pipe = AudioPipeline::new(
            vec![(Box::new(s), fmt)],
            AudioFormat::STEREO_48K,
            SharedTimeline::new(),
            Duration::from_millis(500),
        );
        // Virtual timestamps: chunk k at k*10ms/(1+skew) on the clock.
        let mut out = run(&mut pipe, Some(Duration::ZERO));
        // The end of the recording on the shared clock is 10 s.
        pipe.finish(Some(Duration::ZERO), Duration::from_secs(10), &mut out);
        // Without correction the stream would be 20 ms (960 frames) too long; the
        // controller must have removed all but a few milliseconds of it.
        let frames = out.len() / 2;
        assert!(frames.abs_diff(480_000) < 480, "{frames} frames instead of 480000");
        assert_eq!(pipe.stats().resyncs, 0);
    }
}
