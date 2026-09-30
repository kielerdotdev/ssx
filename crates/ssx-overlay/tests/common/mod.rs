//! Shared test support: private Xvfb, xdotool driver, helper wrapper scripts, test frames.
//!
//! Everything skips cleanly (with a printed reason) when a tool is missing, and every
//! spawned process is killed on drop.
#![allow(dead_code)] // each integration-test binary uses a different subset
#![cfg(target_os = "linux")]

use std::{
    io::{BufRead, BufReader},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub mod sway;

use ssx_overlay::{OverlayInput, OverlayOptions};
use ssx_types::{Frame, Monitor, Point, Rect, WindowInfo};

/// `true` if `prog` is an executable in `PATH`.
pub fn have(prog: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// Prints a SKIP line and returns `true` when any of `progs` is missing.
pub fn skip_unless(progs: &[&str]) -> bool {
    for p in progs {
        if !have(p) {
            eprintln!("SKIP: `{p}` is not installed (CI installs it with apt)");
            return true;
        }
    }
    false
}

/// Path of the helper binary built by cargo for this test run.
pub fn helper_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ssx-overlay"))
}

/// A private Xvfb server.
pub struct Xvfb {
    child: Child,
    pub display: String,
    pub width: u32,
    pub height: u32,
}

impl Xvfb {
    pub fn start(width: u32, height: u32) -> Option<Xvfb> {
        if skip_unless(&["Xvfb", "xdotool", "import"]) {
            return None;
        }
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset", "-screen", "0"])
            .arg(format!("{width}x{height}x24"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        let n: u32 = line.trim().parse().ok()?;
        Some(Xvfb { child, display: format!(":{n}"), width, height })
    }

    /// Runs `xdotool` against this server.
    pub fn xdotool(&self, args: &[&str]) -> bool {
        Command::new("xdotool")
            .env("DISPLAY", &self.display)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Waits until a window with `name` exists (the overlay is mapped).
    pub fn wait_for_window(&self, name: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.xdotool(&["search", "--name", name]) {
                std::thread::sleep(Duration::from_millis(150));
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// Screenshot of the root window (everything visible, including the overlay).
    pub fn screenshot(&self) -> Frame {
        let out = Command::new("import")
            .env("DISPLAY", &self.display)
            .args(["-window", "root", "png:-"])
            .output()
            .expect("run import");
        assert!(out.status.success(), "import failed");
        Frame::decode(&out.stdout).expect("decode screenshot").into_rgba8().expect("rgba")
    }

    /// A wrapper script that points the helper at this server and away from Wayland.
    pub fn helper_wrapper(&self, dir: &Path) -> PathBuf {
        wrapper(dir, &[("DISPLAY", &self.display)], &["WAYLAND_DISPLAY"], &helper_bin())
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes an executable `sh` script that sets/unsets environment variables and execs `target`.
pub fn wrapper(dir: &Path, set: &[(&str, &str)], unset: &[&str], target: &Path) -> PathBuf {
    let mut s = String::from("#!/bin/sh\n");
    for (k, v) in set {
        s += &format!("export {k}='{v}'\n");
    }
    for k in unset {
        s += &format!("unset {k}\n");
    }
    s += &format!("exec '{}' \"$@\"\n", target.display());
    let p = dir.join(format!("helper-{}.sh", set.len() + unset.len()));
    std::fs::write(&p, s).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// A coordinate-coded desktop: every pixel value identifies its position, so any mapping bug
/// shows up as a wrong colour.
pub fn coded_desktop(w: u32, h: u32, origin: Point) -> Frame {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[(x % 251) as u8 | 1, (y % 241) as u8 | 1, ((x / 7 + y / 5) % 200) as u8 + 30, 255]);
        }
    }
    let mut f = Frame::from_rgba8(w, h, data).unwrap();
    f.origin = origin;
    f
}

pub fn pixel(f: &Frame, x: u32, y: u32) -> [u8; 3] {
    let p = &f.row(y)[x as usize * 4..x as usize * 4 + 3];
    [p[0], p[1], p[2]]
}

pub fn monitor(name: &str, r: Rect, scale: f64) -> Monitor {
    Monitor {
        id: name.into(),
        name: name.into(),
        rect: r,
        scale_factor: scale,
        primary: false,
        refresh_hz: None,
        hdr: None,
    }
}

pub fn window(title: &str, r: Rect) -> WindowInfo {
    WindowInfo {
        id: title.into(),
        title: title.into(),
        app_name: None,
        rect: r,
        minimized: false,
        focused: false,
    }
}

pub fn input(desktop: Frame, monitors: Vec<Monitor>, windows: Vec<WindowInfo>, options: OverlayOptions) -> OverlayInput {
    OverlayInput { desktop, monitors, windows, options }
}
