//! Workflow engine tests against the recording mocks in [`super::testing`].

use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use ssx_types::EncodeOptions;

use super::{testing::*, *};
use crate::{
    history::{EntryKind, Query},
    settings::{AfterCapture as C, AfterUpload as U, DestinationOverride, InputKind, Settings, Workflow},
};

// ---- helpers ---------------------------------------------------------------------------

fn wf(input: InputKind, after_capture: Vec<C>, after_upload: Vec<U>) -> Workflow {
    Workflow {
        id: "test".into(),
        name: "Test".into(),
        input,
        after_capture,
        after_upload,
        ..Workflow::default()
    }
}

fn shot_dir() -> PathBuf {
    Path::new("/shots").join("Screenshots").join("2024-03")
}

fn shot_path() -> PathBuf {
    shot_dir().join("Screenshot_2024-03-09_14-05-06.png")
}

const SHOT_NAME: &str = "Screenshot_2024-03-09_14-05-06.png";

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    test_frame(w, h).encode(EncodeOptions::default()).unwrap()
}

struct Ran {
    report: RunReport,
    events: Vec<Event>,
}

fn shot(world: &TestWorld, w: &Workflow) -> Ran {
    shot_with(world, world.settings(), w)
}

fn shot_with(world: &TestWorld, settings: Settings, w: &Workflow) -> Ran {
    let engine = world.engine(settings);
    let sink = CollectingSink::new();
    let report = engine.post_screenshot(w, &world.services(), &sink, &CancelToken::new());
    let events = sink.events();
    check_event_invariants(&events, &report);
    Ran { report, events }
}

fn files(world: &TestWorld, w: &Workflow, paths: &[&str]) -> Ran {
    files_with(world, world.settings(), w, paths)
}

fn files_with(world: &TestWorld, settings: Settings, w: &Workflow, paths: &[&str]) -> Ran {
    let engine = world.engine(settings);
    let sink = CollectingSink::new();
    let paths = paths.iter().map(PathBuf::from).collect();
    let report = engine.post_file(w, paths, &world.services(), &sink, &CancelToken::new());
    let events = sink.events();
    check_event_invariants(&events, &report);
    Ran { report, events }
}

/// Events are well formed and agree with the report.
fn check_event_invariants(events: &[Event], report: &RunReport) {
    assert!(matches!(events.first(), Some(Event::RunStarted { .. })), "first event: {:?}", events.first());
    assert_eq!(events.last(), Some(&Event::RunFinished { outcome: report.outcome }));
    assert_eq!(events.iter().filter(|e| matches!(e, Event::RunStarted { .. })).count(), 1);
    assert_eq!(events.iter().filter(|e| matches!(e, Event::RunFinished { .. })).count(), 1);
    // every step that started later finishes (per item + kind), and never twice
    let mut open: Vec<(Option<usize>, StepKind)> = Vec::new();
    for e in events {
        match e {
            Event::StepStarted { item, step } => open.push((*item, *step)),
            Event::StepFinished { item, step, .. } => {
                if let Some(pos) = open.iter().position(|o| *o == (*item, *step)) {
                    open.remove(pos);
                }
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "steps started but never finished: {open:?}");
    // the finished events are exactly the report's steps
    let mut finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::StepFinished { item, step, status, .. } => Some((*item, *step, status.clone())),
            _ => None,
        })
        .collect();
    let mut reported: Vec<_> =
        report.all_steps().map(|s| (s.item, s.kind, s.status.clone())).collect();
    let key = |t: &(Option<usize>, StepKind, StepStatus)| format!("{:?}{:?}{:?}", t.0, t.1, t.2);
    finished.sort_by_key(key);
    reported.sort_by_key(key);
    assert_eq!(finished, reported, "events and report disagree");
}

fn kinds(steps: &[StepReport]) -> Vec<StepKind> {
    steps.iter().map(|s| s.kind).collect()
}

fn step_of(r: &RunReport, item: usize, kind: StepKind) -> &StepReport {
    r.items[item]
        .steps
        .iter()
        .find(|s| s.kind == kind)
        .unwrap_or_else(|| panic!("no {kind} step for item {item}: {:?}", kinds(&r.items[item].steps)))
}

fn run_step(r: &RunReport, kind: StepKind) -> &StepReport {
    r.steps.iter().find(|s| s.kind == kind).unwrap_or_else(|| panic!("no run-level {kind} step: {:?}", kinds(&r.steps)))
}

fn failure(s: &StepReport) -> &StepFailure {
    match &s.status {
        StepStatus::Failed(f) => f,
        other => panic!("{} expected to fail, was {other:?}", s.kind),
    }
}

fn skip_reason(s: &StepReport) -> &SkipReason {
    match &s.status {
        StepStatus::Skipped(r) => r,
        other => panic!("{} expected to be skipped, was {other:?}", s.kind),
    }
}

fn standard() -> Workflow {
    wf(
        InputKind::CaptureRegion,
        vec![C::SaveToFile, C::CopyImageToClipboard, C::Upload],
        vec![U::CopyUrl, U::ShowNotification],
    )
}

fn url_of(dest: &str, name: &str) -> String {
    format!("https://{dest}.test/{name}")
}

// ---- screenshots: the happy path -------------------------------------------------------

#[test]
fn region_save_copy_upload_copy_url_notify() {
    let world = TestWorld::new();
    let Ran { report, .. } = shot(&world, &standard());

    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    let item = &report.items[0];
    assert_eq!(item.outcome, Outcome::Success);
    assert_eq!(item.local_path.as_deref(), Some(shot_path().as_path()));
    assert!(item.created_by_workflow);
    assert_eq!(item.url.as_deref(), Some(url_of("test", SHOT_NAME).as_str()));
    assert_eq!(item.uploader.as_deref(), Some("test"));
    assert!(item.thumbnail_url.is_some() && item.deletion_url.is_some());
    assert_eq!(report.urls(), vec![url_of("test", SHOT_NAME)]);

    let expected = vec![
        "capture:Region".to_owned(),
        format!("fs.write:{}", shot_path().display()),
        "clipboard.image:64x48".to_owned(),
        format!("upload:test:Image:{SHOT_NAME}:file"),
        format!("clipboard.text:{}", url_of("test", SHOT_NAME)),
        "notify:Success:Upload complete".to_owned(),
    ];
    assert_eq!(world.log.lines(), expected);
}

#[test]
fn step_reports_are_in_listed_order() {
    let world = TestWorld::new();
    let Ran { report, .. } = shot(&world, &standard());
    assert_eq!(kinds(&report.steps), vec![StepKind::Capture, StepKind::CopyUrl, StepKind::ShowNotification]);
    assert_eq!(
        kinds(&report.items[0].steps),
        vec![StepKind::SaveToFile, StepKind::CopyImage, StepKind::Upload, StepKind::RecordHistory]
    );
    assert!(report.all_steps().all(|s| s.status.is_success()), "{:?}", report.all_steps().collect::<Vec<_>>());
    assert!(report.warnings().is_empty() && report.errors().is_empty());
}

#[test]
fn event_stream_has_progress_and_order() {
    let world = TestWorld::new();
    let Ran { events, .. } = shot(&world, &standard());
    let names: Vec<String> = events
        .iter()
        .map(|e| match e {
            Event::RunStarted { workflow_id, .. } => format!("run:{workflow_id}"),
            Event::StepStarted { step, .. } => format!("start:{step}"),
            Event::StepProgress { step, done, total, .. } => format!("progress:{step}:{done}/{}", total.unwrap_or(0)),
            Event::StepFinished { step, .. } => format!("done:{step}"),
            Event::RunFinished { .. } => "end".to_owned(),
        })
        .collect();
    let pos = |s: &str| names.iter().position(|n| n == s).unwrap_or_else(|| panic!("{s} missing in {names:?}"));
    assert_eq!(names[0], "run:test");
    assert!(pos("start:capture") < pos("done:capture"));
    assert!(pos("done:capture") < pos("start:save_to_file"));
    let first_progress = names.iter().position(|n| n.starts_with("progress:upload")).expect("upload progress events");
    assert!(pos("start:upload") < first_progress && first_progress < pos("done:upload"), "{names:?}");
    assert!(pos("done:upload") < pos("start:copy_url"));
    assert_eq!(names.last().map(String::as_str), Some("end"));
}

#[test]
fn upload_streams_the_saved_file_but_sends_bytes_when_unsaved() {
    let world = TestWorld::new();
    shot(&world, &standard());
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].path.as_deref(), Some(shot_path().as_path()));
    assert_eq!(up[0].bytes, None);
    assert_eq!(up[0].mime, "image/png");
    assert_eq!(up[0].kind, DestinationType::Image);

    let world = TestWorld::new();
    shot(&world, &wf(InputKind::CaptureRegion, vec![C::Upload], vec![]));
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].path, None);
    assert!(up[0].bytes.unwrap() > 100);
    assert_eq!(up[0].file_name, SHOT_NAME, "named from the pattern even though nothing is saved");
    assert!(world.fs.files().is_empty(), "nothing was saved");
}

use crate::settings::DestinationType;

#[test]
fn capture_request_reflects_settings_and_input_kind() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.capture.show_cursor = true;
    s.capture.hdr.peak = 9.0;
    for (input, target) in [
        (InputKind::CaptureRegion, CaptureTarget::Region),
        (InputKind::CaptureFullscreen, CaptureTarget::Fullscreen),
        (InputKind::CaptureMonitor, CaptureTarget::Monitor),
        (InputKind::CaptureWindow, CaptureTarget::Window),
        (InputKind::CaptureLastRegion, CaptureTarget::LastRegion),
    ] {
        shot_with(&world, s.clone(), &wf(input, vec![], vec![]));
        let req = *world.capturer.requests.lock().unwrap().last().unwrap();
        assert_eq!(req.target, target);
        assert!(req.include_cursor);
        assert_eq!(req.hdr.peak, 9.0);
    }
}

#[test]
fn window_title_reaches_the_file_name_and_history() {
    let world = TestWorld::new();
    *world.capturer.window_title.lock().unwrap() = Some("My: App / Window".into());
    let mut s = world.settings();
    s.general.file_name_pattern = "%t_%width".into();
    let Ran { report, .. } = shot_with(&world, s, &wf(InputKind::CaptureWindow, vec![C::SaveToFile], vec![]));
    let p = report.items[0].local_path.clone().unwrap();
    assert_eq!(p.file_name().unwrap(), "My_App__Window_64.png");
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e[0].window_title.as_deref(), Some("My: App / Window"));
    assert_eq!((e[0].width, e[0].height), (Some(64), Some(48)));
}

// ---- saving ----------------------------------------------------------------------------

#[test]
fn saving_twice_never_overwrites() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![]);
    let a = shot(&world, &w).report.items[0].local_path.clone().unwrap();
    let b = shot(&world, &w).report.items[0].local_path.clone().unwrap();
    assert_eq!(a, shot_path());
    assert_eq!(b.file_name().unwrap(), "Screenshot_2024-03-09_14-05-06 (2).png");
    assert_eq!(world.fs.files().len(), 2);
}

#[test]
fn folders_formats_and_names_follow_settings() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.general.use_type_subfolders = false;
    s.general.folder_pattern = String::new();
    s.general.image_format = crate::settings::ImageFormatKind::Jpg;
    s.general.file_name_pattern = "shot".into();
    let Ran { report, .. } = shot_with(&world, s, &wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload], vec![]));
    assert_eq!(report.items[0].local_path.as_deref(), Some(Path::new("/shots").join("shot.jpg").as_path()));
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].mime, "image/jpeg");
    let bytes = world.fs.contents(Path::new("/shots").join("shot.jpg")).unwrap();
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG magic");
}

#[test]
fn saved_png_is_a_valid_image() {
    let world = TestWorld::new();
    shot(&world, &wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![]));
    let bytes = world.fs.contents(shot_path()).unwrap();
    let img = image::load_from_memory(&bytes).unwrap();
    assert_eq!((img.width(), img.height()), (64, 48));
}

#[test]
fn save_failure_does_not_lose_the_screenshot() {
    let world = TestWorld::new();
    world.fs.fail_writes(Some("disk full"));
    let Ran { report, .. } = shot(&world, &standard());
    assert_eq!(failure(step_of(&report, 0, StepKind::SaveToFile)).kind, FailureKind::Io);
    assert!(failure(step_of(&report, 0, StepKind::SaveToFile)).message.contains("disk full"));
    // upload still happened, from memory
    assert!(step_of(&report, 0, StepKind::Upload).status.is_success());
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].bytes.is_some(), true);
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    assert!(report.items[0].local_path.is_none());
    // the clipboard copy happened too
    assert!(world.clipboard.image.lock().unwrap().is_some());
}

#[test]
fn save_as_dialog_writes_where_the_user_chose() {
    let world = TestWorld::new();
    *world.save_dialog.choice.lock().unwrap() = Some(PathBuf::from("/chosen/mine.png"));
    let Ran { report, .. } = shot(&world, &wf(InputKind::CaptureRegion, vec![C::SaveAsDialog, C::Upload], vec![]));
    assert_eq!(report.items[0].local_path.as_deref(), Some(Path::new("/chosen/mine.png")));
    assert!(world.fs.contents("/chosen/mine.png").is_some());
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].path.as_deref(), Some(Path::new("/chosen/mine.png")));
    assert!(world.log.lines().iter().any(|l| l == &format!("dialog.save:{}", shot_path().display())));
}

#[test]
fn save_as_dialog_cancel_skips_only_that_step() {
    let world = TestWorld::new();
    let Ran { report, .. } =
        shot(&world, &wf(InputKind::CaptureRegion, vec![C::SaveAsDialog, C::CopyImageToClipboard, C::Upload], vec![]));
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::SaveAsDialog)), &SkipReason::UserDeclined);
    assert!(step_of(&report, 0, StepKind::Upload).status.is_success());
    assert_eq!(report.outcome, Outcome::Success);
    assert!(world.fs.files().is_empty());
}

// ---- editor ----------------------------------------------------------------------------

#[test]
fn edited_image_is_what_gets_saved_and_uploaded() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::OpenEditor, C::SaveToFile, C::CopyImageToClipboard, C::Upload], vec![]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success);
    let lines = world.log.lines();
    let p = |prefix: &str| lines.iter().position(|l| l.starts_with(prefix)).unwrap();
    assert!(p("capture") < p("editor.edit") && p("editor.edit") < p("fs.write") && p("fs.write") < p("upload"));
    let saved = image::load_from_memory(&world.fs.contents(shot_path()).unwrap()).unwrap().to_rgba8();
    assert_eq!(saved.get_pixel(0, 0).0, [255, 255, 255, 255], "edit is in the saved file");
    let clip = world.clipboard.image.lock().unwrap().clone().unwrap();
    assert_eq!(&clip.row(0)[..4], &[255, 255, 255, 255], "and on the clipboard");
}

#[test]
fn editor_cancel_cancels_the_run_and_nothing_else_happens() {
    let world = TestWorld::new();
    *world.editor.mode.lock().unwrap() = EditorMode::Cancel;
    let w = wf(InputKind::CaptureRegion, vec![C::OpenEditor, C::SaveToFile, C::Upload], vec![U::CopyUrl, U::ShowNotification]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert_eq!(report.items[0].outcome, Outcome::Cancelled);
    assert_eq!(step_of(&report, 0, StepKind::OpenEditor).status, StepStatus::Cancelled);
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::SaveToFile)), &SkipReason::Cancelled);
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::Upload)), &SkipReason::Cancelled);
    assert!(world.fs.files().is_empty());
    assert!(world.uploaders.uploads.lock().unwrap().is_empty());
    assert!(world.notifier.shown.lock().unwrap().is_empty(), "no notification for a user cancel");
    assert_eq!(world.history.count(&Query::default()).unwrap(), 0);
}

#[test]
fn editor_failure_aborts_the_item_and_reports() {
    let world = TestWorld::new();
    *world.editor.mode.lock().unwrap() = EditorMode::Fail(Fail::msg("GPU device lost"));
    let w = wf(InputKind::CaptureRegion, vec![C::OpenEditor, C::SaveToFile, C::Upload], vec![U::ShowNotification]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(failure(step_of(&report, 0, StepKind::OpenEditor)).message, "GPU device lost");
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::SaveToFile)), &SkipReason::ItemFailed);
    assert!(world.uploaders.uploads.lock().unwrap().is_empty(), "never upload unedited when the editor broke");
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].level, NotificationLevel::Error);
    assert!(n[0].body.contains("GPU device lost"));
}

// ---- capture failures ------------------------------------------------------------------

#[test]
fn capture_cancelled_by_the_user_is_not_a_failure() {
    let world = TestWorld::new();
    world.capturer.script.lock().unwrap().push_back(Err(Fail::Cancelled));
    let Ran { report, .. } = shot(&world, &standard());
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert!(report.items.is_empty());
    assert_eq!(run_step(&report, StepKind::Capture).status, StepStatus::Cancelled);
    assert!(world.notifier.shown.lock().unwrap().is_empty());
    assert_eq!(world.log.lines(), vec!["capture:Region".to_owned()]);
}

#[test]
fn capture_failure_is_reported_and_notified() {
    let world = TestWorld::new();
    world
        .capturer
        .script
        .lock()
        .unwrap()
        .push_back(Err(Fail::Unsupported("window capture on this compositor".into())));
    let Ran { report, .. } = shot(&world, &standard());
    assert_eq!(report.outcome, Outcome::Failed);
    let f = failure(run_step(&report, StepKind::Capture));
    assert_eq!(f.kind, FailureKind::Unsupported);
    assert!(f.message.contains("window capture"));
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n.len(), 1, "failure notification even though there are no items");
    assert!(n[0].body.contains("window capture"));
    assert!(report.summary().starts_with("Failed"));
}

#[test]
fn hdr_frames_from_the_capturer_are_rejected_with_guidance() {
    let world = TestWorld::new();
    *world.capturer.hdr_frame.lock().unwrap() = true;
    let Ran { report, .. } = shot(&world, &standard());
    assert_eq!(report.outcome, Outcome::Failed);
    let f = failure(run_step(&report, StepKind::Capture));
    assert_eq!(f.kind, FailureKind::Invalid);
    assert!(f.message.contains("tone-mapped"), "{}", f.message);
}

#[test]
fn empty_captures_are_rejected() {
    let world = TestWorld::new();
    *world.capturer.size.lock().unwrap() = (0, 0);
    let Ran { report, .. } = shot(&world, &standard());
    assert!(failure(run_step(&report, StepKind::Capture)).message.contains("empty"));
}

#[test]
fn capture_delay_is_cancellable() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.capture.delay_ms = 60_000;
    let engine = world.engine(s);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        c2.cancel();
    });
    let start = std::time::Instant::now();
    let report = engine.post_screenshot(&standard(), &world.services(), &NullSink, &cancel);
    h.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert!(world.log.lines().is_empty(), "the capturer must not be called after cancellation");
}

#[test]
fn capture_delay_elapses_before_capturing() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.capture.delay_ms = 80;
    let start = std::time::Instant::now();
    let Ran { report, .. } = shot_with(&world, s, &wf(InputKind::CaptureFullscreen, vec![], vec![]));
    assert!(start.elapsed() >= Duration::from_millis(70));
    assert_eq!(report.outcome, Outcome::Success);
}

// ---- upload failures keep the file -----------------------------------------------------

#[test]
fn failed_upload_keeps_the_local_file_and_reports_precisely() {
    let world = TestWorld::new();
    world.uploaders.fail_all(Fail::Retryable("connection reset by peer".into()));
    let w = wf(
        InputKind::CaptureRegion,
        vec![C::SaveToFile, C::Upload, C::DeleteLocalFile],
        vec![U::CopyUrl, U::OpenUrl, U::ShowNotification],
    );
    let Ran { report, .. } = shot(&world, &w);

    assert_eq!(report.outcome, Outcome::PartialSuccess);
    let up = step_of(&report, 0, StepKind::Upload);
    let f = failure(up);
    assert_eq!(f.kind, FailureKind::Service);
    assert!(f.retryable);
    assert_eq!(f.message, "connection reset by peer");
    // the file is still there and reported
    assert!(world.fs.contents(shot_path()).is_some());
    assert_eq!(report.items[0].local_path.as_deref(), Some(shot_path().as_path()));
    assert!(report.items[0].url.is_none());
    // delete was skipped for the right reason and never touched the file system
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::UploadNotConfirmed);
    assert!(world.log.with_prefix("fs.remove").is_empty());
    // url steps are skipped with NoUrl
    assert_eq!(skip_reason(run_step(&report, StepKind::CopyUrl)), &SkipReason::NoUrl);
    assert!(world.opener.fail.lock().unwrap().is_none() && world.log.with_prefix("open:").is_empty());
    // the notification tells the user what happened and where the file is
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].level, NotificationLevel::Warning);
    assert_eq!(n[0].title, "Upload failed");
    assert!(n[0].body.contains("connection reset by peer"), "{}", n[0].body);
    assert!(n[0].body.contains(&shot_path().display().to_string()), "{}", n[0].body);
    // history still records the screenshot, without a URL
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].local_path.as_deref(), Some(shot_path().as_path()));
    assert!(e[0].upload_url.is_none());
    assert!(report.summary().contains("upload: connection reset by peer"), "{}", report.summary());
}

#[test]
fn failed_upload_without_saving_is_a_total_failure() {
    let world = TestWorld::new();
    world.uploaders.fail_all(Fail::msg("401 Unauthorized"));
    let Ran { report, .. } = shot(&world, &wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::ShowNotification]));
    assert_eq!(report.outcome, Outcome::Failed);
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n[0].level, NotificationLevel::Error);
    assert_eq!(world.history.count(&Query::default()).unwrap(), 0, "nothing persisted, nothing to record");
}

#[test]
fn missing_destination_is_a_clear_configuration_error() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.destinations.image = None;
    let Ran { report, .. } = shot_with(&world, s, &standard());
    let f = failure(step_of(&report, 0, StepKind::Upload));
    assert_eq!(f.kind, FailureKind::NotConfigured);
    assert!(f.message.contains("no image uploader") && f.message.contains("destinations.image"), "{}", f.message);
    assert!(world.uploaders.uploads.lock().unwrap().is_empty(), "the service is not even called");
    assert_eq!(report.outcome, Outcome::PartialSuccess, "the file was saved");
}

#[test]
fn empty_url_means_the_upload_is_not_confirmed() {
    let world = TestWorld::new();
    world.uploaders.empty_url_for.lock().unwrap().push("Screenshot".into());
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![U::CopyUrl]);
    let Ran { report, .. } = shot(&world, &w);
    assert!(failure(step_of(&report, 0, StepKind::Upload)).message.contains("no URL"));
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::UploadNotConfirmed);
    assert!(world.fs.contents(shot_path()).is_some(), "file must survive an unconfirmed upload");
    assert!(world.log.with_prefix("clipboard.text").is_empty());
}

#[test]
fn service_panics_become_failed_steps() {
    struct Panicker;
    impl Uploaders for Panicker {
        fn upload(
            &self,
            _: &UploadRequest<'_>,
            _: &dyn Fn(UploadProgress),
            _: &CancelToken,
        ) -> Result<UploadOutcome, ServiceError> {
            panic!("uploader blew up");
        }
    }
    let world = TestWorld::new();
    let services = Services { uploaders: &Panicker, ..world.services() };
    let engine = world.engine(world.settings());
    let report = engine.post_screenshot(&standard(), &services, &NullSink, &CancelToken::new());
    let f = failure(step_of(&report, 0, StepKind::Upload));
    assert_eq!(f.kind, FailureKind::Internal);
    assert!(f.message.contains("uploader blew up"));
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    assert!(world.fs.contents(shot_path()).is_some());
}

// ---- delete-after-upload safety --------------------------------------------------------

#[test]
fn delete_local_file_runs_only_after_confirmed_upload() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success);
    assert!(world.fs.contents(shot_path()).is_none());
    assert!(report.items[0].local_path.is_none());
    assert!(!report.items[0].created_by_workflow);
    let lines = world.log.lines();
    let up = lines.iter().position(|l| l.starts_with("upload:")).unwrap();
    let rm = lines.iter().position(|l| l.starts_with("fs.remove:")).unwrap();
    assert!(up < rm);
    // history keeps the URL and the thumbnail, but no dead path
    let e = world.history.list(&Query::default()).unwrap();
    assert!(e[0].local_path.is_none());
    assert_eq!(e[0].upload_url.as_deref(), Some(url_of("test", SHOT_NAME).as_str()));
    assert!(e[0].thumbnail.is_some());
}

#[test]
fn delete_listed_before_upload_never_runs() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::DeleteLocalFile, C::Upload], vec![]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::UploadNotConfirmed);
    assert!(world.fs.contents(shot_path()).is_some());
}

#[test]
fn delete_without_a_saved_file_is_skipped() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::NoLocalFile);
    assert_eq!(report.outcome, Outcome::Success);
}

#[test]
fn delete_is_idempotent_when_the_file_is_already_gone() {
    struct Vanish<'a>(&'a MemFs);
    impl Uploaders for Vanish<'_> {
        fn upload(&self, r: &UploadRequest<'_>, _: &dyn Fn(UploadProgress), _: &CancelToken) -> Result<UploadOutcome, ServiceError> {
            // the user (or antivirus, or sync client) removes the file during the upload
            if let UploadSource::LocalFile(p) = r.source {
                let _ = self.0.remove_file(p);
            }
            Ok(UploadOutcome::url("https://x.test/a.png"))
        }
    }
    let world = TestWorld::new();
    let v = Vanish(&world.fs);
    let services = Services { uploaders: &v, ..world.services() };
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![]);
    let report = world.engine(world.settings()).post_screenshot(&w, &services, &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Success);
    assert!(step_of(&report, 0, StepKind::DeleteLocalFile).status.is_success());
}

#[test]
fn delete_failure_is_reported_and_keeps_the_path() {
    let world = TestWorld::new();
    world.fs.fail_remove(Some(io::ErrorKind::PermissionDenied));
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![U::CopyUrl]);
    let Ran { report, .. } = shot(&world, &w);
    let f = failure(step_of(&report, 0, StepKind::DeleteLocalFile));
    assert_eq!(f.kind, FailureKind::Io);
    assert!(f.message.contains("uploaded, but could not delete"), "{}", f.message);
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    assert!(report.items[0].local_path.is_some());
    assert!(step_of(&report, 0, StepKind::Upload).status.is_success());
    assert!(run_step(&report, StepKind::CopyUrl).status.is_success(), "url steps still run");
}

#[test]
fn user_files_are_never_deleted() {
    let world = TestWorld::new();
    world.fs.add_file("/home/u/report.pdf", b"pdf");
    let w = wf(InputKind::Files, vec![C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = files(&world, &w, &["/home/u/report.pdf"]);
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::NotCreatedByWorkflow);
    assert!(world.fs.contents("/home/u/report.pdf").is_some());
    assert!(world.log.with_prefix("fs.remove").is_empty());
    assert_eq!(report.outcome, Outcome::Success);
}

#[test]
fn a_saved_copy_of_an_edited_user_image_may_be_deleted_but_the_original_never() {
    let world = TestWorld::new();
    world.fs.add_file("/home/u/photo.png", png_bytes(20, 10));
    let mut s = world.settings();
    s.post_file.images_through_editor = true;
    let w = wf(InputKind::Files, vec![C::OpenEditor, C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = files_with(&world, s, &w, &["/home/u/photo.png"]);
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    assert!(world.fs.contents("/home/u/photo.png").is_some(), "original untouched");
    let removed = world.log.with_prefix("fs.remove");
    assert_eq!(removed.len(), 1);
    assert!(removed[0].contains("Screenshots"), "only the saved copy was removed: {removed:?}");
}

// ---- optional steps never abort --------------------------------------------------------

#[test]
fn optional_step_failures_are_warnings_and_do_not_stop_anything() {
    let world = TestWorld::new();
    *world.clipboard.fail.lock().unwrap() = Some(Fail::msg("clipboard busy"));
    *world.notifier.fail.lock().unwrap() = Some(Fail::msg("no notification daemon"));
    *world.opener.fail.lock().unwrap() = Some(Fail::msg("no browser"));
    *world.pinner.fail.lock().unwrap() = Some(Fail::Unsupported("pinning".into()));
    let w = wf(
        InputKind::CaptureRegion,
        vec![C::CopyImageToClipboard, C::PinToScreen, C::SaveToFile, C::Upload],
        vec![U::CopyUrl, U::OpenUrl, U::ShowQrCode, U::ShowNotification],
    );
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    assert_eq!(report.items[0].outcome, Outcome::Success);
    let warned: Vec<StepKind> = report.warnings().iter().map(|s| s.kind).collect();
    for k in [StepKind::CopyImage, StepKind::PinToScreen, StepKind::CopyUrl, StepKind::OpenUrl, StepKind::ShowQrCode, StepKind::ShowNotification] {
        // (ShowQrCode uses the notifier's show_qr which also fails)
        assert!(warned.contains(&k), "{k} missing from {warned:?}");
    }
    assert!(report.errors().is_empty());
    // later steps still ran: save and upload happened after the failed clipboard and pin
    assert!(step_of(&report, 0, StepKind::SaveToFile).status.is_success());
    assert!(step_of(&report, 0, StepKind::Upload).status.is_success());
    assert_eq!(report.items[0].url.as_deref(), Some(url_of("test", SHOT_NAME).as_str()));
}

#[test]
fn history_failure_is_only_a_warning() {
    let world = TestWorld::new();
    // Break the database: a constraint-violating drop of the table.
    // (Uses a second handle to the same in-memory DB is impossible, so use a read-only path.)
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.sqlite3");
    let h = crate::history::History::open(&path).unwrap();
    drop(world);
    let world = TestWorld::new();
    let services = Services { history: Some(&h), ..world.services() };
    // Lock the file for writing from another connection so inserts time out.
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("BEGIN IMMEDIATE").unwrap();
    let cfg = crate::history::HistoryConfig { busy_timeout: Duration::from_millis(30), ..Default::default() };
    let h2 = crate::history::History::open_with(&path, &cfg).unwrap();
    let services2 = Services { history: Some(&h2), ..services };
    let report = world.engine(world.settings()).post_screenshot(&standard(), &services2, &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Success);
    let hs = step_of(&report, 0, StepKind::RecordHistory);
    assert!(failure(hs).message.contains("history"), "{:?}", hs.status);
    assert_eq!(report.warnings().len(), 1);
    assert!(report.items[0].history_id.is_none());
    raw.execute_batch("ROLLBACK").unwrap();
}

// ---- history ---------------------------------------------------------------------------

#[test]
fn history_entry_has_everything() {
    let world = TestWorld::new();
    let Ran { report, .. } = shot(&world, &standard());
    let id = report.items[0].history_id.expect("recorded");
    let e = world.history.get(id).unwrap().unwrap();
    assert_eq!(e.kind, EntryKind::Image);
    assert_eq!(e.local_path.as_deref(), Some(shot_path().as_path()));
    assert_eq!(e.upload_url.as_deref(), Some(url_of("test", SHOT_NAME).as_str()));
    assert_eq!(e.thumbnail_url.as_deref(), Some(format!("https://test.test/t/{SHOT_NAME}").as_str()));
    assert_eq!(e.deletion_url.as_deref(), Some(format!("https://test.test/del/{SHOT_NAME}").as_str()));
    assert_eq!(e.uploader.as_deref(), Some("test"));
    assert_eq!((e.width, e.height), (Some(64), Some(48)));
    assert_eq!(e.workflow_id.as_deref(), Some("test"));
    assert_eq!(e.size_bytes, Some(world.fs.contents(shot_path()).unwrap().len() as u64));
    assert_eq!(e.sha256.as_deref(), Some(crate::history::sha256_hex(&world.fs.contents(shot_path()).unwrap()).as_str()));
    assert_eq!(e.created_at, 1_709_993_106_000, "clock injected into the engine");
    let thumb = e.thumbnail.expect("thumbnail");
    assert_eq!(image::load_from_memory(&thumb).unwrap().width(), 64);
}

#[test]
fn history_can_be_disabled_or_absent() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.history.enabled = false;
    let Ran { report, .. } = shot_with(&world, s, &standard());
    assert_eq!(world.history.count(&Query::default()).unwrap(), 0);
    assert!(report.items[0].history_id.is_none());
    assert!(!kinds(&report.items[0].steps).contains(&StepKind::RecordHistory));

    let engine = world.engine(world.settings());
    let report = engine.post_screenshot(&standard(), &world.services_without_history(), &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Success);
    assert_eq!(world.history.count(&Query::default()).unwrap(), 0);
}

#[test]
fn history_retention_is_applied() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.history.max_entries = 2;
    for _ in 0..4 {
        shot_with(&world, s.clone(), &standard());
    }
    assert_eq!(world.history.count(&Query::default()).unwrap(), 2);
}

#[test]
fn history_only_records_things_that_exist() {
    let world = TestWorld::new();
    // copy-to-clipboard only: no file, no URL -> nothing worth remembering
    shot(&world, &wf(InputKind::CaptureRegion, vec![C::CopyImageToClipboard], vec![]));
    assert_eq!(world.history.count(&Query::default()).unwrap(), 0);
}

// ---- cancellation ----------------------------------------------------------------------

#[test]
fn cancelling_before_the_run_does_nothing() {
    let world = TestWorld::new();
    let cancel = CancelToken::new();
    cancel.cancel();
    let sink = CollectingSink::new();
    let report = world.engine(world.settings()).post_screenshot(&standard(), &world.services(), &sink, &cancel);
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert!(world.log.lines().is_empty());
    check_event_invariants(&sink.events(), &report);
}

#[test]
fn cancel_during_upload_stops_the_rest_but_keeps_and_records_the_file() {
    let world = TestWorld::new();
    let cancel = CancelToken::new();
    *world.uploaders.cancel_on_start.lock().unwrap() = Some((cancel.clone(), false));
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![U::CopyUrl, U::ShowNotification]);
    let sink = CollectingSink::new();
    let report = world.engine(world.settings()).post_screenshot(&w, &world.services(), &sink, &cancel);
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert_eq!(step_of(&report, 0, StepKind::Upload).status, StepStatus::Cancelled);
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::Cancelled);
    assert_eq!(skip_reason(run_step(&report, StepKind::CopyUrl)), &SkipReason::Cancelled);
    assert!(world.fs.contents(shot_path()).is_some(), "saved file is kept");
    assert!(world.notifier.shown.lock().unwrap().is_empty());
    assert_eq!(world.history.count(&Query::default()).unwrap(), 1, "what was saved is recorded");
    check_event_invariants(&sink.events(), &report);
}

#[test]
fn cancel_that_arrives_after_a_finished_upload_keeps_the_url() {
    let world = TestWorld::new();
    let cancel = CancelToken::new();
    *world.uploaders.cancel_on_start.lock().unwrap() = Some((cancel.clone(), true));
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload, C::DeleteLocalFile], vec![U::CopyUrl]);
    let report = world.engine(world.settings()).post_screenshot(&w, &world.services(), &NullSink, &cancel);
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert!(report.items[0].url.is_some(), "the upload did finish");
    // deletion is a follow-up action and must not happen after a cancel
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::DeleteLocalFile)), &SkipReason::Cancelled);
    assert!(world.fs.contents(shot_path()).is_some());
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e.len(), 1);
    assert!(e[0].upload_url.is_some(), "history records the finished upload");
}

#[test]
fn cancel_mid_multi_upload_from_another_thread() {
    let world = TestWorld::new();
    for i in 0..6 {
        world.fs.add_file(format!("/in/f{i}.bin"), vec![0u8; 10]);
    }
    *world.uploaders.delay.lock().unwrap() = Duration::from_secs(30);
    let mut s = world.settings();
    s.post_file.max_parallel_uploads = 2;
    let engine = world.engine(s);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        c2.cancel();
    });
    let paths: Vec<PathBuf> = (0..6).map(|i| PathBuf::from(format!("/in/f{i}.bin"))).collect();
    let start = std::time::Instant::now();
    let report = engine.post_file(&wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl]), paths, &world.services(), &NullSink, &cancel);
    h.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(15), "must not wait out the slow uploads");
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert_eq!(report.items.len(), 6);
    assert!(report.urls().is_empty());
    assert!(world.log.with_prefix("clipboard.text").is_empty());
}

// ---- after-upload steps ----------------------------------------------------------------

#[test]
fn shorten_url_replaces_the_current_url() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::ShortenUrl, U::CopyUrl, U::OpenUrl]);
    let Ran { report, .. } = shot(&world, &w);
    let item = &report.items[0];
    assert_eq!(item.short_url.as_deref(), Some("https://sho.rt/1"));
    assert_eq!(world.clipboard.text.lock().unwrap().as_deref(), Some("https://sho.rt/1"));
    assert!(world.log.lines().contains(&"open:https://sho.rt/1".to_owned()));
    assert!(world.log.lines().contains(&format!("shorten:short:{}", url_of("test", SHOT_NAME))));
    assert_eq!(report.urls(), vec!["https://sho.rt/1"]);
    assert_eq!(item.url.as_deref(), Some(url_of("test", SHOT_NAME).as_str()), "the original URL stays in the report");
}

#[test]
fn copy_short_url_shortens_on_demand_without_changing_copy_url() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::CopyShortUrl, U::CopyUrl]);
    shot(&world, &w);
    let copies = world.log.with_prefix("clipboard.text");
    assert_eq!(copies, vec!["clipboard.text:https://sho.rt/1".to_owned(), format!("clipboard.text:{}", url_of("test", SHOT_NAME))]);
}

#[test]
fn shortener_failures_fall_back_gracefully() {
    let world = TestWorld::new();
    *world.shortener.fail.lock().unwrap() = Some(Fail::msg("shortener down"));
    let w = wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::ShortenUrl, U::CopyUrl, U::CopyShortUrl]);
    let Ran { report, .. } = shot(&world, &w);
    assert!(failure(step_of(&report, 0, StepKind::ShortenUrl)).message.contains("shortener down"));
    assert_eq!(world.log.with_prefix("clipboard.text").len(), 1, "CopyUrl copied the long URL; CopyShortUrl had nothing to copy");
    assert_eq!(world.clipboard.text.lock().unwrap().as_deref(), Some(url_of("test", SHOT_NAME).as_str()));
    assert_eq!(report.outcome, Outcome::PartialSuccess, "shorten is a normal step");
    let short_copy = report.steps.iter().rev().find(|s| s.kind == StepKind::CopyShortUrl).unwrap();
    assert!(matches!(skip_reason(short_copy), SkipReason::NotApplicable(_)));
}

#[test]
fn missing_shortener_configuration_is_explained() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.destinations.url_shortener = None;
    let Ran { report, .. } = shot_with(&world, s, &wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::ShortenUrl]));
    let f = failure(step_of(&report, 0, StepKind::ShortenUrl));
    assert_eq!(f.kind, FailureKind::NotConfigured);
    assert!(f.message.contains("url_shortener"));
}

#[test]
fn workflow_override_selects_the_uploader() {
    let world = TestWorld::new();
    let mut w = standard();
    w.destination = DestinationOverride { image: Some("special".into()), url_shortener: Some("mine".into()), ..Default::default() };
    shot(&world, &w);
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].destination, "special");
    let mut w = wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::ShortenUrl]);
    w.destination.url_shortener = Some("mine".into());
    shot(&world, &w);
    assert!(world.log.with_prefix("shorten:mine:").len() == 1);
}

#[test]
fn qr_code_is_rendered_and_shown() {
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::Upload], vec![U::ShowQrCode]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success);
    let shown = world.notifier.qr.lock().unwrap().clone();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0, url_of("test", SHOT_NAME));
    assert!(shown[0].1 >= 100);
}

#[test]
fn notifications_can_be_turned_off() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.general.show_notifications = false;
    let Ran { report, .. } = shot_with(&world, s, &standard());
    assert!(matches!(skip_reason(run_step(&report, StepKind::ShowNotification)), SkipReason::NotApplicable(_)));
    assert!(world.notifier.shown.lock().unwrap().is_empty());
}

#[test]
fn success_notification_carries_url_and_path() {
    let world = TestWorld::new();
    shot(&world, &standard());
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].level, NotificationLevel::Success);
    assert_eq!(n[0].url.as_deref(), Some(url_of("test", SHOT_NAME).as_str()));
    assert_eq!(n[0].path.as_deref(), Some(shot_path().as_path()));
    // saved-only workflows say "Saved"
    let world = TestWorld::new();
    shot(&world, &wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![U::ShowNotification]));
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n[0].title, "Saved");
}

// ---- run_command -----------------------------------------------------------------------

#[test]
fn run_command_expands_each_argument_separately_without_a_shell() {
    let world = TestWorld::new();
    let nasty = "/in/my file; rm -rf $(x) `y` \"q\".txt";
    world.fs.add_file(nasty, b"data");
    let w = wf(
        InputKind::Files,
        vec![C::Upload],
        vec![U::RunCommand {
            program: "notify {url}".into(),
            args: vec!["--file={path}".into(), "{file_name}".into(), "{url}".into(), "plain arg".into(), "{{literal}}".into()],
        }],
    );
    let Ran { report, .. } = files(&world, &w, &[nasty]);
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    let runs = world.commands.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].program, "notify {url}", "the program is never templated");
    assert_eq!(
        runs[0].args,
        vec![
            format!("--file={nasty}"),
            "my file; rm -rf $(x) `y` \"q\".txt".to_owned(),
            url_of("test", "my file; rm -rf $(x) `y` \"q\".txt"),
            "plain arg".to_owned(),
            "{literal}".to_owned(),
        ],
        "each argument stays exactly one argument"
    );
    assert_eq!(runs[0].timeout, Duration::from_secs(60));
}

#[test]
fn run_command_failures_are_reported_with_stderr() {
    let world = TestWorld::new();
    *world.commands.exit_code.lock().unwrap() = 2;
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![U::RunCommand { program: "post-process".into(), args: vec!["{path}".into()] }]);
    let Ran { report, .. } = shot(&world, &w);
    let f = failure(step_of(&report, 0, StepKind::RunCommand));
    assert!(f.message.contains("post-process exited with code 2: boom"), "{}", f.message);
    assert_eq!(report.outcome, Outcome::PartialSuccess);
}

#[test]
fn run_command_start_failure_and_template_errors() {
    let world = TestWorld::new();
    *world.commands.fail.lock().unwrap() = Some(Fail::msg("cannot start \"nope\""));
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![U::RunCommand { program: "nope".into(), args: vec![] }]);
    let Ran { report, .. } = shot(&world, &w);
    assert!(failure(step_of(&report, 0, StepKind::RunCommand)).message.contains("cannot start"));

    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![U::RunCommand { program: "x".into(), args: vec!["{bogus}".into()] }]);
    let Ran { report, .. } = shot(&world, &w);
    let f = failure(step_of(&report, 0, StepKind::RunCommand));
    assert_eq!(f.kind, FailureKind::Invalid);
    assert!(f.message.contains("unknown variable"));
    assert!(world.commands.runs.lock().unwrap().is_empty(), "never runs with a broken template");

    // {url} without an upload
    let world = TestWorld::new();
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile], vec![U::RunCommand { program: "x".into(), args: vec!["{url}".into()] }]);
    let Ran { report, .. } = shot(&world, &w);
    assert!(failure(step_of(&report, 0, StepKind::RunCommand)).message.contains("not available"));
}

#[test]
fn run_command_is_skipped_after_a_failed_upload() {
    let world = TestWorld::new();
    world.uploaders.fail_all(Fail::msg("nope"));
    let w = wf(InputKind::CaptureRegion, vec![C::SaveToFile, C::Upload], vec![U::RunCommand { program: "x".into(), args: vec![] }]);
    let Ran { report, .. } = shot(&world, &w);
    assert!(run_step(&report, StepKind::RunCommand).status == StepStatus::Skipped(SkipReason::NotApplicable("no item finished successfully".into())));
    assert!(world.commands.runs.lock().unwrap().is_empty());
}

// ---- pin / ocr / clipboard image -------------------------------------------------------

#[test]
fn ocr_copies_recognised_text() {
    let world = TestWorld::new();
    let Ran { report, .. } = shot(&world, &wf(InputKind::CaptureRegion, vec![C::Ocr, C::PinToScreen], vec![]));
    assert_eq!(world.clipboard.text.lock().unwrap().as_deref(), Some("recognised text"));
    assert!(step_of(&report, 0, StepKind::Ocr).detail.as_deref().unwrap().contains("15 characters"));
    assert!(world.log.with_prefix("pin:").len() == 1);

    let world = TestWorld::new();
    *world.ocr.text.lock().unwrap() = "  ".into();
    let Ran { report, .. } = shot(&world, &wf(InputKind::CaptureRegion, vec![C::Ocr], vec![]));
    assert_eq!(step_of(&report, 0, StepKind::Ocr).detail.as_deref(), Some("no text found"));
    assert!(world.clipboard.text.lock().unwrap().is_none());
}

// ---- clipboard input -------------------------------------------------------------------

#[test]
fn clipboard_text_is_uploaded_as_text() {
    let world = TestWorld::new();
    *world.clipboard.content.lock().unwrap() = Some(ClipboardContent::Text("hello world".into()));
    let w = wf(InputKind::Clipboard, vec![C::SaveToFile, C::Upload], vec![U::CopyUrl]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].kind, DestinationType::Text);
    assert_eq!(up[0].mime, "text/plain");
    assert_eq!(up[0].file_name, "Screenshot_2024-03-09_14-05-06.txt");
    let saved = report.items[0].local_path.clone().unwrap();
    assert!(saved.starts_with("/shots/Text") || saved.to_string_lossy().contains("Text"));
    assert_eq!(world.fs.contents(&saved).unwrap(), b"hello world");
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e[0].kind, EntryKind::Text);
    assert_eq!(e[0].note.as_deref(), Some("hello world"));
}

#[test]
fn clipboard_image_follows_the_screenshot_path() {
    let world = TestWorld::new();
    *world.clipboard.content.lock().unwrap() = Some(ClipboardContent::Image(test_frame(10, 10)));
    let w = wf(InputKind::Clipboard, vec![C::Upload], vec![]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success);
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].kind, DestinationType::Image);
}

#[test]
fn clipboard_files_follow_the_post_file_path() {
    let world = TestWorld::new();
    world.fs.add_file("/a/x.zip", b"z");
    *world.clipboard.content.lock().unwrap() = Some(ClipboardContent::Files(vec!["/a/x.zip".into()]));
    let w = wf(InputKind::Clipboard, vec![C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = shot(&world, &w);
    assert_eq!(report.outcome, Outcome::Success);
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].path.as_deref(), Some(Path::new("/a/x.zip")));
    assert!(world.fs.contents("/a/x.zip").is_some(), "clipboard files are user files");
}

#[test]
fn empty_or_blank_clipboard_is_an_actionable_failure() {
    for content in [None, Some(ClipboardContent::Text("   ".into())), Some(ClipboardContent::Files(vec![]))] {
        let world = TestWorld::new();
        *world.clipboard.content.lock().unwrap() = content;
        let Ran { report, .. } = shot(&world, &wf(InputKind::Clipboard, vec![C::Upload], vec![]));
        assert_eq!(report.outcome, Outcome::Failed);
        assert_eq!(run_step(&report, StepKind::ReadClipboard).status.is_failure(), true);
    }
}

// ---- post_file -------------------------------------------------------------------------

#[test]
fn single_file_upload() {
    let world = TestWorld::new();
    world.fs.add_file("/home/u/report final.pdf", vec![1u8; 1234]);
    let w = wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl, U::ShowNotification]);
    let Ran { report, .. } = files(&world, &w, &["/home/u/report final.pdf"]);
    assert_eq!(report.outcome, Outcome::Success);
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].kind, DestinationType::File);
    assert_eq!(up[0].mime, "application/pdf");
    assert_eq!(up[0].path.as_deref(), Some(Path::new("/home/u/report final.pdf")));
    assert_eq!(up[0].file_name, "report final.pdf");
    assert_eq!(report.items[0].kind, EntryKind::File);
    assert_eq!(report.items[0].input_path.as_deref(), Some(Path::new("/home/u/report final.pdf")));
    assert!(!report.items[0].created_by_workflow);
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e[0].size_bytes, Some(1234));
    assert_eq!(e[0].sha256.as_deref(), Some(crate::history::sha256_hex(&vec![1u8; 1234]).as_str()));
    assert_eq!(e[0].local_path.as_deref(), Some(Path::new("/home/u/report final.pdf")));
}

#[test]
fn multi_file_urls_are_joined_in_input_order() {
    let world = TestWorld::new();
    let names = ["c.txt", "a.txt", "b.txt", "e.txt", "d.txt"];
    for n in names {
        world.fs.add_file(format!("/in/{n}"), n.as_bytes());
    }
    // make later files finish first: reverse-proportional delay is not available, so just add jitter
    *world.uploaders.delay.lock().unwrap() = Duration::from_millis(20);
    let paths: Vec<String> = names.iter().map(|n| format!("/in/{n}")).collect();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let w = wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl, U::ShowNotification]);
    let Ran { report, .. } = files(&world, &w, &refs);
    assert_eq!(report.outcome, Outcome::Success);
    let expected: Vec<String> = names.iter().map(|n| url_of("test", n)).collect();
    assert_eq!(world.clipboard.text.lock().unwrap().clone().unwrap(), expected.join("\n"));
    assert_eq!(report.urls(), expected.iter().map(String::as_str).collect::<Vec<_>>());
    for (i, n) in names.iter().enumerate() {
        assert_eq!(report.items[i].index, i);
        assert_eq!(report.items[i].input_path.as_deref(), Some(Path::new(&format!("/in/{n}"))));
    }
    assert_eq!(world.log.with_prefix("clipboard.text").len(), 1, "one clipboard write for the batch");
    let notes = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(notes.len(), 1, "one summary notification");
    assert_eq!(notes[0].title, "5 uploads complete");
    assert_eq!(world.history.count(&Query::default()).unwrap(), 5);
}

#[test]
fn one_failure_does_not_stop_the_others() {
    let world = TestWorld::new();
    for n in ["a", "b", "c", "d"] {
        world.fs.add_file(format!("/in/{n}.bin"), n.as_bytes());
    }
    world.uploaders.fail_file("b.bin", Fail::msg("413 Payload Too Large"));
    let w = wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl, U::ShowNotification]);
    let Ran { report, .. } = files(&world, &w, &["/in/a.bin", "/in/b.bin", "/in/c.bin", "/in/d.bin"]);
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    let outcomes: Vec<Outcome> = report.items.iter().map(|i| i.outcome).collect();
    assert_eq!(outcomes, vec![Outcome::Success, Outcome::Failed, Outcome::Success, Outcome::Success]);
    assert!(failure(step_of(&report, 1, StepKind::Upload)).message.contains("413"));
    assert_eq!(
        world.clipboard.text.lock().unwrap().clone().unwrap(),
        [url_of("test", "a.bin"), url_of("test", "c.bin"), url_of("test", "d.bin")].join("\n"),
        "clipboard gets only the successes, in order"
    );
    assert!(world.fs.contents("/in/b.bin").is_some(), "the failed file is untouched");
    let n = world.notifier.shown.lock().unwrap().clone();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].level, NotificationLevel::Warning);
    assert!(n[0].body.contains("b.bin") && n[0].body.contains("413"), "{}", n[0].body);
    assert_eq!(report.summary(), "3 of 4 uploaded; upload: 413 Payload Too Large");
    // history: all four have entries (the failed one has no URL)
    let entries = world.history.list(&Query::default()).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries.iter().filter(|e| e.upload_url.is_some()).count(), 3);
}

#[test]
fn all_files_failing_is_a_failure() {
    let world = TestWorld::new();
    world.fs.add_file("/in/a", b"a");
    world.fs.add_file("/in/b", b"b");
    world.uploaders.fail_all(Fail::msg("offline"));
    let w = wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl]);
    let Ran { report, .. } = files(&world, &w, &["/in/a", "/in/b"]);
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(skip_reason(run_step(&report, StepKind::CopyUrl)), &SkipReason::NoUrl);
}

#[test]
fn missing_files_fail_individually() {
    let world = TestWorld::new();
    world.fs.add_file("/in/ok.txt", b"x");
    let w = wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl]);
    let Ran { report, .. } = files(&world, &w, &["/in/gone.txt", "/in/ok.txt"]);
    let f = failure(step_of(&report, 0, StepKind::LoadFile));
    assert_eq!(f.kind, FailureKind::Io);
    assert!(f.message.contains("gone.txt") && f.message.contains("does not exist"));
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::Upload)), &SkipReason::ItemFailed);
    assert_eq!(report.items[1].outcome, Outcome::Success);
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    assert_eq!(world.uploaders.uploads.lock().unwrap().len(), 1);
}

#[test]
fn no_files_is_an_error() {
    let world = TestWorld::new();
    let Ran { report, .. } = files(&world, &wf(InputKind::Files, vec![C::Upload], vec![]), &[]);
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(failure(run_step(&report, StepKind::LoadFile)).message.contains("no files"));
}

#[test]
fn duplicate_paths_are_uploaded_twice() {
    let world = TestWorld::new();
    world.fs.add_file("/in/a.txt", b"a");
    let Ran { report, .. } = files(&world, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/a.txt", "/in/a.txt"]);
    assert_eq!(report.outcome, Outcome::Success);
    assert_eq!(world.uploaders.uploads.lock().unwrap().len(), 2);
}

#[test]
fn parallelism_is_bounded_by_the_setting() {
    for (limit, files_n, expect_max) in [(1u32, 4usize, 1usize), (2, 6, 2), (3, 3, 3)] {
        let world = TestWorld::new();
        for i in 0..files_n {
            world.fs.add_file(format!("/in/{i}.bin"), vec![0u8; 4]);
        }
        *world.uploaders.delay.lock().unwrap() = Duration::from_millis(60);
        let mut s = world.settings();
        s.post_file.max_parallel_uploads = limit;
        let paths: Vec<String> = (0..files_n).map(|i| format!("/in/{i}.bin")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let Ran { report, .. } = files_with(&world, s, &wf(InputKind::Files, vec![C::Upload], vec![]), &refs);
        assert_eq!(report.outcome, Outcome::Success);
        let max = world.uploaders.max_concurrent.load(Ordering::SeqCst);
        assert!(max <= limit as usize, "limit {limit}: saw {max} concurrent uploads");
        assert_eq!(max, expect_max, "limit {limit}: expected the pool to be used fully");
    }
}

#[test]
fn folders_are_zipped_uploaded_and_cleaned_up() {
    let world = TestWorld::new();
    world.fs.add_dir("/in/My Folder");
    world.fs.add_file("/in/My Folder/a.txt", b"a");
    let w = wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl]);
    let Ran { report, .. } = files(&world, &w, &["/in/My Folder"]);
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    assert_eq!(kinds(&report.items[0].steps)[..3], [StepKind::LoadFile, StepKind::Zip, StepKind::Upload]);
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].file_name, "My Folder.zip");
    assert_eq!(up[0].mime, "application/zip");
    assert_eq!(up[0].path.as_deref(), Some(Path::new("/tmp/ssx-zip/My Folder.zip")));
    assert!(world.fs.contents("/tmp/ssx-zip/My Folder.zip").is_none(), "temporary archive removed");
    assert!(world.fs.contents("/in/My Folder/a.txt").is_some());
    assert!(report.items[0].local_path.is_none());
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e[0].local_path.as_deref(), Some(Path::new("/in/My Folder")), "history points at the folder, not the deleted zip");
}

#[test]
fn zip_archive_is_removed_even_when_the_upload_fails() {
    let world = TestWorld::new();
    world.fs.add_dir("/in/d");
    world.uploaders.fail_all(Fail::msg("offline"));
    let Ran { report, .. } = files(&world, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/d"]);
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(world.fs.files().is_empty(), "no litter: {:?}", world.fs.files());
}

#[test]
fn folder_policy_error_and_zipper_failure() {
    let world = TestWorld::new();
    world.fs.add_dir("/in/d");
    let mut s = world.settings();
    s.post_file.folders = crate::settings::FolderPolicy::Error;
    let Ran { report, .. } = files_with(&world, s, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/d"]);
    let f = failure(step_of(&report, 0, StepKind::LoadFile));
    assert!(f.message.contains("is a folder") && f.message.contains("\"zip\""), "{}", f.message);
    assert!(world.uploaders.uploads.lock().unwrap().is_empty());

    let world = TestWorld::new();
    world.fs.add_dir("/in/d");
    *world.zipper.fail.lock().unwrap() = Some(Fail::msg("zip: permission denied"));
    let Ran { report, .. } = files(&world, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/d"]);
    assert!(failure(step_of(&report, 0, StepKind::Zip)).message.contains("permission denied"));
    assert_eq!(report.outcome, Outcome::Failed);
}

#[test]
fn zip_of_a_folder_can_be_deleted_via_the_step_too() {
    let world = TestWorld::new();
    world.fs.add_dir("/in/d");
    let w = wf(InputKind::Files, vec![C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = files(&world, &w, &["/in/d"]);
    assert!(step_of(&report, 0, StepKind::DeleteLocalFile).status.is_success());
    assert_eq!(world.log.with_prefix("fs.remove").len(), 1, "archive removed once, not twice");
    assert_eq!(report.outcome, Outcome::Success);
}

#[test]
fn image_files_go_through_the_editor_only_when_enabled() {
    let world = TestWorld::new();
    world.fs.add_file("/in/photo.png", png_bytes(30, 20));
    let w = wf(InputKind::Files, vec![C::OpenEditor, C::Upload], vec![]);
    // default: off
    let Ran { report, .. } = files(&world, &w, &["/in/photo.png"]);
    assert!(matches!(skip_reason(step_of(&report, 0, StepKind::OpenEditor)), SkipReason::NotApplicable(m) if m.contains("images_through_editor")));
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].kind, DestinationType::File);
    assert_eq!(up[0].path.as_deref(), Some(Path::new("/in/photo.png")));

    let world = TestWorld::new();
    world.fs.add_file("/in/photo.png", png_bytes(30, 20));
    let mut s = world.settings();
    s.post_file.images_through_editor = true;
    let Ran { report, .. } = files_with(&world, s, &w, &["/in/photo.png"]);
    assert_eq!(report.outcome, Outcome::Success);
    assert!(world.log.lines().iter().any(|l| l == "editor.edit:30x20"));
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].kind, DestinationType::Image, "edited images use the image destination");
    assert_eq!(up[0].file_name, "photo.png");
    assert!(up[0].bytes.is_some() && up[0].path.is_none(), "edited bytes, not the original file");
    assert_eq!(world.fs.contents("/in/photo.png").unwrap(), png_bytes(30, 20), "original untouched");
}

#[test]
fn editor_cancel_on_one_file_skips_only_that_file() {
    let world = TestWorld::new();
    world.fs.add_file("/in/a.png", png_bytes(8, 8));
    world.fs.add_file("/in/b.txt", b"b");
    *world.editor.mode.lock().unwrap() = EditorMode::Cancel;
    let mut s = world.settings();
    s.post_file.images_through_editor = true;
    let w = wf(InputKind::Files, vec![C::OpenEditor, C::Upload], vec![U::CopyUrl]);
    let Ran { report, .. } = files_with(&world, s, &w, &["/in/a.png", "/in/b.txt"]);
    assert_eq!(report.items[0].outcome, Outcome::Cancelled);
    assert_eq!(report.items[1].outcome, Outcome::Success);
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    assert_eq!(world.clipboard.text.lock().unwrap().as_deref(), Some(url_of("test", "b.txt").as_str()));
}

#[test]
fn non_image_files_skip_image_steps() {
    let world = TestWorld::new();
    world.fs.add_file("/in/doc.txt", b"t");
    let mut s = world.settings();
    s.post_file.images_through_editor = true;
    let w = wf(InputKind::Files, vec![C::OpenEditor, C::CopyImageToClipboard, C::PinToScreen, C::Ocr, C::SaveToFile, C::Upload], vec![]);
    let Ran { report, .. } = files_with(&world, s, &w, &["/in/doc.txt"]);
    for k in [StepKind::OpenEditor, StepKind::PinToScreen, StepKind::Ocr] {
        assert!(matches!(step_of(&report, 0, k).status, StepStatus::Skipped(_)), "{k}");
    }
    assert_eq!(skip_reason(step_of(&report, 0, StepKind::CopyImage)), &SkipReason::NoImage);
    assert!(matches!(skip_reason(step_of(&report, 0, StepKind::SaveToFile)), SkipReason::NotApplicable(m) if m.contains("already on disk")));
    assert_eq!(report.outcome, Outcome::Success);
}

#[test]
fn single_image_file_can_be_copied_pinned_and_recognised() {
    let world = TestWorld::new();
    world.fs.add_file("/in/p.png", png_bytes(12, 9));
    let w = wf(InputKind::Files, vec![C::CopyImageToClipboard, C::PinToScreen, C::Ocr, C::Upload], vec![]);
    let Ran { report, .. } = files(&world, &w, &["/in/p.png"]);
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    assert!(world.log.lines().contains(&"clipboard.image:12x9".to_owned()));
    assert!(world.log.lines().contains(&"pin:12x9".to_owned()));
    // history got a decoded thumbnail of the image file
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e[0].kind, EntryKind::Image);
    assert!(e[0].thumbnail.is_some());
}

#[test]
fn corrupt_image_files_fail_the_image_step_only() {
    let world = TestWorld::new();
    world.fs.add_file("/in/broken.png", b"not really a png");
    let w = wf(InputKind::Files, vec![C::CopyImageToClipboard, C::Upload], vec![]);
    let Ran { report, .. } = files(&world, &w, &["/in/broken.png"]);
    let f = failure(step_of(&report, 0, StepKind::CopyImage));
    assert!(f.message.contains("not a readable image"), "{}", f.message);
    assert!(step_of(&report, 0, StepKind::Upload).status.is_success(), "optional step failure does not block the upload");
    assert_eq!(report.outcome, Outcome::Success);
}

#[test]
fn batch_skips_ambiguous_image_and_qr_steps_but_runs_per_item_steps() {
    let world = TestWorld::new();
    world.fs.add_file("/in/a.png", png_bytes(5, 5));
    world.fs.add_file("/in/b.png", png_bytes(5, 5));
    let w = wf(
        InputKind::Files,
        vec![C::CopyImageToClipboard, C::PinToScreen, C::Ocr, C::Upload],
        vec![U::ShowQrCode, U::OpenUrl, U::RunCommand { program: "hook".into(), args: vec!["{file_name}".into(), "{url}".into()] }],
    );
    let Ran { report, .. } = files(&world, &w, &["/in/a.png", "/in/b.png"]);
    for i in 0..2 {
        for k in [StepKind::CopyImage, StepKind::PinToScreen, StepKind::Ocr] {
            assert!(matches!(skip_reason(step_of(&report, i, k)), SkipReason::NotApplicable(m) if m.contains("multiple")), "{i} {k}");
        }
    }
    assert!(matches!(skip_reason(run_step(&report, StepKind::ShowQrCode)), SkipReason::NotApplicable(_)));
    assert_eq!(world.log.with_prefix("open:").len(), 2);
    assert_eq!(world.commands.runs.lock().unwrap().len(), 2);
    assert!(world.log.with_prefix("clipboard.image").is_empty() && world.log.with_prefix("pin:").is_empty());
}

#[test]
fn video_files_use_the_video_destination_and_fall_back_to_file() {
    let world = TestWorld::new();
    world.fs.add_file("/in/clip.mp4", b"v");
    let mut s = world.settings();
    s.destinations.video = Some("yt".into());
    files_with(&world, s, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/clip.mp4"]);
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!((up[0].destination.as_str(), up[0].kind), ("yt", DestinationType::Video));

    let world = TestWorld::new();
    world.fs.add_file("/in/clip.mkv", b"v");
    let mut s = world.settings();
    s.destinations.video = None;
    s.destinations.file = Some("files".into());
    let Ran { report, .. } = files_with(&world, s, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/clip.mkv"]);
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!(up[0].destination, "files", "video falls back to the file destination");
    assert_eq!(report.items[0].kind, EntryKind::Video);
}

#[test]
fn extension_overrides_apply_to_files() {
    let world = TestWorld::new();
    world.fs.add_file("/in/a.zip", b"z");
    world.fs.add_file("/in/b.txt", b"t");
    let mut s = world.settings();
    s.destinations.extension_overrides.insert("zip".into(), "archive-host".into());
    files_with(&world, s, &wf(InputKind::Files, vec![C::Upload], vec![]), &["/in/a.zip", "/in/b.txt"]);
    let mut up = world.uploaders.uploads.lock().unwrap().clone();
    up.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    assert_eq!(up[0].destination, "archive-host");
    assert_eq!(up[1].destination, "test");
}

#[test]
fn unicode_and_awkward_file_names_survive() {
    let world = TestWorld::new();
    let name = "/in/日本語 ファイル 🎉 (1).png";
    world.fs.add_file(name, png_bytes(4, 4));
    let Ran { report, .. } = files(&world, &wf(InputKind::Files, vec![C::Upload], vec![U::CopyUrl]), &[name]);
    assert_eq!(report.outcome, Outcome::Success);
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].file_name, "日本語 ファイル 🎉 (1).png");
}

#[test]
fn auto_input_mismatches_are_explained() {
    let world = TestWorld::new();
    let Ran { report, .. } = shot(&world, &wf(InputKind::Files, vec![C::Upload], vec![]));
    assert!(failure(run_step(&report, StepKind::LoadFile)).message.contains("post_file"));
    let Ran { report, .. } = shot(&world, &wf(InputKind::RecordScreen, vec![C::Upload], vec![]));
    assert!(failure(run_step(&report, StepKind::Record)).message.contains("post_video"));
    assert!(world.log.lines().is_empty());
}

// ---- post_video ------------------------------------------------------------------------

fn video(world: &TestWorld, settings: Settings, w: &Workflow, stop_after: Duration) -> Ran {
    let engine = world.engine(settings);
    let stop = CancelToken::new();
    let s2 = stop.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(stop_after);
        s2.cancel();
    });
    let sink = CollectingSink::new();
    let report = engine.post_video(w, VideoSource::Record { stop }, &world.services(), &sink, &CancelToken::new());
    h.join().unwrap();
    let events = sink.events();
    check_event_invariants(&events, &report);
    Ran { report, events }
}

fn video_path() -> PathBuf {
    Path::new("/shots").join("Recordings").join("2024-03").join("Screenshot_2024-03-09_14-05-06.mp4")
}

#[test]
fn record_stop_upload_via_the_video_destination() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.destinations.video = Some("vid".into());
    let w = wf(InputKind::RecordScreen, vec![C::Upload, C::DeleteLocalFile], vec![U::CopyUrl, U::ShowNotification]);
    let Ran { report, .. } = video(&world, s, &w, Duration::from_millis(50));
    assert_eq!(report.outcome, Outcome::Success, "{}", report.summary());
    let req = world.recorder.requests.lock().unwrap()[0].clone();
    assert_eq!(req.output_dir, video_path().parent().unwrap());
    assert_eq!(req.file_stem, "Screenshot_2024-03-09_14-05-06");
    assert_eq!(req.kind, RecordKind::Video);
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!((up[0].destination.as_str(), up[0].kind, up[0].mime.as_str()), ("vid", DestinationType::Video, "video/mp4"));
    assert_eq!(up[0].path.as_deref(), Some(video_path().as_path()));
    // recorded by this workflow, so deleting after upload is allowed
    assert!(world.fs.contents(video_path()).is_none());
    let lines = world.log.lines();
    let pos = |p: &str| lines.iter().position(|l| l.starts_with(p)).unwrap();
    assert!(pos("record.start") < pos("record.stop") && pos("record.stop") < pos("upload") && pos("upload") < pos("fs.remove"));
    let e = world.history.list(&Query::default()).unwrap();
    assert_eq!(e[0].kind, EntryKind::Video);
    assert!(e[0].local_path.is_none());
    assert_eq!((e[0].width, e[0].height), (Some(640), Some(360)));
    assert_eq!(report.items[0].kind, EntryKind::Video);
}

#[test]
fn video_falls_back_to_the_file_uploader_and_is_kept_when_upload_fails() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.destinations.video = None;
    s.destinations.file = Some("filehost".into());
    world.uploaders.fail_all(Fail::Retryable("timeout".into()));
    let w = wf(InputKind::RecordScreen, vec![C::Upload, C::DeleteLocalFile], vec![]);
    let Ran { report, .. } = video(&world, s, &w, Duration::from_millis(20));
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].destination, "filehost");
    assert!(world.fs.contents(video_path()).is_some(), "the recording is never lost to a failed upload");
    assert_eq!(report.outcome, Outcome::PartialSuccess);
    assert_eq!(report.items[0].local_path.as_deref(), Some(video_path().as_path()));
    assert_eq!(world.history.count(&Query::default()).unwrap(), 1);
}

#[test]
fn gif_recordings_use_the_image_destination() {
    let world = TestWorld::new();
    let mut s = world.settings();
    s.destinations.image = Some("img".into());
    let w = wf(InputKind::RecordGif, vec![C::Upload], vec![]);
    let Ran { report, .. } = video(&world, s, &w, Duration::from_millis(20));
    assert_eq!(report.outcome, Outcome::Success);
    let up = world.uploaders.uploads.lock().unwrap().clone();
    assert_eq!((up[0].destination.as_str(), up[0].kind, up[0].mime.as_str()), ("img", DestinationType::Image, "image/gif"));
    assert_eq!(world.recorder.requests.lock().unwrap()[0].kind, RecordKind::Gif);
}

#[test]
fn recorder_failures_are_reported() {
    let world = TestWorld::new();
    *world.recorder.fail_start.lock().unwrap() = Some(Fail::Unsupported("screen recording on Wayland without portal".into()));
    let w = wf(InputKind::RecordScreen, vec![C::Upload], vec![U::ShowNotification]);
    let Ran { report, .. } = video(&world, world.settings(), &w, Duration::from_millis(10));
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(failure(run_step(&report, StepKind::Record)).kind, FailureKind::Unsupported);
    assert_eq!(world.notifier.shown.lock().unwrap().len(), 1);

    let world = TestWorld::new();
    *world.recorder.fail_stop.lock().unwrap() = Some(Fail::msg("encoder crashed"));
    let Ran { report, .. } = video(&world, world.settings(), &w, Duration::from_millis(10));
    assert!(failure(run_step(&report, StepKind::Record)).message.contains("encoder crashed"));
    assert!(world.uploaders.uploads.lock().unwrap().is_empty());

    let world = TestWorld::new();
    *world.recorder.fail_start.lock().unwrap() = Some(Fail::Cancelled);
    let Ran { report, .. } = video(&world, world.settings(), &w, Duration::from_millis(10));
    assert_eq!(report.outcome, Outcome::Cancelled, "region selection cancelled");
    assert!(world.notifier.shown.lock().unwrap().is_empty());
}

#[test]
fn cancelling_the_run_aborts_the_recording_and_discards_it() {
    let world = TestWorld::new();
    let engine = world.engine(world.settings());
    let stop = CancelToken::new();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let h = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        c2.cancel();
    });
    let w = wf(InputKind::RecordScreen, vec![C::Upload], vec![U::CopyUrl]);
    let report = engine.post_video(&w, VideoSource::Record { stop: stop.clone() }, &world.services(), &NullSink, &cancel);
    h.join().unwrap();
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert!(world.log.lines().contains(&"record.abort".to_owned()));
    assert!(!world.log.lines().contains(&"record.stop".to_owned()));
    assert!(world.uploaders.uploads.lock().unwrap().is_empty());
    assert!(world.fs.files().is_empty());
}

#[test]
fn an_existing_recording_can_be_posted() {
    let world = TestWorld::new();
    world.fs.add_file("/rec/last.mp4", b"video");
    let engine = world.engine(world.settings());
    let w = wf(InputKind::RecordScreen, vec![C::Upload, C::DeleteLocalFile], vec![U::CopyUrl]);
    let report = engine.post_video(&w, VideoSource::Recorded("/rec/last.mp4".into()), &world.services(), &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Success);
    assert_eq!(world.uploaders.uploads.lock().unwrap()[0].kind, DestinationType::Video);
    assert!(world.fs.contents("/rec/last.mp4").is_none(), "a just-recorded video is ours to delete");

    let report = engine.post_video(&w, VideoSource::Recorded("/rec/missing.mp4".into()), &world.services(), &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Failed);
    assert!(failure(step_of(&report, 0, StepKind::LoadFile)).message.contains("does not exist"));
}

// ---- misc ------------------------------------------------------------------------------

#[test]
fn unsupported_defaults_fail_gracefully() {
    let engine = TestWorld::new().engine(TestWorld::new().settings());
    let svc = Services::with_defaults();
    let report = engine.post_screenshot(&standard(), &svc, &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Failed);
    let f = failure(run_step(&report, StepKind::Capture));
    assert_eq!(f.kind, FailureKind::Unsupported);
    assert!(f.message.contains("screen capture"));
}

#[test]
fn post_image_skips_capture() {
    let world = TestWorld::new();
    let engine = world.engine(world.settings());
    let mut c = Captured::new(test_frame(20, 10));
    c.window_title = Some("Given".into());
    let report = engine.post_image(&standard(), c, &world.services(), &NullSink, &CancelToken::new());
    assert_eq!(report.outcome, Outcome::Success);
    assert!(world.log.with_prefix("capture").is_empty());
    assert!(report.steps.iter().all(|s| s.kind != StepKind::Capture));
}

#[test]
fn report_helpers() {
    let world = TestWorld::new();
    let Ran { report, .. } = shot(&world, &standard());
    assert_eq!(report.summary(), "Done");
    assert_eq!(report.all_steps().count(), report.steps.len() + report.items[0].steps.len());
    assert!(report.items[0].first_failure().is_none());
    assert_eq!(StepKind::CopyImage.name(), "copy_image_to_clipboard");
    assert_eq!(StepKind::Upload.importance(), Importance::Normal);
    assert_eq!(StepKind::Capture.importance(), Importance::Critical);
    assert_eq!(StepKind::CopyUrl.importance(), Importance::Optional);
}

#[test]
fn engine_is_shareable_across_threads() {
    fn check<T: Send + Sync>() {}
    check::<Engine>();
    check::<RunReport>();
    let world = TestWorld::new();
    let engine = std::sync::Arc::new(world.engine(world.settings()));
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let r = engine.post_screenshot(&wf(InputKind::CaptureRegion, vec![C::Upload], vec![]), &world.services(), &NullSink, &CancelToken::new());
                assert_eq!(r.outcome, Outcome::Success);
            });
        }
    });
    assert_eq!(world.uploaders.uploads.lock().unwrap().len(), 4);
}

// ---- property-style invariants ---------------------------------------------------------

mod property {
    use proptest::prelude::*;

    use super::*;

    fn ac() -> impl Strategy<Value = C> {
        prop_oneof![
            Just(C::OpenEditor),
            Just(C::CopyImageToClipboard),
            Just(C::SaveToFile),
            Just(C::SaveAsDialog),
            Just(C::PinToScreen),
            Just(C::Ocr),
            Just(C::Upload),
            Just(C::DeleteLocalFile),
        ]
    }

    fn au() -> impl Strategy<Value = U> {
        prop_oneof![
            Just(U::CopyUrl),
            Just(U::CopyShortUrl),
            Just(U::OpenUrl),
            Just(U::ShortenUrl),
            Just(U::ShowQrCode),
            Just(U::ShowNotification),
            Just(U::RunCommand { program: "p".into(), args: vec!["{url}".into()] }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// For any step list and any pattern of failing services: every listed step yields
        /// exactly one report in order, events are well formed, files are only deleted after a
        /// confirmed upload and never the user's originals, and the outcome is coherent.
        #[test]
        fn engine_invariants_hold(
            after_capture in proptest::collection::vec(ac(), 0..8),
            after_upload in proptest::collection::vec(au(), 0..6),
            upload_fails in any::<bool>(),
            save_fails in any::<bool>(),
            clipboard_fails in any::<bool>(),
            editor in 0u8..3,
            dialog_choice in any::<bool>(),
            files_input in any::<bool>(),
        ) {
            let world = TestWorld::new();
            world.fs.add_file("/in/user.png", png_bytes(8, 8));
            if upload_fails { world.uploaders.fail_all(Fail::msg("upload failed")); }
            if save_fails { world.fs.fail_writes(Some("disk full")); }
            if clipboard_fails { *world.clipboard.fail.lock().unwrap() = Some(Fail::msg("busy")); }
            *world.editor.mode.lock().unwrap() = match editor { 0 => EditorMode::Edit, 1 => EditorMode::Cancel, _ => EditorMode::Fail(Fail::msg("editor broke")) };
            if dialog_choice { *world.save_dialog.choice.lock().unwrap() = Some(PathBuf::from("/chosen/x.png")); }
            let mut settings = world.settings();
            settings.post_file.images_through_editor = true;
            let w = wf(if files_input { InputKind::Files } else { InputKind::CaptureRegion }, after_capture.clone(), after_upload.clone());
            let engine = world.engine(settings);
            let sink = CollectingSink::new();
            let cancel = CancelToken::new();
            let report = if files_input {
                engine.post_file(&w, vec![PathBuf::from("/in/user.png")], &world.services(), &sink, &cancel)
            } else {
                engine.post_screenshot(&w, &world.services(), &sink, &cancel)
            };
            check_event_invariants(&sink.events(), &report);

            // 1. one report per listed after_capture step, in order, at the start of the item
            prop_assert_eq!(report.items.len(), 1);
            let item = &report.items[0];
            let item_kinds = kinds(&item.steps);
            let listed: Vec<StepKind> = after_capture.iter().map(|a| match a {
                C::OpenEditor => StepKind::OpenEditor, C::CopyImageToClipboard => StepKind::CopyImage,
                C::SaveToFile => StepKind::SaveToFile, C::SaveAsDialog => StepKind::SaveAsDialog,
                C::PinToScreen => StepKind::PinToScreen, C::Ocr => StepKind::Ocr,
                C::Upload => StepKind::Upload, C::DeleteLocalFile => StepKind::DeleteLocalFile,
            }).collect();
            let non_prep: Vec<StepKind> = item_kinds.iter().copied().filter(|k| !matches!(k, StepKind::LoadFile | StepKind::Zip)).collect();
            prop_assert!(non_prep.len() >= listed.len());
            prop_assert_eq!(&non_prep[..listed.len()], &listed[..]);

            // 2. deletion only after a confirmed upload, only of workflow-created files
            let lines = world.log.lines();
            let removed = world.log.with_prefix("fs.remove:");
            if !removed.is_empty() {
                prop_assert!(item.url.is_some(), "deleted without a confirmed upload");
                let up = lines.iter().position(|l| l.starts_with("upload:")).expect("upload happened");
                let rm = lines.iter().position(|l| l.starts_with("fs.remove:")).unwrap();
                prop_assert!(up < rm);
            }
            prop_assert!(world.fs.contents("/in/user.png").is_some(), "user file deleted!");
            if upload_fails && !after_capture.contains(&C::Upload) {
                prop_assert!(item.url.is_none());
            }
            if upload_fails {
                // whatever was saved is still there
                if let Some(p) = &item.local_path {
                    prop_assert!(world.fs.contents(p).is_some());
                }
            }

            // 3. outcome coherence
            match report.outcome {
                Outcome::Success => {
                    prop_assert!(report.errors().is_empty(), "success with errors: {:?}", report.errors());
                    prop_assert!(report.items.iter().all(|i| i.outcome == Outcome::Success));
                }
                Outcome::Cancelled => prop_assert!(editor == 1 || report.items.iter().all(|i| i.outcome == Outcome::Cancelled)),
                Outcome::Failed | Outcome::PartialSuccess => prop_assert!(!report.errors().is_empty() || report.items.iter().any(|i| i.outcome != Outcome::Success)),
            }
            if editor == 1 && after_capture.contains(&C::OpenEditor) && world.log.with_prefix("editor.edit").len() == 1 {
                prop_assert_eq!(report.outcome, Outcome::Cancelled);
                prop_assert!(world.uploaders.uploads.lock().unwrap().is_empty() || after_capture.iter().position(|a| *a == C::Upload) < after_capture.iter().position(|a| *a == C::OpenEditor));
            }
            // 4. history never points at a file that the run deleted
            for e in world.history.list(&Query::default()).unwrap() {
                if let Some(p) = e.local_path {
                    prop_assert!(world.fs.contents(&p).is_some(), "history references deleted {p:?}");
                }
            }
        }
    }
}
