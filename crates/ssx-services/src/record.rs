//! The real [`Recorder`]: `ssx-record` with region selection through the overlay.
//!
//! `ssx-record`'s [`SsxRecorder`] records "whatever its source factory opens". This module
//! provides the factory that decides *what to record* for ssx:
//!
//! * a [`RecordingPlan`] (what the daemon's tray/hotkey/IPC or the CLI asked for, set with
//!   [`ServiceRecorder::set_plan`] just before the recording starts) chooses between the
//!   whole desktop, one monitor, an exact rectangle, or **an interactive region** picked on the
//!   frozen-desktop overlay;
//! * on GNOME/KDE (the `portal` capture backend) the compositor shows its own picker, so the
//!   overlay is skipped there;
//! * audio (none, microphone, system, both) comes from the plan, else the configured default.
//!
//! One recording at a time is the *caller's* rule (the daemon enforces it); the plan is a
//! single slot precisely because of that. Recordings never overwrite an existing file and a
//! cancelled or failed start leaves nothing behind (both are `ssx-record`'s guarantees).

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ssx_core::{
    ipc::{RecordAudio, RecordTarget},
    workflow::{
        CancelToken, RecordRequest, Recorder, RecordingSession as CoreSession, ServiceError,
    },
};
use ssx_platform::BackendKind;
use ssx_record::{
    adapter::{
        AudioSelection, OpenedSources, RecorderSettings, SourceFactory, SsxRecorder, default_audio,
    },
    source::{CaptureTarget, SourceConfig, auto::SourceKind},
    time::Fps,
};
use ssx_types::{Monitor, Rect};

pub use ssx_record::encode::Mp4Mode;

use crate::{
    capture::{LastRegionStore, PickRequest, RegionSelector, ScreenCapturer},
    overlay::OverlaySelector,
};

/// What the next recording captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingPlan {
    /// The picture.
    pub target: RecordTarget,
    /// The sound; `None` keeps the recorder's configured default.
    pub audio: Option<RecordAudio>,
}

impl Default for RecordingPlan {
    fn default() -> Self {
        Self { target: RecordTarget::Interactive, audio: None }
    }
}

/// How the recorder is set up.
#[derive(Clone)]
pub struct RecorderOptions {
    /// Force a capture path (as `SSX_BACKEND` does); `None` chooses for the session.
    pub backend: Option<BackendKind>,
    /// Frames per second.
    pub fps: Fps,
    /// MP4 layout. The daemon uses fragmented MP4 (playable even if the process dies), the
    /// CLI the classic layout.
    pub mp4: Mp4Mode,
    /// Audio when the plan does not say.
    pub audio: RecordAudio,
    /// The overlay for [`RecordTarget::Interactive`]; without it the whole desktop is recorded.
    pub selector: Option<Arc<OverlaySelector>>,
}

impl std::fmt::Debug for RecorderOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecorderOptions")
            .field("backend", &self.backend)
            .field("mp4", &self.mp4)
            .field("audio", &self.audio)
            .field("selector", &self.selector.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for RecorderOptions {
    fn default() -> Self {
        Self {
            backend: None,
            fps: Fps::FPS_30,
            mp4: Mp4Mode::default(),
            audio: RecordAudio::None,
            selector: None,
        }
    }
}

/// The [`Recorder`] service. See the [module docs](self).
pub struct ServiceRecorder {
    inner: SsxRecorder,
    plan: Arc<Mutex<RecordingPlan>>,
    cancel: Arc<Mutex<CancelToken>>,
}

impl std::fmt::Debug for ServiceRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRecorder").finish_non_exhaustive()
    }
}

/// The `ssx-record` source kind for a capture backend family.
pub fn source_kind(backend: Option<BackendKind>) -> SourceKind {
    match backend {
        None => SourceKind::Auto,
        Some(BackendKind::Windows) => SourceKind::Windows,
        Some(BackendKind::Wayland) => SourceKind::Wayland,
        Some(BackendKind::Portal) => SourceKind::Portal,
        Some(BackendKind::X11) => SourceKind::X11,
    }
}

/// Audio selection of a [`RecordAudio`].
pub fn audio_selection(a: RecordAudio) -> AudioSelection {
    match a {
        RecordAudio::None => AudioSelection::default(),
        RecordAudio::Mic => AudioSelection { system: false, microphone: true },
        RecordAudio::System => AudioSelection { system: true, microphone: false },
        RecordAudio::Both => AudioSelection { system: true, microphone: true },
    }
}

impl ServiceRecorder {
    /// Builds the recorder.
    pub fn new(options: RecorderOptions) -> Self {
        let plan = Arc::new(Mutex::new(RecordingPlan::default()));
        let cancel = Arc::new(Mutex::new(CancelToken::new()));
        let mut settings = RecorderSettings {
            fps: options.fps,
            source: source_kind(options.backend),
            audio: audio_selection(options.audio),
            ..RecorderSettings::default()
        };
        settings.video.mp4 = options.mp4;
        let factory = PlanFactory {
            plan: Arc::clone(&plan),
            cancel: Arc::clone(&cancel),
            selector: options.selector,
            backend: options.backend,
            snapshots: ScreenCapturer::detect(options.backend, LastRegionStore::none()),
        };
        Self { inner: SsxRecorder::new(settings).with_factory(Arc::new(factory)), plan, cancel }
    }

    /// Lets the *next* start be cancelled while the overlay is still asking for a region
    /// (the engine's `Recorder::start` has no token of its own).
    pub fn set_cancel(&self, cancel: CancelToken) {
        *self.cancel.lock().unwrap_or_else(PoisonError::into_inner) = cancel;
    }

    /// Sets what the *next* recording captures.
    pub fn set_plan(&self, plan: RecordingPlan) {
        *self.plan.lock().unwrap_or_else(PoisonError::into_inner) = plan;
    }

    /// The plan the next recording will use.
    pub fn plan(&self) -> RecordingPlan {
        self.plan.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Recorder for ServiceRecorder {
    fn start(&self, req: &RecordRequest) -> Result<Box<dyn CoreSession>, ServiceError> {
        self.inner.start(req)
    }
}

/// Opens the sources for a [`RecordingPlan`].
struct PlanFactory {
    plan: Arc<Mutex<RecordingPlan>>,
    cancel: Arc<Mutex<CancelToken>>,
    selector: Option<Arc<OverlaySelector>>,
    backend: Option<BackendKind>,
    /// Frozen desktops and monitor lists come from a source of our own; the recording itself
    /// opens its own capture path.
    snapshots: ScreenCapturer,
}

/// Rounds a region down to even dimensions: 4:2:0 encoders cannot take odd ones.
pub fn even_region(r: Rect) -> Option<Rect> {
    let (w, h) = (r.width & !1, r.height & !1);
    (w >= 2 && h >= 2).then(|| Rect::new(r.x, r.y, w, h))
}

/// The monitor a [`RecordTarget::Monitor`] means: the id given, else the primary.
pub fn pick_monitor<'a>(monitors: &'a [Monitor], id: Option<&str>) -> Option<&'a Monitor> {
    match id {
        Some(id) => monitors.iter().find(|m| m.id == id),
        None => monitors.iter().find(|m| m.primary).or_else(|| monitors.first()),
    }
}

impl PlanFactory {
    fn target(&self, plan: &RecordingPlan) -> Result<CaptureTarget, ServiceError> {
        Ok(match &plan.target {
            RecordTarget::Desktop => CaptureTarget::Desktop,
            RecordTarget::Rect { x, y, width, height } => {
                let r = even_region(Rect::new(*x, *y, *width, *height)).ok_or_else(|| {
                    ServiceError::failed(format!(
                        "the recording region {width}x{height} is too small (at least 2x2 pixels)"
                    ))
                })?;
                CaptureTarget::Region(r)
            }
            RecordTarget::Monitor { id } => {
                let monitors = self.snapshots.monitors()?;
                let m = pick_monitor(&monitors, id.as_deref()).ok_or_else(|| {
                    ServiceError::failed(match id {
                        Some(id) => {
                            format!("no monitor with id {id:?}; list them with `ssx monitors`")
                        }
                        None => "the capture backend reported no monitors".to_owned(),
                    })
                })?;
                CaptureTarget::Region(even_region(m.rect).unwrap_or(m.rect))
            }
            RecordTarget::Interactive => self.interactive(&self.cancel_token())?,
        })
    }

    fn cancel_token(&self) -> CancelToken {
        self.cancel.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn interactive(&self, cancel: &CancelToken) -> Result<CaptureTarget, ServiceError> {
        // GNOME and KDE (the portal): the compositor's own picker chooses, and it is the only
        // way to record there, so an overlay would be pointless.
        let portal = matches!(
            self.snapshots.backend_report().map(|r| r.kind),
            Ok(Some(BackendKind::Portal))
        ) || self.backend == Some(BackendKind::Portal);
        if portal {
            return Ok(CaptureTarget::Pick);
        }
        let Some(selector) = &self.selector else {
            tracing::info!("no selection overlay available: recording the whole desktop");
            return Ok(CaptureTarget::Desktop);
        };
        let opts = ssx_capture::CaptureOptions { include_cursor: false };
        let (desktop, monitors, windows) = self.snapshots.with(|s| {
            let desktop = s.capture_desktop(&opts)?;
            Ok((desktop, s.monitors().unwrap_or_default(), s.windows().unwrap_or_default()))
        })?;
        selector.set_mode(crate::overlay::PickMode::Rect);
        let picked = selector
            .pick(&PickRequest {
                desktop: &desktop,
                monitors: &monitors,
                windows: &windows,
                initial: None,
                cancel,
            })?
            .ok_or(ServiceError::Cancelled)?;
        let clipped =
            picked.rect.intersect(desktop.rect()).and_then(even_region).ok_or_else(|| {
                ServiceError::failed("the selected region is too small to record")
            })?;
        Ok(CaptureTarget::Region(clipped))
    }
}

impl SourceFactory for PlanFactory {
    fn open(
        &self,
        req: &RecordRequest,
        settings: &RecorderSettings,
    ) -> Result<OpenedSources, ServiceError> {
        let plan = self.plan.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let target = self.target(&plan)?;
        let cfg = SourceConfig { target, fps: settings.fps, cursor: req.include_cursor };
        let video = ssx_record::source::auto::open(settings.source, cfg)
            .map_err(|e| ServiceError::failed(format!("cannot start the screen capture: {e}")))?;
        let audio = plan.audio.map_or(settings.audio, audio_selection);
        Ok(OpenedSources { video, audio: default_audio(audio) })
    }
}

/// One encoder candidate and whether it works on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderProbe {
    /// `FFmpeg` encoder name (`libx264`, `h264_vaapi`, `mpeg4`, ...).
    pub name: String,
    /// It opened and encoded a test picture.
    pub usable: bool,
    /// Why not (empty when usable).
    pub detail: String,
}

/// Probes the H.264 encoder chain (hardware first, then software) on this machine: what
/// `ssx doctor` shows under "recorder". Empty when built without `FFmpeg`.
pub fn probe_encoders() -> Vec<EncoderProbe> {
    #[cfg(feature = "ffmpeg")]
    {
        use ssx_record::encode::{
            Codec, Container, HwPolicy, Prober, SelectionRequest, candidates, ffmpeg::FfmpegProber,
            select::Platform,
        };
        let req = SelectionRequest {
            container: Container::Mp4,
            codec: Codec::H264,
            hw: HwPolicy::PreferHardware,
            allow_gpl: true,
            allow_codec_fallback: true,
            platform: Platform::current(),
            encoder_override: None,
        };
        let prober = FfmpegProber;
        candidates(&req)
            .into_iter()
            .map(|c| {
                let r = prober.probe(&c);
                EncoderProbe {
                    name: c.name.to_owned(),
                    usable: r.is_usable(),
                    detail: if r.is_usable() { String::new() } else { format!("{r:?}") },
                }
            })
            .collect()
    }
    #[cfg(not(feature = "ffmpeg"))]
    {
        Vec::new()
    }
}

/// `true` when this build can encode MP4 (`FFmpeg` linked); GIF needs nothing extra.
pub const fn has_ffmpeg() -> bool {
    cfg!(feature = "ffmpeg")
}

/// A longest-sane wait used by callers that stop a recording after a fixed time.
pub const MAX_RECORDING: Duration = Duration::from_secs(12 * 60 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, primary: bool) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            rect: Rect::new(0, 0, 100, 60),
            scale_factor: 1.0,
            primary,
            refresh_hz: None,
            hdr: None,
        }
    }

    #[test]
    fn regions_are_made_even_and_tiny_ones_refused() {
        assert_eq!(even_region(Rect::new(3, 4, 101, 51)), Some(Rect::new(3, 4, 100, 50)));
        assert_eq!(even_region(Rect::new(0, 0, 640, 480)), Some(Rect::new(0, 0, 640, 480)));
        assert_eq!(even_region(Rect::new(0, 0, 1, 480)), None);
        assert_eq!(even_region(Rect::new(0, 0, 3, 3)), Some(Rect::new(0, 0, 2, 2)));
    }

    #[test]
    fn monitors_are_chosen_by_id_else_primary_else_first() {
        let ms = vec![monitor("a", false), monitor("b", true)];
        assert_eq!(pick_monitor(&ms, Some("a")).unwrap().id, "a");
        assert_eq!(pick_monitor(&ms, None).unwrap().id, "b");
        assert!(pick_monitor(&ms, Some("zzz")).is_none());
        let no_primary = vec![monitor("x", false)];
        assert_eq!(pick_monitor(&no_primary, None).unwrap().id, "x");
        assert!(pick_monitor(&[], None).is_none());
    }

    #[test]
    fn audio_choices_map_to_sources() {
        assert_eq!(audio_selection(RecordAudio::None), AudioSelection::default());
        assert!(audio_selection(RecordAudio::Mic).microphone);
        assert!(!audio_selection(RecordAudio::Mic).system);
        assert!(audio_selection(RecordAudio::System).system);
        let both = audio_selection(RecordAudio::Both);
        assert!(both.system && both.microphone);
    }

    #[test]
    fn backends_map_to_capture_paths() {
        assert_eq!(source_kind(None), SourceKind::Auto);
        assert_eq!(source_kind(Some(BackendKind::X11)), SourceKind::X11);
        assert_eq!(source_kind(Some(BackendKind::Portal)), SourceKind::Portal);
        assert_eq!(source_kind(Some(BackendKind::Wayland)), SourceKind::Wayland);
        assert_eq!(source_kind(Some(BackendKind::Windows)), SourceKind::Windows);
    }

    #[test]
    fn the_plan_is_a_single_replaceable_slot() {
        let r = ServiceRecorder::new(RecorderOptions::default());
        assert_eq!(r.plan(), RecordingPlan::default());
        r.set_plan(RecordingPlan {
            target: RecordTarget::Rect { x: 1, y: 2, width: 3, height: 4 },
            audio: Some(RecordAudio::Mic),
        });
        assert_eq!(r.plan().audio, Some(RecordAudio::Mic));
    }

    #[test]
    fn explicit_rectangles_are_validated_before_anything_starts() {
        let r = ServiceRecorder::new(RecorderOptions::default());
        r.set_plan(RecordingPlan {
            target: RecordTarget::Rect { x: 0, y: 0, width: 1, height: 100 },
            audio: None,
        });
        let e = r
            .start(&RecordRequest {
                kind: ssx_core::workflow::RecordKind::Video,
                output_dir: std::env::temp_dir(),
                file_stem: "ssx-record-test-never-created".into(),
                include_cursor: false,
            })
            .map(|_| ())
            .unwrap_err();
        assert!(e.to_string().contains("too small"), "{e}");
        assert!(
            !std::env::temp_dir().join("ssx-record-test-never-created.mp4").exists(),
            "a failed start leaves nothing behind"
        );
    }
}
