//! Glue to the workflow engine: [`SsxRecorder`] implements `ssx_core::workflow::Recorder`
//! and hands out [`RecordingSessionAdapter`]s that implement
//! `ssx_core::workflow::RecordingSession` on top of this crate's
//! [`RecordingSession`](crate::session::RecordingSession).
//!
//! What the workflow engine asks for is small (`RecordRequest`: container kind, output
//! directory, file stem, cursor flag); everything else comes from [`RecorderSettings`]
//! (the user's recording preferences) and a [`SourceFactory`], which is where a UI plugs in
//! region selection (the default factory records the whole desktop of the current session).
//! The output name is reserved with `ssx_core::pattern::create_unique`, so a recording
//! never overwrites an existing file, and a cancelled or failed recording leaves nothing
//! behind.

use std::{sync::Arc, time::Duration};

use ssx_core::workflow::{
    RecordKind, RecordRequest, RecordedVideo, Recorder, RecordingSession as CoreSession,
    ServiceError,
};

use crate::{
    audio::AudioSource,
    encode::{
        AudioSettings, CachingProber, Container, GifSettings, ProbeResult, Prober, VideoSettings,
        select::Candidate,
    },
    error::{RecordError, SourceError},
    session::{Limits, PipelineSettings, RecordConfig, RecordingSession},
    source::{CaptureTarget, FrameSource, SourceConfig, auto::SourceKind},
    time::Fps,
};

/// Which audio the recorder captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioSelection {
    /// Record what the system plays (loopback).
    pub system: bool,
    /// Record the default microphone.
    pub microphone: bool,
}

/// The user's recording preferences.
#[derive(Debug, Clone)]
pub struct RecorderSettings {
    /// Frame rate.
    pub fps: Fps,
    /// Container for [`RecordKind::Video`].
    pub video_container: Container,
    /// Video encoder settings.
    pub video: VideoSettings,
    /// Audio encoder settings.
    pub audio_encoding: AudioSettings,
    /// Which audio to capture.
    pub audio: AudioSelection,
    /// GIF settings for [`RecordKind::Gif`].
    pub gif: GifSettings,
    /// Pipeline tuning.
    pub pipeline: PipelineSettings,
    /// Limits.
    pub limits: Limits,
    /// Which capture path to use.
    pub source: SourceKind,
    /// What to capture when the factory is the default one.
    pub target: CaptureTarget,
}

impl Default for RecorderSettings {
    fn default() -> Self {
        Self {
            fps: Fps::FPS_30,
            video_container: Container::Mp4,
            video: VideoSettings::default(),
            audio_encoding: AudioSettings::default(),
            audio: AudioSelection::default(),
            gif: GifSettings::default(),
            pipeline: PipelineSettings::default(),
            limits: Limits::default(),
            source: SourceKind::Auto,
            target: CaptureTarget::Desktop,
        }
    }
}

/// The frame source and audio sources for one recording.
pub struct OpenedSources {
    /// Video.
    pub video: Box<dyn FrameSource>,
    /// Audio (may be empty).
    pub audio: Vec<Box<dyn AudioSource>>,
}

impl std::fmt::Debug for OpenedSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedSources")
            .field("video", &self.video.name())
            .field("audio", &self.audio.len())
            .finish()
    }
}

/// Creates the sources of a recording. A UI implements this to let the user pick a region
/// or window first; returning [`ServiceError::Cancelled`] cancels the recording.
pub trait SourceFactory: Send + Sync {
    /// Opens the sources for `req` (not yet started).
    fn open(
        &self,
        req: &RecordRequest,
        settings: &RecorderSettings,
    ) -> Result<OpenedSources, ServiceError>;
}

/// The default factory: the platform's own capture path for `settings.target`, plus the
/// audio selected in the settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoSourceFactory;

impl SourceFactory for AutoSourceFactory {
    fn open(
        &self,
        req: &RecordRequest,
        settings: &RecorderSettings,
    ) -> Result<OpenedSources, ServiceError> {
        let cfg = SourceConfig {
            target: settings.target.clone(),
            fps: settings.fps,
            cursor: req.include_cursor,
        };
        let video = crate::source::auto::open(settings.source, cfg).map_err(source_error)?;
        Ok(OpenedSources { video, audio: default_audio(settings.audio) })
    }
}

/// The audio sources for `sel` on this platform (none when the `audio` feature is off).
pub fn default_audio(sel: AudioSelection) -> Vec<Box<dyn AudioSource>> {
    #[allow(unused_mut)]
    let mut v: Vec<Box<dyn AudioSource>> = Vec::new();
    #[cfg(feature = "audio")]
    {
        use crate::audio::device::{CpalSource, DeviceKind, DeviceSelector};
        if sel.system {
            v.push(Box::new(CpalSource::new(DeviceKind::SystemLoopback, DeviceSelector::Default)));
        }
        if sel.microphone {
            v.push(Box::new(CpalSource::new(DeviceKind::Microphone, DeviceSelector::Default)));
        }
    }
    #[cfg(not(feature = "audio"))]
    let _ = sel;
    v
}

fn source_error(e: SourceError) -> ServiceError {
    match e {
        SourceError::PermissionDenied(_) => ServiceError::Cancelled,
        SourceError::Unsupported(m) => ServiceError::Unsupported(m),
        other => ServiceError::failed(other.to_string()),
    }
}

fn record_error(e: RecordError) -> ServiceError {
    match e {
        RecordError::Source(s) => source_error(s),
        RecordError::Io { source, .. } => ServiceError::Io(source),
        RecordError::Unsupported(m) => ServiceError::Unsupported(m),
        other => ServiceError::failed(other.to_string()),
    }
}

/// A prober for builds without `FFmpeg`: nothing but GIF works.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFfmpegProber;

impl Prober for NoFfmpegProber {
    fn probe(&self, _: &Candidate) -> ProbeResult {
        ProbeResult::Unavailable("this build has no FFmpeg".into())
    }
}

/// The application-facing recorder. Cheap to share; keep one for the whole process so the
/// encoder probe results are cached.
pub struct SsxRecorder {
    settings: RecorderSettings,
    factory: Arc<dyn SourceFactory>,
    prober: Arc<dyn Prober + Send + Sync>,
}

impl std::fmt::Debug for SsxRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsxRecorder").field("settings", &self.settings).finish_non_exhaustive()
    }
}

impl SsxRecorder {
    /// A recorder with the default source factory and the real `FFmpeg` prober (cached).
    pub fn new(settings: RecorderSettings) -> Self {
        #[cfg(feature = "ffmpeg")]
        let prober: Arc<dyn Prober + Send + Sync> =
            Arc::new(CachingProber::new(crate::encode::ffmpeg::FfmpegProber));
        #[cfg(not(feature = "ffmpeg"))]
        let prober: Arc<dyn Prober + Send + Sync> = Arc::new(CachingProber::new(NoFfmpegProber));
        Self { settings, factory: Arc::new(AutoSourceFactory), prober }
    }

    /// Replaces the source factory (region/window selection UI, tests).
    pub fn with_factory(mut self, factory: Arc<dyn SourceFactory>) -> Self {
        self.factory = factory;
        self
    }

    /// Replaces the encoder prober (tests).
    pub fn with_prober(mut self, prober: Arc<dyn Prober + Send + Sync>) -> Self {
        self.prober = prober;
        self
    }

    /// The settings in use.
    pub fn settings(&self) -> &RecorderSettings {
        &self.settings
    }

    /// Starts a recording and returns the native session (with pause/resume/stats) rather
    /// than the workflow adapter.
    pub fn start_session(&self, req: &RecordRequest) -> Result<RecordingSession, ServiceError> {
        let container = match req.kind {
            RecordKind::Video => self.settings.video_container,
            RecordKind::Gif => Container::Gif,
        };
        let name = format!("{}.{}", req.file_stem, container.extension());
        let (path, _reservation) =
            ssx_core::pattern::create_unique(&req.output_dir, &name).map_err(ServiceError::Io)?;
        let result = (|| {
            let sources = self.factory.open(req, &self.settings)?;
            let mut cfg = RecordConfig::new(&path);
            cfg.container = container;
            cfg.fps = self.settings.fps;
            cfg.video = self.settings.video.clone();
            cfg.audio = self.settings.audio_encoding;
            cfg.gif = self.settings.gif;
            cfg.pipeline = self.settings.pipeline.clone();
            cfg.limits = self.settings.limits;
            RecordingSession::start(cfg, sources.video, sources.audio, &*self.prober)
                .map_err(record_error)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }
}

impl Recorder for SsxRecorder {
    fn start(&self, req: &RecordRequest) -> Result<Box<dyn CoreSession>, ServiceError> {
        Ok(Box::new(RecordingSessionAdapter::new(self.start_session(req)?)))
    }
}

/// Presents a [`RecordingSession`] as the workflow engine's session trait.
#[derive(Debug)]
pub struct RecordingSessionAdapter {
    session: RecordingSession,
}

impl RecordingSessionAdapter {
    /// Wraps `session`.
    pub fn new(session: RecordingSession) -> Self {
        Self { session }
    }

    /// The wrapped session (pause, resume, stats).
    pub fn session(&self) -> &RecordingSession {
        &self.session
    }
}

impl CoreSession for RecordingSessionAdapter {
    fn stop(self: Box<Self>) -> Result<RecordedVideo, ServiceError> {
        let rec = self.session.stop().map_err(record_error)?;
        for w in &rec.warnings {
            tracing::warn!("recording: {w}");
        }
        Ok(RecordedVideo {
            path: rec.path,
            width: Some(rec.size.width),
            height: Some(rec.size.height),
            duration: Some(rec.duration).filter(|d| *d > Duration::ZERO),
        })
    }

    fn abort(self: Box<Self>) {
        self.session.abort();
    }
}
