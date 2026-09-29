//! Retry policy for DXGI Desktop Duplication's first frame (pure logic).
//!
//! `IDXGIOutputDuplication::AcquireNextFrame` is awkward for a *screenshot*, which needs
//! exactly one complete image:
//!
//! * it blocks until the desktop *changes*, so on a static screen it can time out;
//! * the first frame often carries only a pointer update and no image
//!   (`LastPresentTime == 0`), so it must be released and another one requested;
//! * `DXGI_ERROR_ACCESS_LOST` (mode change, desktop switch) invalidates the duplication,
//!   which must then be re-created, but only a bounded number of times so a flapping
//!   display cannot loop forever.
//!
//! [`classify`] and [`next_step`] turn each call's result into a decision so the loop in
//! `dda.rs` is trivial.

use crate::error::hresult;

/// How long a single `AcquireNextFrame` call waits, in milliseconds.
pub(crate) const ACQUIRE_TIMEOUT_MS: u32 = 100;
/// Total `AcquireNextFrame` calls per duplication before giving up (~2 s worst case).
pub(crate) const MAX_ATTEMPTS: u32 = 20;
/// How many times a lost duplication is re-created within one capture.
pub(crate) const MAX_RECREATES: u32 = 1;

/// What one `AcquireNextFrame` call produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A frame that contains a desktop image.
    Image,
    /// A frame with no image (pointer-only update); it must be released.
    NoImage,
    /// `DXGI_ERROR_WAIT_TIMEOUT`.
    Timeout,
    /// `DXGI_ERROR_ACCESS_LOST`.
    AccessLost,
    /// Any other failure.
    Failed,
}

/// What the acquisition loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The image is in hand.
    Done,
    /// Ask for another frame.
    Retry,
    /// Drop the duplication, create a new one, and ask again.
    Recreate,
    /// Give up with an error.
    Fail,
}

/// Whether frame metadata describes a frame that carries a desktop image.
/// (`DXGI_OUTDUPL_FRAME_INFO::LastPresentTime` is zero for pointer-only updates.)
pub(crate) fn frame_has_image(last_present_time: i64) -> bool {
    last_present_time != 0
}

/// Classifies the result of one `AcquireNextFrame` call: `Ok(last_present_time)` on
/// success, `Err(hresult)` on failure.
pub(crate) fn classify(result: Result<i64, i32>) -> Outcome {
    match result {
        Ok(last_present_time) if frame_has_image(last_present_time) => Outcome::Image,
        Ok(_) => Outcome::NoImage,
        Err(code) if code == hresult::DXGI_ERROR_WAIT_TIMEOUT => Outcome::Timeout,
        Err(code) if code == hresult::DXGI_ERROR_ACCESS_LOST => Outcome::AccessLost,
        Err(_) => Outcome::Failed,
    }
}

/// Decides the next step. `attempts` counts `AcquireNextFrame` calls made so far
/// *including* the one that produced `outcome`; `recreates` counts re-creations so far.
pub(crate) fn next_step(outcome: Outcome, attempts: u32, recreates: u32) -> Step {
    match outcome {
        Outcome::Image => Step::Done,
        Outcome::AccessLost if recreates < MAX_RECREATES => Step::Recreate,
        Outcome::NoImage | Outcome::Timeout if attempts < MAX_ATTEMPTS => Step::Retry,
        // Budget exhausted, or an error that retrying cannot fix.
        Outcome::Failed | Outcome::AccessLost | Outcome::NoImage | Outcome::Timeout => Step::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_ends_the_loop_immediately() {
        assert_eq!(next_step(Outcome::Image, 1, 0), Step::Done);
        assert_eq!(next_step(Outcome::Image, MAX_ATTEMPTS, MAX_RECREATES), Step::Done);
    }

    #[test]
    fn pointer_only_and_timeout_retry_until_the_budget_is_spent() {
        for o in [Outcome::NoImage, Outcome::Timeout] {
            assert_eq!(next_step(o, 1, 0), Step::Retry, "{o:?}");
            assert_eq!(next_step(o, MAX_ATTEMPTS - 1, 0), Step::Retry, "{o:?}");
            assert_eq!(next_step(o, MAX_ATTEMPTS, 0), Step::Fail, "{o:?}");
            assert_eq!(next_step(o, MAX_ATTEMPTS + 5, 0), Step::Fail, "{o:?}");
        }
    }

    #[test]
    fn access_lost_recreates_once_then_fails() {
        assert_eq!(next_step(Outcome::AccessLost, 1, 0), Step::Recreate);
        assert_eq!(next_step(Outcome::AccessLost, 3, MAX_RECREATES), Step::Fail);
    }

    #[test]
    fn hard_failures_are_not_retried() {
        assert_eq!(next_step(Outcome::Failed, 1, 0), Step::Fail);
    }

    #[test]
    fn a_full_sequence_terminates() {
        // Simulate: pointer-only, timeout, access lost, then a good image.
        let script = [Outcome::NoImage, Outcome::Timeout, Outcome::AccessLost, Outcome::Image];
        let (mut attempts, mut recreates) = (0, 0);
        let mut last = Step::Retry;
        for o in script {
            attempts += 1;
            last = next_step(o, attempts, recreates);
            match last {
                Step::Recreate => recreates += 1,
                Step::Done | Step::Fail => break,
                Step::Retry => {}
            }
        }
        assert_eq!(last, Step::Done);
        assert_eq!(recreates, 1);
    }

    #[test]
    fn worst_case_wall_time_is_bounded() {
        assert!(u64::from(MAX_ATTEMPTS) * u64::from(ACQUIRE_TIMEOUT_MS) <= 3000);
    }

    #[test]
    fn classifies_acquire_results() {
        assert_eq!(classify(Ok(42)), Outcome::Image);
        assert_eq!(classify(Ok(0)), Outcome::NoImage, "pointer-only update");
        assert_eq!(classify(Err(hresult::DXGI_ERROR_WAIT_TIMEOUT)), Outcome::Timeout);
        assert_eq!(classify(Err(hresult::DXGI_ERROR_ACCESS_LOST)), Outcome::AccessLost);
        assert_eq!(classify(Err(hresult::E_ACCESSDENIED)), Outcome::Failed);
        assert_eq!(classify(Err(hresult::DXGI_ERROR_DEVICE_REMOVED)), Outcome::Failed);
    }

    #[test]
    fn present_time_zero_means_no_image() {
        assert!(!frame_has_image(0));
        assert!(frame_has_image(1));
        assert!(frame_has_image(123_456_789));
        assert!(frame_has_image(-1));
    }
}
