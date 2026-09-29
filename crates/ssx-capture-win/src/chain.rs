//! The capture fallback chain (pure decision logic).
//!
//! Monitor capture tries Windows.Graphics.Capture, then DXGI Desktop Duplication, then GDI.
//! Each stage is a [`MonitorCapturer`] so the ordering and the "when do we give up"
//! rules can be tested with fakes, without a GPU. The rules:
//!
//! * A stage's failure is logged with the reason and the next stage is tried.
//! * If every stage fails, the **last** error is returned (it is the most specific about
//!   the final resort; earlier reasons are in the log).
//! * Errors that no other API could fix ([`WinError::NotFound`],
//!   [`WinError::InvalidRegion`]) stop the chain immediately.
//!
//! Window capture has its own, stricter policy ([`capture_window_with_fallback`]): if the
//! window is minimised or protected, "capture the monitor and crop" would silently return
//! *other* content (or a black rectangle), so those errors are surfaced instead.

use ssx_capture::CaptureOptions;
use ssx_types::{Frame, Monitor};

use crate::error::WinError;

/// A monitor to capture: the public description plus the native `HMONITOR` value.
///
/// The handle is carried as a plain integer so this module stays platform independent;
/// the Windows glue converts it back. It is re-resolved from the monitor id on every
/// capture, so a stale handle (monitor unplugged meanwhile) cannot survive between calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MonitorTarget {
    pub(crate) monitor: Monitor,
    /// Raw `HMONITOR`.
    pub(crate) handle: isize,
}

/// One stage of the chain.
pub(crate) trait MonitorCapturer: Send + Sync {
    /// Short name used in log lines (`"wgc"`, `"dda"`, `"gdi"`).
    fn name(&self) -> &'static str;

    fn capture(&self, target: &MonitorTarget, opts: CaptureOptions) -> Result<Frame, WinError>;
}

/// Whether the next stage should be tried after `e`.
pub(crate) fn should_fall_back(e: &WinError) -> bool {
    !matches!(e, WinError::NotFound(_) | WinError::InvalidRegion(_))
}

/// Whether "capture the monitor and crop" is an acceptable substitute after a failed
/// native window capture with `e`.
pub(crate) fn should_fall_back_for_window(e: &WinError) -> bool {
    should_fall_back(e) && !matches!(e, WinError::Minimized | WinError::Protected)
}

/// Runs `chain` in order until one stage succeeds.
pub(crate) fn capture_with_chain(
    chain: &[Box<dyn MonitorCapturer>],
    target: &MonitorTarget,
    opts: CaptureOptions,
) -> Result<Frame, WinError> {
    let mut last_error = None;
    for (i, stage) in chain.iter().enumerate() {
        match stage.capture(target, opts) {
            Ok(frame) => {
                if i > 0 {
                    tracing::info!(
                        backend = stage.name(),
                        monitor = %target.monitor.id,
                        "capture succeeded after falling back"
                    );
                }
                return Ok(frame);
            }
            Err(e) => {
                let more = i + 1 < chain.len();
                if !should_fall_back(&e) {
                    return Err(e);
                }
                tracing::warn!(
                    backend = stage.name(),
                    monitor = %target.monitor.id,
                    error = %e,
                    "capture backend failed{}",
                    if more { "; falling back to the next one" } else { "; no more backends" }
                );
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| WinError::Other("no capture backend is available".into())))
}

/// Native window capture with a monitor-crop fallback (see the module docs for when the
/// fallback is *not* taken). If both fail the fallback's error is returned.
pub(crate) fn capture_window_with_fallback(
    primary: impl FnOnce() -> Result<Frame, WinError>,
    fallback: impl FnOnce() -> Result<Frame, WinError>,
) -> Result<Frame, WinError> {
    match primary() {
        Ok(frame) => Ok(frame),
        Err(e) if should_fall_back_for_window(&e) => {
            tracing::warn!(
                error = %e,
                "window capture failed; falling back to monitor capture + crop \
                 (occluding windows will appear in the result)"
            );
            fallback()
        }
        Err(e) => Err(e),
    }
}

/// Runs `op`; if it fails because the GPU device was lost, runs it once more (the
/// operation is expected to re-create its device).
pub(crate) fn retry_once_on_device_lost<T>(
    what: &'static str,
    mut op: impl FnMut() -> Result<T, WinError>,
) -> Result<T, WinError> {
    match op() {
        Err(WinError::DeviceLost(reason)) => {
            tracing::warn!(what, %reason, "GPU device lost; retrying once with a fresh device");
            op()
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ssx_types::{ColorSpace, HdrInfo, PixelFormat, Rect, Size};

    use super::*;

    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// A capturer that records its invocation and returns a canned result.
    struct Fake {
        name: &'static str,
        log: Log,
        result: Box<dyn Fn() -> Result<Frame, WinError> + Send + Sync>,
    }

    impl MonitorCapturer for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capture(&self, _: &MonitorTarget, _: CaptureOptions) -> Result<Frame, WinError> {
            self.log.lock().unwrap().push(self.name);
            (self.result)()
        }
    }

    fn target() -> MonitorTarget {
        MonitorTarget {
            monitor: Monitor {
                id: r"\\.\DISPLAY1".into(),
                name: "Test".into(),
                rect: Rect::new(0, 0, 1, 1),
                scale_factor: 1.0,
                primary: true,
                refresh_hz: None,
                hdr: Some(HdrInfo::SDR),
            },
            handle: 1,
        }
    }

    fn frame(tag: u8) -> Frame {
        let mut f = Frame::new(Size::new(1, 1), PixelFormat::Bgra8, ColorSpace::Srgb);
        f.data_mut()[0] = tag;
        f
    }

    fn fake(
        name: &'static str,
        log: &Log,
        result: impl Fn() -> Result<Frame, WinError> + Send + Sync + 'static,
    ) -> Box<dyn MonitorCapturer> {
        Box::new(Fake { name, log: Arc::clone(log), result: Box::new(result) })
    }

    fn api_err(msg: &'static str) -> WinError {
        WinError::api("test", 0x8000_4005_u32.cast_signed(), msg)
    }

    fn calls(log: &Log) -> Vec<&'static str> {
        log.lock().unwrap().clone()
    }

    #[test]
    fn first_success_short_circuits() {
        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Ok(frame(1))),
            fake("dda", &log, || Ok(frame(2))),
            fake("gdi", &log, || Ok(frame(3))),
        ];
        let f = capture_with_chain(&chain, &target(), CaptureOptions::default()).unwrap();
        assert_eq!(f.data()[0], 1);
        assert_eq!(calls(&log), ["wgc"]);
    }

    #[test]
    fn falls_back_in_order_wgc_dda_gdi() {
        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Err(api_err("wgc down"))),
            fake("dda", &log, || Err(api_err("dda down"))),
            fake("gdi", &log, || Ok(frame(3))),
        ];
        let f = capture_with_chain(&chain, &target(), CaptureOptions::default()).unwrap();
        assert_eq!(f.data()[0], 3);
        assert_eq!(calls(&log), ["wgc", "dda", "gdi"]);
    }

    #[test]
    fn middle_stage_can_win() {
        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Err(WinError::Timeout(std::time::Duration::from_secs(1)))),
            fake("dda", &log, || Ok(frame(2))),
            fake("gdi", &log, || Ok(frame(3))),
        ];
        let f = capture_with_chain(&chain, &target(), CaptureOptions::default()).unwrap();
        assert_eq!(f.data()[0], 2);
        assert_eq!(calls(&log), ["wgc", "dda"]);
    }

    #[test]
    fn all_failing_returns_the_last_error() {
        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Err(api_err("first"))),
            fake("dda", &log, || Err(api_err("second"))),
            fake("gdi", &log, || Err(api_err("third"))),
        ];
        let e = capture_with_chain(&chain, &target(), CaptureOptions::default()).unwrap_err();
        assert!(e.to_string().contains("third"), "{e}");
        assert_eq!(calls(&log), ["wgc", "dda", "gdi"]);
    }

    #[test]
    fn unfixable_errors_stop_the_chain() {
        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Err(WinError::NotFound("x".into()))),
            fake("dda", &log, || Ok(frame(2))),
        ];
        let e = capture_with_chain(&chain, &target(), CaptureOptions::default()).unwrap_err();
        assert!(matches!(e, WinError::NotFound(_)));
        assert_eq!(calls(&log), ["wgc"]);

        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Err(WinError::InvalidRegion(Rect::default()))),
            fake("gdi", &log, || Ok(frame(3))),
        ];
        assert!(capture_with_chain(&chain, &target(), CaptureOptions::default()).is_err());
        assert_eq!(calls(&log), ["wgc"]);
    }

    #[test]
    fn unsupported_stage_is_skipped_not_fatal() {
        let log = Log::default();
        let chain = [
            fake("wgc", &log, || Err(WinError::Unsupported("monitor capture"))),
            fake("gdi", &log, || Ok(frame(3))),
        ];
        assert!(capture_with_chain(&chain, &target(), CaptureOptions::default()).is_ok());
        assert_eq!(calls(&log), ["wgc", "gdi"]);
    }

    #[test]
    fn empty_chain_is_an_error_not_a_panic() {
        let e = capture_with_chain(&[], &target(), CaptureOptions::default()).unwrap_err();
        assert!(matches!(e, WinError::Other(_)));
    }

    #[test]
    fn fall_back_predicates() {
        assert!(should_fall_back(&WinError::Minimized));
        assert!(should_fall_back(&api_err("x")));
        assert!(!should_fall_back(&WinError::NotFound("a".into())));
        assert!(!should_fall_back(&WinError::InvalidRegion(Rect::default())));

        assert!(should_fall_back_for_window(&api_err("CreateForWindow")));
        assert!(should_fall_back_for_window(&WinError::Timeout(std::time::Duration::ZERO)));
        assert!(should_fall_back_for_window(&WinError::SourceClosed));
        assert!(!should_fall_back_for_window(&WinError::Minimized));
        assert!(!should_fall_back_for_window(&WinError::Protected));
        assert!(!should_fall_back_for_window(&WinError::NotFound("a".into())));
    }

    #[test]
    fn window_capture_prefers_native_and_skips_fallback() {
        let mut fallback_called = false;
        let f = capture_window_with_fallback(
            || Ok(frame(1)),
            || {
                fallback_called = true;
                Ok(frame(2))
            },
        )
        .unwrap();
        assert_eq!(f.data()[0], 1);
        assert!(!fallback_called);
    }

    #[test]
    fn window_capture_falls_back_on_generic_failure() {
        let f = capture_window_with_fallback(|| Err(api_err("wgc")), || Ok(frame(2))).unwrap();
        assert_eq!(f.data()[0], 2);
    }

    #[test]
    fn window_capture_never_crops_the_monitor_for_minimised_or_protected() {
        for e in [WinError::Minimized, WinError::Protected] {
            let expected = e.to_string();
            let mut fallback_called = false;
            let err = capture_window_with_fallback(
                || Err(e),
                || {
                    fallback_called = true;
                    Ok(frame(2))
                },
            )
            .unwrap_err();
            assert!(!fallback_called, "{expected}");
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn window_fallback_failure_is_the_returned_error() {
        let err = capture_window_with_fallback(|| Err(api_err("native")), || Err(api_err("crop")))
            .unwrap_err();
        assert!(err.to_string().contains("crop"), "{err}");
    }

    #[test]
    fn device_lost_is_retried_exactly_once() {
        let mut attempts = 0;
        let r = retry_once_on_device_lost("t", || {
            attempts += 1;
            if attempts == 1 { Err(WinError::DeviceLost("gone".into())) } else { Ok(7) }
        });
        assert_eq!(r.unwrap(), 7);
        assert_eq!(attempts, 2);

        let mut attempts = 0;
        let r: Result<(), _> = retry_once_on_device_lost("t", || {
            attempts += 1;
            Err(WinError::DeviceLost("still gone".into()))
        });
        assert!(matches!(r, Err(WinError::DeviceLost(_))));
        assert_eq!(attempts, 2, "never a third attempt");
    }

    #[test]
    fn other_errors_are_not_retried() {
        let mut attempts = 0;
        let r: Result<(), _> = retry_once_on_device_lost("t", || {
            attempts += 1;
            Err(WinError::Minimized)
        });
        assert!(r.is_err());
        assert_eq!(attempts, 1);
        let mut attempts = 0;
        assert!(
            retry_once_on_device_lost("t", || {
                attempts += 1;
                Ok(())
            })
            .is_ok()
        );
        assert_eq!(attempts, 1);
    }
}
