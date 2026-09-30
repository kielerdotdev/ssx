//! Runs the HDR preview off the UI thread.
//!
//! The UI calls [`PreviewEngine::request`] every frame with the parameters it wants. The
//! request goes through a [`Debouncer`] (so a dragged slider renders once it rests), then to
//! a single worker thread. The worker always takes the *newest* job from its queue and skips
//! the ones that were replaced while it was busy, so it never falls behind. Results come back
//! tagged with the job number; only a result newer than the one on screen is kept.
//!
//! The worker owns the scene cache (the scene depends only on the SDR-white level), so the
//! scene is generated once per level, not per slider change.

use std::{
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, Instant},
};

use ssx_core::settings::HdrConfig;

use crate::{
    debounce::Debouncer,
    hdr_scene::{Rendered, build_scene, render_all},
};

/// Everything a preview depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewParams {
    /// The settings being edited.
    pub config: HdrConfig,
    /// The SDR-white level the scene is built for, in nits.
    pub sdr_white_nits: f32,
}

/// A finished preview.
#[derive(Debug, Clone)]
pub struct PreviewResult {
    /// What it was rendered for.
    pub params: PreviewParams,
    /// The three renderings and their numbers, or why rendering failed.
    pub rendered: Result<Rendered, String>,
    /// How long the worker took.
    pub took: Duration,
}

struct Job {
    id: u64,
    params: PreviewParams,
}

struct Done {
    id: u64,
    result: PreviewResult,
}

/// Debounced, coalescing background renderer. See the [module docs](self).
pub struct PreviewEngine {
    debounce: Debouncer<PreviewParams>,
    jobs: Option<Sender<Job>>,
    results: Receiver<Done>,
    next_id: u64,
    shown_id: u64,
    sent: u64,
    last_sent: Option<PreviewParams>,
    latest: Option<Arc<PreviewResult>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for PreviewEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewEngine")
            .field("pending", &self.debounce.is_pending())
            .field("in_flight", &self.in_flight())
            .field("has_result", &self.latest.is_some())
            .finish()
    }
}

/// Default wait after the last change.
pub const DEFAULT_DELAY: Duration = Duration::from_millis(120);

impl PreviewEngine {
    /// Starts the worker. `wake` is called (from the worker thread) whenever a result is
    /// ready, typically `ctx.request_repaint()`.
    pub fn new(delay: Duration, wake: impl Fn() + Send + 'static) -> Self {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let handle = std::thread::Builder::new()
            .name("ssx-hdr-preview".to_owned())
            .spawn(move || worker(&jobs_rx, &done_tx, &wake))
            .ok();
        Self {
            debounce: Debouncer::new(delay),
            jobs: handle.is_some().then_some(jobs_tx),
            results: done_rx,
            next_id: 1,
            shown_id: 0,
            sent: 0,
            last_sent: None,
            latest: None,
            handle,
        }
    }

    /// Asks for a preview of `params`. Cheap; call it every frame.
    ///
    /// The very first request is not delayed (the page should not open empty).
    pub fn request(&mut self, params: PreviewParams, now: Instant) {
        if self.last_sent == Some(params) && !self.debounce.is_pending() {
            return;
        }
        if self.sent == 0 && self.latest.is_none() {
            self.debounce.cancel();
            self.dispatch(params);
            return;
        }
        if self.last_sent == Some(params) {
            // Back to the value that is already rendered or rendering: drop the newer one.
            self.debounce.cancel();
            return;
        }
        self.debounce.submit(params, now);
    }

    fn dispatch(&mut self, params: PreviewParams) {
        let id = self.next_id;
        self.next_id += 1;
        self.sent += 1;
        self.last_sent = Some(params);
        if let Some(tx) = &self.jobs {
            let _ = tx.send(Job { id, params });
        }
    }

    /// Sends a due job to the worker and collects finished ones. Returns when to call again
    /// (`None` = only when something changes).
    pub fn tick(&mut self, now: Instant) -> Option<Duration> {
        if let Some(p) = self.debounce.take_due(now) {
            self.dispatch(p);
        }
        while let Ok(done) = self.results.try_recv() {
            if done.id > self.shown_id {
                self.shown_id = done.id;
                self.latest = Some(Arc::new(done.result));
            }
        }
        self.debounce.time_until_due(now)
    }

    /// The newest finished preview.
    pub fn latest(&self) -> Option<&Arc<PreviewResult>> {
        self.latest.as_ref()
    }

    /// Whether a job is with the worker.
    pub fn in_flight(&self) -> bool {
        self.shown_id < self.next_id - 1
    }

    /// Whether a change is waiting for the debounce or the worker (the UI shows a spinner).
    pub fn busy(&self) -> bool {
        self.debounce.is_pending() || self.in_flight()
    }

    /// Blocks until nothing is pending or in flight, or `timeout` passes (tests).
    pub fn wait_idle(&mut self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            let _ = self.tick(now);
            if let Some(p) = self.debounce.take_now() {
                self.dispatch(p);
            }
            if !self.busy() {
                return true;
            }
            if now >= end {
                return false;
            }
            match self.results.recv_timeout(Duration::from_millis(10)) {
                Ok(done) => {
                    if done.id > self.shown_id {
                        self.shown_id = done.id;
                        self.latest = Some(Arc::new(done.result));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return !self.busy(),
            }
        }
    }
}

impl Drop for PreviewEngine {
    fn drop(&mut self) {
        // Closing the channel ends the worker; do not block the UI thread on a render.
        self.jobs = None;
        drop(self.handle.take());
    }
}

fn worker(jobs: &Receiver<Job>, done: &Sender<Done>, wake: &dyn Fn()) {
    let mut scene: Option<(u32, crate::hdr_scene::Scene)> = None;
    while let Ok(mut job) = jobs.recv() {
        // Skip everything that was replaced while we were busy.
        while let Ok(newer) = jobs.try_recv() {
            job = newer;
        }
        let t0 = Instant::now();
        let bits = job.params.sdr_white_nits.to_bits();
        if scene.as_ref().is_none_or(|(b, _)| *b != bits) {
            scene = Some((bits, build_scene(job.params.sdr_white_nits)));
        }
        let rendered = match &scene {
            Some((_, s)) => render_all(s, &job.params.config).map_err(|e| e.to_string()),
            None => Err("no scene".to_owned()),
        };
        let result = PreviewResult { params: job.params, rendered, took: t0.elapsed() };
        if done.send(Done { id: job.id, result }).is_err() {
            return;
        }
        wake();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn params(knee: f32) -> PreviewParams {
        PreviewParams { config: HdrConfig { knee, ..HdrConfig::faithful() }, sdr_white_nits: 200.0 }
    }

    #[test]
    fn first_request_renders_immediately_and_wakes_the_ui() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let mut e = PreviewEngine::new(Duration::from_secs(60), move || {
            w.fetch_add(1, Ordering::SeqCst);
        });
        e.request(params(1.0), Instant::now());
        assert!(e.busy());
        assert!(e.wait_idle(Duration::from_secs(30)));
        let r = e.latest().expect("a result");
        assert_eq!(r.params, params(1.0));
        assert!(r.rendered.as_ref().unwrap().readouts[0].ui_untouched());
        // the worker wakes the UI right after delivering, so the count can trail by a moment
        let end = Instant::now() + Duration::from_secs(5);
        while woken.load(Ordering::SeqCst) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(woken.load(Ordering::SeqCst) >= 1);
        assert!(!e.busy());
    }

    #[test]
    fn a_burst_of_changes_is_coalesced_and_only_the_last_is_shown() {
        let mut e = PreviewEngine::new(Duration::from_millis(50), || {});
        let t0 = Instant::now();
        e.request(params(1.0), t0);
        for (i, k) in [0.9, 0.8, 0.7, 0.6, 0.5].into_iter().enumerate() {
            e.request(params(k), t0 + Duration::from_millis(i as u64 + 1));
        }
        assert!(e.wait_idle(Duration::from_secs(60)));
        let r = e.latest().unwrap();
        assert_eq!(r.params, params(0.5), "the newest parameters win");
        // at most: the first immediate job + one for the burst
        assert!(e.sent <= 2, "sent {} jobs", e.sent);
    }

    #[test]
    fn unchanged_parameters_do_not_render_again() {
        let mut e = PreviewEngine::new(Duration::from_millis(1), || {});
        let p = params(1.0);
        e.request(p, Instant::now());
        assert!(e.wait_idle(Duration::from_secs(30)));
        let sent = e.sent;
        for _ in 0..10 {
            e.request(p, Instant::now());
            let _ = e.tick(Instant::now());
        }
        assert_eq!(e.sent, sent);
        assert!(!e.busy());
    }

    #[test]
    fn returning_to_the_rendered_value_cancels_the_pending_one() {
        let mut e = PreviewEngine::new(Duration::from_secs(60), || {});
        let t0 = Instant::now();
        e.request(params(1.0), t0);
        assert!(e.wait_idle(Duration::from_secs(30)));
        e.request(params(0.5), t0);
        assert!(e.busy(), "waiting for the debounce");
        e.request(params(1.0), t0);
        assert!(!e.busy(), "back to what is on screen: nothing to do");
    }

    #[test]
    fn tick_reports_when_to_come_back() {
        let mut e = PreviewEngine::new(Duration::from_millis(500), || {});
        let t0 = Instant::now();
        e.request(params(1.0), t0);
        assert!(e.wait_idle(Duration::from_secs(30)));
        e.request(params(0.4), t0);
        let wait = e.tick(t0 + Duration::from_millis(100)).unwrap();
        assert!(wait <= Duration::from_millis(400) && wait > Duration::ZERO, "{wait:?}");
        assert_eq!(e.tick(t0 + Duration::from_secs(1)), None, "dispatched; nothing waits any more");
    }

    #[test]
    fn dropping_the_engine_does_not_hang() {
        let mut e = PreviewEngine::new(DEFAULT_DELAY, || {});
        e.request(params(1.0), Instant::now());
        drop(e);
    }

    #[test]
    fn different_sdr_white_levels_are_rendered_separately() {
        let mut e = PreviewEngine::new(Duration::from_millis(1), || {});
        e.request(params(1.0), Instant::now());
        assert!(e.wait_idle(Duration::from_secs(30)));
        let mut p = params(1.0);
        p.sdr_white_nits = 480.0;
        e.request(p, Instant::now());
        assert!(e.wait_idle(Duration::from_secs(30)));
        assert!((e.latest().unwrap().params.sdr_white_nits - 480.0).abs() < 1e-6);
        assert!(e.latest().unwrap().rendered.as_ref().unwrap().readouts[0].ui_untouched());
    }
}
