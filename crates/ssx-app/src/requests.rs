//! Turning IPC requests and menu actions into [`JobRequest`]s: which workflow, with what
//! parameters. Pure functions over [`Settings`], unit-tested.
//!
//! Rules worth knowing:
//!
//! * A reference resolves as the CLI does: id, then CLI name, then display name (any case).
//! * `Capture { target }` without a workflow uses the built-in workflow for that target
//!   (`capture-region`, `capture-fullscreen`, ...) *as the user configured it*; if the user
//!   deleted it, the first workflow with the same input kind, and failing that a plain
//!   "save and copy" one, so `ssx capture fullscreen` keeps working with a trimmed-down settings file.
//! * Recording workflows (`RunWorkflow` on one, `StartRecording`, `ToggleRecording`) become
//!   [`JobSpec::Record`]; `RunWorkflow` and the hotkey *toggle*, `StartRecording` does not.
//! * Files workflows cannot be started without files: `RunWorkflow` on one is an error that
//!   says which command takes files.

use std::path::PathBuf;

use ssx_core::{
    ipc::{CaptureKind, PostAction, RecordSpec, RecordTarget, RegionMode, Request, WorkflowInfo},
    settings::{AfterCapture, AfterUpload, InputKind, Settings, Workflow},
};
use ssx_services::PickMode;

use crate::daemon::{JobRequest, JobSpec, Origin, RecordPlanSpec, Rejection};

/// The overlay mode of an IPC region mode.
pub fn pick_mode(m: RegionMode) -> PickMode {
    match m {
        RegionMode::Rect => PickMode::Rect,
        RegionMode::Ellipse => PickMode::Ellipse,
        RegionMode::Freeform => PickMode::Freeform,
        RegionMode::Window => PickMode::Window,
        RegionMode::Monitor => PickMode::Monitor,
    }
}

/// The default workflow ids per capture kind and their input kinds.
fn capture_default(kind: CaptureKind) -> (&'static str, InputKind, &'static str) {
    match kind {
        CaptureKind::Region => ("capture-region", InputKind::CaptureRegion, "Capture region"),
        CaptureKind::Fullscreen => {
            ("capture-fullscreen", InputKind::CaptureFullscreen, "Capture screen")
        }
        CaptureKind::Monitor => ("capture-monitor", InputKind::CaptureMonitor, "Capture monitor"),
        CaptureKind::Window => ("capture-window", InputKind::CaptureWindow, "Capture window"),
        CaptureKind::LastRegion => {
            ("capture-last-region", InputKind::CaptureLastRegion, "Repeat last region")
        }
    }
}

fn adhoc(
    id: &str,
    name: &str,
    input: InputKind,
    after_capture: Vec<AfterCapture>,
    after_upload: Vec<AfterUpload>,
) -> Workflow {
    Workflow {
        id: id.to_owned(),
        name: name.to_owned(),
        input,
        after_capture,
        after_upload,
        ..Workflow::default()
    }
}

fn find(settings: &Settings, reference: &str) -> Result<Workflow, Rejection> {
    settings.find_workflow(reference).cloned().ok_or_else(|| {
        let known: Vec<&str> = settings
            .workflows
            .iter()
            .map(|w| w.trigger.cli_name.as_deref().unwrap_or(&w.id))
            .collect();
        Rejection::UnknownWorkflow(format!(
            "there is no workflow called {reference:?}; available: {}",
            known.join(", ")
        ))
    })
}

/// The workflow for `Capture { target, workflow }`.
pub fn workflow_for_capture(
    settings: &Settings,
    kind: CaptureKind,
    name: Option<&str>,
) -> Result<Workflow, Rejection> {
    if let Some(name) = name {
        let wf = find(settings, name)?;
        return if wf.input.is_still_capture() {
            Ok(wf)
        } else {
            Err(Rejection::Invalid(format!(
                "workflow {:?} does not take a screenshot; use `ssx run` for it",
                wf.name
            )))
        };
    }
    let (id, input, label) = capture_default(kind);
    Ok(settings
        .workflow_by_id(id)
        .or_else(|| settings.workflows.iter().find(|w| w.input == input))
        .cloned()
        .unwrap_or_else(|| {
            adhoc(
                &format!("ipc-{id}"),
                label,
                input,
                vec![AfterCapture::SaveToFile, AfterCapture::CopyImageToClipboard],
                vec![AfterUpload::ShowNotification],
            )
        }))
}

/// The workflow and the "edit first" flag for `PostFiles { action }`.
pub fn workflow_for_files(
    settings: &Settings,
    action: &PostAction,
) -> Result<(Workflow, bool), Rejection> {
    let default = || {
        settings
            .workflow_by_id("upload-files")
            .or_else(|| settings.workflows.iter().find(|w| w.input == InputKind::Files))
            .cloned()
            .unwrap_or_else(|| {
                adhoc(
                    "upload-files",
                    "Upload files",
                    InputKind::Files,
                    vec![AfterCapture::Upload],
                    vec![AfterUpload::CopyUrl, AfterUpload::ShowNotification],
                )
            })
    };
    match action {
        PostAction::Upload => Ok((default(), false)),
        PostAction::Edit => Ok((default(), true)),
        PostAction::Workflow { workflow } => {
            let wf = find(settings, workflow)?;
            if wf.input == InputKind::Files {
                Ok((wf, false))
            } else {
                Err(Rejection::Invalid(format!(
                    "workflow {:?} does not take files (its input is {:?})",
                    wf.name, wf.input
                )))
            }
        }
    }
}

/// The workflow for a recording request.
pub fn workflow_for_recording(
    settings: &Settings,
    spec: &RecordSpec,
) -> Result<Workflow, Rejection> {
    if let Some(name) = spec.workflow.as_deref() {
        let wf = find(settings, name)?;
        return if wf.input.is_recording() {
            Ok(wf)
        } else {
            Err(Rejection::Invalid(format!("workflow {:?} does not record", wf.name)))
        };
    }
    let (id, input, label) = if spec.gif {
        ("record-gif", InputKind::RecordGif, "Record GIF")
    } else {
        ("record-screen", InputKind::RecordScreen, "Record screen")
    };
    Ok(settings
        .workflow_by_id(id)
        .or_else(|| settings.workflows.iter().find(|w| w.input == input))
        .cloned()
        .unwrap_or_else(|| {
            adhoc(
                &format!("ipc-{id}"),
                label,
                input,
                vec![AfterCapture::Upload],
                vec![AfterUpload::CopyUrl, AfterUpload::ShowNotification],
            )
        }))
}

/// The recording plan of a request, validated.
pub fn plan_of(spec: &RecordSpec) -> Result<RecordPlanSpec, Rejection> {
    let target = spec.target.clone().unwrap_or(RecordTarget::Interactive);
    if let RecordTarget::Rect { width, height, .. } = &target
        && (*width == 0 || *height == 0)
    {
        return Err(Rejection::Invalid("the recording region needs a width and a height".to_owned()));
    }
    Ok(RecordPlanSpec { target, audio: spec.audio })
}

/// A job for a workflow started by reference (`RunWorkflow`, the tray, a hotkey).
pub fn job_for_workflow(wf: Workflow, origin: Origin) -> Result<JobRequest, Rejection> {
    if wf.input == InputKind::Files {
        return Err(Rejection::Invalid(format!(
            "workflow {:?} takes files: use `ssx post-file --workflow {} PATH...` or the tray's \"Upload files...\"",
            wf.name, wf.id
        )));
    }
    let spec = if wf.input.is_recording() {
        JobSpec::Record { workflow: wf, plan: RecordPlanSpec::default(), toggle: true }
    } else {
        JobSpec::Workflow { workflow: wf, delay_ms: None, mode: None }
    };
    Ok(JobRequest { run_id: None, origin, spec })
}

/// Maps the run-starting requests. Returns the job and whether the caller asked to wait.
/// `Err(None)` means "this is not a run-starting request" (the caller handles it elsewhere).
pub fn job_for_request(
    settings: &Settings,
    req: &Request,
) -> Result<Option<(JobRequest, bool)>, Rejection> {
    Ok(Some(match req {
        Request::RunWorkflow { wait, .. } => {
            let wf = match req.workflow_ref().map_err(Rejection::Invalid)? {
                ssx_core::ipc::WorkflowRef::Id(id) => settings
                    .workflow_by_id(id)
                    .cloned()
                    .map_or_else(|| find(settings, id), Ok)?,
                ssx_core::ipc::WorkflowRef::Name(n) => find(settings, n)?,
            };
            (job_for_workflow(wf, Origin::Ipc)?, *wait)
        }
        Request::Capture { target, workflow, delay_ms, wait, mode } => {
            let wf = workflow_for_capture(settings, *target, workflow.as_deref())?;
            if mode.is_some() && wf.input != InputKind::CaptureRegion {
                return Err(Rejection::Invalid(format!(
                    "a selection mode only applies to region capture, not to workflow {:?}",
                    wf.name
                )));
            }
            let job = JobRequest {
                run_id: None,
                origin: Origin::Ipc,
                spec: JobSpec::Workflow { workflow: wf, delay_ms: *delay_ms, mode: *mode },
            };
            (job, *wait)
        }
        Request::StartRecording(spec) | Request::ToggleRecording(spec) => {
            let toggle = matches!(req, Request::ToggleRecording(_));
            let workflow = workflow_for_recording(settings, spec)?;
            let plan = plan_of(spec)?;
            let job = JobRequest {
                run_id: None,
                origin: Origin::Ipc,
                spec: JobSpec::Record { workflow, plan, toggle },
            };
            (job, spec.wait)
        }
        _ => return Ok(None),
    }))
}

/// The job for a `PostFiles` batch (run id from the coalescer).
pub fn job_for_files(
    settings: &Settings,
    action: &PostAction,
    paths: Vec<PathBuf>,
    run_id: u64,
) -> Result<JobRequest, Rejection> {
    let (workflow, edit_first) = workflow_for_files(settings, action)?;
    Ok(JobRequest {
        run_id: Some(run_id),
        origin: Origin::Ipc,
        spec: JobSpec::Files { workflow, paths, edit_first },
    })
}

/// `ListWorkflows`.
pub fn list_workflows(settings: &Settings) -> Vec<WorkflowInfo> {
    settings
        .workflows
        .iter()
        .map(|w| WorkflowInfo {
            id: w.id.clone(),
            name: w.name.clone(),
            cli_name: w.trigger.cli_name.clone(),
            hotkey: w.trigger.hotkey.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use ssx_core::ipc::{RecordAudio, ShowTarget};

    use super::*;

    fn s() -> Settings {
        Settings::default()
    }

    fn run(id: Option<&str>, name: Option<&str>) -> Request {
        Request::RunWorkflow { id: id.map(Into::into), name: name.map(Into::into), wait: true }
    }

    fn job(req: &Request) -> (JobRequest, bool) {
        job_for_request(&s(), req).unwrap().unwrap()
    }

    #[test]
    fn run_workflow_resolves_ids_cli_names_and_display_names() {
        for r in [
            run(Some("capture-fullscreen"), None),
            run(None, Some("screen")),
            run(None, Some("CAPTURE SCREEN, SAVE AND COPY")),
            run(Some("screen"), None), // an id slot holding a cli name still resolves
        ] {
            let (j, wait) = job(&r);
            assert!(wait);
            assert_eq!(j.spec.workflow().id, "capture-fullscreen", "{r:?}");
            assert_eq!(j.origin, Origin::Ipc);
        }
    }

    #[test]
    fn unknown_workflows_list_what_exists() {
        let e = job_for_request(&s(), &run(None, Some("nope"))).unwrap_err();
        let Rejection::UnknownWorkflow(m) = e else { panic!("{e:?}") };
        assert!(m.contains("region") && m.contains("screen"), "{m}");
        let e = job_for_request(&s(), &run(None, None)).unwrap_err();
        assert!(matches!(e, Rejection::Invalid(_)));
        let e = job_for_request(&s(), &run(Some("a"), Some("b"))).unwrap_err();
        assert!(matches!(e, Rejection::Invalid(m) if m.contains("not both")));
    }

    #[test]
    fn files_workflows_cannot_be_run_without_files_and_the_error_says_how() {
        let e = job_for_request(&s(), &run(Some("upload-files"), None)).unwrap_err();
        let Rejection::Invalid(m) = e else { panic!() };
        assert!(m.contains("ssx post-file --workflow upload-files"), "{m}");
    }

    #[test]
    fn recording_workflows_toggle_when_run_and_start_when_asked_to() {
        let (j, _) = job(&run(Some("record-screen"), None));
        assert!(matches!(j.spec, JobSpec::Record { toggle: true, .. }));
        let (j, wait) = job(&Request::StartRecording(RecordSpec { wait: true, ..RecordSpec::default() }));
        assert!(wait);
        let JobSpec::Record { workflow, plan, toggle } = j.spec else { panic!() };
        assert_eq!(workflow.id, "record-screen");
        assert!(!toggle);
        assert_eq!(plan, RecordPlanSpec::default());
        let (j, _) = job(&Request::ToggleRecording(RecordSpec {
            gif: true,
            target: Some(RecordTarget::Monitor { id: Some("DP-1".into()) }),
            audio: Some(RecordAudio::Mic),
            ..RecordSpec::default()
        }));
        let JobSpec::Record { workflow, plan, toggle } = j.spec else { panic!() };
        assert_eq!(workflow.id, "record-gif");
        assert!(toggle);
        assert_eq!(plan.audio, Some(RecordAudio::Mic));
        assert_eq!(plan.target, RecordTarget::Monitor { id: Some("DP-1".into()) });
    }

    #[test]
    fn recording_requests_are_validated() {
        let bad_rect = RecordSpec {
            target: Some(RecordTarget::Rect { x: 0, y: 0, width: 0, height: 10 }),
            ..RecordSpec::default()
        };
        assert!(matches!(
            job_for_request(&s(), &Request::StartRecording(bad_rect)).unwrap_err(),
            Rejection::Invalid(_)
        ));
        let not_recording = RecordSpec { workflow: Some("region".into()), ..RecordSpec::default() };
        let e = job_for_request(&s(), &Request::StartRecording(not_recording)).unwrap_err();
        assert!(matches!(e, Rejection::Invalid(m) if m.contains("does not record")));
        let unknown = RecordSpec { workflow: Some("zzz".into()), ..RecordSpec::default() };
        assert!(matches!(
            job_for_request(&s(), &Request::ToggleRecording(unknown)).unwrap_err(),
            Rejection::UnknownWorkflow(_)
        ));
    }

    #[test]
    fn capture_uses_the_configured_builtin_then_falls_back() {
        for (kind, id) in [
            (CaptureKind::Region, "capture-region"),
            (CaptureKind::Fullscreen, "capture-fullscreen"),
            (CaptureKind::Monitor, "capture-monitor"),
            (CaptureKind::Window, "capture-window"),
            (CaptureKind::LastRegion, "capture-last-region"),
        ] {
            assert_eq!(workflow_for_capture(&s(), kind, None).unwrap().id, id);
        }
        // The user deleted the built-in: first workflow with the same input, else a plain one.
        let mut trimmed = s();
        trimmed.workflows.retain(|w| w.id != "capture-fullscreen");
        let plain = workflow_for_capture(&trimmed, CaptureKind::Fullscreen, None).unwrap();
        assert_eq!(plain.input, InputKind::CaptureFullscreen);
        assert!(plain.after_capture.contains(&AfterCapture::SaveToFile));
        assert!(plain.id.starts_with("ipc-"));
        let mut other = trimmed.clone();
        other.workflows.push(Workflow {
            id: "mine".into(),
            name: "Mine".into(),
            input: InputKind::CaptureFullscreen,
            ..Workflow::default()
        });
        assert_eq!(workflow_for_capture(&other, CaptureKind::Fullscreen, None).unwrap().id, "mine");
    }

    #[test]
    fn capture_with_a_named_workflow_and_options() {
        let req = Request::Capture {
            target: CaptureKind::Region,
            workflow: Some("region-edit".into()),
            delay_ms: Some(250),
            wait: false,
            mode: Some(RegionMode::Ellipse),
        };
        let (j, wait) = job(&req);
        assert!(!wait);
        let JobSpec::Workflow { workflow, delay_ms, mode } = j.spec else { panic!() };
        assert_eq!(workflow.id, "capture-region-edit");
        assert_eq!((delay_ms, mode), (Some(250), Some(RegionMode::Ellipse)));

        let e = job_for_request(
            &s(),
            &Request::Capture {
                target: CaptureKind::Region,
                workflow: Some("record".into()),
                delay_ms: None,
                wait: false,
                mode: None,
            },
        )
        .unwrap_err();
        assert!(matches!(e, Rejection::Invalid(m) if m.contains("does not take a screenshot")));

        let e = job_for_request(
            &s(),
            &Request::Capture {
                target: CaptureKind::Fullscreen,
                workflow: None,
                delay_ms: None,
                wait: false,
                mode: Some(RegionMode::Window),
            },
        )
        .unwrap_err();
        assert!(matches!(e, Rejection::Invalid(m) if m.contains("only applies to region")));
    }

    #[test]
    fn files_actions_pick_the_workflow_and_the_editor_flag() {
        let (wf, edit) = workflow_for_files(&s(), &PostAction::Upload).unwrap();
        assert_eq!((wf.id.as_str(), edit), ("upload-files", false));
        let (_, edit) = workflow_for_files(&s(), &PostAction::Edit).unwrap();
        assert!(edit);
        let (wf, _) =
            workflow_for_files(&s(), &PostAction::Workflow { workflow: "upload".into() }).unwrap();
        assert_eq!(wf.id, "upload-files");
        let e = workflow_for_files(&s(), &PostAction::Workflow { workflow: "region".into() })
            .unwrap_err();
        assert!(matches!(e, Rejection::Invalid(m) if m.contains("does not take files")));
        assert!(matches!(
            workflow_for_files(&s(), &PostAction::Workflow { workflow: "zzz".into() }),
            Err(Rejection::UnknownWorkflow(_))
        ));
        // Trimmed-down settings still work.
        let mut trimmed = s();
        trimmed.workflows.clear();
        let (wf, _) = workflow_for_files(&trimmed, &PostAction::Upload).unwrap();
        assert!(wf.after_capture.contains(&AfterCapture::Upload));
        assert!(wf.after_upload.contains(&AfterUpload::CopyUrl));
    }

    #[test]
    fn a_files_job_carries_the_batch_id_and_paths() {
        let j = job_for_files(&s(), &PostAction::Upload, vec!["/a".into(), "/b".into()], 41).unwrap();
        assert_eq!(j.run_id, Some(41));
        let JobSpec::Files { paths, edit_first, .. } = j.spec else { panic!() };
        assert_eq!(paths.len(), 2);
        assert!(!edit_first);
    }

    #[test]
    fn other_requests_are_not_jobs() {
        for r in [
            Request::Ping,
            Request::Quit,
            Request::Status,
            Request::ListWorkflows,
            Request::StopRecording,
            Request::RecordingStatus,
            Request::ReloadSettings,
            Request::Show { target: ShowTarget::Settings },
            Request::CancelRun { run_id: 1 },
            Request::WaitRun { run_id: 1 },
        ] {
            assert!(job_for_request(&s(), &r).unwrap().is_none(), "{r:?}");
        }
    }

    #[test]
    fn listing_and_modes() {
        let l = list_workflows(&s());
        assert_eq!(l.len(), s().workflows.len());
        let region = l.iter().find(|w| w.id == "capture-region").unwrap();
        assert_eq!(region.cli_name.as_deref(), Some("region"));
        assert_eq!(region.hotkey.as_deref(), Some("Ctrl+PrintScreen"));
        assert_eq!(pick_mode(RegionMode::Freeform), PickMode::Freeform);
        assert_eq!(pick_mode(RegionMode::Monitor), PickMode::Monitor);
    }
}
