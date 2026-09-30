//! End to end against a real headless sway: proves `SSX_BACKEND=wayland` works through the
//! whole stack (backend detection, screencopy, tone-map pass-through, PNG encode, CLI).
//!
//! sway paints a solid colour background on its headless output, so the expected pixels are
//! known without any Wayland client of our own. The test skips when `sway` is missing or
//! cannot start headless (CI installs it).
#![cfg(target_os = "linux")]

mod common;

use std::{
    fs::File,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use common::{TestEnv, have, read_image, rgb};

const BG: [u8; 3] = [0x33, 0x66, 0x99];

struct Sway {
    child: Child,
    dir: tempfile::TempDir,
    socket: String,
}

impl Sway {
    fn start() -> Option<Sway> {
        if !have("sway") {
            eprintln!("SKIP: `sway` is not installed (CI installs it with `apt install sway`)");
            return None;
        }
        // Short path: the Wayland socket lives in here and unix socket paths are limited.
        let dir = tempfile::Builder::new().prefix("ssxsway").tempdir().ok()?;
        let cfg = dir.path().join("sway.conf");
        std::fs::write(
            &cfg,
            "xwayland disable\ndefault_border none\n\
             output HEADLESS-1 resolution 800x600 position 0 0\n\
             output HEADLESS-1 bg #336699 solid_color\n",
        )
        .ok()?;
        let log = File::create(dir.path().join("sway.log")).ok()?;
        let mut child = Command::new("sway")
            .arg("-c")
            .arg(&cfg)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("WLR_BACKENDS", "headless")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .env("XDG_RUNTIME_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone().ok()?)
            .stderr(log)
            .spawn()
            .map_err(|e| eprintln!("SKIP: cannot spawn sway: {e}"))
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let log = std::fs::read_to_string(dir.path().join("sway.log")).unwrap_or_default();
                let tail: Vec<&str> = log.lines().rev().take(5).collect();
                assert!(std::env::var_os("CI").is_none(), "sway exited early ({status}): {tail:?}");
                eprintln!("SKIP: sway cannot run headless here ({status}): {tail:?}");
                return None;
            }
            let socket = std::fs::read_dir(dir.path()).ok()?.flatten().find_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                (n.starts_with("wayland-") && !n.to_ascii_lowercase().ends_with(".lock"))
                    .then_some(n)
            });
            if let Some(socket) = socket {
                return Some(Sway { child, dir, socket });
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                eprintln!("SKIP: sway did not create a Wayland socket in time");
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn env(&self) -> TestEnv {
        let mut env = TestEnv::new();
        env.set("WAYLAND_DISPLAY", self.socket.clone())
            .set("XDG_RUNTIME_DIR", self.dir.path().display().to_string())
            .set("XDG_SESSION_TYPE", "wayland")
            .set("SSX_BACKEND", "wayland");
        env
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Retries until sway has rendered its background (the first frames can be black).
fn capture_until_background(env: &TestEnv, args: &[&str], out: &Path) -> ssx_types::Frame {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let r = env.ssx(args);
        if r.code == 0 {
            let img = read_image(out);
            if rgb(&img, 0, 0) == BG && rgb(&img, img.width() - 1, img.height() - 1) == BG {
                return img;
            }
        }
        assert!(
            Instant::now() < deadline,
            "sway never showed the background; last run: {}\n{}",
            r.stdout,
            r.stderr
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn wayland_backend_captures_a_headless_sway_end_to_end() {
    let Some(sway) = Sway::start() else { return };
    let env = sway.env();

    // monitors: one 800x600 output, named like sway names it.
    let deadline = Instant::now() + Duration::from_secs(15);
    let monitors = loop {
        let r = env.ssx(&["monitors", "--json"]);
        if r.code == 0 && r.json().as_array().is_some_and(|a| !a.is_empty()) {
            break r.json();
        }
        assert!(Instant::now() < deadline, "no monitors: {}\n{}", r.stdout, r.stderr);
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(monitors[0]["id"], "HEADLESS-1");
    assert_eq!(
        monitors[0]["rect"],
        serde_json::json!({"x": 0, "y": 0, "width": 800, "height": 600})
    );

    // fullscreen: every pixel is the background colour.
    let full = env.path("full.png");
    let img = capture_until_background(
        &env,
        &["capture", "fullscreen", "-o", full.to_str().unwrap()],
        &full,
    );
    assert_eq!((img.width(), img.height()), (800, 600));
    for y in (0..600).step_by(7) {
        for x in (0..800).step_by(11) {
            assert_eq!(rgb(&img, x, y), BG, "pixel ({x},{y})");
        }
    }

    // --rect and monitor --id take the same path through the wayland backend.
    let crop = env.path("crop.png");
    env.ssx(&["capture", "region", "--rect", "10,20,100,50", "-o", crop.to_str().unwrap()]).ok();
    let c = read_image(&crop);
    assert_eq!((c.width(), c.height()), (100, 50));
    assert!(rgb(&c, 0, 0) == BG && rgb(&c, 99, 49) == BG);
    let mon = env.path("mon.png");
    env.ssx(&["capture", "monitor", "--id", "HEADLESS-1", "-o", mon.to_str().unwrap()]).ok();
    assert_eq!(read_image(&mon).width(), 800);

    // doctor names the backend and why the others were not needed.
    let d = env.ssx(&["doctor", "--json"]).json();
    assert_eq!(d["capture"]["ok"], true, "{d}");
    assert!(d["capture"]["backend"].as_str().unwrap().starts_with("wayland"), "{}", d["capture"]);
    assert_eq!(d["capture"]["attempts"][0]["backend"], "wayland");
    assert_eq!(d["session"]["session_type"], "wayland");
    assert_eq!(d["monitors"][0]["hdr"], "unknown");
}
