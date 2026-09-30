//! [`OverlaySelector`]: interactive region selection through the `ssx-overlay` helper.
//!
//! The overlay lives in its own process (see the `ssx-overlay` README for why). This module
//! is the client: it writes the frozen desktop into shared memory, starts the helper, and
//! turns its answer into a [`Picked`]. `ssx_overlay::select_via_helper` does the same but
//! cannot be *cancelled*: the tray's Cancel entry (and daemon shutdown) must be able to close
//! an overlay that is waiting for the user, so the small client below is our own and polls
//! the [`CancelToken`], killing the helper when it fires. The wire format and the frame
//! hand-off come from `ssx_overlay::protocol` / `ssx_overlay::shm`, so the two cannot drift.
//!
//! Modes: rectangle, ellipse and freeform selections come back as a rectangle plus, for the
//! latter two, a coverage mask (pixels outside the shape become transparent). Window and
//! monitor picking come back as the window's / monitor's rectangle, with the window's title
//! attached so `%t` works in file names.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use ssx_core::workflow::{CancelToken, ServiceError};
use ssx_overlay::{
    OverlayInput, OverlayOptions, OverlayOutcome, SelectMode, SelectionShape,
    protocol::{FrameDesc, PROTOCOL_VERSION, Request, Response},
    shm::SharedFrame,
};
use ssx_types::{ColorSpace, Frame, Monitor, PixelFormat, Rect};

use crate::{
    capture::{PickRequest, Picked, RegionSelector},
    helpers::{Discovery, discover, discover_with},
};

/// Environment variable that names the helper (or switches it off with `none`).
pub const HELPER_ENV: &str = "SSX_OVERLAY";

/// File-name base of the helper.
pub const HELPER_BASE: &str = "ssx-overlay";

/// How long the overlay may stay open when nobody answers (matches `ssx-overlay`'s client).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the client checks for cancellation while the overlay is up.
const CANCEL_POLL: Duration = Duration::from_millis(40);

/// What the overlay asks the user to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickMode {
    /// Drag a rectangle.
    #[default]
    Rect,
    /// Drag an ellipse.
    Ellipse,
    /// Draw a free-hand outline.
    Freeform,
    /// Click a window.
    Window,
    /// Click a monitor.
    Monitor,
}

impl PickMode {
    /// The overlay's own mode.
    pub fn to_overlay(self) -> SelectMode {
        match self {
            Self::Rect => SelectMode::Rect,
            Self::Ellipse => SelectMode::Ellipse,
            Self::Freeform => SelectMode::Freeform,
            Self::Window => SelectMode::Window,
            Self::Monitor => SelectMode::Monitor,
        }
    }

    /// Parses `rect`, `ellipse`, `freeform`, `window` or `monitor`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "rect" | "rectangle" => Self::Rect,
            "ellipse" => Self::Ellipse,
            "freeform" | "free" => Self::Freeform,
            "window" => Self::Window,
            "monitor" | "screen" => Self::Monitor,
            _ => return None,
        })
    }
}

/// The [`RegionSelector`] implementation. See the [module docs](self).
#[derive(Debug)]
pub struct OverlaySelector {
    helper: PathBuf,
    mode: Mutex<PickMode>,
    remember_last: bool,
    timeout: Duration,
}

impl OverlaySelector {
    /// Looks for the helper (environment variable, next to the executable, `PATH`).
    /// `None` when it is missing or switched off; see [`discovery`] for the reason.
    pub fn discover() -> Option<Self> {
        discover(HELPER_BASE, HELPER_ENV).into_path().map(Self::with_helper)
    }

    /// The discovery result itself, for `ssx doctor`.
    pub fn discovery() -> Discovery {
        discover(HELPER_BASE, HELPER_ENV)
    }

    /// [`discover`](Self::discover) with injectable inputs (tests).
    pub fn discover_with(env_path: Option<PathBuf>, exe_dir: Option<PathBuf>) -> Option<Self> {
        discover_with(
            &crate::helpers::exe_name(HELPER_BASE),
            env_path.map(PathBuf::into_os_string),
            exe_dir,
            crate::command::find_in_path,
        )
        .into_path()
        .map(Self::with_helper)
    }

    /// Uses this helper program.
    pub fn with_helper(path: impl Into<PathBuf>) -> Self {
        Self {
            helper: path.into(),
            mode: Mutex::new(PickMode::Rect),
            remember_last: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Starts every selection from the previously captured region (default: off, because a
    /// drag that starts inside an existing selection moves it instead of making a new one).
    #[must_use]
    pub fn remember_last(mut self, on: bool) -> Self {
        self.remember_last = on;
        self
    }

    /// How long the overlay may stay open (default ten minutes).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets what the *next* selections ask for. The daemon lets one interactive capture run
    /// at a time, so a per-run mode can live here.
    pub fn set_mode(&self, mode: PickMode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// The current mode.
    pub fn mode(&self) -> PickMode {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The helper program in use.
    pub fn helper(&self) -> &Path {
        &self.helper
    }

    /// Runs the overlay and returns the raw outcome (no interpretation).
    pub fn run(
        &self,
        input: &OverlayInput,
        cancel: &CancelToken,
    ) -> Result<OverlayOutcome, ServiceError> {
        run_helper(&self.helper, input, self.timeout, cancel)
    }
}

/// Turns the overlay's answer into what the capturer needs. `None` = the user gave up.
pub fn outcome_to_picked(outcome: OverlayOutcome) -> Option<Picked> {
    match outcome {
        OverlayOutcome::Selected(sel) => {
            let mask = match sel.shape {
                SelectionShape::Rect => None,
                SelectionShape::Ellipse | SelectionShape::Freeform(_) => Some(sel.mask()),
            };
            Some(Picked { rect: sel.rect, mask, window: sel.snapped_window })
        }
        OverlayOutcome::Window(w) => Some(Picked { rect: w.rect, mask: None, window: Some(w) }),
        OverlayOutcome::Monitor(m) => Some(Picked::rect(m.rect)),
        // Colour picking is not enabled for captures; treat a stray answer as "no".
        OverlayOutcome::ColorPicked(_) | OverlayOutcome::Cancelled => None,
    }
}

impl RegionSelector for OverlaySelector {
    fn select(&self, monitors: &[Monitor], desktop: &Frame) -> Result<Option<Rect>, ServiceError> {
        let req = PickRequest {
            desktop,
            monitors,
            windows: &[],
            initial: None,
            cancel: &CancelToken::new(),
        };
        Ok(self.pick(&req)?.map(|p| p.rect))
    }

    fn pick(&self, req: &PickRequest<'_>) -> Result<Option<Picked>, ServiceError> {
        let mode = self.mode();
        let input = OverlayInput {
            desktop: req.desktop.clone(),
            monitors: req.monitors.to_vec(),
            windows: req.windows.to_vec(),
            options: OverlayOptions {
                mode: mode.to_overlay(),
                initial: req.initial.filter(|_| self.remember_last && mode == PickMode::Rect),
                // The colour picker is a different feature; a stray `C` must not end a capture.
                allow_color_pick: false,
                ..OverlayOptions::default()
            },
        };
        Ok(outcome_to_picked(self.run(&input, req.cancel)?))
    }
}

/// Kills and reaps the child when dropped, so no code path can leave an overlay covering the
/// screen.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn child(&mut self) -> Result<&mut Child, ServiceError> {
        self.0.as_mut().ok_or_else(|| ServiceError::failed("the overlay helper is gone"))
    }

    /// Waits up to `grace` for a clean exit, then kills.
    fn finish(mut self, grace: Duration) -> Option<std::process::ExitStatus> {
        let mut c = self.0.take()?;
        let deadline = Instant::now() + grace;
        loop {
            match c.try_wait() {
                Ok(Some(s)) => return Some(s),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = c.kill();
                    return c.wait().ok();
                }
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn overlay_failure(e: impl std::fmt::Display) -> ServiceError {
    ServiceError::failed(format!(
        "the selection overlay failed: {e}. Run `ssx doctor` to see what the desktop offers, or \
         capture an exact region with `ssx capture region --rect x,y,w,h`"
    ))
}

/// The cancellable helper client. See the module docs for why it exists.
fn run_helper(
    helper: &Path,
    input: &OverlayInput,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<OverlayOutcome, ServiceError> {
    let f = &input.desktop;
    if f.width() == 0 || f.height() == 0 {
        return Err(overlay_failure("the desktop frame is empty"));
    }
    if !matches!(f.format(), PixelFormat::Rgba8 | PixelFormat::Bgra8)
        || f.color_space() != ColorSpace::Srgb
    {
        return Err(overlay_failure(format!(
            "the overlay needs an 8-bit sRGB frame, got {:?}/{:?}",
            f.format(),
            f.color_space()
        )));
    }
    cancel.check().map_err(|_| ServiceError::Cancelled)?;
    let shared = SharedFrame::create(f).map_err(overlay_failure)?;
    let request = Request {
        version: PROTOCOL_VERSION,
        frame: FrameDesc {
            width: f.width(),
            height: f.height(),
            stride: f.stride(),
            format: f.format(),
            origin: f.origin,
            scale_factor: f.scale_factor,
            source: shared.source(),
        },
        monitors: input.monitors.clone(),
        windows: input.windows.clone(),
        options: OverlayOptions {
            timeout_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
            ..input.options.clone()
        },
    };
    let json = serde_json::to_vec(&request).map_err(overlay_failure)?;

    let child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| overlay_failure(format!("cannot start {}: {e}", helper.display())))?;
    let mut guard = ChildGuard(Some(child));

    // Feed the request from a thread: a wedged helper that never reads must not be able to
    // block us past the timeout or a cancellation.
    if let Some(mut stdin) = guard.child()?.stdin.take() {
        thread::spawn(move || {
            let _ = stdin.write_all(&json);
        });
    }
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    if let Some(mut stdout) = guard.child()?.stdout.take() {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            let _ = out_tx.send(buf);
        });
    }
    let (err_tx, err_rx) = mpsc::channel::<String>();
    if let Some(mut stderr) = guard.child()?.stderr.take() {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            let tail = &buf[buf.len().saturating_sub(2048)..];
            let _ = err_tx.send(String::from_utf8_lossy(tail).trim().to_owned());
        });
    }

    // Wait for the answer, watching the clock and the cancel token. The helper enforces
    // `timeout_ms` itself; the client's deadline is only a backstop for a wedged helper.
    let deadline = Instant::now() + timeout + Duration::from_secs(3);
    let stdout = loop {
        match out_rx.recv_timeout(CANCEL_POLL) {
            Ok(out) => break out,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled() {
                    drop(guard); // kills and reaps: the overlay disappears at once
                    return Err(ServiceError::Cancelled);
                }
                if Instant::now() >= deadline {
                    drop(guard);
                    return Err(overlay_failure(format!(
                        "the helper did not answer within {} seconds and was stopped",
                        timeout.as_secs()
                    )));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break Vec::new(),
        }
    };
    let status = guard.finish(Duration::from_secs(5));
    let stderr_tail = err_rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
    drop(shared);

    let text = String::from_utf8_lossy(&stdout);
    let Some(line) = text.lines().rev().find(|l| l.trim_start().starts_with('{')) else {
        return Err(overlay_failure(format!(
            "the helper exited with {} without an answer{}{stderr_tail}",
            status.map_or_else(|| "an unknown status".into(), |s| s.to_string()),
            if stderr_tail.is_empty() { "" } else { ": " },
        )));
    };
    match serde_json::from_str::<Response>(line) {
        Ok(Response::Outcome { outcome, .. }) => Ok(outcome),
        Ok(Response::Error { message }) => Err(overlay_failure(message)),
        Err(e) => Err(overlay_failure(format!("unreadable answer ({e}): {line}"))),
    }
}

#[cfg(test)]
mod tests {
    use ssx_overlay::{PickedColor, Selection};
    use ssx_types::{Point, WindowInfo};

    use super::*;

    fn window(id: &str, rect: Rect) -> WindowInfo {
        WindowInfo {
            id: id.into(),
            title: format!("title {id}"),
            app_name: None,
            rect,
            minimized: false,
            focused: false,
        }
    }

    fn selected(rect: Rect, shape: SelectionShape) -> OverlayOutcome {
        OverlayOutcome::Selected(Selection { rect, shape, snapped_window: None })
    }

    #[test]
    fn rectangles_have_no_mask_and_shapes_carry_theirs() {
        let r = Rect::new(-5, 3, 20, 10);
        let p = outcome_to_picked(selected(r, SelectionShape::Rect)).unwrap();
        assert_eq!((p.rect, p.mask.is_none(), p.window.is_none()), (r, true, true));

        let p = outcome_to_picked(selected(r, SelectionShape::Ellipse)).unwrap();
        let mask = p.mask.unwrap();
        assert_eq!(mask.len(), 200);
        assert_eq!(mask[0], 0, "the corner of an ellipse's box is outside it");
        assert_eq!(mask[5 * 20 + 10], 255, "its centre is inside");

        let tri = vec![Point::new(-5, 3), Point::new(14, 3), Point::new(-5, 12)];
        let p = outcome_to_picked(selected(r, SelectionShape::Freeform(tri))).unwrap();
        assert!(p.mask.unwrap().contains(&255));
    }

    #[test]
    fn windows_monitors_and_cancellation() {
        let w = window("w1", Rect::new(10, 20, 30, 40));
        let p = outcome_to_picked(OverlayOutcome::Window(w.clone())).unwrap();
        assert_eq!(p.rect, w.rect);
        assert_eq!(p.window.unwrap().title, "title w1");

        let m = Monitor {
            id: "m".into(),
            name: "m".into(),
            rect: Rect::new(1920, 0, 1280, 1024),
            scale_factor: 1.0,
            primary: false,
            refresh_hz: None,
            hdr: None,
        };
        assert_eq!(outcome_to_picked(OverlayOutcome::Monitor(m)).unwrap().rect.x, 1920);
        assert!(outcome_to_picked(OverlayOutcome::Cancelled).is_none());
        let c =
            OverlayOutcome::ColorPicked(PickedColor { point: Point::new(0, 0), rgb: [1, 2, 3] });
        assert!(outcome_to_picked(c).is_none());
    }

    #[test]
    fn modes_parse_and_map() {
        for (s, m, o) in [
            ("rect", PickMode::Rect, SelectMode::Rect),
            ("Ellipse", PickMode::Ellipse, SelectMode::Ellipse),
            ("freeform", PickMode::Freeform, SelectMode::Freeform),
            ("window", PickMode::Window, SelectMode::Window),
            ("monitor", PickMode::Monitor, SelectMode::Monitor),
        ] {
            assert_eq!(PickMode::parse(s), Some(m));
            assert_eq!(m.to_overlay(), o);
        }
        assert_eq!(PickMode::parse("triangle"), None);
        let sel = OverlaySelector::with_helper("/x");
        assert_eq!(sel.mode(), PickMode::Rect);
        sel.set_mode(PickMode::Window);
        assert_eq!(sel.mode(), PickMode::Window);
    }

    #[cfg(unix)]
    mod helper_process {
        use std::os::unix::fs::PermissionsExt;

        use ssx_overlay::demo::demo_frame;

        use super::*;

        fn fake_helper(dir: &Path, body: &str) -> PathBuf {
            let p = dir.join("fake-overlay");
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        }

        fn input() -> OverlayInput {
            OverlayInput {
                desktop: demo_frame(64, 48, Point::new(0, 0)),
                monitors: vec![],
                windows: vec![],
                options: OverlayOptions::default(),
            }
        }

        fn answer(outcome: &OverlayOutcome) -> String {
            let r = Response::Outcome {
                outcome: outcome.clone(),
                timing: ssx_overlay::protocol::Timing::default(),
            };
            format!("cat >/dev/null; printf '%s\\n' '{}'", serde_json::to_string(&r).unwrap())
        }

        #[test]
        fn a_selection_travels_through_a_real_child_process() {
            let d = tempfile::tempdir().unwrap();
            let want = selected(Rect::new(3, 4, 5, 6), SelectionShape::Rect);
            let sel = OverlaySelector::with_helper(fake_helper(d.path(), &answer(&want)));
            let got = sel
                .pick(&PickRequest {
                    desktop: &input().desktop,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &CancelToken::new(),
                })
                .unwrap()
                .unwrap();
            assert_eq!(got.rect, Rect::new(3, 4, 5, 6));
        }

        #[test]
        fn esc_is_none_errors_are_explained_and_crashes_are_reported() {
            let d = tempfile::tempdir().unwrap();
            let cancelled = OverlaySelector::with_helper(fake_helper(
                d.path(),
                &answer(&OverlayOutcome::Cancelled),
            ));
            let req = |c: &CancelToken| {
                // The frame must outlive the request: keep it in a leaked box for the test.
                let f: &'static Frame = Box::leak(Box::new(input().desktop));
                (f, c.clone())
            };
            let (f, c) = req(&CancelToken::new());
            let none = cancelled
                .pick(&PickRequest {
                    desktop: f,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &c,
                })
                .unwrap();
            assert!(none.is_none());

            let d2 = tempfile::tempdir().unwrap();
            let reported = OverlaySelector::with_helper(fake_helper(
                d2.path(),
                "cat >/dev/null; echo '{\"Error\":{\"message\":\"no display backend\"}}'",
            ));
            let e = reported
                .pick(&PickRequest {
                    desktop: f,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &c,
                })
                .unwrap_err();
            let msg = e.to_string();
            assert!(msg.contains("no display backend") && msg.contains("--rect"), "{msg}");

            let d3 = tempfile::tempdir().unwrap();
            let crashed = OverlaySelector::with_helper(fake_helper(
                d3.path(),
                "cat >/dev/null; echo boom >&2; exit 7",
            ));
            let msg = crashed
                .pick(&PickRequest {
                    desktop: f,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &c,
                })
                .unwrap_err()
                .to_string();
            assert!(msg.contains("exit status: 7") && msg.contains("boom"), "{msg}");

            let missing = OverlaySelector::with_helper("/nonexistent/ssx-overlay");
            let msg = missing
                .pick(&PickRequest {
                    desktop: f,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &c,
                })
                .unwrap_err()
                .to_string();
            assert!(msg.contains("cannot start"), "{msg}");
        }

        #[test]
        fn cancelling_kills_a_helper_that_is_waiting_for_the_user() {
            let d = tempfile::tempdir().unwrap();
            let marker = d.path().join("pid");
            let body = format!("echo $$ > {}; exec sleep 60", marker.display());
            let sel = OverlaySelector::with_helper(fake_helper(d.path(), &body));
            let cancel = CancelToken::new();
            let c2 = cancel.clone();
            let t = thread::spawn(move || {
                thread::sleep(Duration::from_millis(300));
                c2.cancel();
            });
            let start = Instant::now();
            let frame = input().desktop;
            let e = sel
                .pick(&PickRequest {
                    desktop: &frame,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &cancel,
                })
                .unwrap_err();
            t.join().unwrap();
            assert!(e.is_cancelled(), "{e}");
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
            // The helper process is really gone (not orphaned with the overlay up).
            let pid = std::fs::read_to_string(&marker).unwrap().trim().to_owned();
            thread::sleep(Duration::from_millis(100));
            assert!(!Path::new(&format!("/proc/{pid}")).exists(), "helper {pid} still running");
        }

        #[test]
        fn an_already_cancelled_token_does_not_start_the_helper() {
            let cancel = CancelToken::new();
            cancel.cancel();
            let sel = OverlaySelector::with_helper("/nonexistent/never-started");
            let frame = input().desktop;
            let e = sel
                .pick(&PickRequest {
                    desktop: &frame,
                    monitors: &[],
                    windows: &[],
                    initial: None,
                    cancel: &cancel,
                })
                .unwrap_err();
            assert!(e.is_cancelled());
        }

        #[test]
        fn the_last_region_is_only_offered_when_asked_and_only_for_rectangles() {
            // The fake helper echoes whether the request contained an `initial` region.
            let d = tempfile::tempdir().unwrap();
            let body = "cat > \"$0.in\"; printf '{\"Error\":{\"message\":\"seen\"}}\\n'";
            let path = fake_helper(d.path(), body);
            let frame = input().desktop;
            let ask = |sel: &OverlaySelector| {
                let _ = sel.pick(&PickRequest {
                    desktop: &frame,
                    monitors: &[],
                    windows: &[],
                    initial: Some(Rect::new(1, 2, 3, 4)),
                    cancel: &CancelToken::new(),
                });
                std::fs::read_to_string(format!("{}.in", path.display())).unwrap()
            };
            let off = ask(&OverlaySelector::with_helper(&path));
            assert!(off.contains("\"initial\":null"), "{off}");
            let on = ask(&OverlaySelector::with_helper(&path).remember_last(true));
            assert!(on.contains("\"initial\":{"), "{on}");
            let sel = OverlaySelector::with_helper(&path).remember_last(true);
            sel.set_mode(PickMode::Ellipse);
            assert!(ask(&sel).contains("\"initial\":null"));
            assert!(ask(&sel).contains("\"mode\":\"Ellipse\""));
        }
    }
}
