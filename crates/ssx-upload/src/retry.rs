//! Retry wrapper: exponential backoff with jitter that honours `Retry-After`.
//!
//! Only errors for which [`UploadError::is_retryable`] holds are retried, and only when the
//! request body can be produced again ([`crate::UploadSource::is_replayable`]). A server
//! asking us to wait longer than [`RetryPolicy::max_retry_after`] is treated as a final
//! failure rather than parking the upload for an hour. Sleeping races the cancellation
//! token, so cancelling during a backoff returns immediately.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng as _;

use crate::context::UploadContext;
use crate::error::UploadError;
use crate::types::{UploadKind, UploadRequest, UploadResult, Uploader};

/// How randomness is mixed into the backoff delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jitter {
    /// Deterministic delays (tests).
    None,
    /// Uniform in `[0, delay]` ("full jitter": best at spreading load).
    Full,
    /// Uniform in `[delay/2, delay]`.
    Equal,
}

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first (1 disables retries).
    pub max_attempts: u32,
    /// Delay before the first retry.
    pub initial_backoff: Duration,
    /// Upper bound for the computed backoff.
    pub max_backoff: Duration,
    /// Growth factor per retry.
    pub multiplier: f64,
    /// Jitter mode.
    pub jitter: Jitter,
    /// Longest `Retry-After` we are willing to sleep; longer hints abort the upload.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            multiplier: 2.0,
            jitter: Jitter::Full,
            max_retry_after: Duration::from_secs(300),
        }
    }
}

impl RetryPolicy {
    /// Delay before retry number `retry` (1 = first retry) after `err`, or `None` when the
    /// upload should fail now (non-retryable error, or an unreasonable `Retry-After`).
    pub fn delay_for(&self, retry: u32, err: &UploadError) -> Option<Duration> {
        if !err.is_retryable() {
            return None;
        }
        let exp =
            self.multiplier.max(1.0).powi(i32::try_from(retry.saturating_sub(1)).unwrap_or(30));
        let base = self.initial_backoff.as_secs_f64() * exp;
        let base = Duration::from_secs_f64(base.min(self.max_backoff.as_secs_f64()).max(0.0));
        let jittered = match self.jitter {
            Jitter::None => base,
            Jitter::Full => base.mul_f64(rand::rng().random_range(0.0..=1.0)),
            Jitter::Equal => base.mul_f64(rand::rng().random_range(0.5..=1.0)),
        };
        match err.retry_after() {
            Some(hint) if hint > self.max_retry_after => None,
            Some(hint) => Some(hint.max(jittered)),
            None => Some(jittered),
        }
    }
}

/// Wraps an [`Uploader`] with a [`RetryPolicy`].
pub struct RetryingUploader {
    inner: Arc<dyn Uploader>,
    policy: RetryPolicy,
}

impl std::fmt::Debug for RetryingUploader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryingUploader")
            .field("inner", &self.inner.name())
            .field("policy", &self.policy)
            .finish()
    }
}

impl RetryingUploader {
    /// Wrap `inner`.
    pub fn new(inner: Arc<dyn Uploader>, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

#[async_trait]
impl Uploader for RetryingUploader {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn supports(&self, kind: UploadKind) -> bool {
        self.inner.supports(kind)
    }

    async fn upload(
        &self,
        req: &UploadRequest,
        ctx: &UploadContext,
    ) -> Result<UploadResult, UploadError> {
        let mut attempt: u32 = 1;
        loop {
            ctx.check_cancelled()?;
            ctx.progress.attempt_started(attempt);
            let err = match self.inner.upload(req, ctx).await {
                Ok(r) => return Ok(r),
                Err(e) => e,
            };
            if attempt >= self.policy.max_attempts || !req.source.is_replayable() {
                return Err(err);
            }
            let Some(delay) = self.policy.delay_for(attempt, &err) else {
                return Err(err);
            };
            tracing::warn!(
                uploader = self.inner.name(),
                attempt,
                ?delay,
                error = %err,
                "upload failed, retrying"
            );
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => return Err(UploadError::Cancelled),
                () = tokio::time::sleep(delay) => {}
            }
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::time::Instant;

    struct Scripted {
        script: Mutex<Vec<Result<(), UploadError>>>,
        calls: Mutex<Vec<Instant>>,
    }

    #[async_trait]
    impl Uploader for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn supports(&self, _kind: UploadKind) -> bool {
            true
        }
        async fn upload(
            &self,
            _req: &UploadRequest,
            _ctx: &UploadContext,
        ) -> Result<UploadResult, UploadError> {
            self.calls.lock().unwrap().push(Instant::now());
            let next = self.script.lock().unwrap().remove(0);
            next.map(|()| UploadResult { url: "ok".into(), ..Default::default() })
        }
    }

    fn scripted(script: Vec<Result<(), UploadError>>) -> Arc<Scripted> {
        Arc::new(Scripted { script: Mutex::new(script), calls: Mutex::new(vec![]) })
    }

    fn net() -> UploadError {
        UploadError::Network { message: "reset".into(), timed_out: false }
    }

    fn policy() -> RetryPolicy {
        RetryPolicy { jitter: Jitter::None, ..RetryPolicy::default() }
    }

    fn ctx() -> UploadContext {
        UploadContext::new(crate::http::build_http_client().unwrap())
    }

    #[tokio::test(start_paused = true)]
    async fn exponential_backoff_without_jitter() {
        let s = scripted(vec![Err(net()), Err(net()), Err(net()), Ok(())]);
        let up = RetryingUploader::new(s.clone(), policy());
        let r = up.upload(&UploadRequest::text("x"), &ctx()).await.unwrap();
        assert_eq!(r.url, "ok");
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        let gaps: Vec<_> = calls.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(
            gaps,
            vec![Duration::from_millis(500), Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn honours_retry_after_over_backoff() {
        let rl = UploadError::RateLimited { retry_after: Some(Duration::from_secs(7)) };
        let s = scripted(vec![Err(rl), Ok(())]);
        let up = RetryingUploader::new(s.clone(), policy());
        up.upload(&UploadRequest::text("x"), &ctx()).await.unwrap();
        let calls = s.calls.lock().unwrap();
        assert_eq!(calls[1] - calls[0], Duration::from_secs(7));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_on_absurd_retry_after() {
        let rl = UploadError::RateLimited { retry_after: Some(Duration::from_secs(3600)) };
        let s = scripted(vec![Err(rl)]);
        let up = RetryingUploader::new(s.clone(), policy());
        let e = up.upload(&UploadRequest::text("x"), &ctx()).await.unwrap_err();
        assert!(matches!(e, UploadError::RateLimited { .. }));
        assert_eq!(s.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stops_at_max_attempts_and_skips_non_retryable() {
        let s = scripted(vec![Err(net()), Err(net()), Err(net()), Err(net())]);
        let up = RetryingUploader::new(s.clone(), policy());
        assert!(up.upload(&UploadRequest::text("x"), &ctx()).await.is_err());
        assert_eq!(s.calls.lock().unwrap().len(), 4);

        let s = scripted(vec![Err(UploadError::Auth { message: "no".into() })]);
        let up = RetryingUploader::new(s.clone(), policy());
        assert!(matches!(
            up.upload(&UploadRequest::text("x"), &ctx()).await,
            Err(UploadError::Auth { .. })
        ));
        assert_eq!(s.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_backoff() {
        let s = scripted(vec![Err(net()), Ok(())]);
        let up = RetryingUploader::new(s, policy());
        let c = ctx();
        let token = c.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            token.cancel();
        });
        let e = up.upload(&UploadRequest::text("x"), &c).await.unwrap_err();
        assert!(e.is_cancelled());
    }

    #[test]
    fn jitter_stays_within_bounds_and_backoff_is_capped() {
        let p = RetryPolicy { jitter: Jitter::Full, ..RetryPolicy::default() };
        for retry in 1..40 {
            let d = p.delay_for(retry, &net()).unwrap();
            assert!(d <= p.max_backoff, "{d:?}");
        }
        let p = RetryPolicy { jitter: Jitter::Equal, ..RetryPolicy::default() };
        let d = p.delay_for(1, &net()).unwrap();
        assert!(d >= Duration::from_millis(250) && d <= Duration::from_millis(500));
    }
}
