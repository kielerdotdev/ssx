//! A small shared tokio runtime for the async uploaders behind the synchronous core traits.
//!
//! `ssx-core`'s service traits are synchronous (the engine runs on plain threads), while
//! `ssx-upload` is async. [`SharedRuntime::block_on_cancellable`] bridges the two: the calling
//! thread blocks, a periodic tick forwards progress to the caller (so callbacks run on the
//! *calling* thread, as the [`Uploaders`](ssx_core::workflow::Uploaders) contract promises)
//! and turns [`CancelToken`] cancellation into a tokio [`CancellationToken`] that the
//! uploaders honour by aborting the socket.
//!
//! The runtime is created lazily and shared by every call, so a multi-file `post_file` with
//! parallel uploads does not spawn a runtime (and its threads) per file.

use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use ssx_core::workflow::{CancelToken, ServiceError};
use tokio_util::sync::CancellationToken;

/// How often the blocked thread wakes up to look at the [`CancelToken`] and to flush progress.
const TICK: Duration = Duration::from_millis(25);

/// How long a cancelled future may take to unwind before it is dropped instead.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// A lazily created, shareable multi-thread runtime.
#[derive(Clone)]
pub struct SharedRuntime {
    inner: Arc<OnceLock<Result<tokio::runtime::Runtime, String>>>,
}

impl std::fmt::Debug for SharedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedRuntime").field("started", &self.inner.get().is_some()).finish()
    }
}

impl Default for SharedRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedRuntime {
    /// A runtime handle; nothing is started until the first call.
    pub fn new() -> Self {
        Self { inner: Arc::new(OnceLock::new()) }
    }

    fn runtime(&self) -> Result<&tokio::runtime::Runtime, ServiceError> {
        self.inner
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("ssx-net")
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| ServiceError::failed(format!("cannot start the network runtime: {e}")))
    }

    /// Runs the future produced by `make` to completion on the calling thread's behalf.
    ///
    /// `make` receives a token that is cancelled as soon as `cancel` is; `on_tick` runs on the
    /// calling thread roughly every 25 ms while waiting and once more at the end. If the
    /// future does not finish within ten seconds of cancellation it is dropped and
    /// [`ServiceError::Cancelled`] returned.
    pub fn block_on_cancellable<T, F>(
        &self,
        cancel: &CancelToken,
        mut on_tick: impl FnMut(),
        make: impl FnOnce(CancellationToken) -> F,
    ) -> Result<T, ServiceError>
    where
        F: Future<Output = T>,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            // `block_on` would panic here; report it instead.
            return Err(ServiceError::failed(
                "internal error: a blocking ssx service was called from inside an async runtime",
            ));
        }
        cancel.check().map_err(|_| ServiceError::Cancelled)?;
        let rt = self.runtime()?;
        let token = CancellationToken::new();
        let fut = make(token.clone());
        let out = rt.block_on(async {
            tokio::pin!(fut);
            let mut cancelled_at: Option<Instant> = None;
            let mut interval = tokio::time::interval(TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    out = &mut fut => break Some(out),
                    _ = interval.tick() => {
                        on_tick();
                        if cancel.is_cancelled() {
                            let since = *cancelled_at.get_or_insert_with(|| {
                                token.cancel();
                                Instant::now()
                            });
                            if since.elapsed() > CANCEL_GRACE {
                                break None;
                            }
                        }
                    }
                }
            }
        });
        on_tick();
        out.ok_or(ServiceError::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn returns_the_futures_output_and_ticks_on_the_calling_thread() {
        let rt = SharedRuntime::new();
        let ticks = AtomicU32::new(0);
        let caller = std::thread::current().id();
        let out = rt
            .block_on_cancellable(
                &CancelToken::new(),
                || {
                    assert_eq!(std::thread::current().id(), caller);
                    ticks.fetch_add(1, Ordering::SeqCst);
                },
                |_| async {
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    7
                },
            )
            .unwrap();
        assert_eq!(out, 7);
        assert!(ticks.load(Ordering::SeqCst) >= 3, "ticked while waiting");
    }

    #[test]
    fn cancellation_reaches_the_future_through_the_token() {
        let rt = SharedRuntime::new();
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            c2.cancel();
        });
        let started = Instant::now();
        let out = rt
            .block_on_cancellable(
                &cancel,
                || {},
                |token| async move {
                    tokio::select! {
                        () = token.cancelled() => "aborted",
                        () = tokio::time::sleep(Duration::from_secs(60)) => "finished",
                    }
                },
            )
            .unwrap();
        h.join().unwrap();
        assert_eq!(out, "aborted", "the future observed the cancellation itself");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_already_cancelled_token_never_starts_the_future() {
        let rt = SharedRuntime::new();
        let cancel = CancelToken::new();
        cancel.cancel();
        let e = rt
            .block_on_cancellable(&cancel, || {}, |_| async { panic!("must not run") })
            .unwrap_err();
        assert!(e.is_cancelled());
    }

    #[test]
    fn calling_from_inside_a_runtime_is_an_error_not_a_panic() {
        let rt = SharedRuntime::new();
        let tokio_rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let e = tokio_rt.block_on(async {
            rt.block_on_cancellable(&CancelToken::new(), || {}, |_| async { 1 }).unwrap_err()
        });
        assert!(e.to_string().contains("async runtime"), "{e}");
    }

    #[test]
    fn the_runtime_is_shared_between_clones_and_threads() {
        let rt = SharedRuntime::new();
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let rt = rt.clone();
                std::thread::spawn(move || {
                    rt.block_on_cancellable(&CancelToken::new(), || {}, |_| async move { i * 2 })
                        .unwrap()
                })
            })
            .collect();
        let mut got: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        got.sort_unstable();
        assert_eq!(got, [0, 2, 4, 6]);
    }
}
