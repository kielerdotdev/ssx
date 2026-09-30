//! `ssx post-file`, `post-video`, `upload` and `edit`: everything that starts from files.

use std::path::{Path, PathBuf};

use ssx_core::{
    ipc::PostAction,
    settings::{AfterCapture, AfterUpload, InputKind, Settings, Workflow},
    workflow::Captured,
};
use ssx_types::Frame;

use crate::{
    app::{App, Session},
    cli::{EditArgs, KindArg, PostFileArgs, PostVideoArgs, UploadArgs},
    commands::capture::{FinishOpts, finish_image},
    error::{CliError, CliResult},
    flows::{Wanted, adhoc_workflow, apply_destination_flags},
    forward::{Forward, forward_post_files},
    progress::{ProgressLines, Verbosity},
    report::{RunResult, print_result},
};

/// The workflow `post-file` runs: the named one, else the configured "upload-files", else a
/// built-in equivalent (upload, copy the URL, notify) so a trimmed-down settings file still
/// works.
pub fn file_workflow(settings: &Settings, name: Option<&str>) -> CliResult<Workflow> {
    if let Some(name) = name {
        return settings.find_workflow(name).cloned().ok_or_else(|| {
            let known: Vec<&str> = settings
                .workflows
                .iter()
                .map(|w| w.trigger.cli_name.as_deref().unwrap_or(&w.id))
                .collect();
            CliError::new(format!("there is no workflow called {name:?}"))
                .hint(format!("available workflows: {}", known.join(", ")))
        });
    }
    if let Some(w) = settings.workflow_by_id("upload-files") {
        return Ok(w.clone());
    }
    let mut wf = adhoc_workflow(
        "upload-files",
        "Upload files",
        InputKind::Files,
        Wanted { upload: true, copy_url: true, ..Wanted::default() },
        None,
    );
    wf.after_upload.push(AfterUpload::ShowNotification);
    Ok(wf)
}

/// Runs `wf` over `paths` and prints the result.
fn run_files(
    app: &App,
    session: &Session,
    wf: &Workflow,
    paths: Vec<PathBuf>,
    explicit: &[ssx_core::workflow::StepKind],
    json: bool,
) -> CliResult<()> {
    let sink = ProgressLines::new(
        Verbosity::from_flags(app.global.quiet, app.global.verbose),
        app.err,
        paths.len(),
    );
    let report = session.engine.post_file(wf, paths, &session.bundle(), &sink, &app.cancel);
    print_result(&RunResult::from_report(&report, explicit), json, app.global.quiet, app.err)
}

/// `ssx post-file`.
pub fn post_file(app: &App, args: PostFileArgs) -> CliResult<()> {
    if args.coalesce && args.kind.is_none() && args.to.is_none() {
        let action = match &args.workflow {
            Some(w) => PostAction::Workflow { workflow: w.clone() },
            None => PostAction::Upload,
        };
        match forward_post_files(&args.paths, action) {
            Forward::Sent => {
                tracing::info!("handed {} file(s) to the running ssx instance", args.paths.len());
                return Ok(());
            }
            Forward::NotRunning => {
                tracing::debug!("no running ssx instance, uploading from this process");
            }
            Forward::Failed(why) => tracing::warn!("{why}; uploading from this process instead"),
        }
    }
    let settings = app.load_settings()?;
    let mut wf = file_workflow(&settings, args.workflow.as_deref())?;
    apply_destination_flags(&mut wf, &settings, args.to.as_deref(), args.kind)?;
    let session = Session::new(app, settings)?;
    run_files(app, &session, &wf, args.paths, &[], args.json)
}

/// `ssx post-video`.
pub fn post_video(app: &App, args: PostVideoArgs) -> CliResult<()> {
    let settings = app.load_settings()?;
    let mut wf = file_workflow(&settings, args.workflow.as_deref())?;
    // Uses `post_file` rather than the engine's recording input: that input treats the file
    // as one ssx just recorded, and such files are eligible for `delete_local_file`. A video
    // the user picked must never be deleted.
    if let Err(e) =
        apply_destination_flags(&mut wf, &settings, args.to.as_deref(), Some(KindArg::Video))
    {
        // No video and no file destination configured: the engine explains the same thing per
        // file, with better context, so this is not worth failing early for.
        tracing::debug!("{e}");
    }
    let session = Session::new(app, settings)?;
    run_files(app, &session, &wf, args.paths, &[], args.json)
}

/// `ssx upload`: quiet and script-friendly (URLs on stdout, nothing else unless asked).
pub fn upload(app: &App, args: UploadArgs) -> CliResult<()> {
    let settings = app.load_settings()?;
    let wanted = Wanted { upload: true, copy_url: args.copy, ..Wanted::default() };
    let mut wf =
        adhoc_workflow("cli-upload", "ssx upload", InputKind::Files, wanted, args.to.as_deref());
    wf.after_capture = vec![AfterCapture::Upload];
    let session = Session::new(app, settings)?;
    run_files(app, &session, &wf, args.paths, &wanted.explicit_steps(), args.json)
}

/// The default output of `ssx edit`: `<stem>-edited.<ext>` next to the original.
pub fn edited_path(original: &Path) -> PathBuf {
    let stem = original
        .file_stem()
        .map_or_else(|| "image".to_owned(), |s| s.to_string_lossy().into_owned());
    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp"))
        .unwrap_or_else(|| "png".to_owned());
    original.with_file_name(format!("{stem}-edited.{ext}"))
}

/// `ssx edit`.
pub fn edit(app: &App, args: EditArgs) -> CliResult<()> {
    let bytes = std::fs::read(&args.path)
        .map_err(|e| CliError::new(format!("cannot read {}: {e}", args.path.display())))?;
    let frame = Frame::decode(&bytes).map_err(|e| {
        CliError::new(format!("{} is not an image ssx can open: {e}", args.path.display()))
            .hint("supported formats: PNG, JPEG, WebP, BMP, GIF")
    })?;
    let settings = app.load_settings()?;
    let session = Session::new(app, settings)?;
    let output = if args.in_place {
        args.path.clone()
    } else {
        args.output.clone().unwrap_or_else(|| edited_path(&args.path))
    };
    let upload = args.upload || args.to.is_some();
    finish_image(
        app,
        &session,
        Captured::new(frame),
        true,
        &FinishOpts {
            output: Some(output),
            format: None,
            copy: args.copy,
            upload,
            to: args.to,
            copy_url: false,
            json: args.json,
            workflow_id: "cli-edit",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edited_names_sit_next_to_the_original() {
        assert_eq!(edited_path(Path::new("/a/b/shot.png")), PathBuf::from("/a/b/shot-edited.png"));
        assert_eq!(edited_path(Path::new("x.JPG")), PathBuf::from("x-edited.jpg"));
        assert_eq!(
            edited_path(Path::new("x.bmp")),
            PathBuf::from("x-edited.png"),
            "unknown formats become PNG"
        );
        assert_eq!(edited_path(Path::new("noext")), PathBuf::from("noext-edited.png"));
    }

    #[test]
    fn the_file_workflow_falls_back_when_the_configured_one_is_gone() {
        let mut s = Settings::default();
        let w = file_workflow(&s, None).unwrap();
        assert_eq!(w.id, "upload-files");
        assert!(w.after_capture.contains(&AfterCapture::Upload));

        s.workflows.retain(|w| w.id != "upload-files");
        let fallback = file_workflow(&s, None).unwrap();
        assert!(fallback.after_capture == [AfterCapture::Upload]);
        assert!(fallback.after_upload.contains(&AfterUpload::CopyUrl));
        assert!(fallback.after_upload.contains(&AfterUpload::ShowNotification));
    }

    #[test]
    fn an_unknown_workflow_lists_the_known_ones() {
        let e = file_workflow(&Settings::default(), Some("nope")).unwrap_err();
        let hint = e.hint.unwrap();
        assert!(hint.contains("region") && hint.contains("upload"), "{hint}");
        assert_eq!(file_workflow(&Settings::default(), Some("upload")).unwrap().id, "upload-files");
    }
}
