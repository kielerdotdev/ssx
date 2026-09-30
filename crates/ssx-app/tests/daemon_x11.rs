//! The daemon end to end under a private Xvfb: the real `ssx-app`, the real `ssx` CLI, the real
//! `ssx-overlay` helper driven with `xdotool`, a local mock upload server and an uploader
//! configured through a `.sxcu` file.
//!
//! What is *not* covered here (no desktop environment in this sandbox): a real notification
//! daemon, a real tray host and compositor hotkey portals. The D-Bus tests in `tray_dbus.rs`
//! use mocks for those, and the README lists the manual checks.
#![cfg(unix)]

mod common;

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use common::{
    Daemon, Fixture, TestEnv, WORKFLOWS, Xdo, Xvfb, expect_error, expected_scene, finished,
    fixture, fixture_sized, overlay_bin, skip_unless, wait_until,
};
use ssx_core::ipc::{
    CaptureKind, ErrorCode, PostAction, RecordSpec, RecordTarget, RegionMode, Request, Response,
    ShowTarget,
};
use ssx_core::workflow::Outcome;
use ssx_types::Frame;

fn read_image(path: &Path) -> Frame {
    Frame::decode(&std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .expect("decodes")
}

fn rgb_at(f: &Frame, x: u32, y: u32) -> [u8; 3] {
    let p = &f.row(y)[x as usize * 4..x as usize * 4 + 3];
    [p[0], p[1], p[2]]
}

/// The first pixel where `f` differs from the painted scene's crop at `(ox, oy)`.
fn scene_diff(f: &Frame, ox: i32, oy: i32) -> Option<String> {
    let want = expected_scene(f.width(), f.height(), ox, oy);
    for y in 0..f.height() {
        for x in 0..f.width() {
            let (got, w) = (rgb_at(f, x, y), want[(y * f.width() + x) as usize]);
            if got != w {
                return Some(format!("pixel ({x},{y}): got {got:?}, expected {w:?}"));
            }
        }
    }
    None
}

fn clipboard(display: &str) -> Option<String> {
    let out = Command::new("xclip")
        .env("DISPLAY", display)
        .args(["-selection", "clipboard", "-o"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_by_name(env: &TestEnv, name: &str, wait: bool) -> Response {
    env.request(&Request::RunWorkflow { id: None, name: Some(name.into()), wait })
}

#[test]
fn ping_status_quit_and_a_second_launch_forwards() {
    let Some(f) = fixture() else { return };
    let mut d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    assert!(matches!(f.env.request(&Request::Ping), Response::Pong { .. }));
    let Response::Status(st) = f.env.request(&Request::Status) else { panic!("status") };
    assert_eq!(st.pid, d.pid());
    assert!(!st.tray, "--no-tray");
    assert_eq!(st.hotkey_backend, "none", "--no-hotkeys");
    assert!(st.config_dir.ends_with("cfg"), "{}", st.config_dir);
    assert!(st.settings_problem.is_none());

    // A second launch forwards a request and exits 0 without starting a second daemon.
    let out = f.env.command(&common::app_bin()).args(["--no-tray"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
    assert!(d.running(), "the first instance is undisturbed");

    assert_eq!(f.env.request(&Request::Quit), Response::Ok);
    let status = d.wait_exit(Duration::from_secs(20)).expect("the daemon exits after Quit");
    assert!(status.success(), "{status}");
    assert!(!f.env.run.join("ssx/ssx.sock").exists(), "the socket is removed");
    // And the lock is free again: a new daemon can start.
    let mut again = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    assert_eq!(f.env.request(&Request::Quit), Response::Ok);
    assert!(again.wait_exit(Duration::from_secs(20)).is_some());
    let log = f.env.daemon_log();
    assert!(
        log.contains("starting") && log.contains("quit requested") && log.contains("bye"),
        "{log}"
    );
}

#[test]
fn fullscreen_upload_history_and_clipboard_end_to_end() {
    if skip_unless(&["xclip"]) {
        return;
    }
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);

    let started = Instant::now();
    let summary = finished(run_by_name(&f.env, "shot", true));
    let elapsed = started.elapsed();
    eprintln!("trigger -> upload finished (fullscreen, mock server): {elapsed:?}");
    assert_eq!(summary.outcome, Outcome::Success, "{}", summary.message);
    assert_eq!(summary.items.len(), 1);
    assert_eq!(summary.items[0].url.as_deref(), Some("http://mock.test/u1"));
    let path = summary.items[0].path.clone().expect("the saved file");
    assert!(path.starts_with(&f.save), "{}", path.display());
    let img = read_image(&path);
    assert_eq!((img.width(), img.height()), (800, 600));
    assert_eq!(scene_diff(&img, 0, 0), None, "pixel-exact fullscreen capture");

    // The mock saw exactly one upload.
    assert_eq!(f.mock.requests().len(), 1);
    // The URL is on the clipboard (served by the daemon).
    let clip = wait_until(Duration::from_secs(5), || {
        clipboard(&f.x.display).filter(|c| c.contains("mock.test"))
    });
    assert_eq!(clip.as_deref().map(str::trim), Some("http://mock.test/u1"));
    // And in the history, visible to the CLI.
    let h = f.env.ssx(&["history", "list", "--json"]).ok();
    let list: serde_json::Value = serde_json::from_str(&h.stdout).unwrap();
    assert_eq!(list.as_array().map(Vec::len), Some(1), "{}", h.stdout);
    assert_eq!(list[0]["upload_url"], "http://mock.test/u1", "{}", h.stdout);
}

#[test]
fn trigger_to_capture_latency_and_the_cli_hand_off() {
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    // IPC trigger -> capture finished (saved), five runs.
    let mut times: Vec<Duration> = (0..5)
        .map(|_| {
            let t = Instant::now();
            let s = finished(run_by_name(&f.env, "local", true));
            assert_eq!(s.outcome, Outcome::Success, "{}", s.message);
            t.elapsed()
        })
        .collect();
    times.sort();
    eprintln!(
        "IPC trigger -> fullscreen capture saved: median {:?}, best {:?}, worst {:?}",
        times[2], times[0], times[4]
    );
    assert!(times[2] < Duration::from_secs(3), "{times:?}");
    assert_eq!(std::fs::read_dir(&f.save).unwrap().count(), 5);

    // `ssx run` hands the workflow to the daemon (stdout: the saved path).
    let out = f.env.ssx(&["run", "local"]).ok();
    let printed = out.stdout.trim();
    assert!(printed.starts_with(f.save.to_str().unwrap()), "{printed}");
    assert!(Path::new(printed).is_file());
    let log = f.env.daemon_log();
    assert!(!log.contains("panicked"), "{log}");
    // The same run through the CLI without the daemon works too (in-process engine).
    let mut env2 = TestEnv::new().with_x11(&f.x.display);
    env2.set("SSX_CONFIG_DIR", f.env.cfg.display().to_string());
    env2.set("SSX_NO_DAEMON", "1");
    let out = env2.ssx(&["run", "local"]).ok();
    assert!(Path::new(out.stdout.trim()).is_file(), "{}", out.stdout);
}

#[test]
fn a_hotkey_press_runs_its_workflow() {
    if skip_unless(&["xdotool"]) {
        return;
    }
    let Some(f) = fixture_sized(
        "800x600x24",
        &format!(
            "{WORKFLOWS}\n[[workflows]]\nid = \"hk\"\nname = \"Hotkey shot\"\ninput = \"capture_fullscreen\"\n\
             after_capture = [\"save_to_file\"]\n[workflows.trigger]\ncli_name = \"hk\"\nhotkey = \"Ctrl+Shift+F9\"\n"
        ),
    ) else {
        return;
    };
    let _d = Daemon::start(&f.env, &["--no-tray"]);
    // Registration runs on its own thread right after start-up: wait for it.
    let st = wait_until(Duration::from_secs(10), || match f.env.request(&Request::Status) {
        Response::Status(st) if st.hotkeys_registered > 0 => Some(st),
        _ => None,
    })
    .unwrap_or_else(|| panic!("hotkeys never registered\n{}", f.env.daemon_log()));
    assert_eq!(st.hotkey_backend, "global-hotkey", "{st:?}");
    assert_eq!(st.hotkeys_registered, 1, "{st:?}");
    // (Two workflows on one chord is a settings *error*, so the daemon never gets that far;
    // the plan-level reporting of clashes and unsupported chords is unit-tested.)
    assert!(st.hotkey_problems.is_empty(), "{:?}", st.hotkey_problems);
    let before = std::fs::read_dir(&f.save).map_or(0, Iterator::count);
    assert!(f.x.xdotool_key("ctrl+shift+F9"), "xdotool key");
    let saved = wait_until(Duration::from_secs(15), || {
        let n = std::fs::read_dir(&f.save).map_or(0, Iterator::count);
        (n > before).then_some(n)
    });
    assert!(saved.is_some(), "the hotkey ran the workflow\n{}", f.env.daemon_log());
}

/// Waits for the overlay window and returns.
fn wait_overlay(x: &Xvfb) {
    assert!(
        wait_until(Duration::from_secs(20), || {
            x.xdotool(&["search", "--name", "ssx-overlay"]).then_some(())
        })
        .is_some(),
        "the overlay window never appeared"
    );
    std::thread::sleep(Duration::from_millis(250));
}

fn drag(x: &Xvfb, from: (i32, i32), to: (i32, i32)) {
    let (fx, fy, tx, ty) =
        (from.0.to_string(), from.1.to_string(), to.0.to_string(), to.1.to_string());
    let (mx, my) = (from.0.midpoint(to.0).to_string(), from.1.midpoint(to.1).to_string());
    assert!(x.xdotool(&[
        "mousemove",
        &fx,
        &fy,
        "sleep",
        "0.05",
        "mousedown",
        "1",
        "sleep",
        "0.05",
        "mousemove",
        &mx,
        &my,
        "sleep",
        "0.05",
        "mousemove",
        &tx,
        &ty,
        "sleep",
        "0.1",
        "mouseup",
        "1",
        "sleep",
        "0.1"
    ]));
}

fn overlay_env(f: &mut Fixture) {
    f.env.set("SSX_OVERLAY", overlay_bin().display().to_string());
}

#[test]
fn a_region_workflow_with_the_real_overlay_is_pixel_exact_and_exclusive() {
    if skip_unless(&["xdotool"]) {
        return;
    }
    let Some(mut f) = fixture() else { return };
    overlay_env(&mut f);
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);

    // Drag (100,100) -> (300,250), Enter: exactly that rectangle of the painted scene.
    let Response::Accepted { run_id } = run_by_name(&f.env, "region", false) else { panic!() };
    wait_overlay(&f.x);
    // While the overlay is open a second interactive capture is refused, not queued.
    let busy = run_by_name(&f.env, "region", false);
    expect_error(&busy, ErrorCode::Busy);
    // Non-interactive work still runs.
    let s = finished(run_by_name(&f.env, "local", true));
    assert_eq!(s.outcome, Outcome::Success);
    drag(&f.x, (100, 100), (300, 250));
    assert!(f.x.xdotool(&["key", "Return", "sleep", "0.1"]));
    let s = finished(f.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Success, "{}", s.message);
    let img = read_image(s.items[0].path.as_ref().expect("saved"));
    assert_eq!((img.width(), img.height()), (200, 150));
    assert_eq!(scene_diff(&img, 100, 100), None, "the selection is pixel-exact");

    // Esc cancels: the run ends cancelled and nothing is saved for it.
    let saved_before = std::fs::read_dir(&f.save).unwrap().count();
    let Response::Accepted { run_id } = run_by_name(&f.env, "region", false) else { panic!() };
    wait_overlay(&f.x);
    assert!(f.x.xdotool(&["key", "Escape", "sleep", "0.1"]));
    let s = finished(f.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Cancelled, "{}", s.message);
    assert_eq!(std::fs::read_dir(&f.save).unwrap().count(), saved_before);

    // CancelRun (the tray's Cancel) closes the overlay.
    let Response::Accepted { run_id } = run_by_name(&f.env, "region", false) else { panic!() };
    wait_overlay(&f.x);
    assert_eq!(f.env.request(&Request::CancelRun { run_id }), Response::Ok);
    let s = finished(f.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Cancelled);
    assert!(
        wait_until(Duration::from_secs(5), || {
            (!f.x.xdotool(&["search", "--name", "ssx-overlay"])).then_some(())
        })
        .is_some(),
        "the overlay window is gone after a cancel"
    );
    // The overlay is free again.
    let Response::Accepted { run_id } = run_by_name(&f.env, "region", false) else { panic!() };
    wait_overlay(&f.x);
    assert!(f.x.xdotool(&["key", "Escape", "sleep", "0.1"]));
    finished(f.env.request(&Request::WaitRun { run_id }));
}

#[test]
fn window_and_ellipse_modes_and_the_cli_forwarding() {
    if skip_unless(&["xdotool"]) {
        return;
    }
    let Some(mut f) = fixture() else { return };
    overlay_env(&mut f);
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);

    // Window mode: click the blue window (60,30 40x40): its exact rectangle is captured.
    let req = Request::Capture {
        target: CaptureKind::Region,
        workflow: Some("region".into()),
        delay_ms: None,
        wait: false,
        mode: Some(RegionMode::Window),
    };
    let Response::Accepted { run_id } = f.env.request(&req) else { panic!() };
    wait_overlay(&f.x);
    assert!(f.x.xdotool(&["mousemove", "80", "50", "sleep", "0.2", "click", "1", "sleep", "0.2"]));
    let s = finished(f.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Success, "{}", s.message);
    let img = read_image(s.items[0].path.as_ref().unwrap());
    assert_eq!((img.width(), img.height()), (40, 40));
    assert_eq!(scene_diff(&img, 60, 30), None);

    // Ellipse mode: the corners of the bounding box are transparent, the centre is scene.
    let req = Request::Capture {
        target: CaptureKind::Region,
        workflow: Some("region".into()),
        delay_ms: None,
        wait: false,
        mode: Some(RegionMode::Ellipse),
    };
    let Response::Accepted { run_id } = f.env.request(&req) else { panic!() };
    wait_overlay(&f.x);
    drag(&f.x, (300, 200), (500, 400));
    assert!(f.x.xdotool(&["key", "Return", "sleep", "0.1"]));
    let s = finished(f.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Success, "{}", s.message);
    let img = read_image(s.items[0].path.as_ref().unwrap());
    assert_eq!((img.width(), img.height()), (200, 200));
    assert_eq!(img.row(0)[3], 0, "the corner outside the ellipse is transparent");
    assert_eq!(img.row(100)[100 * 4 + 3], 255, "the centre is opaque");

    // `ssx capture region` with no daemon-changing flag is handed to the daemon; Ctrl-C of
    // the CLI cancels the overlay in the daemon.
    let mut cli = f.env.spawn_ssx(&["capture", "region"]);
    wait_overlay(&f.x);
    assert!(f.x.xdotool(&["key", "Escape", "sleep", "0.1"]));
    let status = wait_until(Duration::from_secs(20), || cli.try_wait().ok().flatten())
        .expect("the CLI returns after the overlay ends");
    assert_eq!(status.code(), Some(3), "cancelled exits 3");
}

#[test]
fn three_post_file_processes_within_the_window_are_one_batch() {
    if skip_unless(&["xclip"]) {
        return;
    }
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    let files: Vec<PathBuf> = (1..=3)
        .map(|i| {
            let p = f.env.path(&format!("f{i}.png"));
            Frame::from_rgba8(4, 4, [i * 40, 20, 30, 255].repeat(16)).unwrap().save(&p).unwrap();
            p
        })
        .collect();
    // What Explorer does: one process per selected file, all started at once.
    let t = Instant::now();
    let children: Vec<_> = files
        .iter()
        .map(|p| f.env.spawn_ssx(&["post-file", "--coalesce", "--", p.to_str().unwrap()]))
        .collect();
    for c in children {
        let out = c.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    }
    eprintln!("3 forwarding processes finished after {:?}", t.elapsed());
    // One batch: one run, three uploads, one clipboard write holding all three links.
    let clip = wait_until(Duration::from_secs(20), || {
        clipboard(&f.x.display)
            .filter(|c| c.lines().filter(|l| l.contains("mock.test")).count() == 3)
    })
    .expect("all three links arrive on the clipboard together");
    let mut links: Vec<&str> = clip.lines().collect();
    links.sort_unstable();
    assert_eq!(links, ["http://mock.test/u1", "http://mock.test/u2", "http://mock.test/u3"]);
    assert_eq!(f.mock.requests().len(), 3);
    let log = f.env.daemon_log();
    let batches: Vec<&str> =
        log.lines().filter(|l| l.contains("uploading a batch of files")).collect();
    assert_eq!(batches.len(), 1, "one batch, not three runs:\n{log}");
    assert!(batches[0].contains("files=3") && batches[0].contains("requests=3"), "{}", batches[0]);
    let h = f.env.ssx(&["history", "list", "--json"]).ok();
    let list: serde_json::Value = serde_json::from_str(&h.stdout).unwrap();
    assert_eq!(list.as_array().map(Vec::len), Some(3));
}

#[test]
fn ipc_post_files_share_one_run_across_waiting_callers() {
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    let files: Vec<PathBuf> = (1..=3)
        .map(|i| {
            let p = f.env.path(&format!("g{i}.png"));
            Frame::from_rgba8(2, 2, [i * 50, 1, 2, 255].repeat(4)).unwrap().save(&p).unwrap();
            p
        })
        .collect();
    let summaries: Vec<_> = std::thread::scope(|s| {
        let hs: Vec<_> = files
            .iter()
            .map(|p| {
                let env = &f.env;
                let p = p.clone();
                s.spawn(move || {
                    finished(env.request(&Request::PostFiles {
                        paths: vec![p.clone(), p],
                        action: PostAction::Upload,
                        wait: true,
                    }))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(summaries.iter().all(|s| s.run_id == summaries[0].run_id), "one run for all callers");
    assert!(summaries.iter().all(|s| s.items.len() == 3 && s.outcome == Outcome::Success));
    assert_eq!(f.mock.requests().len(), 3, "duplicates were dropped inside the batch");
    // A file that does not exist fails that item only.
    let s = finished(f.env.request(&Request::PostFiles {
        paths: vec![f.env.path("nope.png"), files[0].clone()],
        action: PostAction::Upload,
        wait: true,
    }));
    assert_eq!(s.items.len(), 2);
    assert!(s.items.iter().any(|i| i.error.is_some()) && s.items.iter().any(|i| i.url.is_some()));
}

#[test]
fn settings_hot_reload_applies_valid_edits_and_keeps_the_old_config_on_errors() {
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    let names = |env: &TestEnv| -> Vec<String> {
        let Response::Workflows { workflows } = env.request(&Request::ListWorkflows) else {
            panic!()
        };
        workflows.into_iter().map(|w| w.name).collect()
    };
    assert!(names(&f.env).contains(&"Shot only".to_owned()));
    let path = f.env.cfg.join("settings.toml");
    let original = std::fs::read_to_string(&path).unwrap();

    // A valid edit is applied (watched through the file system, no request needed).
    std::fs::write(&path, original.replace("Shot only", "Renamed shot")).unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || names(&f.env)
            .contains(&"Renamed shot".to_owned())
            .then_some(()))
        .is_some(),
        "the edit was not picked up\n{}",
        f.env.daemon_log()
    );
    // The renamed workflow works with the new settings.
    let s = finished(run_by_name(&f.env, "local", true));
    assert_eq!(s.outcome, Outcome::Success);

    // An invalid edit (broken TOML) is rejected; the old configuration stays in force and the
    // problem is reported.
    std::fs::write(&path, "version = 1\n[general\nbroken = ").unwrap();
    let problem = wait_until(Duration::from_secs(10), || {
        let Response::Status(st) = f.env.request(&Request::Status) else { return None };
        st.settings_problem
    })
    .expect("the problem is reported in Status");
    assert!(!problem.is_empty());
    assert!(names(&f.env).contains(&"Renamed shot".to_owned()), "the old config is kept");
    assert_eq!(finished(run_by_name(&f.env, "local", true)).outcome, Outcome::Success);

    // Semantically invalid (parses, fails validation): also rejected.
    std::fs::write(
        &path,
        original.replace("[general]", "[general]\nimage_quality = 500\nimage_format = \"png\""),
    )
    .unwrap();
    let again = wait_until(Duration::from_secs(10), || {
        let Response::Status(st) = f.env.request(&Request::Status) else { return None };
        st.settings_problem.filter(|p| p != &problem)
    });
    assert!(again.is_some(), "a validation error is reported too");

    // Fixing the file clears the problem and applies it (a ReloadSettings request works as
    // well as the watcher).
    std::fs::write(&path, original.replace("Shot only", "Fixed shot")).unwrap();
    assert_eq!(f.env.request(&Request::ReloadSettings), Response::Ok);
    assert!(
        wait_until(Duration::from_secs(10), || {
            let Response::Status(st) = f.env.request(&Request::Status) else { return None };
            (st.settings_problem.is_none() && names(&f.env).contains(&"Fixed shot".to_owned()))
                .then_some(())
        })
        .is_some()
    );
    let log = f.env.daemon_log();
    assert!(log.contains("settings reloaded") && log.contains("settings rejected"), "{log}");
}

fn ffprobe(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=codec_name,width,height,nb_read_frames:format=duration")
        .args(["-of", "json"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe cannot decode {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ffprobe json")
}

fn find_mp4(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "mp4") {
                return Some(p);
            }
        }
    }
    None
}

fn start_recording(f: &Fixture) -> u64 {
    let r = f.env.request(&Request::StartRecording(RecordSpec {
        workflow: Some("rec".into()),
        target: Some(RecordTarget::Rect { x: 20, y: 30, width: 321, height: 241 }),
        ..RecordSpec::default()
    }));
    let Response::Recording(st) = r else { panic!("{r:?}") };
    assert!(st.active, "{st:?}");
    let run_id = st.run_id.expect("run id");
    // Wait until frames are being captured and a second or two have passed.
    let ok = wait_until(Duration::from_secs(30), || {
        let Response::Recording(s) = f.env.request(&Request::RecordingStatus) else { return None };
        (s.active && !s.selecting && s.elapsed_ms >= 1800).then_some(())
    });
    assert!(ok.is_some(), "the recording never got going\n{}", f.env.daemon_log());
    run_id
}

#[test]
fn recording_through_the_daemon_produces_a_decodable_mp4() {
    if skip_unless(&["ffprobe"]) {
        return;
    }
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    let run_id = start_recording(&f);
    // Only one recording at a time, and a second start is refused.
    let again = f.env.request(&Request::StartRecording(RecordSpec {
        workflow: Some("rec".into()),
        ..RecordSpec::default()
    }));
    expect_error(&again, ErrorCode::Busy);
    // The recording is stopped like the hotkey would: toggle.
    let r = f.env.request(&Request::ToggleRecording(RecordSpec {
        workflow: Some("rec".into()),
        ..RecordSpec::default()
    }));
    assert!(matches!(r, Response::Recording(_)), "{r:?}");
    let s = finished(f.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Success, "{}", s.message);
    let path = s.items[0].path.clone().expect("the recording file");
    let info = ffprobe(&path);
    let stream = &info["streams"][0];
    eprintln!("recording: {stream}, duration {}", info["format"]["duration"]);
    // 321x241 is rounded down to even dimensions for the encoder.
    assert_eq!((stream["width"].as_u64(), stream["height"].as_u64()), (Some(320), Some(240)));
    let frames: u64 =
        stream["nb_read_frames"].as_str().and_then(|v| v.parse().ok()).expect("frames");
    assert!(frames >= 40, "about 2 s at 30 fps, got {frames}");
    assert!(matches!(stream["codec_name"].as_str(), Some("h264" | "mpeg4")), "{stream}");
    // The slot is free again: a new recording can start right away.
    let r = f.env.request(&Request::StartRecording(RecordSpec {
        workflow: Some("rec".into()),
        target: Some(RecordTarget::Desktop),
        ..RecordSpec::default()
    }));
    assert!(matches!(r, Response::Recording(s) if s.active));
    assert_eq!(f.env.request(&Request::StopRecording), Response::Ok);
    let Response::Recording(st) = f.env.request(&Request::RecordingStatus) else { panic!() };
    let _ = st;
    // Wait for the daemon to become idle before the fixture (and the X server) goes away.
    wait_until(Duration::from_secs(30), || {
        let Response::Status(s) = f.env.request(&Request::Status) else { return None };
        s.active_runs.is_empty().then_some(())
    });
}

#[test]
fn sigterm_finalises_a_recording_in_flight() {
    if skip_unless(&["ffprobe"]) {
        return;
    }
    let Some(f) = fixture() else { return };
    let mut d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    let _ = start_recording(&f);
    d.sigterm();
    let status = d.wait_exit(Duration::from_secs(40)).expect("the daemon exits after SIGTERM");
    assert!(status.success(), "{status}\n{}", f.env.daemon_log());
    let file = find_mp4(&f.save)
        .unwrap_or_else(|| panic!("no recording was left\n{}", f.env.daemon_log()));
    let info = ffprobe(&file);
    let frames: u64 =
        info["streams"][0]["nb_read_frames"].as_str().and_then(|v| v.parse().ok()).expect("frames");
    assert!(frames >= 30, "the file is finalised and decodable, {frames} frames");
    let log = f.env.daemon_log();
    assert!(log.contains("shutting down"), "{log}");
}

#[test]
fn a_show_request_without_helpers_opens_nothing_but_answers() {
    let Some(f) = fixture() else { return };
    let _d = Daemon::start(&f.env, &["--no-tray", "--no-hotkeys"]);
    // No ssx-editor-ui / ssx-settings-ui in this environment: the app notifies (no daemon
    // here) and still answers Ok.
    assert_eq!(f.env.request(&Request::Show { target: ShowTarget::Editor }), Response::Ok);
    assert_eq!(f.env.request(&Request::Show { target: ShowTarget::History }), Response::Ok);
}

#[test]
fn idle_cost_and_memory_stay_flat() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Some(f) = fixture_sized("1920x1080x24", WORKFLOWS) else { return };
    // Hotkeys on (the X11 grab thread is part of the idle cost).
    let d = Daemon::start(&f.env, &["--no-tray"]);
    std::thread::sleep(Duration::from_secs(2));
    let secs: u64 = std::env::var("SSX_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let (t0, rss0) = d.proc_usage().expect("proc");
    std::thread::sleep(Duration::from_secs(secs));
    let (t1, rss1) = d.proc_usage().expect("proc");
    let ticks_per_s = 100.0; // USER_HZ on Linux
    let cpu = (t1 - t0) as f64 / ticks_per_s / secs as f64 * 100.0;
    eprintln!("idle over {secs} s: CPU {cpu:.3} % of one core, RSS {rss1} KiB (was {rss0} KiB)");
    assert!(cpu < 1.0, "idle CPU {cpu} %");

    // Bounded memory: 10 full-screen 1080p captures (8 MB frames) leave nothing behind.
    let (_, before) = d.proc_usage().unwrap();
    for _ in 0..10 {
        let s = finished(run_by_name(&f.env, "local", true));
        assert_eq!(s.outcome, Outcome::Success);
    }
    std::thread::sleep(Duration::from_secs(1));
    let (_, after) = d.proc_usage().unwrap();
    eprintln!("RSS before 10 captures {before} KiB, after {after} KiB");
    assert!(
        after < before + 80 * 1024,
        "frames must be dropped after each run: {before} -> {after} KiB"
    );
}
