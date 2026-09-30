//! `ssx capture`, and the shared "finish an image" step also used by `ssx edit`.
//!
//! Flow: wait for `--delay` (cancellably) → capture → optional editor → then either
//!
//! * `--output PATH`: this command writes the file itself (an explicit path means overwrite,
//!   which the engine's collision-avoiding `SaveToFile` step deliberately never does), then
//!   the *engine* uploads that file (`post_file`), so history, retries and URL handling are
//!   the engine's; or
//! * no `--output`: the engine saves into the configured folder using the filename pattern,
//!   copies and uploads, all in one `post_image` run.

use std::path::{Path, PathBuf};

use ssx_core::{
    ipc::{CaptureKind, Request},
    settings::{AfterCapture, InputKind, Settings},
    workflow::{
        CaptureRequest, CaptureTarget as CoreTarget, Captured, Capturer as _, Clipboard as _,
        EditResult, Editor as _, Input,
    },
};
use ssx_services::ExplicitTarget;
use ssx_types::{EncodeOptions, Frame};

use crate::{
    app::{App, Session},
    cli::{CaptureArgs, CaptureOpts, CaptureTarget},
    error::{CliError, CliResult},
    flows::{
        Wanted, adhoc_workflow, apply_destination_flags, choose_format, with_default_extension,
    },
    forward::{Daemon, run_remote},
    progress::{ProgressLines, Verbosity},
    report::{RunResult, print_result},
};

/// What to do with an image once it exists.
#[derive(Debug, Clone, Default)]
pub struct FinishOpts {
    /// Write here (overwriting) instead of the save folder.
    pub output: Option<PathBuf>,
    /// Encoding format flag.
    pub format: Option<crate::cli::FormatArg>,
    /// Copy the image to the clipboard.
    pub copy: bool,
    /// Upload.
    pub upload: bool,
    /// Upload destination.
    pub to: Option<String>,
    /// Copy the URL afterwards.
    pub copy_url: bool,
    /// JSON output.
    pub json: bool,
    /// Workflow id recorded in the history.
    pub workflow_id: &'static str,
}

/// `true` when no option changes what happens to the image, so the running ssx app can do the
/// whole job with the workflow the user configured for region captures.
fn hands_over_to_the_app(opts: &CaptureOpts) -> bool {
    opts.output.is_none()
        && opts.format.is_none()
        && !opts.cursor
        && !opts.copy
        && !opts.upload
        && opts.to.is_none()
        && !opts.copy_url
        && !opts.edit
}

/// An interactive region capture done by the running ssx app: one overlay at a time, its
/// notifications, its tray state. Ctrl-C here cancels the overlay there.
fn capture_via_app(
    app: &App,
    daemon: &Daemon,
    mode: Option<crate::cli::ModeArg>,
    opts: &CaptureOpts,
) -> CliResult<()> {
    let request = Request::Capture {
        target: CaptureKind::Region,
        workflow: None,
        delay_ms: opts.delay,
        wait: false,
        mode: mode.map(crate::cli::ModeArg::region_mode),
    };
    let summary = run_remote(daemon, request, &app.cancel)?;
    let result = RunResult::from_summary("capture-region", &summary);
    print_result(&result, opts.json, app.global.quiet, app.err)
}

/// Runs `ssx capture`.
pub fn run(app: &App, args: CaptureArgs) -> CliResult<()> {
    let CaptureArgs { target, opts } = args;
    if let CaptureTarget::Region { rect: None, mode } = &target
        && hands_over_to_the_app(&opts)
        && let Some(daemon) = Daemon::connect()
    {
        tracing::info!("handing the region capture to the running ssx app");
        return capture_via_app(app, &daemon, *mode, &opts);
    }
    let settings = app.load_settings()?;
    let upload = opts.upload || opts.to.is_some();
    if opts.copy_url && !upload {
        return Err(CliError::usage("--copy-url needs --upload (or --to)"));
    }
    let mode = match &target {
        CaptureTarget::Region { mode: Some(m), .. } => m.pick_mode(),
        _ => ssx_services::PickMode::Rect,
    };
    let session = Session::with_mode(app, settings.clone(), mode)?;
    let captured = take_screenshot(app, &session, &target, &opts, &settings)?;
    finish_image(
        app,
        &session,
        captured,
        opts.edit,
        &FinishOpts {
            output: opts.output,
            format: opts.format,
            copy: opts.copy,
            upload,
            to: opts.to,
            copy_url: opts.copy_url,
            json: opts.json,
            workflow_id: "cli-capture",
        },
    )
}

/// Waits for the delay, then captures `target`.
fn take_screenshot(
    app: &App,
    session: &Session,
    target: &CaptureTarget,
    opts: &CaptureOpts,
    settings: &Settings,
) -> CliResult<Captured> {
    let delay = opts.delay.unwrap_or(settings.capture.delay_ms);
    if delay > 0 && !app.cancel.sleep(std::time::Duration::from_millis(u64::from(delay))) {
        return Err(CliError::cancelled());
    }
    let cursor = opts.cursor || settings.capture.show_cursor;
    let hdr = settings.capture.hdr;
    let capturer = &session.services.capturer;
    let by_trait = |t: CoreTarget| {
        capturer.capture(&CaptureRequest { target: t, include_cursor: cursor, hdr }, &app.cancel)
    };
    let explicit = |t: ExplicitTarget| capturer.capture_explicit(&t, cursor, &hdr);
    let captured = match target {
        CaptureTarget::Fullscreen => by_trait(CoreTarget::Fullscreen),
        CaptureTarget::Monitor { id: None } => by_trait(CoreTarget::Monitor),
        CaptureTarget::Monitor { id: Some(id) } => explicit(ExplicitTarget::Monitor(id.clone())),
        CaptureTarget::Window { id: None, .. } => by_trait(CoreTarget::Window),
        CaptureTarget::Window { id: Some(id), .. } => explicit(ExplicitTarget::Window(id.clone())),
        CaptureTarget::Region { rect: Some(r), .. } => explicit(ExplicitTarget::Rect(*r)),
        CaptureTarget::Region { rect: None, .. } => by_trait(CoreTarget::Region),
        CaptureTarget::LastRegion => by_trait(CoreTarget::LastRegion),
    };
    Ok(captured?)
}

/// Edits (optionally), saves, copies and uploads `captured`, prints the result and turns the
/// outcome into the process result.
pub fn finish_image(
    app: &App,
    session: &Session,
    mut captured: Captured,
    edit: bool,
    opts: &FinishOpts,
) -> CliResult<()> {
    let settings = &session.settings;
    if edit {
        match session.services.editor.edit(&captured.frame, &app.cancel)? {
            EditResult::Edited(frame) => captured.frame = frame,
            EditResult::Cancelled => return Err(CliError::cancelled()),
        }
    }
    let format = choose_format(
        opts.format,
        opts.output.as_deref().and_then(Path::extension).and_then(|e| e.to_str()),
        settings.general.image_format,
    )?;
    let sink =
        ProgressLines::new(Verbosity::from_flags(app.global.quiet, app.global.verbose), app.err, 1);
    let wanted = Wanted {
        save: opts.output.is_none(),
        copy_image: opts.copy,
        upload: opts.upload,
        copy_url: opts.copy_url,
    };
    let mut wf = adhoc_workflow(
        opts.workflow_id,
        "ssx capture",
        InputKind::CaptureFullscreen,
        wanted,
        opts.to.as_deref(),
    );
    apply_destination_flags(&mut wf, settings, opts.to.as_deref(), None)?;

    let result = if let Some(out) = &opts.output {
        let path = with_default_extension(out, format);
        write_image(&captured.frame, &path, format, settings)?;
        // The engine has nothing to copy for a file we wrote ourselves, so copy here.
        let copy_error = if opts.copy {
            session
                .services
                .clipboard
                .set_image(&captured.frame)
                .err()
                .map(|e| format!("copy_image_to_clipboard: {e}"))
        } else {
            None
        };
        let mut result = if opts.upload {
            wf.after_capture.retain(|s| *s == AfterCapture::Upload);
            let report =
                session.engine.post_file(&wf, vec![path], &session.bundle(), &sink, &app.cancel);
            RunResult::from_report(
                &report,
                &Wanted { copy_image: false, ..wanted }.explicit_steps(),
            )
        } else {
            RunResult::saved(opts.workflow_id, path)
        };
        if let Some(e) = copy_error {
            result.add_error(e);
        }
        result
    } else {
        // The engine encodes with the configured options: honour --format with a per-run
        // engine that has the same naming sources.
        let mut settings = settings.clone();
        settings.general.image_format = format;
        let engine = session.engine_for(settings);
        let report = engine.run(&wf, Input::Image(captured), &session.bundle(), &sink, &app.cancel);
        RunResult::from_report(&report, &wanted.explicit_steps())
    };
    print_result(&result, opts.json, app.global.quiet, app.err)
}

/// Encodes `frame` and writes it to `path` (creating folders, replacing an existing file).
pub fn write_image(
    frame: &Frame,
    path: &Path,
    format: ssx_core::settings::ImageFormatKind,
    settings: &Settings,
) -> CliResult<()> {
    let bytes = frame
        .encode(EncodeOptions {
            format: format.to_types(),
            jpeg_quality: settings.general.image_quality.clamp(1, 100),
            png_fast: false,
        })
        .map_err(|e| CliError::new(format!("cannot encode the image: {e}")))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::new(format!("cannot create {}: {e}", dir.display())))?;
    }
    ssx_core::settings::atomic_write(path, &bytes)
        .map_err(|e| CliError::new(format!("cannot write {}: {e}", path.display())))
}
