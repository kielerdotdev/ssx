//! Cooperative cancellation.

use std::{
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Default)]
struct Inner {
    flag: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

/// A cloneable cancellation flag that can also be *waited on*, so a cancelled delay or
/// polling loop wakes up immediately instead of at its next tick.
///
/// Cancelling is one-way and idempotent. Every clone observes it.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<Inner>);

/// Returned by [`CancelToken::check`] once the token is cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

impl CancelToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels the token and wakes every waiter.
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        let _guard = self.0.lock.lock().unwrap_or_else(PoisonError::into_inner);
        self.0.cv.notify_all();
    }

    /// `true` once [`cancel`](Self::cancel) has been called on any clone.
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    /// `Err(Cancelled)` if cancelled; convenient with `?`.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() { Err(Cancelled) } else { Ok(()) }
    }

    /// Blocks for at most `timeout`; returns `true` as soon as the token is cancelled and
    /// `false` if the timeout elapsed first.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut guard = self.0.lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.is_cancelled() {
                return true;
            }
            let remaining = match deadline {
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(r) if !r.is_zero() => r,
                    _ => return false,
                },
                None => Duration::from_secs(3600),
            };
            guard = self
                .0
                .cv
                .wait_timeout(guard, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Sleeps for `duration` unless cancelled. Returns `true` if the full duration elapsed,
    /// `false` if cancelled (before or during the sleep).
    pub fn sleep(&self, duration: Duration) -> bool {
        !self.wait_timeout(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled_and_is_idempotent() {
        let t = CancelToken::new();
        assert!(!t.is_cancelled());
        assert_eq!(t.check(), Ok(()));
        t.cancel();
        t.cancel();
        assert!(t.is_cancelled());
        assert_eq!(t.check(), Err(Cancelled));
    }

    #[test]
    fn clones_share_state() {
        let a = CancelToken::new();
        let b = a.clone();
        b.cancel();
        assert!(a.is_cancelled());
    }

    #[test]
    fn sleep_completes_when_not_cancelled() {
        let t = CancelToken::new();
        let start = Instant::now();
        assert!(t.sleep(Duration::from_millis(30)));
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(t.sleep(Duration::ZERO));
    }

    #[test]
    fn sleep_returns_immediately_when_already_cancelled() {
        let t = CancelToken::new();
        t.cancel();
        let start = Instant::now();
        assert!(!t.sleep(Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancel_wakes_a_sleeper() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let h = std::thread::spawn(move || {
            let start = Instant::now();
            let completed = t2.sleep(Duration::from_secs(30));
            (completed, start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        t.cancel();
        let (completed, elapsed) = h.join().unwrap();
        assert!(!completed);
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn huge_timeouts_do_not_overflow() {
        let t = CancelToken::new();
        t.cancel();
        assert!(t.wait_timeout(Duration::MAX));
    }
}
