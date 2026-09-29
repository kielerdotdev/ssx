//! Monitor-layout discovery through Wayland `wl_output` + `xdg-output`, tested against a
//! real compositor: headless `sway` (wlroots). This validates the protocol handling
//! (binding, versions, event ordering) that no mock could; it says nothing about how
//! Mutter or KWin behave. Skips cleanly when `sway` is not installed or cannot start.
#![cfg(target_os = "linux")]
#![allow(clippy::float_cmp)] // scale factors in these fixtures are exactly representable

#[allow(dead_code)] // each test binary uses a subset of the harness
mod common;

use std::{
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use common::{Bus, PortalBehavior, start_portal};
use ssx_capture::{CaptureBackend, CaptureOptions};
use ssx_capture_portal::PortalCapture;
use ssx_types::{Point, Rect};

struct Sway {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Sway {
    fn start(config: &str) -> Option<Sway> {
        let dir =
            std::env::temp_dir().join(format!("ssx-sway-{}-{}", std::process::id(), config.len()));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).ok()?;
        std::fs::write(dir.join("sway.conf"), config).ok()?;
        let child = Command::new("sway")
            .arg("-c")
            .arg(dir.join("sway.conf"))
            .arg("--unsupported-gpu")
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_RENDERER", "pixman")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run sway ({e}); install `sway` to run the wl_output tests");
                return None;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                eprintln!("SKIP: sway exited early ({status})");
                return None;
            }
            let found = std::fs::read_dir(&dir).ok().and_then(|d| {
                d.filter_map(Result::ok).map(|e| e.path()).find(|p| {
                    p.extension().is_none()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("wayland-"))
                })
            });
            if let Some(socket) = found {
                return Some(Sway { child, dir, socket });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("SKIP: sway did not create a Wayland socket in time");
        let _ = child.kill();
        None
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn detect_with_sway(bus: &Bus, sway: &Sway) -> PortalCapture {
    let mut cfg = bus.config();
    cfg.wayland_outputs = true;
    cfg.wayland_socket = Some(sway.socket.clone());
    PortalCapture::with_config(cfg).expect("detect")
}

#[test]
fn xdg_output_layout_from_a_real_compositor_integer_scale() {
    let Some(bus) = Bus::start() else { return };
    let Some(sway) = Sway::start("output HEADLESS-1 mode 800x600 scale 2 position 100 50\n") else {
        return;
    };
    // 800x600 physical at scale 2 = 400x300 logical at (100,50): the screenshot is 800x600.
    let _portal = start_portal(
        &bus,
        PortalBehavior::Png { w: 800, h: 600, file_name: "sway.png".into(), delay: Duration::ZERO },
    );
    let cap = detect_with_sway(&bus, &sway);
    assert!(cap.capabilities().enumerate_monitors);
    let monitors = cap.monitors().expect("monitors from sway");
    assert_eq!(monitors.len(), 1, "{monitors:?}");
    let m = &monitors[0];
    assert_eq!(m.id, "HEADLESS-1");
    assert_eq!(m.scale_factor, 2.0);
    assert_eq!(m.rect, Rect::new(200, 100, 800, 600), "logical rect times scale");

    let desktop = cap.capture_desktop(&CaptureOptions::default()).unwrap();
    assert_eq!(desktop.origin, Point::new(200, 100));
    assert_eq!(desktop.scale_factor, 2.0);
    let mon = cap.capture_monitor("HEADLESS-1", &CaptureOptions::default()).unwrap();
    assert_eq!((mon.width(), mon.height()), (800, 600));
    assert_eq!(mon.origin, Point::new(200, 100));
}

#[test]
fn xdg_output_layout_from_a_real_compositor_fractional_scale() {
    let Some(bus) = Bus::start() else { return };
    let Some(sway) = Sway::start("output HEADLESS-1 mode 1000x500 scale 1.25\n") else { return };
    let _portal = start_portal(
        &bus,
        PortalBehavior::Png {
            w: 1000,
            h: 500,
            file_name: "frac.png".into(),
            delay: Duration::ZERO,
        },
    );
    let cap = detect_with_sway(&bus, &sway);
    let m = &cap.monitors().expect("monitors from sway")[0];
    assert!((m.scale_factor - 1.25).abs() < 0.01, "{m:?}");
    assert_eq!(m.rect.size().width, 1000);
    assert_eq!(m.rect.size().height, 500);
    let f = cap.capture_monitor("HEADLESS-1", &CaptureOptions::default()).unwrap();
    assert_eq!((f.width(), f.height()), (1000, 500));
}

#[test]
fn unreachable_compositor_means_no_monitor_enumeration() {
    let Some(bus) = Bus::start() else { return };
    let _portal = start_portal(
        &bus,
        PortalBehavior::Png { w: 8, h: 8, file_name: "n.png".into(), delay: Duration::ZERO },
    );
    let mut cfg = bus.config();
    cfg.wayland_outputs = true;
    cfg.wayland_socket = Some(PathBuf::from("/nonexistent/ssx/wayland-9"));
    let cap = PortalCapture::with_config(cfg).unwrap();
    assert!(!cap.capabilities().enumerate_monitors);
    assert!(cap.monitors().is_err());
    // capturing the whole desktop still works
    assert!(cap.capture_desktop(&CaptureOptions::default()).is_ok());
}
