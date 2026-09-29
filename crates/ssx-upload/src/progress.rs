//! Upload progress reporting.
//!
//! Uploaders call [`ProgressSink::report`] from the body stream, which can happen thousands
//! of times per second for a fast link. [`ThrottledProgress`] therefore sits between the
//! stream and the UI: it forwards at most one update per interval, always forwards the final
//! (`sent == total`) update, and guarantees the values it forwards never go backwards.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Receiver of progress updates. Implementations must be cheap and non-blocking.
pub trait ProgressSink: Send + Sync {
    /// `sent` bytes of the request body have been handed to the HTTP client; `total` is the
    /// full body length when known. Within one attempt `sent` never decreases.
    fn report(&self, sent: u64, total: Option<u64>);

    /// A (re)try starts; `attempt` is 1-based. Progress restarts from zero after this.
    fn attempt_started(&self, _attempt: u32) {}
}

/// Discards all progress.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullProgress;

impl ProgressSink for NullProgress {
    fn report(&self, _sent: u64, _total: Option<u64>) {}
}

/// Forwards to an inner sink at most once per `interval`, plus always at completion.
pub struct ThrottledProgress {
    inner: Arc<dyn ProgressSink>,
    interval: Duration,
    state: Mutex<State>,
}

impl std::fmt::Debug for ThrottledProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThrottledProgress").field("interval", &self.interval).finish()
    }
}

#[derive(Default)]
struct State {
    last_emit: Option<Instant>,
    max_seen: u64,
    emitted: Option<u64>,
}

impl ThrottledProgress {
    /// Wrap `inner`, forwarding at most one update per `interval`.
    pub fn new(inner: Arc<dyn ProgressSink>, interval: Duration) -> Self {
        Self { inner, interval, state: Mutex::new(State::default()) }
    }
}

impl ProgressSink for ThrottledProgress {
    fn report(&self, sent: u64, total: Option<u64>) {
        let now = Instant::now();
        let emit = {
            let Ok(mut st) = self.state.lock() else { return };
            st.max_seen = st.max_seen.max(sent);
            let sent = st.max_seen;
            let done = total.is_some_and(|t| sent >= t);
            let due = st.last_emit.is_none_or(|t| now.duration_since(t) >= self.interval);
            if (due || done) && st.emitted != Some(sent) {
                st.last_emit = Some(now);
                st.emitted = Some(sent);
                Some(sent)
            } else {
                None
            }
        };
        if let Some(sent) = emit {
            self.inner.report(sent, total);
        }
    }

    fn attempt_started(&self, attempt: u32) {
        if let Ok(mut st) = self.state.lock() {
            *st = State::default();
        }
        self.inner.attempt_started(attempt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec(Mutex<Vec<(u64, Option<u64>)>>);
    impl ProgressSink for Rec {
        fn report(&self, sent: u64, total: Option<u64>) {
            self.0.lock().unwrap().push((sent, total));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn throttles_but_always_emits_final() {
        let rec = Arc::new(Rec::default());
        let t = ThrottledProgress::new(rec.clone(), Duration::from_millis(100));
        for i in 1..=49u64 {
            t.report(i * 2, Some(100));
        }
        {
            let v = rec.0.lock().unwrap();
            assert_eq!(v.len(), 1, "burst within one interval collapses to one update");
        }
        tokio::time::advance(Duration::from_millis(150)).await;
        t.report(99, Some(100));
        t.report(100, Some(100));
        let v = rec.0.lock().unwrap();
        assert_eq!(v.last(), Some(&(100, Some(100))));
        assert!(v.windows(2).all(|w| w[0].0 <= w[1].0), "monotonic: {v:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn never_goes_backwards() {
        let rec = Arc::new(Rec::default());
        let t = ThrottledProgress::new(rec.clone(), Duration::ZERO);
        t.report(10, None);
        t.report(5, None);
        t.report(20, None);
        let v = rec.0.lock().unwrap();
        assert!(v.windows(2).all(|w| w[0].0 <= w[1].0), "{v:?}");
    }
}
