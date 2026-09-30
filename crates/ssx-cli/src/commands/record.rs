//! `ssx record`: record the screen here, or control the recording in the running ssx app.
//!
//! * `ssx record [--gif] [--region | --rect X,Y,W,H | --monitor ID] [--seconds N]
//!   [--audio mic|system|both|none] [-o PATH]` records **in this process** until Ctrl-C or
//!   `--seconds`, then finalises the file and prints its path. The default target is the whole
//!   desktop; `--region` asks for a region on the selection overlay. Ctrl-C while the overlay is
//!   up cancels; Ctrl-C while recording *stops* (a second one discards).
//! * `ssx record start|toggle [same target flags]`, `stop`, `status` talk to the running app
//!   (so the tray shows the red icon, the workflow uploads and copies the link when the
//!   recording ends). Without a target flag `start`/`toggle` behave like the hotkey: the app's
//!   default (pick a region).

use ssx_core::ipc::{ErrorCode, RecordSpec, RecordTarget, RecordingStatus, Request, Response};

use crate::{
    app::App,
    cli::{RecordArgs, RecordCmd, RecordOpts},
    error::{CliError, CliResult},
    forward::{Daemon, error_from_response},
    output::{err_line, out_line},
};

/// The IPC target of the target flags (`None`: the app's default).
pub fn target_of(opts: &RecordOpts) -> Option<RecordTarget> {
    if opts.region {
        Some(RecordTarget::Interactive)
    } else if let Some(r) = opts.rect {
        Some(RecordTarget::Rect { x: r.x, y: r.y, width: r.width, height: r.height })
    } else {
        opts.monitor.as_ref().map(|id| RecordTarget::Monitor { id: Some(id.clone()) })
    }
}

/// The request body for `start` / `toggle`.
pub fn spec_of(opts: &RecordOpts) -> RecordSpec {
    RecordSpec {
        workflow: None,
        gif: opts.gif,
        target: target_of(opts),
        audio: opts.audio.map(crate::cli::AudioArg::record_audio),
        wait: false,
    }
}

/// The text of `record status`.
pub fn render_status(s: &RecordingStatus) -> String {
    if !s.active {
        return "not recording".to_owned();
    }
    let wf = s.workflow.as_deref().unwrap_or("?");
    if s.selecting {
        format!("choosing what to record ({wf})")
    } else {
        format!("recording {wf} for {} s", s.elapsed_ms / 1000)
    }
}

fn app_running() -> CliResult<Daemon> {
    Daemon::connect_always().ok_or_else(|| {
        CliError::new("the ssx background app is not running").hint(
            "start it with `ssx daemon start`; `ssx record` without a subcommand records here",
        )
    })
}

fn expect_recording(resp: Response) -> CliResult<RecordingStatus> {
    match resp {
        Response::Recording(s) => Ok(s),
        Response::Error { code, message } => Err(error_from_response(code, &message)),
        other => Err(CliError::new(format!("unexpected answer from the ssx app: {other:?}"))),
    }
}

fn status_of(daemon: &Daemon) -> CliResult<RecordingStatus> {
    expect_recording(daemon.call(Request::RecordingStatus).map_err(CliError::new)?)
}

fn via_app(cmd: &RecordCmd) -> CliResult<()> {
    let daemon = app_running()?;
    match cmd {
        RecordCmd::Start(o) => {
            let s = expect_recording(
                daemon.call(Request::StartRecording(spec_of(o))).map_err(CliError::new)?,
            )?;
            err_line(&format!("recording started ({})", render_status(&s)));
        }
        RecordCmd::Toggle(o) => {
            let was_active = status_of(&daemon)?.active;
            let s = expect_recording(
                daemon.call(Request::ToggleRecording(spec_of(o))).map_err(CliError::new)?,
            )?;
            if was_active {
                err_line("stopping the recording; the workflow carries on with the file");
            } else {
                err_line(&format!("recording started ({})", render_status(&s)));
            }
        }
        RecordCmd::Stop => match daemon.call(Request::StopRecording).map_err(CliError::new)? {
            Response::Ok => {
                err_line("stopping the recording; the workflow carries on with the file");
            }
            Response::Error { code: ErrorCode::NotRunning, .. } => {
                return Err(CliError::new("no recording is running"));
            }
            Response::Error { code, message } => return Err(error_from_response(code, &message)),
            other => return Err(CliError::new(format!("unexpected answer: {other:?}"))),
        },
        RecordCmd::Status { json } => {
            let s = status_of(&daemon)?;
            if *json {
                out_line(&serde_json::to_string_pretty(&s)?);
            } else {
                out_line(&render_status(&s));
            }
        }
    }
    Ok(())
}

/// Dispatches `ssx record ...`.
pub fn run(app: &App, args: RecordArgs) -> CliResult<()> {
    match &args.cmd {
        Some(cmd) => via_app(cmd),
        None => standalone::run(app, &args),
    }
}

#[cfg(feature = "record")]
mod standalone {
    use std::{
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use ssx_core::{
        ipc::RecordAudio,
        settings::Settings,
        workflow::{CancelToken, RecordKind, RecordRequest, Recorder},
    };
    use ssx_services::{
        OverlaySelector,
        record::{RecorderOptions, RecordingPlan, ServiceRecorder},
    };

    use super::{RecordArgs, target_of};
    use crate::{
        app::App,
        error::{CliError, CliResult},
        output::{err_line, out_line},
    };

    /// Where the recording goes and what it is.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Destination {
        /// Directory (created if missing).
        pub dir: PathBuf,
        /// File name without extension.
        pub stem: String,
        /// Video or GIF.
        pub kind: RecordKind,
        /// The final path the recorder will create.
        pub path: PathBuf,
    }

    /// Works out the destination from `-o`, `--gif` and the settings. Pure apart from the
    /// caller-supplied clock text and the existence check.
    pub fn destination(
        output: Option<&Path>,
        gif_flag: bool,
        settings: &Settings,
        now: &str,
    ) -> Result<Destination, String> {
        let ext = output
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let gif = gif_flag || ext.as_deref() == Some("gif");
        match ext.as_deref() {
            None | Some("mp4" | "gif") => {}
            Some(other) => {
                return Err(format!(
                    "cannot record to .{other} files; use .mp4 for video or .gif for an animation"
                ));
            }
        }
        if gif_flag && ext.as_deref() == Some("mp4") {
            return Err("--gif and an .mp4 file name contradict each other".to_owned());
        }
        let kind = if gif { RecordKind::Gif } else { RecordKind::Video };
        let (dir, stem) = if let Some(p) = output {
            let abs = std::path::absolute(p)
                .map_err(|e| format!("cannot resolve {}: {e}", p.display()))?;
            let stem = abs
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("{} has no file name", p.display()))?
                .to_owned();
            let dir = abs.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            (dir, stem)
        } else {
            let root = settings.general.resolve_save_dir();
            let dir = if settings.general.use_type_subfolders {
                root.join(&settings.general.subfolders.video)
            } else {
                root
            };
            (dir, format!("Recording_{now}"))
        };
        let ext = if gif { "gif" } else { "mp4" };
        let path = dir.join(format!("{stem}.{ext}"));
        Ok(Destination { dir, stem, kind, path })
    }

    pub fn run(app: &App, args: &RecordArgs) -> CliResult<()> {
        let settings = app.load_settings()?;
        let now = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let dest = destination(args.output.as_deref(), args.opts.gif, &settings, &now)
            .map_err(CliError::usage)?;
        if args.output.is_some() && dest.path.exists() {
            return Err(CliError::new(format!("{} already exists", dest.path.display()))
                .hint("recordings never overwrite files; remove it or choose another name"));
        }
        if let Some(s) = args.seconds
            && (!s.is_finite() || s <= 0.0)
        {
            return Err(CliError::usage("--seconds must be a positive number"));
        }
        std::fs::create_dir_all(&dest.dir)
            .map_err(|e| CliError::new(format!("cannot create {}: {e}", dest.dir.display())))?;

        let selector = OverlaySelector::discover().map(std::sync::Arc::new);
        if args.opts.region && selector.is_none() {
            return Err(CliError::new(
                "--region needs the ssx-overlay helper, which was not found",
            )
            .hint("put it next to `ssx` or on PATH, set SSX_OVERLAY, or use --rect X,Y,W,H"));
        }
        let recorder = ServiceRecorder::new(RecorderOptions {
            backend: app.backend()?,
            selector,
            ..RecorderOptions::default()
        });
        recorder.set_plan(RecordingPlan {
            target: target_of(&args.opts).unwrap_or(ssx_core::ipc::RecordTarget::Desktop),
            audio: Some(
                args.opts.audio.map_or(RecordAudio::None, crate::cli::AudioArg::record_audio),
            ),
        });
        // Before the recording starts (the overlay may be up) Ctrl-C cancels everything.
        recorder.set_cancel(app.cancel.clone());
        let session = recorder.start(&RecordRequest {
            kind: dest.kind,
            output_dir: dest.dir.clone(),
            file_stem: dest.stem.clone(),
            include_cursor: settings.capture.show_cursor,
        })?;

        // From here on the first Ctrl-C stops (keeps the file), the second discards.
        let stop = CancelToken::new();
        app.stop_on_first_interrupt(stop.clone());
        if !app.global.quiet {
            err_line(&match args.seconds {
                Some(s) => format!("recording for {s} s (Ctrl-C stops earlier)"),
                None => "recording: press Ctrl-C to stop".to_owned(),
            });
        }
        let limit = args.seconds.map(Duration::from_secs_f64);
        let started = Instant::now();
        loop {
            if app.cancel.is_cancelled() {
                session.abort();
                return Err(CliError::cancelled());
            }
            let left = limit.map_or(Duration::from_millis(250), |l| {
                l.saturating_sub(started.elapsed()).min(Duration::from_millis(250))
            });
            if limit.is_some_and(|l| started.elapsed() >= l) || stop.wait_timeout(left) {
                break;
            }
        }
        let video = session.stop()?;
        if args.json {
            out_line(&serde_json::to_string_pretty(&serde_json::json!({
                "path": video.path,
                "width": video.width,
                "height": video.height,
                "duration_ms": video.duration.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            }))?);
        } else {
            out_line(&video.path.display().to_string());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn s() -> Settings {
            Settings {
                general: ssx_core::settings::General {
                    save_dir: Some("/saves".into()),
                    ..ssx_core::settings::General::default()
                },
                ..Settings::default()
            }
        }

        #[test]
        fn the_default_destination_is_the_video_folder_with_a_timestamp() {
            let d = destination(None, false, &s(), "2026-01-02_03-04-05").unwrap();
            assert_eq!(d.dir, Path::new("/saves/Recordings"));
            assert_eq!(d.path, Path::new("/saves/Recordings/Recording_2026-01-02_03-04-05.mp4"));
            assert_eq!(d.kind, RecordKind::Video);
            let g = destination(None, true, &s(), "t").unwrap();
            assert_eq!(
                (g.kind, g.path.extension().unwrap()),
                (RecordKind::Gif, std::ffi::OsStr::new("gif"))
            );
        }

        #[test]
        fn subfolders_can_be_switched_off() {
            let mut st = s();
            st.general.use_type_subfolders = false;
            assert_eq!(destination(None, false, &st, "t").unwrap().dir, Path::new("/saves"));
        }

        #[test]
        fn an_output_path_decides_the_kind_and_must_be_mp4_or_gif() {
            let d = destination(Some(Path::new("/tmp/x/clip.gif")), false, &s(), "t").unwrap();
            assert_eq!(d.kind, RecordKind::Gif);
            assert_eq!((d.dir.as_path(), d.stem.as_str()), (Path::new("/tmp/x"), "clip"));
            let d = destination(Some(Path::new("/tmp/x/clip.MP4")), false, &s(), "t").unwrap();
            assert_eq!(d.kind, RecordKind::Video);
            assert_eq!(d.path, Path::new("/tmp/x/clip.mp4"));
            let d = destination(Some(Path::new("/tmp/x/noext")), false, &s(), "t").unwrap();
            assert_eq!(d.path, Path::new("/tmp/x/noext.mp4"));
            assert!(
                destination(Some(Path::new("/tmp/a.webm")), false, &s(), "t")
                    .unwrap_err()
                    .contains(".webm")
            );
            assert!(
                destination(Some(Path::new("/tmp/a.mp4")), true, &s(), "t")
                    .unwrap_err()
                    .contains("contradict")
            );
        }

        #[test]
        fn relative_outputs_become_absolute() {
            let d = destination(Some(Path::new("clip.mp4")), false, &s(), "t").unwrap();
            assert!(d.dir.is_absolute());
        }
    }
}

#[cfg(not(feature = "record"))]
mod standalone {
    use super::RecordArgs;
    use crate::{
        app::App,
        error::{CliError, CliResult},
    };

    pub fn run(_: &App, _: &RecordArgs) -> CliResult<()> {
        Err(CliError::new("this build of ssx cannot record the screen")
            .hint("build with the `record` feature, or use `ssx record start` with the ssx app"))
    }
}

#[cfg(test)]
mod tests {
    use ssx_core::ipc::RecordAudio;
    use ssx_types::Rect;

    use super::*;
    use crate::cli::AudioArg;

    #[test]
    fn target_flags_map_to_ipc_targets() {
        assert_eq!(target_of(&RecordOpts::default()), None);
        let region = RecordOpts { region: true, ..RecordOpts::default() };
        assert_eq!(target_of(&region), Some(RecordTarget::Interactive));
        let rect = RecordOpts { rect: Some(Rect::new(-5, 10, 640, 480)), ..RecordOpts::default() };
        assert_eq!(
            target_of(&rect),
            Some(RecordTarget::Rect { x: -5, y: 10, width: 640, height: 480 })
        );
        let mon = RecordOpts { monitor: Some("DP-1".into()), ..RecordOpts::default() };
        assert_eq!(target_of(&mon), Some(RecordTarget::Monitor { id: Some("DP-1".into()) }));
    }

    #[test]
    fn the_request_body_carries_gif_and_audio() {
        let o = RecordOpts { gif: true, audio: Some(AudioArg::Both), ..RecordOpts::default() };
        let spec = spec_of(&o);
        assert!(spec.gif && !spec.wait && spec.workflow.is_none());
        assert_eq!(spec.audio, Some(RecordAudio::Both));
        assert_eq!(spec_of(&RecordOpts::default()), RecordSpec::default());
    }

    #[test]
    fn status_text() {
        assert_eq!(render_status(&RecordingStatus::default()), "not recording");
        let s = RecordingStatus {
            active: true,
            selecting: false,
            workflow: Some("record-screen".into()),
            elapsed_ms: 65_900,
            run_id: Some(3),
        };
        assert_eq!(render_status(&s), "recording record-screen for 65 s");
        let sel = RecordingStatus { selecting: true, ..s };
        assert!(render_status(&sel).starts_with("choosing what to record"));
    }
}
