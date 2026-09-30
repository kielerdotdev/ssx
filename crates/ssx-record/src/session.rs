//! The recording session: source, convert and encode stages on their own threads.
//!
//! ```text
//!  capture thread            convert thread              encoder thread
//!  ┌──────────────┐  q1    ┌───────────────┐  q2      ┌───────────────────────┐
//!  │ FrameSource  │──────► │ tonemap (GPU/ │────────► │ Encoder (FFmpeg/GIF)  │──► file
//!  │ timeline map │ lossy  │ CPU) + swscale│ blocking │ + audio blocks (qa)   │
//!  │ CFR pacer    │        └───────────────┘          └───────────────────────┘
//!  └──────────────┘                                          ▲
//!                        audio thread: sources ► resample/drift ► mixer ─ qa ─┘
//! ```
//!
//! **Backpressure.** All frame dropping happens in one place, the capture thread's push
//! into `q1`. When the queue is full a duplicate slot is sacrificed before a real frame
//! (a duplicate carries no new picture), and only then the newest real frame. Later
//! stages block instead of dropping, so a slow encoder propagates back to `q1` and is
//! counted exactly once. Queue capacities derive from a byte budget, so memory is bounded
//! (`peak_queue_frames` in the stats proves it). A dropped slot leaves a hole in the
//! encoded timeline (the file keeps correct timing, its frame rate dips), or, with
//! [`GapPolicy::Duplicate`], the encoder thread fills holes with repeats.
//!
//! **Timeline.** Every slot `k` of the constant-rate grid has presentation time `k/fps`
//! and its pixels are the newest source frame not later than that (`pacing`). The timeline
//! starts at the first frame; audio is trimmed to the same origin. Pauses are removed
//! from the timeline for both. The recording ends at the wall time of `stop()` (or the
//! limit / end of the source), and the last slots are filled with duplicates up to that
//! instant so an idle screen keeps its full duration.
//!
//! **Finishing.** `stop()` drains every queue in order (capture, convert, audio, encoder)
//! and only then finalises the file, so `moov` is written even when audio ended early
//! (the mix is padded with silence to the video end). `abort()` and dropping the session
//! discard everything and delete the partial file.

use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use ssx_hdr::TonemapSettings;
use ssx_types::{Frame, Size};

use crate::{
    audio::{AudioFormat, AudioSource, pipeline::AudioPipeline},
    convert::{ConvertConfig, Converter, GpuMode, output_size},
    encode::{
        self, AudioParams, AudioSettings, Container, EncodeSummary, Encoder, EncoderInput,
        GifSettings, InputKind, OutputSpec, Prober, SelectionRequest, VideoParams, VideoSettings,
    },
    error::{RecordError, Result},
    pacing::CfrPacer,
    queue::{Push, Queue, Rank},
    source::{FrameSource, SourceEvent},
    stats::{Counters, StatsSnapshot},
    time::{Clock, Fps},
    timeline::SharedTimeline,
};

const NONE: u64 = u64::MAX;

/// What to do when the encoder could not keep up and slots were dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GapPolicy {
    /// Leave a hole in the timeline: correct duration, the frame rate dips.
    #[default]
    Hold,
    /// Re-encode the previous frame for every missing slot: a strictly constant frame
    /// rate, at the price of more encoder work exactly when it is overloaded.
    Duplicate,
}

/// Recording limits. Reaching one ends the recording gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// Stop after this much recorded time (pauses excluded).
    pub max_duration: Option<Duration>,
    /// Stop when the output file reaches this many bytes.
    pub max_bytes: Option<u64>,
}

/// Pipeline tuning.
#[derive(Debug, Clone)]
pub struct PipelineSettings {
    /// Memory for frames queued between capture and convert. The queue holds
    /// `budget / frame size` frames, clamped to 2..=32.
    pub queue_budget_bytes: usize,
    /// See [`GapPolicy`].
    pub gap_policy: GapPolicy,
    /// GPU policy for HDR tone mapping.
    pub gpu: GpuMode,
    /// HDR tone-mapping parameters.
    pub tonemap: TonemapSettings,
    /// Format of the mixed audio fed to the encoder.
    pub audio_format: AudioFormat,
    /// How long a silent audio source may hold back the mix.
    pub audio_stall: Duration,
    /// How often the output size is sampled for `max_bytes`.
    pub size_check_interval: Duration,
}

impl Default for PipelineSettings {
    fn default() -> Self {
        Self {
            queue_budget_bytes: 192 * 1024 * 1024,
            gap_policy: GapPolicy::Hold,
            gpu: GpuMode::Auto,
            tonemap: TonemapSettings::default(),
            audio_format: AudioFormat::STEREO_48K,
            audio_stall: Duration::from_millis(500),
            size_check_interval: Duration::from_millis(250),
        }
    }
}

/// Everything that configures a recording.
#[derive(Debug, Clone)]
pub struct RecordConfig {
    /// Output file. Its extension is not consulted; `container` decides.
    pub output: PathBuf,
    /// Container (MP4, `WebM`, MKV or GIF).
    pub container: Container,
    /// Constant output frame rate.
    pub fps: Fps,
    /// Video encoder settings.
    pub video: VideoSettings,
    /// Audio encoder settings.
    pub audio: AudioSettings,
    /// GIF settings.
    pub gif: GifSettings,
    /// Pipeline tuning.
    pub pipeline: PipelineSettings,
    /// Limits.
    pub limits: Limits,
}

impl RecordConfig {
    /// Defaults for `output`, with the container taken from its extension (MP4 if it has
    /// none we know).
    pub fn new(output: impl Into<PathBuf>) -> Self {
        let output = output.into();
        let container = output
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Container::from_extension)
            .unwrap_or(Container::Mp4);
        Self {
            output,
            container,
            fps: Fps::FPS_30,
            video: VideoSettings::default(),
            audio: AudioSettings::default(),
            gif: GifSettings::default(),
            pipeline: PipelineSettings::default(),
            limits: Limits::default(),
        }
    }

    fn validate(&self) -> Result<()> {
        if !(0.5..=240.0).contains(&self.fps.as_f64()) {
            return Err(RecordError::InvalidConfig(format!(
                "frame rate {} is out of range",
                self.fps
            )));
        }
        if self.pipeline.audio_format.channels == 0 || self.pipeline.audio_format.channels > 2 {
            return Err(RecordError::InvalidConfig("audio must be mono or stereo".into()));
        }
        Ok(())
    }
}

/// Why a recording ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// `stop()` was called.
    Stopped,
    /// The duration limit was reached.
    MaxDuration,
    /// The file size limit was reached.
    MaxSize,
    /// The source ended by itself (window closed, stream stopped).
    SourceEnded,
    /// A source or encoder error ended it; the message says which.
    Failed(String),
}

/// The result of a finished recording.
#[derive(Debug, Clone)]
pub struct Recording {
    /// The finished file.
    pub path: PathBuf,
    /// Picture size.
    pub size: Size,
    /// Constant frame rate of the timeline.
    pub fps: Fps,
    /// Length of the video timeline (paused time excluded).
    pub duration: Duration,
    /// File size in bytes.
    pub file_bytes: u64,
    /// Final counters.
    pub stats: StatsSnapshot,
    /// Encoder description.
    pub encoder: String,
    /// The file has an audio track.
    pub has_audio: bool,
    /// Why it ended.
    pub end_reason: EndReason,
    /// Problems that did not stop the recording (audio dropped, encoder fallback, ...).
    pub warnings: Vec<String>,
    /// Frames converted on the GPU / tone-mapped on the CPU (HDR sources).
    pub gpu_frames: u64,
    /// HDR frames tone-mapped on the CPU.
    pub cpu_hdr_frames: u64,
    /// Highest number of frames waiting in the capture queue.
    pub peak_queue_frames: u64,
    /// Capacity of that queue (the memory bound).
    pub queue_capacity: usize,
}

/// Static facts about a running session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Output path.
    pub output: PathBuf,
    /// Picture size of the output.
    pub size: Size,
    /// Frame rate.
    pub fps: Fps,
    /// Container.
    pub container: Container,
    /// Video source name.
    pub source: &'static str,
    /// Encoder description (`libx264 (software) + aac`).
    pub encoder: String,
    /// Audio is being recorded.
    pub has_audio: bool,
}

/// Creates the encoder on the encoder thread (`FFmpeg` contexts are not `Send`).
pub type EncoderFactory = Box<dyn FnOnce(&OutputSpec) -> Result<Box<dyn Encoder>> + Send>;

struct ConvItem {
    slot: u64,
    frame: Arc<Frame>,
    duplicate: bool,
}

struct VideoMsg {
    slot: u64,
    input: Arc<EncoderInput>,
}

struct Shared {
    clock: Clock,
    timeline: SharedTimeline,
    counters: Counters,
    stop: AtomicBool,
    abort: AtomicBool,
    /// Clock time at which `stop()` was called.
    stop_at_ns: AtomicU64,
    /// Recording time of the first video frame.
    origin_ns: AtomicU64,
    /// End of the video timeline, relative to the origin.
    end_ns: AtomicU64,
    reason: Mutex<Option<EndReason>>,
    reason_cv: Condvar,
    fatal: Mutex<Option<RecordError>>,
    warnings: Mutex<Vec<String>>,
    max_queue_len: AtomicU64,
    convert_stats: Mutex<(u64, u64)>,
}

impl Shared {
    fn new(clock: Clock) -> Self {
        Self {
            clock,
            timeline: SharedTimeline::new(),
            counters: Counters::default(),
            stop: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            stop_at_ns: AtomicU64::new(NONE),
            origin_ns: AtomicU64::new(NONE),
            end_ns: AtomicU64::new(NONE),
            reason: Mutex::new(None),
            reason_cv: Condvar::new(),
            fatal: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
            max_queue_len: AtomicU64::new(0),
            convert_stats: Mutex::new((0, 0)),
        }
    }

    fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn origin(&self) -> Option<Duration> {
        match self.origin_ns.load(Ordering::Acquire) {
            NONE => None,
            n => Some(Duration::from_nanos(n)),
        }
    }

    fn end(&self) -> Option<Duration> {
        match self.end_ns.load(Ordering::Acquire) {
            NONE => None,
            n => Some(Duration::from_nanos(n)),
        }
    }

    fn warn(&self, message: impl Into<String>) {
        let message = message.into();
        tracing::warn!("{message}");
        self.warnings.lock().unwrap_or_else(PoisonError::into_inner).push(message);
    }

    /// Records why the recording ends; the first reason wins.
    fn finish_with(&self, why: EndReason) {
        let mut g = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if g.is_none() {
            *g = Some(why);
        }
        drop(g);
        self.reason_cv.notify_all();
    }

    fn reason(&self) -> Option<EndReason> {
        self.reason.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_fatal(&self, e: RecordError) {
        let mut g = self.fatal.lock().unwrap_or_else(PoisonError::into_inner);
        if g.is_none() {
            let text = e.to_string();
            *g = Some(e);
            drop(g);
            self.finish_with(EndReason::Failed(text));
        }
    }
}

struct Queues {
    q1: Arc<Queue<ConvItem>>,
    q2: Arc<Queue<VideoMsg>>,
    qa: Arc<Queue<Vec<f32>>>,
}

struct EncoderReady {
    kind: InputKind,
    description: String,
    has_audio: bool,
}

type EncoderOutcome = Result<Option<EncodeSummary>>;

/// A running recording. See the module docs.
pub struct RecordingSession {
    shared: Arc<Shared>,
    queues: Queues,
    info: SessionInfo,
    capture: Option<JoinHandle<()>>,
    convert: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<()>>,
    encoder: Option<JoinHandle<EncoderOutcome>>,
    queue_capacity: usize,
    done: bool,
}

impl std::fmt::Debug for RecordingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingSession").field("info", &self.info).finish_non_exhaustive()
    }
}

fn pixel_bytes(f: ssx_types::PixelFormat) -> usize {
    f.bytes_per_pixel()
}

impl RecordingSession {
    /// Starts a recording with the real encoders: selects one with `prober` (see
    /// [`encode::select`]) and opens it on the encoder thread.
    ///
    /// `audio` sources that fail to start are dropped with a warning; the recording is
    /// then video-only.
    pub fn start(
        cfg: RecordConfig,
        source: Box<dyn FrameSource>,
        audio: Vec<Box<dyn AudioSource>>,
        prober: &dyn Prober,
    ) -> Result<Self> {
        cfg.validate()?;
        let selection = if cfg.container == Container::Gif {
            None
        } else {
            let sel = encode::select(
                &SelectionRequest::from_settings(cfg.container, &cfg.video),
                prober,
            )?;
            tracing::info!(
                encoder = %sel.chosen.describe(),
                rejected = ?sel.rejected_lines(),
                "video encoder selected"
            );
            Some(sel)
        };
        let fallback_note = selection.as_ref().filter(|s| s.codec_fallback).map(|s| {
            format!(
                "no {} encoder is available here; recording with {} instead",
                cfg.video.codec.resolve(cfg.container).name(),
                s.chosen.describe()
            )
        });
        let factory: EncoderFactory = Box::new(move |spec| encode::open(spec, selection.as_ref()));
        let session = Self::start_with(cfg, source, audio, factory)?;
        if let Some(note) = fallback_note {
            session.shared.warn(note);
        }
        Ok(session)
    }

    /// Like [`start`](Self::start) but with an explicit encoder factory (tests inject slow
    /// or failing encoders here).
    pub fn start_with(
        cfg: RecordConfig,
        mut source: Box<dyn FrameSource>,
        audio: Vec<Box<dyn AudioSource>>,
        factory: EncoderFactory,
    ) -> Result<Self> {
        cfg.validate()?;
        let clock = Clock::start();
        let shared = Arc::new(Shared::new(clock));

        source.start(clock).map_err(RecordError::Source)?;
        let src_info = source.info();
        let is_gif = cfg.container == Container::Gif;
        let out_size =
            if is_gif { src_info.size } else { output_size(src_info.size, cfg.video.max_size) };

        // Audio: start what starts; anything else degrades to video-only.
        let mut audio_started: Vec<(Box<dyn AudioSource>, AudioFormat)> = Vec::new();
        if is_gif && !audio.is_empty() {
            shared.warn("GIF has no audio track; audio sources are ignored");
        } else {
            for mut a in audio {
                let name = a.name();
                match a.start(clock) {
                    Ok(fmt) => audio_started.push((a, fmt)),
                    Err(e) => shared.warn(format!("audio source `{name}` unavailable: {e}")),
                }
            }
        }
        let spec = OutputSpec {
            path: cfg.output.clone(),
            container: cfg.container,
            video: VideoParams { size: out_size, fps: cfg.fps },
            audio: (!audio_started.is_empty()).then_some(AudioParams {
                sample_rate: cfg.pipeline.audio_format.sample_rate,
                channels: cfg.pipeline.audio_format.channels,
            }),
            video_settings: cfg.video.clone(),
            audio_settings: cfg.audio,
            gif: cfg.gif,
        };

        // Queues.
        let frame_bytes = (src_info.size.area() as usize).max(1) * pixel_bytes(src_info.format);
        let cap1 = (cfg.pipeline.queue_budget_bytes / frame_bytes).clamp(2, 32);
        let queues = Queues {
            q1: Arc::new(Queue::new(cap1)),
            q2: Arc::new(Queue::new(3)),
            qa: Arc::new(Queue::new(usize::MAX / 2)),
        };

        // Encoder thread first: it decides what the convert stage must produce.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<EncoderReady>>();
        let encoder = {
            let ctx = EncoderCtx {
                shared: Arc::clone(&shared),
                q2: Arc::clone(&queues.q2),
                qa: Arc::clone(&queues.qa),
                q1: Arc::clone(&queues.q1),
                spec: spec.clone(),
                gap: cfg.pipeline.gap_policy,
                limits: cfg.limits,
                check_every: cfg.pipeline.size_check_interval,
                fps: cfg.fps,
            };
            std::thread::Builder::new()
                .name("ssx-encode".into())
                .spawn(move || encoder_thread(ctx, factory, &ready_tx))
                .map_err(|e| {
                    RecordError::Internal(format!("cannot start the encoder thread: {e}"))
                })?
        };
        let ready = match ready_rx.recv() {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let _ = encoder.join();
                source.stop();
                for (mut a, _) in audio_started {
                    a.stop();
                }
                let _ = std::fs::remove_file(&cfg.output);
                return Err(e);
            }
            Err(_) => {
                let _ = encoder.join();
                source.stop();
                return Err(RecordError::Internal("the encoder thread died while starting".into()));
            }
        };
        if !ready.has_audio && !audio_started.is_empty() {
            shared.warn("the encoder could not add an audio track; recording without audio");
            for (mut a, _) in audio_started.drain(..) {
                a.stop();
            }
        }

        // Convert thread.
        let convert = {
            let (sh, q1, q2) =
                (Arc::clone(&shared), Arc::clone(&queues.q1), Arc::clone(&queues.q2));
            let ccfg = ConvertConfig {
                out_size,
                kind: ready.kind,
                gpu: cfg.pipeline.gpu,
                tonemap: cfg.pipeline.tonemap,
            };
            std::thread::Builder::new()
                .name("ssx-convert".into())
                .spawn(move || convert_thread(&sh, &q1, &q2, ccfg))
                .map_err(|e| {
                    RecordError::Internal(format!("cannot start the convert thread: {e}"))
                })?
        };

        // Audio thread (or close the queue right away).
        let audio_handle = if audio_started.is_empty() {
            queues.qa.close();
            None
        } else {
            let (sh, qa) = (Arc::clone(&shared), Arc::clone(&queues.qa));
            let format = cfg.pipeline.audio_format;
            let stall = cfg.pipeline.audio_stall;
            Some(
                std::thread::Builder::new()
                    .name("ssx-audio".into())
                    .spawn(move || audio_thread(&sh, &qa, audio_started, format, stall))
                    .map_err(|e| {
                        RecordError::Internal(format!("cannot start the audio thread: {e}"))
                    })?,
            )
        };

        // Capture thread.
        let capture = {
            let ctx = CaptureCtx {
                shared: Arc::clone(&shared),
                q1: Arc::clone(&queues.q1),
                fps: cfg.fps,
                max_duration: cfg.limits.max_duration,
                realtime: src_info.realtime,
            };
            std::thread::Builder::new()
                .name("ssx-capture".into())
                .spawn(move || capture_thread(ctx, source))
                .map_err(|e| {
                    RecordError::Internal(format!("cannot start the capture thread: {e}"))
                })?
        };

        let info = SessionInfo {
            output: cfg.output.clone(),
            size: out_size,
            fps: cfg.fps,
            container: cfg.container,
            source: src_info.name,
            encoder: ready.description,
            has_audio: ready.has_audio,
        };
        tracing::info!(?info, "recording started");
        Ok(Self {
            shared,
            queues,
            info,
            capture: Some(capture),
            convert: Some(convert),
            audio: audio_handle,
            encoder: Some(encoder),
            queue_capacity: cap1,
            done: false,
        })
    }

    /// Static facts about this recording.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Pauses: frames and audio are discarded and the timeline skips the gap.
    pub fn pause(&self) {
        let now = self.shared.clock.now();
        self.shared.timeline.with(|t| t.pause(now));
    }

    /// Resumes after [`pause`](Self::pause).
    pub fn resume(&self) {
        let now = self.shared.clock.now();
        self.shared.timeline.with(|t| t.resume(now));
    }

    /// `true` while paused.
    pub fn is_paused(&self) -> bool {
        self.shared.timeline.is_paused()
    }

    /// A snapshot of the live counters.
    pub fn stats(&self) -> StatsSnapshot {
        let snap = self.shared.counters.snapshot(Duration::ZERO);
        StatsSnapshot { recorded: self.info.fps.slot_time(snap.slots), ..snap }
    }

    /// Warnings collected so far.
    pub fn warnings(&self) -> Vec<String> {
        self.shared.warnings.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Why the recording is ending on its own (limit reached, source ended, error), if it is.
    /// The session still needs `stop()` to be finalised.
    pub fn finished(&self) -> Option<EndReason> {
        self.shared.reason()
    }

    /// Waits until the recording ends on its own or `timeout` passes.
    pub fn wait_finished(&self, timeout: Duration) -> Option<EndReason> {
        let deadline = Instant::now() + timeout;
        let mut g = self.shared.reason.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(r) = g.clone() {
                return Some(r);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            g = self
                .shared
                .reason_cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn join_all(&mut self) -> Result<EncoderOutcome> {
        let join = |h: Option<JoinHandle<()>>, what: &str| -> Result<()> {
            match h {
                Some(h) => h
                    .join()
                    .map_err(|_| RecordError::Internal(format!("the {what} thread panicked"))),
                None => Ok(()),
            }
        };
        let mut first_err = None;
        for (h, what) in [
            (self.capture.take(), "capture"),
            (self.convert.take(), "convert"),
            (self.audio.take(), "audio"),
        ] {
            if let Err(e) = join(h, what) {
                first_err.get_or_insert(e);
                // A dead stage must not leave the others waiting forever.
                self.shared.abort.store(true, Ordering::SeqCst);
                self.queues.q1.close_and_clear();
                self.queues.q2.close_and_clear();
                self.queues.qa.close_and_clear();
            }
        }
        let outcome = match self.encoder.take() {
            Some(h) => {
                h.join().map_err(|_| RecordError::Internal("the encoder thread panicked".into()))?
            }
            None => Err(RecordError::Internal("encoder already joined".into())),
        };
        match first_err {
            Some(e) => Err(e),
            None => Ok(outcome),
        }
    }

    /// Stops the recording, finalises the file and returns its description.
    ///
    /// Errors: [`RecordError::Empty`] if no frame was captured (the file is deleted), or
    /// the encoder's error if it failed (the partial file is deleted).
    pub fn stop(mut self) -> Result<Recording> {
        self.done = true;
        let now = self.shared.clock.now();
        self.shared.stop_at_ns.store(now.as_nanos() as u64, Ordering::SeqCst);
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.finish_with(EndReason::Stopped);
        let outcome = self.join_all()?;
        let summary = match outcome {
            Ok(Some(s)) => s,
            Ok(None) => return Err(RecordError::Internal("the recording was aborted".into())),
            Err(e) => {
                let _ = std::fs::remove_file(&self.info.output);
                return Err(e);
            }
        };
        let (gpu_frames, cpu_hdr_frames) =
            *self.shared.convert_stats.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stats = self.shared.counters.snapshot(summary.duration);
        stats.file_bytes = summary.bytes;
        Ok(Recording {
            path: summary.path,
            size: self.info.size,
            fps: self.info.fps,
            duration: summary.duration,
            file_bytes: summary.bytes,
            stats,
            encoder: summary.encoder,
            has_audio: summary.has_audio,
            end_reason: self.shared.reason().unwrap_or(EndReason::Stopped),
            warnings: self.warnings(),
            gpu_frames,
            cpu_hdr_frames,
            peak_queue_frames: self.shared.max_queue_len.load(Ordering::Relaxed),
            queue_capacity: self.queue_capacity,
        })
    }

    /// Stops immediately and deletes the partial file.
    pub fn abort(mut self) {
        self.abort_inner();
    }

    fn abort_inner(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        self.shared.abort.store(true, Ordering::SeqCst);
        self.shared.stop.store(true, Ordering::SeqCst);
        self.queues.q1.close_and_clear();
        self.queues.q2.close_and_clear();
        self.queues.qa.close_and_clear();
        let _ = self.join_all();
        let _ = std::fs::remove_file(&self.info.output);
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        if !self.done {
            tracing::warn!("recording session dropped without stop(); discarding the partial file");
            self.abort_inner();
        }
    }
}

// ---- capture stage --------------------------------------------------------------------

struct CaptureCtx {
    shared: Arc<Shared>,
    q1: Arc<Queue<ConvItem>>,
    fps: Fps,
    max_duration: Option<Duration>,
    /// `false`: block on a full queue instead of dropping (offline sources).
    realtime: bool,
}

fn emit(
    ctx: &CaptureCtx,
    pacer: &CfrPacer<Arc<Frame>>,
    out: &mut Vec<crate::pacing::Paced<Arc<Frame>>>,
) {
    let c = &ctx.shared.counters;
    for p in out.drain(..) {
        c.slots.fetch_add(1, Ordering::Relaxed);
        if p.duplicate {
            c.duplicated.fetch_add(1, Ordering::Relaxed);
        }
        let rank = if p.duplicate { Rank::Cheap } else { Rank::Real };
        let item = ConvItem { slot: p.slot, frame: p.item, duplicate: p.duplicate };
        if !ctx.realtime {
            // Offline source: wait for the encoder instead of dropping.
            if ctx.q1.push_blocking(item, || ctx.shared.aborted()).is_ok() {
                ctx.shared.max_queue_len.fetch_max(ctx.q1.len() as u64, Ordering::Relaxed);
            }
            continue;
        }
        match ctx.q1.push_lossy(rank, item) {
            Push::Queued => {
                ctx.shared.max_queue_len.fetch_max(ctx.q1.len() as u64, Ordering::Relaxed);
            }
            Push::Dropped(lost) => {
                c.dropped_backpressure.fetch_add(1, Ordering::Relaxed);
                if lost.duplicate {
                    c.dropped_duplicates.fetch_add(1, Ordering::Relaxed);
                }
            }
            Push::Closed(_) => {}
        }
    }
    c.surplus_dropped.store(pacer.stats().surplus_dropped, Ordering::Relaxed);
}

fn capture_thread(ctx: CaptureCtx, mut source: Box<dyn FrameSource>) {
    let sh = &ctx.shared;
    let fps = ctx.fps;
    let mut pacer: CfrPacer<Arc<Frame>> = CfrPacer::new(fps);
    let mut out = Vec::new();
    let poll = fps.frame_duration().clamp(Duration::from_millis(2), Duration::from_millis(50));
    let mut origin: Option<Duration> = None;

    // Recording time since the origin for a clock time (`None` while paused / before start).
    let rel = |origin: Option<Duration>, clock_t: Duration| -> Option<Duration> {
        let o = origin?;
        sh.timeline.map(clock_t).map(|r| r.saturating_sub(o))
    };

    loop {
        if sh.aborted() || sh.stopping() || sh.reason().is_some() {
            break;
        }
        match source.next_frame(poll) {
            Ok(SourceEvent::Frame(vf)) => {
                sh.counters.captured.fetch_add(1, Ordering::Relaxed);
                let Some(rec) = sh.timeline.map(vf.timestamp) else {
                    sh.counters.paused_discarded.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                let o = *origin.get_or_insert_with(|| {
                    sh.origin_ns.store(rec.as_nanos() as u64, Ordering::Release);
                    rec
                });
                let t = rec.saturating_sub(o);
                if ctx.max_duration.is_some_and(|m| t >= m) {
                    sh.finish_with(EndReason::MaxDuration);
                    break;
                }
                pacer.push(t, Arc::new(vf.frame), &mut out);
            }
            Ok(SourceEvent::Timeout) => {
                if let Some(t) = rel(origin, sh.clock.now()) {
                    if ctx.max_duration.is_some_and(|m| t >= m) {
                        sh.finish_with(EndReason::MaxDuration);
                        break;
                    }
                    pacer.tick(t, &mut out);
                }
            }
            Ok(SourceEvent::Ended) => {
                sh.finish_with(EndReason::SourceEnded);
                break;
            }
            Err(e) => {
                sh.warn(format!("capture stopped: {e}"));
                sh.finish_with(EndReason::Failed(e.to_string()));
                break;
            }
        }
        emit(&ctx, &pacer, &mut out);
    }
    source.stop();

    // End of the video timeline: the later of the last emitted slot and the wall time of
    // the stop request (or of now, if we ended by ourselves), capped at the limit.
    let stop_at = match sh.stop_at_ns.load(Ordering::SeqCst) {
        NONE => sh.clock.now(),
        n => Duration::from_nanos(n),
    };
    let by_time = match origin {
        Some(o) => {
            let rec = stop_at.saturating_sub(sh.timeline.with(|t| t.paused_before(stop_at)));
            rec.saturating_sub(o)
        }
        None => Duration::ZERO,
    };
    let mut end = fps.slot_time(pacer.next_slot()).max(by_time);
    if pacer.started() {
        if let Some(m) = ctx.max_duration {
            end = end.min(m);
        }
        pacer.tick(end, &mut out);
        emit(&ctx, &pacer, &mut out);
    } else {
        end = Duration::ZERO;
    }
    sh.end_ns.store(end.as_nanos() as u64, Ordering::Release);
    if sh.aborted() {
        ctx.q1.close_and_clear();
    } else {
        ctx.q1.close();
    }
}

// ---- convert stage --------------------------------------------------------------------

fn convert_thread(
    sh: &Arc<Shared>,
    q1: &Queue<ConvItem>,
    q2: &Queue<VideoMsg>,
    cfg: ConvertConfig,
) {
    let mut conv = Converter::new(cfg);
    let mut cache: Option<(Arc<Frame>, Arc<EncoderInput>)> = None;
    let mut consecutive_errors = 0u32;
    let mut warned = false;
    loop {
        if sh.aborted() {
            break;
        }
        match q1.pop(Duration::from_millis(50)) {
            Ok(Some(item)) => {
                let input = match &cache {
                    Some((f, i)) if Arc::ptr_eq(f, &item.frame) => Arc::clone(i),
                    _ => match conv.convert(&item.frame) {
                        Ok(i) => {
                            let i = Arc::new(i);
                            cache = Some((Arc::clone(&item.frame), Arc::clone(&i)));
                            consecutive_errors = 0;
                            i
                        }
                        Err(e) => {
                            sh.counters.dropped_convert.fetch_add(1, Ordering::Relaxed);
                            consecutive_errors += 1;
                            if !warned {
                                warned = true;
                                sh.warn(format!("frame conversion failed: {e}"));
                            }
                            if consecutive_errors >= 30 {
                                sh.set_fatal(e);
                                break;
                            }
                            continue;
                        }
                    },
                };
                if q2.push_blocking(VideoMsg { slot: item.slot, input }, || sh.aborted()).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    *sh.convert_stats.lock().unwrap_or_else(PoisonError::into_inner) =
        (conv.gpu_frames, conv.cpu_hdr_frames);
    if sh.aborted() {
        q2.close_and_clear();
    } else {
        q2.close();
    }
}

// ---- audio stage ----------------------------------------------------------------------

fn audio_thread(
    sh: &Arc<Shared>,
    qa: &Queue<Vec<f32>>,
    sources: Vec<(Box<dyn AudioSource>, AudioFormat)>,
    format: AudioFormat,
    stall: Duration,
) {
    let mut pipe = AudioPipeline::new(sources, format, sh.timeline.clone(), stall);
    let mut block = Vec::new();
    let flush_warnings = |pipe: &mut AudioPipeline| {
        for w in pipe.take_warnings() {
            sh.warn(w);
        }
    };
    flush_warnings(&mut pipe);
    loop {
        if sh.aborted() {
            qa.close_and_clear();
            return;
        }
        pipe.poll(sh.origin(), Duration::from_millis(10), &mut block);
        flush_warnings(&mut pipe);
        if !block.is_empty() {
            let _ = qa.push_unbounded(std::mem::take(&mut block));
        }
        if let Some(end) = sh.end() {
            pipe.finish(sh.origin(), end, &mut block);
            flush_warnings(&mut pipe);
            if !block.is_empty() {
                let _ = qa.push_unbounded(std::mem::take(&mut block));
            }
            break;
        }
    }
    let s = pipe.stats();
    sh.counters.audio_silence_samples.fetch_add(s.gap_frames, Ordering::Relaxed);
    qa.close();
}

// ---- encoder stage --------------------------------------------------------------------

struct EncoderCtx {
    shared: Arc<Shared>,
    q1: Arc<Queue<ConvItem>>,
    q2: Arc<Queue<VideoMsg>>,
    qa: Arc<Queue<Vec<f32>>>,
    spec: OutputSpec,
    gap: GapPolicy,
    limits: Limits,
    check_every: Duration,
    fps: Fps,
}

fn encoder_thread(
    ctx: EncoderCtx,
    factory: EncoderFactory,
    ready: &mpsc::Sender<Result<EncoderReady>>,
) -> EncoderOutcome {
    let sh = &ctx.shared;
    let mut enc = match factory(&ctx.spec) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(None);
        }
    };
    let has_audio = enc.has_audio();
    let _ = ready.send(Ok(EncoderReady {
        kind: enc.input_kind(),
        description: enc.description(),
        has_audio,
    }));

    let channels = usize::from(ctx.spec.audio.map_or(2, |a| a.channels));
    let mut expected = 0u64;
    let mut last_input: Option<Arc<EncoderInput>> = None;
    let mut audio_failed = false;
    let mut video_closed = false;
    let mut audio_closed = false;
    let mut last_check = Instant::now();
    let mut failure: Option<RecordError> = None;

    while !(video_closed && audio_closed) {
        if sh.aborted() {
            enc.abort();
            return Ok(None);
        }
        // Audio first: it is tiny and must never wait behind video.
        loop {
            match ctx.qa.pop(Duration::ZERO) {
                Ok(Some(block)) => {
                    if !has_audio || audio_failed {
                        continue;
                    }
                    match enc.write_audio(&block) {
                        Ok(()) => {
                            sh.counters
                                .audio_samples
                                .fetch_add((block.len() / channels) as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            audio_failed = true;
                            sh.warn(format!(
                                "audio encoding failed, the rest of the file has no sound: {e}"
                            ));
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    audio_closed = true;
                    break;
                }
            }
        }
        match ctx.q2.pop(Duration::from_millis(4)) {
            Ok(Some(msg)) => {
                if ctx.gap == GapPolicy::Duplicate
                    && msg.slot > expected
                    && let Some(prev) = &last_input
                {
                    for s in expected..msg.slot {
                        if let Err(e) = enc.write_video(s, prev) {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                if failure.is_none() {
                    match enc.write_video(msg.slot, &msg.input) {
                        Ok(()) => {
                            sh.counters.encoded.fetch_add(1, Ordering::Relaxed);
                            expected = msg.slot + 1;
                            last_input = Some(msg.input);
                        }
                        Err(e) => failure = Some(e),
                    }
                }
                if failure.is_some() {
                    break;
                }
            }
            Ok(None) => {}
            Err(_) => video_closed = true,
        }
        if last_check.elapsed() >= ctx.check_every {
            last_check = Instant::now();
            let bytes = enc.bytes_written();
            sh.counters.file_bytes.store(bytes, Ordering::Relaxed);
            if ctx.limits.max_bytes.is_some_and(|m| bytes >= m) {
                sh.finish_with(EndReason::MaxSize);
            }
        }
    }

    if let Some(e) = failure {
        // The output is unusable: stop the pipeline, discard the file, report the error.
        sh.set_fatal(RecordError::encoder("encoder", e.to_string()));
        ctx.q1.close_and_clear();
        ctx.q2.close_and_clear();
        ctx.qa.close_and_clear();
        enc.abort();
        return Err(e);
    }
    let end = sh.end().unwrap_or_else(|| ctx.fps.slot_time(expected));
    if expected == 0 {
        enc.abort();
        return Err(RecordError::Empty);
    }
    match enc.finish(end) {
        Ok(mut s) => {
            sh.counters.file_bytes.store(s.bytes, Ordering::Relaxed);
            // The end of the timeline is what the recording covers, even if the last
            // slots were dropped.
            s.duration = end.max(s.duration);
            Ok(Some(s))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_handle_can_move_between_threads() {
        fn is_send<T: Send>() {}
        is_send::<RecordingSession>();
    }

    #[test]
    fn config_defaults_follow_the_extension() {
        assert_eq!(RecordConfig::new("a.webm").container, Container::WebM);
        assert_eq!(RecordConfig::new("a.GIF").container, Container::Gif);
        assert_eq!(RecordConfig::new("a.mkv").container, Container::Mkv);
        assert_eq!(RecordConfig::new("noext").container, Container::Mp4);
    }

    #[test]
    fn invalid_configs_are_rejected() {
        let mut c = RecordConfig::new("a.mp4");
        c.fps = Fps::new(1000, 1);
        assert!(matches!(c.validate(), Err(RecordError::InvalidConfig(_))));
        let mut c = RecordConfig::new("a.mp4");
        c.pipeline.audio_format.channels = 6;
        assert!(c.validate().is_err());
        assert!(RecordConfig::new("a.mp4").validate().is_ok());
    }
}
