//! The daemon on a real headless sway (wlroots): wlr-screencopy capture and the layer-shell
//! overlay, driven with virtual pointer/keyboard input.
//!
//! The harness (a private sway, the `zwlr_virtual_pointer_v1` / `zwp_virtual_keyboard_v1`
//! injector, `grim` as an independent observer) is `ssx-overlay`'s, included by path so there
//! is one copy. The desktop is a coordinate-coded picture set as sway's wallpaper at the
//! output's exact size, so every pixel identifies its position and a wrong crop shows up as a
//! wrong colour.
//!
//! Skips (with a printed reason) when `sway` is missing or cannot start headless.
#![cfg(target_os = "linux")]

mod common;

#[path = "common/ovl.rs"]
mod ovl;

use std::time::{Duration, Instant};

use common::{Daemon, TestEnv, UploadMock, expect_error, finished, overlay_bin, wait_until};
use ovl::sway::{OutputCfg, Sway, evdev};
use ssx_core::{
    ipc::{ErrorCode, Request, Response},
    workflow::Outcome,
};
use ssx_types::Frame;

const W: u32 = 800;
const H: u32 = 600;

/// The wallpaper: pixel `(x, y)` encodes its own position.
fn coded(x: u32, y: u32) -> [u8; 3] {
    [(x % 251) as u8 | 1, (y % 241) as u8 | 1, ((x / 7 + y / 5) % 200) as u8 + 30]
}

fn write_wallpaper(path: &std::path::Path) {
    let mut img = image::RgbImage::new(W, H);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgb(coded(x, y));
    }
    img.save(path).expect("wallpaper");
}

fn dim(p: [u8; 3]) -> [u8; 3] {
    p.map(|c| (f32::from(c) * 0.5 + 0.5) as u8)
}

fn rgb_at(f: &Frame, x: u32, y: u32) -> [u8; 3] {
    let p = &f.row(y)[x as usize * 4..x as usize * 4 + 3];
    [p[0], p[1], p[2]]
}

fn read_image(path: &std::path::Path) -> Frame {
    Frame::decode(&std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .expect("decodes")
}

struct Rig {
    sway: Sway,
    env: TestEnv,
    save: std::path::PathBuf,
    _mock: UploadMock,
}

const WORKFLOWS: &str = r#"
[[workflows]]
id = "shot-local"
name = "Shot only"
input = "capture_fullscreen"
after_capture = ["save_to_file"]
[workflows.trigger]
cli_name = "local"

[[workflows]]
id = "region-save"
name = "Region"
input = "capture_region"
after_capture = ["save_to_file"]
[workflows.trigger]
cli_name = "region"
"#;

fn rig() -> Option<Rig> {
    let sway = Sway::start(&[OutputCfg::new("HEADLESS-1", W, H, 0, 0)])?;
    let env = TestEnv::new().with_run_dir(sway.dir.path());
    let wall = env.path("wall.png");
    write_wallpaper(&wall);
    sway.command(&format!("output HEADLESS-1 bg {} fill", wall.display()));
    let mut env = env;
    env.set("WAYLAND_DISPLAY", sway.wayland_display());
    env.set("XDG_SESSION_TYPE", "wayland");
    env.set("SSX_BACKEND", "wayland");
    env.set("SSX_OVERLAY", overlay_bin().display().to_string());
    let mock = UploadMock::start();
    env.write_uploader(&mock.url());
    let save = env.path("shots");
    env.write_settings(&common::settings_with_mock_uploader(&format!(
        "[general]\nsave_dir = {save:?}\nuse_type_subfolders = false\nfolder_pattern = \"\"\n\
         file_name_pattern = \"shot_%i\"\nshow_notifications = false\n\n{WORKFLOWS}"
    )));
    sway.input().set_layout(W, H);
    Some(Rig { sway, env, save, _mock: mock })
}

fn run_by_name(env: &TestEnv, name: &str, wait: bool) -> Response {
    env.request(&Request::RunWorkflow { id: None, name: Some(name.into()), wait })
}

/// Waits until sway shows the wallpaper and returns what `grim` (an independent screencopy
/// client) sees: the reference every capture is compared with. The wallpaper is rendered by
/// sway, which may round a pixel by one, so the oracle is grim's frame, not the formula.
fn wait_wallpaper(sway: &Sway) -> Frame {
    let mut previous: Option<Frame> = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(f) = sway.grim("HEADLESS-1") {
            let lit = rgb_at(&f, 5, 5) != [0, 0, 0] && rgb_at(&f, W - 1, H - 1) != [0, 0, 0];
            if lit && previous.as_ref().is_some_and(|p| same(p, &f)) {
                return f;
            }
            previous = Some(f);
        }
        assert!(Instant::now() < deadline, "sway never showed a stable wallpaper");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn same(a: &Frame, b: &Frame) -> bool {
    a.width() == b.width()
        && a.height() == b.height()
        && (0..a.height()).all(|y| a.row(y) == b.row(y))
}

#[test]
fn the_daemon_captures_wlroots_fullscreen_pixel_exact_and_reports_its_speed() {
    let Some(r) = rig() else { return };
    let _d = Daemon::start(&r.env, &["--no-tray", "--no-hotkeys"]);
    let reference = wait_wallpaper(&r.sway);

    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..5 {
        let t = Instant::now();
        let s = finished(run_by_name(&r.env, "local", true));
        times.push(t.elapsed());
        assert_eq!(s.outcome, Outcome::Success, "{}\n{}", s.message, r.env.daemon_log());
        last = s.items[0].path.clone();
    }
    times.sort();
    eprintln!(
        "wlroots: IPC trigger -> fullscreen capture saved: median {:?}, best {:?}, worst {:?}",
        times[2], times[0], times[4]
    );
    let img = read_image(&last.expect("saved path"));
    assert_eq!((img.width(), img.height()), (W, H));
    for y in 0..H {
        for x in 0..W {
            assert_eq!(rgb_at(&img, x, y), rgb_at(&reference, x, y), "pixel ({x},{y})");
        }
    }
}

#[test]
fn a_region_workflow_uses_the_layer_shell_overlay_and_crops_exactly() {
    let Some(r) = rig() else { return };
    let _d = Daemon::start(&r.env, &["--no-tray", "--no-hotkeys"]);
    let reference = wait_wallpaper(&r.sway);

    // Start the interactive run without waiting; the overlay opens on the output.
    let Response::Accepted { run_id, .. } = run_by_name(&r.env, "region", false) else {
        panic!("the region run is accepted\n{}", r.env.daemon_log())
    };
    let probe = rgb_at(&reference, W - 10, H - 10);
    let shown = wait_until(Duration::from_secs(20), || {
        r.sway.grim("HEADLESS-1").filter(|f| rgb_at(f, W - 10, H - 10) == dim(probe)).map(|_| ())
    });
    assert!(shown.is_some(), "the overlay never dimmed the output\n{}", r.env.daemon_log());
    std::thread::sleep(Duration::from_millis(300));

    // One interactive run at a time: a second region trigger is refused while it is open.
    let second = run_by_name(&r.env, "region", false);
    let msg = expect_error(&second, ErrorCode::Busy);
    assert!(!msg.is_empty());

    // Drag out (120,80)-(420,330) and confirm with Enter.
    let inp = r.sway.input();
    inp.move_to(120.0, 80.0);
    inp.left_down();
    inp.move_to(250.0, 200.0);
    inp.move_to(420.0, 330.0);
    inp.left_up();
    inp.tap(evdev::ENTER);

    let s = finished(r.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Success, "{}\n{}", s.message, r.env.daemon_log());
    let img = read_image(&s.items[0].path.clone().expect("saved"));
    assert_eq!((img.width(), img.height()), (300, 250), "the selected size, exactly");
    for y in 0..img.height() {
        for x in 0..img.width() {
            assert_eq!(
                rgb_at(&img, x, y),
                rgb_at(&reference, 120 + x, 80 + y),
                "pixel ({x},{y}) of the crop"
            );
        }
    }

    // Escape cancels the next one: no file, a Cancelled outcome, and the slot is free again.
    let files = std::fs::read_dir(&r.save).map_or(0, Iterator::count);
    let Response::Accepted { run_id, .. } = run_by_name(&r.env, "region", false) else {
        panic!("accepted")
    };
    let shown = wait_until(Duration::from_secs(20), || {
        r.sway.grim("HEADLESS-1").filter(|f| rgb_at(f, W - 10, H - 10) == dim(probe)).map(|_| ())
    });
    assert!(shown.is_some(), "the overlay opened again");
    std::thread::sleep(Duration::from_millis(300));
    inp.tap(evdev::ESC);
    let s = finished(r.env.request(&Request::WaitRun { run_id }));
    assert_eq!(s.outcome, Outcome::Cancelled, "{}", s.message);
    assert_eq!(std::fs::read_dir(&r.save).map_or(0, Iterator::count), files);
    let gone = wait_until(Duration::from_secs(10), || {
        r.sway.grim("HEADLESS-1").filter(|f| rgb_at(f, W - 10, H - 10) == probe).map(|_| ())
    });
    assert!(gone.is_some(), "the overlay went away");
}
