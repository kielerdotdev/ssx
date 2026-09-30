//! Smoke test of the *real binary* in a real X11 window: starts `ssx-editor-ui` under Xvfb on
//! the Vulkan software rasteriser (lavapipe), drives it with `xdotool`, screenshots the root
//! window with ImageMagick's `import`, and checks that it paints the loaded image, draws where
//! the mouse drags, undoes, saves, and exits with the documented JSON and exit code.
//!
//! Skips (with a printed reason) when Xvfb, xdotool, import or a Vulkan software driver is
//! missing. CI installs them (`xvfb xdotool imagemagick mesa-vulkan-drivers`).

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn have(tool: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool} >/dev/null 2>&1"))
        .status()
        .is_ok_and(|s| s.success())
}

fn skip_reason() -> Option<String> {
    for t in ["xvfb-run", "Xvfb", "xdotool", "import"] {
        if !have(t) {
            return Some(format!("{t} is not installed"));
        }
    }
    let icd_dirs = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"];
    let has_lvp = icd_dirs.iter().any(|d| Path::new(d).join("lvp_icd.json").exists()
        || Path::new(d).join("lvp_icd.x86_64.json").exists());
    if !has_lvp {
        return Some("no lavapipe Vulkan driver (lvp_icd.json)".into());
    }
    None
}

fn icd_file() -> PathBuf {
    for d in ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"] {
        for n in ["lvp_icd.json", "lvp_icd.x86_64.json"] {
            let p = Path::new(d).join(n);
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from("/usr/share/vulkan/icd.d/lvp_icd.json")
}

fn load(p: &Path) -> image::RgbaImage {
    image::open(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).to_rgba8()
}

/// Pixels of the canvas area (below the bars, above the status bar) that differ noticeably.
fn canvas_diff(a: &image::RgbaImage, b: &image::RgbaImage) -> usize {
    let mut n = 0;
    for y in 120..760 {
        for x in 0..1280 {
            let (p, q) = (a.get_pixel(x, y), b.get_pixel(x, y));
            let d: i32 = (0..3).map(|i| (i32::from(p[i]) - i32::from(q[i])).abs()).sum();
            if d > 60 {
                n += 1;
            }
        }
    }
    n
}

/// Pixels in the accent blue of selection outlines and handles.
fn accent(img: &image::RgbaImage) -> usize {
    img.enumerate_pixels()
        .filter(|(x, y, p)| {
            *y > 120 && *y < 760 && *x < 1280 && (60..95).contains(&p[0]) && (130..160).contains(&p[1]) && p[2] > 200
        })
        .count()
}

#[test]
fn real_window_paints_draws_undoes_saves_and_exits_cleanly() {
    if let Some(why) = skip_reason() {
        eprintln!("SKIPPED real_window_paints_draws_undoes_saves_and_exits_cleanly: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let input = d.join("in.png");
    common::dashboard().save(&input).unwrap();
    let out = d.join("out.png");
    let bin = env!("CARGO_BIN_EXE_ssx-editor-ui");
    let script = format!(
        r#"#!/bin/bash
set -u
cd "{d}"
"{bin}" "{input}" --output "{out}" --json --ephemeral > result.json 2> stderr.txt &
PID=$!
WID=""
for i in $(seq 1 100); do
  WID=$(xdotool search --name "ssx editor" 2>/dev/null | head -1)
  [ -n "$WID" ] && break
  sleep 0.3
done
# Without a window manager nothing focuses the window; do what a WM would.
xdotool windowfocus "$WID" 2>/dev/null || true
xdotool mousemove 640 420
sleep 4
import -window root shot1.png
xdotool key r
sleep 0.4
xdotool mousemove 400 250
sleep 0.3
xdotool mousedown 1
for p in "440 270" "480 290" "520 310" "560 330"; do xdotool mousemove $p; sleep 0.25; done
xdotool mouseup 1
sleep 1.5
import -window root shot2.png
xdotool key ctrl+z
sleep 1.5
import -window root shot3.png
xdotool key ctrl+y
sleep 0.8
xdotool key t
sleep 0.3
xdotool mousemove 700 650
xdotool click 1
sleep 0.6
xdotool type --delay 90 "Hello"
sleep 1.2
import -window root shot4.png
xdotool key Escape
sleep 0.4
xdotool key ctrl+s
sleep 2
xdotool key ctrl+Return
for i in $(seq 1 60); do
  kill -0 $PID 2>/dev/null || break
  sleep 0.3
done
if kill -0 $PID 2>/dev/null; then echo "still-running" > status.txt; kill $PID; else wait $PID; echo "exit=$?" > status.txt; fi
"#,
        d = d.display(),
        bin = bin,
        input = input.display(),
        out = out.display()
    );
    let script_path = d.join("drive.sh");
    std::fs::write(&script_path, script).unwrap();
    let status = Command::new("xvfb-run")
        .args(["-a", "-s", "-screen 0 1440x900x24", "bash"])
        .arg(&script_path)
        .env("WGPU_BACKEND", "vulkan")
        .env("VK_ICD_FILENAMES", icd_file())
        .env("SSX_CONFIG_DIR", d.join("cfg"))
        .status()
        .expect("xvfb-run starts");
    assert!(status.success(), "driver script failed; stderr: {}", std::fs::read_to_string(d.join("stderr.txt")).unwrap_or_default());

    if let Some(keep) = common::dump_dir() {
        let _ = std::fs::create_dir_all(&keep);
        for n in ["shot1.png", "shot2.png", "shot3.png", "shot4.png", "stderr.txt", "result.json"] {
            let _ = std::fs::copy(d.join(n), keep.join(format!("xvfb-{n}")));
        }
    }
    let stderr = std::fs::read_to_string(d.join("stderr.txt")).unwrap_or_default();
    let (s1, s2, s3, s4) = (load(&d.join("shot1.png")), load(&d.join("shot2.png")), load(&d.join("shot3.png")), load(&d.join("shot4.png")));
    // 1. It painted the image: the dashboard's dark navy header is on screen.
    let navy = s1.enumerate_pixels().filter(|(_, y, p)| *y > 120 && p[0] < 40 && p[1] < 50 && (55..90).contains(&p[2])).count();
    assert!(navy > 20_000, "the loaded image is not visible ({navy} header pixels); stderr:\n{stderr}");
    // 2. Dragging with the rectangle tool drew a rectangle and selected it.
    assert!(canvas_diff(&s1, &s2) > 300, "drag changed nothing; stderr:\n{stderr}");
    assert!(accent(&s2) > accent(&s1) + 40, "the new rectangle is not selected (no handles drawn)");
    // 3. Undo removed it again (apart from status-bar style noise, which is outside the canvas).
    assert!(canvas_diff(&s1, &s3) < 400, "undo did not restore the picture: {}", canvas_diff(&s1, &s3));
    // 4. Typing produced text (red glyph pixels appear near the click).
    assert!(canvas_diff(&s3, &s4) > 200, "typed text is not visible");
    // 5. Ctrl+S wrote the output; Ctrl+Enter finished with the documented outcome.
    assert!(out.exists(), "Ctrl+S did not write the output; stderr:\n{stderr}");
    let saved = image::open(&out).unwrap();
    assert_eq!((saved.width(), saved.height()), (1280, 760));
    let status = std::fs::read_to_string(d.join("status.txt")).unwrap_or_default();
    assert_eq!(status.trim(), "exit=0", "the editor did not exit cleanly: {status}\nstderr:\n{stderr}");
    let json = std::fs::read_to_string(d.join("result.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["action"], "save");
    assert_eq!(v["path"], out.display().to_string());
}
