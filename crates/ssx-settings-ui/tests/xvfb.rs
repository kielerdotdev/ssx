//! The *real binary* in a real X11 window: starts `ssx-settings-ui` under Xvfb on the Vulkan
//! software rasteriser (lavapipe), switches through every page with the real keyboard
//! shortcuts (`xdotool`), screenshots each with ImageMagick's `import`, changes a setting with
//! a real mouse click, saves with a real click, and checks the file on disk and the exit code.
//!
//! The process gets a private `HOME`, so nothing outside the test folder is touched. Skips
//! (with a printed reason) when Xvfb, xdotool, import or a Vulkan software driver is missing.
//! Set `SSX_UI_DUMP=dir` to keep the screenshots (`xvfb-<page>.png`).

mod common;

use std::{path::Path, process::Command};

use common::tools::*;
use ssx_core::settings::Settings;
use ssx_settings_ui::nav::Page;

const WIDTH: u32 = 1120;
const HEIGHT: u32 = 760;

fn load(p: &Path) -> image::RgbaImage {
    image::open(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).to_rgba8()
}

/// The nav rail row of page number `index` (0 = General): the selected one has a blue
/// background, the others the dark rail.
fn nav_is_selected(img: &image::RgbaImage, index: usize) -> bool {
    let p = img.get_pixel(150, 85 + 38 * index as u32);
    i32::from(p[2]) > i32::from(p[0]) + 40
}

/// Pixels that differ noticeably between two pictures, in the page area (right of the rail).
fn content_diff(a: &image::RgbaImage, b: &image::RgbaImage) -> usize {
    let mut n = 0;
    for y in 0..HEIGHT - 50 {
        for x in 220..WIDTH {
            let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
            let d: i32 = (0..3).map(|i| (i32::from(p[i]) - i32::from(q[i])).abs()).sum();
            if d > 60 {
                n += 1;
            }
        }
    }
    n
}

/// Pixels with a strong colour (thumbnails, colour bars) in a rectangle.
fn colourful(img: &image::RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut n = 0;
    for y in y0..y1.min(img.height()) {
        for x in x0..x1.min(img.width()) {
            let p = img.get_pixel(x, y);
            let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
            if mx - mn > 90 && mx > 120 {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn the_real_window_shows_every_page_saves_a_real_edit_and_exits_cleanly() {
    if let Some(why) = skip_reason(&["xvfb-run", "Xvfb", "xdotool", "import"]) {
        eprintln!(
            "SKIPPED the_real_window_shows_every_page_saves_a_real_edit_and_exits_cleanly: {why}"
        );
        return;
    }
    // A fixed folder (not a random temp name) so that the paths in the screenshots are stable.
    let root = std::env::temp_dir().join("ssx-settings-ui-xvfb");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let cfg = demo_config(&root);
    let home = private_home(&root);
    let bin = env!("CARGO_BIN_EXE_ssx-settings-ui");

    // Per page: how long to wait after switching (the history thumbnails, the HDR preview and
    // the diagnostics are computed off-thread), and whether to scroll down for a second shot.
    let mut steps = String::new();
    for (page, digit) in pages_with_digits() {
        let wait = match page {
            Page::History => 5,
            Page::Integration => 6,
            Page::Capture => 4,
            _ => 2,
        };
        steps.push_str(&format!("xdotool key ctrl+{digit}\nsleep {wait}\nshot {}\n", page.slug()));
        if page == Page::Capture {
            steps.push_str("xdotool mousemove 660 400\nxdotool click --repeat 14 --delay 60 5\nsleep 1.5\nxdotool mousemove 700 850\nsleep 0.5\nshot capture-preview\nxdotool mousemove 660 400\nxdotool click --repeat 20 --delay 60 4\nsleep 1\nxdotool mousemove 700 850\n");
        }
    }
    let script = format!(
        r#"#!/bin/bash
set -u
cd "{root}"
shot() {{ import -window root -crop {WIDTH}x{HEIGHT}+0+0 +repage "shot-$1.png"; }}
"{bin}" --page general --json --config-dir "{cfg}" > result.json 2> stderr.txt &
PID=$!
WID=""
for i in $(seq 1 100); do
  WID=$(xdotool search --name "ssx settings" 2>/dev/null | head -1)
  [ -n "$WID" ] && break
  sleep 0.3
done
# Without a window manager nothing focuses the window; do what a WM would.
xdotool windowfocus "$WID" 2>/dev/null || true
xdotool mousemove 700 850
sleep 3
shot general-first
{steps}
# A real edit: Capture page, click the "3 s" delay chip, then Save (which closes the window).
xdotool key ctrl+2
sleep 2
xdotool mousemove 532 165
sleep 0.3
xdotool click 1
sleep 1
xdotool mousemove 700 850
sleep 0.5
shot edited
xdotool mousemove 1074 731
sleep 0.3
xdotool click 1
for i in $(seq 1 60); do
  kill -0 $PID 2>/dev/null || break
  sleep 0.3
done
if kill -0 $PID 2>/dev/null; then echo "still-running" > status.txt; kill $PID; else wait $PID; echo "exit=$?" > status.txt; fi
"#,
        root = root.display(),
        bin = bin,
        cfg = cfg.display(),
    );
    let script_path = root.join("drive.sh");
    std::fs::write(&script_path, script).unwrap();
    let mut cmd = Command::new("xvfb-run");
    cmd.args(["-a", "-s", "-screen 0 1440x900x24", "bash"]).arg(&script_path);
    for (k, v) in process_env(&home) {
        cmd.env(k, v);
    }
    let status = cmd.status().expect("xvfb-run starts");
    let stderr = std::fs::read_to_string(root.join("stderr.txt")).unwrap_or_default();
    assert!(status.success(), "driver script failed; stderr:\n{stderr}");

    if let Some(keep) = common::dump_dir() {
        let _ = std::fs::create_dir_all(&keep);
        for e in std::fs::read_dir(&root).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix("shot-") {
                let _ = std::fs::copy(e.path(), keep.join(format!("xvfb-{rest}")));
            }
        }
        let _ = std::fs::copy(root.join("stderr.txt"), keep.join("xvfb-stderr.txt"));
    }

    // Every page painted, and the rail shows which one is open.
    let mut previous = load(&root.join("shot-general-first.png"));
    assert_eq!(
        (previous.width(), previous.height()),
        (WIDTH, HEIGHT),
        "the window is the documented size"
    );
    assert!(nav_is_selected(&previous, 0), "General is open first");
    for (i, page) in Page::ALL.iter().enumerate() {
        let img = load(&root.join(format!("shot-{}.png", page.slug())));
        assert!(
            nav_is_selected(&img, i),
            "{} is highlighted in the rail after Ctrl+{}",
            page.slug(),
            i + 1
        );
        for (j, _) in Page::ALL.iter().enumerate().filter(|(j, _)| *j != i) {
            assert!(!nav_is_selected(&img, j), "only {} is highlighted, not row {j}", page.slug());
        }
        let diff = content_diff(&previous, &img);
        // (General is the page the window opened on)
        assert!(
            i == 0 || diff > 3000,
            "{} looks like the page before it ({diff} pixels differ); stderr:\n{stderr}",
            page.slug()
        );
        previous = img;
    }
    // The history shows its thumbnails (strongly coloured pictures in the grid).
    let history = load(&root.join("shot-history.png"));
    assert!(colourful(&history, 230, 210, 1090, 700) > 6000, "no thumbnails in the history grid");
    // The HDR preview is visible after scrolling: the colour bars are there.
    let preview = load(&root.join("shot-capture-preview.png"));
    assert!(
        colourful(&preview, 230, 100, 1090, 700) > 6000,
        "no live HDR preview on the Capture page"
    );
    // The click on "3 s" made the page dirty.
    let edited = load(&root.join("shot-edited.png"));
    let first = load(&root.join("shot-capture.png"));
    assert!(content_diff(&first, &edited) > 40, "the click on the 3 s chip changed nothing");

    // Save wrote the file with just that change, and the window closed normally.
    let status = std::fs::read_to_string(root.join("status.txt")).unwrap_or_default();
    assert_eq!(
        status.trim(),
        "exit=0",
        "the window did not exit cleanly: {status}\nstderr:\n{stderr}"
    );
    let saved = Settings::load(&cfg.join("settings.toml")).unwrap().settings;
    assert_eq!(saved.capture.delay_ms, 3000, "the real click was saved; stderr:\n{stderr}");
    let mut expected = common::demo::lived_in_settings();
    expected.capture.delay_ms = 3000;
    assert_eq!(saved.workflows, expected.workflows, "nothing else changed");
    assert_eq!(saved.uploaders, expected.uploaders);
    let json = std::fs::read_to_string(root.join("result.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["saved"], true, "{json}");
    assert_eq!(v["settings_file"], cfg.join("settings.toml").display().to_string());
    // and the process only touched its own folders
    assert!(!home.join(".config/autostart").exists(), "autostart was never touched");
}
