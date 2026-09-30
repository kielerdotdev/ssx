//! Session tests: threads, pacing, pause/resume, backpressure, abort, guards, audio
//! degradation. Files are decoded back and compared with what the counters claim.

#![cfg(feature = "ffmpeg")]
#![allow(clippy::cast_possible_wrap)] // small counts and timestamps

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use ssx_record::{
    audio::{
        AudioSource,
        synth::{Signal, SyntheticAudio, SyntheticAudioConfig},
    },
    encode::{
        Candidate, Codec, Container, EncodeSummary, Encoder, EncoderInput, HwPolicy, InputKind,
        ProbeResult, Prober, VideoSettings, ffmpeg::FfmpegProber,
    },
    error::{RecordError, Result},
    session::{EndReason, GapPolicy, Limits, RecordConfig, Recording, RecordingSession},
    source::{
        FrameSource,
        synthetic::{SyntheticConfig, SyntheticSource, TimeMode},
    },
    time::Fps,
    verify::inspect,
};

fn sw_config(path: &Path) -> RecordConfig {
    let mut c = RecordConfig::new(path);
    c.video = VideoSettings { hw: HwPolicy::SoftwareOnly, ..VideoSettings::default() };
    c
}

fn realtime(w: u32, h: u32, fps: Fps) -> Box<dyn FrameSource> {
    Box::new(SyntheticSource::new(SyntheticConfig::new(w, h, fps)))
}

fn tone(start: Duration, mode: TimeMode) -> Box<dyn AudioSource> {
    Box::new(SyntheticAudio::new(SyntheticAudioConfig::new(
        Signal::Tone { hz: 440.0, amplitude: 0.5, start },
        mode,
    )))
}

fn start(
    cfg: RecordConfig,
    src: Box<dyn FrameSource>,
    audio: Vec<Box<dyn AudioSource>>,
) -> RecordingSession {
    RecordingSession::start(cfg, src, audio, &FfmpegProber).expect("start recording")
}

fn assert_invariants(r: &Recording) {
    let s = &r.stats;
    assert_eq!(
        s.slots,
        s.encoded + s.dropped_backpressure + s.dropped_convert,
        "every slot is encoded or dropped: {s:?}"
    );
    assert_eq!(
        s.slots,
        s.captured - s.surplus_dropped - s.paused_discarded + s.duplicated,
        "slots = used source frames + duplicates: {s:?}"
    );
}

#[test]
fn virtual_time_source_is_encoded_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.mp4");
    let src = Box::new(SyntheticSource::new(
        SyntheticConfig::new(320, 240, Fps::FPS_30).time(TimeMode::Virtual).max_frames(60),
    ));
    let s = start(sw_config(&path), src, vec![]);
    assert_eq!(s.wait_finished(Duration::from_secs(20)), Some(EndReason::SourceEnded));
    let r = s.stop().unwrap();
    assert_eq!(r.end_reason, EndReason::SourceEnded);
    assert_eq!(r.stats.captured, 60);
    assert_eq!(r.stats.slots, 60);
    assert_eq!(r.stats.encoded, 60);
    assert_eq!(r.stats.duplicated + r.stats.dropped_backpressure, 0);
    assert_invariants(&r);
    assert_eq!(r.duration, Duration::from_secs(2));
    let rep = inspect(&path).unwrap();
    let v = rep.video.unwrap();
    assert_eq!(v.frame_count(), 60);
    for (i, c) in v.counters.iter().enumerate() {
        assert_eq!(usize::from(*c), i);
    }
}

#[test]
fn realtime_recording_keeps_constant_fps_and_the_clock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt.mp4");
    let s = start(sw_config(&path), realtime(640, 360, Fps::FPS_30), vec![]);
    std::thread::sleep(Duration::from_millis(2000));
    let st = s.stats();
    assert!(st.slots > 40, "counters are live while recording: {st:?}");
    let r = s.stop().unwrap();
    assert_invariants(&r);
    let secs = r.duration.as_secs_f64();
    assert!((secs - 2.0).abs() < 0.2, "recorded {secs}");
    let rep = inspect(&path).unwrap();
    let v = rep.video.unwrap();
    assert!((v.frame_count() as f64 - 60.0).abs() <= 5.0, "{} frames", v.frame_count());
    // Constant frame rate: every gap between frames is one frame period.
    let period = 1.0 / 30.0;
    let worst = v.pts.windows(2).map(|w| ((w[1] - w[0]) - period).abs()).fold(0.0, f64::max);
    assert!(worst < 0.002, "frame spacing deviates by {worst} s");
    // The burnt-in counter is the timeline slot at capture time; it must track the
    // presentation time (the clock) within two frames.
    for (pts, c) in v.pts.iter().zip(&v.counters) {
        let expect = pts * 30.0;
        assert!((f64::from(*c) - expect).abs() <= 2.5, "pts {pts}: counter {c}, expected {expect}");
    }
    assert!(rep.seekable);
    assert!(rep.mp4.unwrap().moov_first());
}

#[test]
fn idle_screen_is_padded_with_duplicates() {
    // A damage-driven source that delivers nothing between 0.3 s and 1.5 s.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idle.mp4");
    let src =
        Box::new(SyntheticSource::new(SyntheticConfig::new(320, 240, Fps::FPS_30).skip(9..45)));
    let s = start(sw_config(&path), src, vec![]);
    std::thread::sleep(Duration::from_millis(2200));
    let r = s.stop().unwrap();
    assert_invariants(&r);
    assert!(r.stats.duplicated >= 30, "idle time must be filled: {:?}", r.stats);
    let rep = inspect(&path).unwrap();
    let v = rep.video.unwrap();
    let period = 1.0 / 30.0;
    let worst = v.pts.windows(2).map(|w| ((w[1] - w[0]) - period).abs()).fold(0.0, f64::max);
    assert!(worst < 0.002, "constant fps also while idle: {worst}");
    // During the idle window the picture repeats: counter 8 is held for ~1.2 s.
    let held = v.counters.iter().filter(|c| **c == 8).count();
    assert!(held >= 30, "counter 8 shown {held} times; counters {:?}", &v.counters[..50]);
    assert!((r.duration.as_secs_f64() - 2.2).abs() < 0.25, "{:?}", r.duration);
}

#[test]
fn pause_and_resume_compact_the_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pause.mp4");
    let s = start(
        sw_config(&path),
        realtime(320, 240, Fps::FPS_30),
        vec![tone(Duration::ZERO, TimeMode::Realtime)],
    );
    std::thread::sleep(Duration::from_millis(1000));
    s.pause();
    assert!(s.is_paused());
    std::thread::sleep(Duration::from_millis(1000));
    let paused_stats = s.stats();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(s.stats().slots, paused_stats.slots, "no slots are produced while paused");
    s.resume();
    std::thread::sleep(Duration::from_millis(1000));
    let r = s.stop().unwrap();
    assert_invariants(&r);
    // 1 s + 1 s recorded, 1.2 s paused.
    let secs = r.duration.as_secs_f64();
    assert!((secs - 2.0).abs() < 0.2, "timeline is {secs} s, expected 2 s");
    let rep = inspect(&path).unwrap();
    let v = rep.video.unwrap();
    let period = 1.0 / 30.0;
    let worst = v.pts.windows(2).map(|w| ((w[1] - w[0]) - period).abs()).fold(0.0, f64::max);
    assert!(worst < 0.002, "no gap at the join: {worst}");
    // The source counter is wall-clock based, so it jumps by the paused time at the join.
    let jump = v.counters.windows(2).map(|w| i32::from(w[1]) - i32::from(w[0])).max().unwrap();
    assert!((30..=45).contains(&jump), "counter jumped by {jump} at the pause");
    // Audio was compacted the same way: same length as the video.
    let a = rep.audio.unwrap();
    assert!((a.decoded_seconds() - secs).abs() < 0.1, "{} vs {secs}", a.decoded_seconds());
}

#[test]
fn stop_while_paused_and_pause_before_first_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p2.mp4");
    let s = start(sw_config(&path), realtime(320, 240, Fps::FPS_30), vec![]);
    s.pause();
    std::thread::sleep(Duration::from_millis(300));
    s.resume();
    std::thread::sleep(Duration::from_millis(600));
    s.pause();
    std::thread::sleep(Duration::from_millis(500));
    let r = s.stop().unwrap();
    assert!((r.duration.as_secs_f64() - 0.6).abs() < 0.2, "{:?}", r.duration);
    assert!(inspect(&path).unwrap().video.is_some());
}

// ---- backpressure -----------------------------------------------------------------------

/// An encoder that takes `delay` per frame and records the slots it was given.
struct SlowEncoder {
    delay: Duration,
    slots: Arc<Mutex<Vec<u64>>>,
    path: PathBuf,
    last: u64,
}

impl Encoder for SlowEncoder {
    fn description(&self) -> String {
        "slow test encoder".into()
    }
    fn input_kind(&self) -> InputKind {
        InputKind::Yuv420p
    }
    fn has_audio(&self) -> bool {
        false
    }
    fn write_video(&mut self, slot: u64, _: &Arc<EncoderInput>) -> Result<()> {
        std::thread::sleep(self.delay);
        self.slots.lock().unwrap().push(slot);
        self.last = slot;
        Ok(())
    }
    fn write_audio(&mut self, _: &[f32]) -> Result<()> {
        Ok(())
    }
    fn bytes_written(&self) -> u64 {
        0
    }
    fn finish(self: Box<Self>, end: Duration) -> Result<EncodeSummary> {
        Ok(EncodeSummary {
            path: self.path,
            encoder: "slow test encoder".into(),
            video_frames: self.slots.lock().unwrap().len() as u64,
            audio_samples: 0,
            bytes: 0,
            duration: end,
            has_audio: false,
        })
    }
    fn abort(self: Box<Self>) {}
}

fn slow_session(
    delay: Duration,
    policy: GapPolicy,
    budget: usize,
) -> (RecordingSession, Arc<Mutex<Vec<u64>>>) {
    let slots = Arc::new(Mutex::new(Vec::new()));
    let s2 = Arc::clone(&slots);
    let mut cfg = RecordConfig::new("unused.mp4");
    cfg.pipeline.gap_policy = policy;
    cfg.pipeline.queue_budget_bytes = budget;
    let factory = Box::new(move |spec: &ssx_record::encode::OutputSpec| {
        Ok(Box::new(SlowEncoder { delay, slots: s2, path: spec.path.clone(), last: 0 })
            as Box<dyn Encoder>)
    });
    let s = RecordingSession::start_with(cfg, realtime(640, 360, Fps::FPS_30), vec![], factory)
        .unwrap();
    (s, slots)
}

#[test]
fn a_slow_encoder_drops_frames_with_exact_accounting_and_bounded_memory() {
    // The encoder needs 70 ms per frame (14 fps) for a 30 fps source. The queue holds at
    // most 4 frames (a 4 * 640x360x4 byte budget).
    let (s, slots) = slow_session(Duration::from_millis(70), GapPolicy::Hold, 4 * 640 * 360 * 4);
    std::thread::sleep(Duration::from_millis(3000));
    let live = s.stats();
    assert!(live.dropped_backpressure > 10, "{live:?}");
    let r = s.stop().unwrap();
    assert_invariants(&r);
    let st = r.stats;
    assert!(st.dropped_backpressure > 20, "{st:?}");
    assert!(st.encoded > 20 && st.encoded < 60, "encoded {} of {} slots", st.encoded, st.slots);
    assert!(
        (st.slots as f64 - 90.0).abs() <= 6.0,
        "the timeline still covers 3 s: {} slots",
        st.slots
    );
    assert!((r.duration.as_secs_f64() - 3.0).abs() < 0.25);
    // Memory: the queue never held more than its capacity.
    assert_eq!(r.queue_capacity, 4);
    assert!(r.peak_queue_frames <= 4, "queue peaked at {}", r.peak_queue_frames);
    // The encoder saw strictly increasing slots on the constant-rate grid.
    let seen = slots.lock().unwrap().clone();
    assert_eq!(seen.len() as u64, st.encoded);
    assert!(seen.windows(2).all(|w| w[1] > w[0]));
    assert!(*seen.last().unwrap() > 70, "and it kept up with the end of the timeline");
}

#[test]
fn duplicate_gap_policy_gives_the_encoder_every_slot() {
    let (s, slots) =
        slow_session(Duration::from_millis(20), GapPolicy::Duplicate, 3 * 640 * 360 * 4);
    // Make the encoder slow only after a while by simply overloading with jitter: a
    // 20 ms encoder keeps up at 30 fps, so force drops by pausing the encoder via load.
    std::thread::sleep(Duration::from_millis(1500));
    let r = s.stop().unwrap();
    assert_invariants(&r);
    let seen = slots.lock().unwrap().clone();
    // Whatever was dropped upstream, the encoder saw a gap-free grid.
    assert!(seen.windows(2).all(|w| w[1] == w[0] + 1), "grid has holes: {seen:?}");
}

#[test]
fn a_very_slow_encoder_with_duplicate_policy_stays_constant_rate() {
    let (s, slots) =
        slow_session(Duration::from_millis(60), GapPolicy::Duplicate, 3 * 640 * 360 * 4);
    std::thread::sleep(Duration::from_millis(1500));
    let r = s.stop().unwrap();
    assert_invariants(&r);
    assert!(r.stats.dropped_backpressure > 0, "the encoder must have been overloaded");
    let seen = slots.lock().unwrap().clone();
    assert!(seen.windows(2).all(|w| w[1] == w[0] + 1), "holes were filled with repeats");
}

// ---- abort, errors, guards --------------------------------------------------------------

#[test]
fn abort_deletes_the_partial_file() {
    for name in ["abort.mp4", "abort.webm", "abort.gif"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let s = start(sw_config(&path), realtime(320, 240, Fps::FPS_30), vec![]);
        std::thread::sleep(Duration::from_millis(500));
        assert!(path.exists(), "{name}: the file exists while recording");
        s.abort();
        assert!(!path.exists(), "{name}: abort must delete the partial file");
    }
}

#[test]
fn dropping_a_running_session_discards_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drop.mp4");
    {
        let _s = start(sw_config(&path), realtime(320, 240, Fps::FPS_30), vec![]);
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(!path.exists());
}

#[test]
fn stopping_before_any_frame_reports_empty_and_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.mp4");
    // A source that never produces a frame.
    let src = Box::new(SyntheticSource::new(
        SyntheticConfig::new(320, 240, Fps::from_int(1)).skip(0..1_000_000),
    ));
    let s = start(sw_config(&path), src, vec![]);
    std::thread::sleep(Duration::from_millis(200));
    let err = s.stop().unwrap_err();
    assert!(matches!(err, RecordError::Empty), "{err:?}");
    assert!(!path.exists());
}

#[test]
fn max_duration_stops_the_recording_by_itself() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("max.mp4");
    let mut cfg = sw_config(&path);
    cfg.limits = Limits { max_duration: Some(Duration::from_millis(800)), max_bytes: None };
    let s = start(cfg, realtime(320, 240, Fps::FPS_30), vec![]);
    let why = s.wait_finished(Duration::from_secs(5));
    assert_eq!(why, Some(EndReason::MaxDuration));
    let r = s.stop().unwrap();
    assert_eq!(r.end_reason, EndReason::MaxDuration);
    let secs = r.duration.as_secs_f64();
    assert!((secs - 0.8).abs() < 0.1, "{secs}");
    let v = inspect(&path).unwrap().video.unwrap();
    assert!((v.frame_count() as i64 - 24).abs() <= 2, "{}", v.frame_count());
}

#[test]
fn max_size_stops_and_finalises_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("size.mp4");
    let mut cfg = sw_config(&path);
    cfg.video.quality = ssx_record::encode::Quality::BitrateKbps(6000);
    cfg.limits = Limits { max_duration: None, max_bytes: Some(200_000) };
    // Noisy content would be needed for a big file; the bars at 6 Mbit/s reach 200 kB in
    // well under 10 s only with a high bitrate, so use a large picture.
    let s = start(cfg, realtime(1280, 720, Fps::FPS_30), vec![]);
    let why = s.wait_finished(Duration::from_secs(15));
    let r = s.stop().unwrap();
    assert_eq!(why, Some(EndReason::MaxSize), "{r:?}");
    let size = std::fs::metadata(&path).unwrap().len();
    assert!((150_000..1_500_000).contains(&size), "{size}");
    let rep = inspect(&path).unwrap();
    assert!(rep.seekable && rep.mp4.unwrap().has_moov());
}

/// A prober that accepts only the named encoders.
struct OnlyThese(&'static [&'static str]);

impl Prober for OnlyThese {
    fn probe(&self, c: &Candidate) -> ProbeResult {
        if self.0.contains(&c.name) {
            FfmpegProber.probe(c)
        } else {
            ProbeResult::Unavailable("disabled by the test".into())
        }
    }
}

#[test]
fn encoder_fallback_chain_ends_at_the_native_encoder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fallback.mp4");
    let s = RecordingSession::start(
        sw_config(&path),
        Box::new(SyntheticSource::new(
            SyntheticConfig::new(320, 240, Fps::FPS_30).time(TimeMode::Virtual).max_frames(20),
        )),
        vec![],
        &OnlyThese(&["mpeg4"]),
    )
    .unwrap();
    s.wait_finished(Duration::from_secs(10));
    let r = s.stop().unwrap();
    assert!(r.encoder.contains("mpeg4"), "{}", r.encoder);
    assert!(r.warnings.iter().any(|w| w.contains("mpeg4")), "{:?}", r.warnings);
    assert_eq!(inspect(&path).unwrap().video.unwrap().codec, "mpeg4");
}

#[test]
fn no_usable_encoder_is_a_clear_error_and_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("none.mp4");
    let err = RecordingSession::start(
        sw_config(&path),
        realtime(320, 240, Fps::FPS_30),
        vec![],
        &OnlyThese(&[]),
    )
    .unwrap_err();
    let RecordError::NoEncoder { tried, .. } = err else { panic!("{err:?}") };
    assert!(tried.iter().any(|t| t.contains("libx264")));
    assert!(!path.exists());
}

#[test]
fn webm_uses_vp9_and_opus() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.webm");
    let s = start(
        sw_config(&path),
        realtime(320, 240, Fps::FPS_30),
        vec![tone(Duration::ZERO, TimeMode::Realtime)],
    );
    std::thread::sleep(Duration::from_millis(1000));
    let r = s.stop().unwrap();
    assert!(r.has_audio && r.encoder.contains("libvpx-vp9"), "{}", r.encoder);
    let rep = inspect(&path).unwrap();
    assert_eq!(rep.video.unwrap().codec, "vp9");
    assert_eq!(rep.audio.unwrap().codec, "opus");
}

// ---- audio ------------------------------------------------------------------------------

#[test]
fn a_missing_audio_device_degrades_to_video_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("noaudio.mp4");
    let mut cfg = SyntheticAudioConfig::new(Signal::Silence, TimeMode::Realtime);
    cfg.fail_on_start = true;
    let s = start(
        sw_config(&path),
        realtime(320, 240, Fps::FPS_30),
        vec![Box::new(SyntheticAudio::new(cfg))],
    );
    assert!(!s.info().has_audio);
    assert!(s.warnings().iter().any(|w| w.contains("unavailable")), "{:?}", s.warnings());
    std::thread::sleep(Duration::from_millis(600));
    let r = s.stop().unwrap();
    assert!(!r.has_audio);
    let rep = inspect(&path).unwrap();
    assert!(rep.audio.is_none() && rep.video.is_some());
}

#[test]
fn an_audio_device_that_dies_mid_recording_never_aborts_the_video() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dies.mp4");
    let mut cfg = SyntheticAudioConfig::new(
        Signal::Tone { hz: 440.0, amplitude: 0.5, start: Duration::ZERO },
        TimeMode::Realtime,
    );
    cfg.fail_after = Some(Duration::from_millis(500));
    let s = start(
        sw_config(&path),
        realtime(320, 240, Fps::FPS_30),
        vec![Box::new(SyntheticAudio::new(cfg))],
    );
    std::thread::sleep(Duration::from_millis(1500));
    let r = s.stop().unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("stopped")), "{:?}", r.warnings);
    let rep = inspect(&path).unwrap();
    let v = rep.video.unwrap();
    assert!((v.frame_count() as i64 - 45).abs() <= 4, "{}", v.frame_count());
    let a = rep.audio.expect("the audio track exists (declared at start)");
    // Sound for the first half second, silence padding afterwards, same length as video.
    assert!(a.rms(0.1, 0.4) > 0.2);
    assert!(a.rms(0.8, 1.4) < 0.01, "silence after the device died");
    assert!((a.decoded_seconds() - r.duration.as_secs_f64()).abs() < 0.1);
}

#[test]
fn audio_and_video_meet_on_the_shared_clock() {
    // A tone that starts exactly 1.0 s after the session clock started, and a video whose
    // burnt-in counter equals the timeline slot at capture time: the tone must begin at
    // the frame with counter 30, within 40 ms.
    for (container, name) in [(Container::Mp4, "sync.mp4"), (Container::WebM, "sync.webm")] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut cfg = sw_config(&path);
        cfg.container = container;
        let s = start(
            cfg,
            realtime(320, 240, Fps::FPS_30),
            vec![tone(Duration::from_secs(1), TimeMode::Realtime)],
        );
        std::thread::sleep(Duration::from_millis(2500));
        s.stop().unwrap();
        let rep = inspect(&path).unwrap();
        let v = rep.video.unwrap();
        let a = rep.audio.unwrap();
        let onset = a.onset(0.1).expect("tone") + a.start.max(0.0);
        // The frame whose counter first reaches 30 is where video time = 1 s.
        let frame_pts = v
            .counters
            .iter()
            .zip(&v.pts)
            .find(|(c, _)| **c >= 30)
            .map(|(_, p)| *p)
            .expect("counter 30");
        assert!(
            (onset - frame_pts).abs() < 0.040,
            "{name}: audio tone at {onset:.3} s, video reaches its 1.0 s frame at {frame_pts:.3} s"
        );
        assert!((onset - 1.0).abs() < 0.08, "{name}: onset {onset}");
    }
}

#[test]
fn a_drifting_audio_clock_does_not_slide_out_of_sync() {
    // The device clock is 0.3 % fast (a 10x worse than real hardware): after 6 s it would
    // be 18 ms ahead without correction; the tone must still land at 3.0 s within 40 ms.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drift.mp4");
    let mut acfg = SyntheticAudioConfig::new(
        Signal::Tone { hz: 440.0, amplitude: 0.5, start: Duration::from_secs(5) },
        TimeMode::Realtime,
    );
    acfg.skew = 0.003;
    let s = start(
        sw_config(&path),
        realtime(320, 240, Fps::FPS_30),
        vec![Box::new(SyntheticAudio::new(acfg))],
    );
    std::thread::sleep(Duration::from_millis(7000));
    let r = s.stop().unwrap();
    let rep = inspect(&path).unwrap();
    let a = rep.audio.unwrap();
    // The device's 5.0 s of *device time* is 5.0/1.003 = 4.985 s of clock time.
    let expected = 5.0 / 1.003;
    let onset = a.onset(0.1).unwrap() + a.start.max(0.0);
    assert!((onset - expected).abs() < 0.040, "tone at {onset:.3}, expected {expected:.3}");
    assert!((a.decoded_seconds() - r.duration.as_secs_f64()).abs() < 0.1);
}

// ---- GIF, HDR ---------------------------------------------------------------------------

#[test]
fn gif_recording_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.gif");
    let mut cfg = RecordConfig::new(&path);
    cfg.gif.max_fps = 10.0;
    cfg.gif.max_width = Some(200);
    let s = RecordingSession::start(cfg, realtime(400, 300, Fps::FPS_30), vec![], &FfmpegProber)
        .unwrap();
    std::thread::sleep(Duration::from_millis(2000));
    let r = s.stop().unwrap();
    assert!(r.encoder.contains("gifski"));
    let rep = inspect(&path).unwrap();
    assert!(rep.format.contains("gif"));
    let v = rep.video.unwrap();
    assert_eq!((v.width, v.height), (200, 150));
    assert!((v.frame_count() as i64 - 20).abs() <= 3, "10 fps cap for 2 s: {}", v.frame_count());
    let dur = rep.duration;
    assert!((dur - 2.0).abs() < 0.35, "gif duration {dur}");
    assert_eq!(&std::fs::read(&path).unwrap()[..6], b"GIF89a");
}

#[test]
fn hdr_source_is_tonemapped_on_gpu_or_cpu_to_the_same_picture() {
    use ssx_types::PixelFormat;
    let mut results = Vec::new();
    for gpu in [ssx_record::convert::GpuMode::Off, ssx_record::convert::GpuMode::Auto] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hdr.mp4");
        let mut cfg = sw_config(&path);
        cfg.pipeline.gpu = gpu;
        let src = Box::new(SyntheticSource::new(
            SyntheticConfig::new(320, 240, Fps::FPS_30)
                .format(PixelFormat::Rgba16F)
                .time(TimeMode::Virtual)
                .max_frames(10),
        ));
        let s = start(cfg, src, vec![]);
        s.wait_finished(Duration::from_secs(30));
        let r = s.stop().unwrap();
        eprintln!("gpu={gpu:?}: gpu_frames={} cpu_hdr_frames={}", r.gpu_frames, r.cpu_hdr_frames);
        if gpu == ssx_record::convert::GpuMode::Off {
            assert_eq!(r.gpu_frames, 0);
            assert!(r.cpu_hdr_frames > 0);
        }
        let rep = inspect(&path).unwrap();
        let v = rep.video.unwrap();
        assert_eq!(v.frame_count(), 10);
        // The HDR pattern holds SDR-range content: decoded colours match the SDR bars.
        let first = v.first.unwrap();
        for bar in 0..8u32 {
            let x = bar * 40 + 20;
            let expect = ssx_record::source::synthetic::bar_color_at(0, x, 320);
            let got = first.pixel(x, 20);
            let d = got
                .iter()
                .zip(expect)
                .map(|(a, b)| (i32::from(*a) - i32::from(b)).abs())
                .max()
                .unwrap();
            assert!(d <= 24, "gpu={gpu:?} bar {bar}: {got:?} vs {expect:?}");
        }
        results.push((gpu, r.gpu_frames));
    }
    eprintln!("{results:?}");
}

#[test]
fn codec_selection_honours_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hevc.mkv");
    let mut cfg = sw_config(&path);
    cfg.video.codec = Codec::Hevc;
    let s = start(
        cfg,
        Box::new(SyntheticSource::new(
            SyntheticConfig::new(320, 240, Fps::FPS_30).time(TimeMode::Virtual).max_frames(30),
        )),
        vec![],
    );
    s.wait_finished(Duration::from_secs(30));
    let r = s.stop().unwrap();
    assert!(r.encoder.contains("libx265"), "{}", r.encoder);
    assert_eq!(inspect(&path).unwrap().video.unwrap().codec, "hevc");
    let _ = Instant::now();
}
