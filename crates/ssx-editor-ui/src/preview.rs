//! Live preview of whole-image effects.
//!
//! Dragging a slider must not freeze the window while a blur runs over 8 megapixels, so the
//! effect is applied to a *copy* of the document on a worker thread. Requests are coalesced:
//! while a job runs only the newest pending request is kept, so a fast drag costs at most
//! one wasted job. The canvas renders the finished preview document through its own renderer
//! (never touching the session or its history); "Apply" then commits the same effect.

use std::{
    sync::{
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
    time::{Duration, Instant},
};

use ssx_editor::{Document, Renderer};
use ssx_imgfx::Effect;

struct Job {
    generation: u64,
    doc: Document,
    effect: Effect,
}

struct Done {
    generation: u64,
    effect: Effect,
    result: Result<Document, String>,
}

/// State of the preview for the open effect dialog.
pub struct Preview {
    tx: Sender<Job>,
    rx: Receiver<Done>,
    generation: u64,
    requested: Option<Effect>,
    /// The finished preview.
    doc: Option<Arc<Document>>,
    /// The effect `doc` was computed with.
    doc_effect: Option<Effect>,
    error: Option<String>,
    /// Renders `doc`; separate from the session's so caches never mix.
    pub renderer: Renderer,
    /// Changes whenever `doc` changes, so the canvas knows to redraw.
    pub revision: u64,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview")
            .field("generation", &self.generation)
            .field("has_doc", &self.doc.is_some())
            .finish_non_exhaustive()
    }
}

impl Preview {
    /// Starts the worker thread; `wake` is called when a result is ready.
    pub fn new(wake: impl Fn() + Send + 'static) -> Self {
        let (job_tx, job_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        let spawned =
            std::thread::Builder::new().name("ssx-effect-preview".into()).spawn(move || {
                while let Ok(mut job) = job_rx.recv() {
                    // Skip to the newest request.
                    while let Ok(newer) = job_rx.try_recv() {
                        job = newer;
                    }
                    let Job { generation, mut doc, effect } = job;
                    let result =
                        doc.apply_effect(&effect, None).map(|()| doc).map_err(|e| e.to_string());
                    if done_tx.send(Done { generation, effect, result }).is_err() {
                        break;
                    }
                    wake();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("cannot start the effect preview thread: {e}");
        }
        Self {
            tx: job_tx,
            rx: done_rx,
            generation: 0,
            requested: None,
            doc: None,
            doc_effect: None,
            error: None,
            renderer: Renderer::new(),
            revision: 0,
        }
    }

    /// Asks for a preview of `effect` on a copy of `base` (ignored when it is already the
    /// latest request).
    pub fn request(&mut self, base: &Document, effect: Effect) {
        if self.requested.as_ref() == Some(&effect) {
            return;
        }
        self.generation += 1;
        self.requested = Some(effect.clone());
        let _ = self.tx.send(Job { generation: self.generation, doc: base.clone(), effect });
    }

    /// Collects finished work; returns `true` when the preview document changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.rx.try_recv() {
            if done.generation != self.generation {
                continue;
            }
            match done.result {
                Ok(doc) => {
                    self.doc = Some(Arc::new(doc));
                    self.doc_effect = Some(done.effect);
                    self.error = None;
                    self.revision += 1;
                    changed = true;
                }
                Err(e) => {
                    self.error = Some(e);
                    changed = true;
                }
            }
        }
        changed
    }

    /// `true` while the newest request has not produced a result yet.
    pub fn is_busy(&self) -> bool {
        self.requested.is_some() && self.requested != self.doc_effect && self.error.is_none()
    }

    /// The preview document, if one is ready.
    pub fn doc(&self) -> Option<&Arc<Document>> {
        self.doc.as_ref()
    }

    /// The last error, if the effect failed.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The finished document when it was computed with exactly `effect`, so "Apply" can commit
    /// it without recomputing.
    pub fn result_for(&self, effect: &Effect) -> Option<Document> {
        (self.doc_effect.as_ref() == Some(effect)).then(|| self.doc.as_deref().cloned()).flatten()
    }

    /// Forgets everything (dialog closed).
    pub fn clear(&mut self) {
        self.generation += 1;
        self.requested = None;
        self.doc = None;
        self.doc_effect = None;
        self.error = None;
        self.revision += 1;
    }

    /// `true` while a preview document exists for the canvas to show.
    pub fn active(&self) -> bool {
        self.doc.is_some()
    }

    /// Blocks until the newest request finishes (tests).
    pub fn wait_idle(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            self.poll();
            if !self.is_busy() {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use ssx_imgfx::solid_frame;

    use super::*;

    fn doc() -> Document {
        Document::new(solid_frame(64, 48, [100, 150, 200, 255])).unwrap()
    }

    #[test]
    fn preview_matches_applying_the_effect() {
        let mut p = Preview::new(|| {});
        let base = doc();
        let e = Effect::Invert;
        p.request(&base, e.clone());
        assert!(p.is_busy());
        assert!(p.wait_idle(Duration::from_secs(10)));
        let got = p.result_for(&e).expect("finished");
        let mut expect = base.clone();
        expect.apply_effect(&e, None).unwrap();
        assert_eq!(got, expect);
        assert!(p.active() && p.revision >= 1);
        assert!(p.result_for(&Effect::Sepia).is_none(), "results are keyed by effect");
    }

    #[test]
    fn only_the_newest_request_counts() {
        let mut p = Preview::new(|| {});
        let base = doc();
        for s in [1.0, 2.0, 3.0, 4.0, 5.0] {
            p.request(&base, Effect::GaussianBlur { sigma: s });
        }
        assert!(p.wait_idle(Duration::from_secs(20)));
        assert!(p.result_for(&Effect::GaussianBlur { sigma: 5.0 }).is_some());
        assert!(p.result_for(&Effect::GaussianBlur { sigma: 1.0 }).is_none());
    }

    #[test]
    fn identical_requests_are_not_repeated() {
        let mut p = Preview::new(|| {});
        let base = doc();
        p.request(&base, Effect::Grayscale);
        let g = p.generation;
        p.request(&base, Effect::Grayscale);
        assert_eq!(p.generation, g);
    }

    #[test]
    fn errors_are_reported_and_clear_resets() {
        let mut p = Preview::new(|| {});
        let base = doc();
        p.request(&base, Effect::GaussianBlur { sigma: f32::NAN });
        assert!(p.wait_idle(Duration::from_secs(10)));
        assert!(p.error().is_some());
        assert!(!p.active());
        p.request(&base, Effect::Sepia);
        assert!(p.wait_idle(Duration::from_secs(10)));
        assert!(p.error().is_none());
        p.clear();
        assert!(!p.active() && !p.is_busy());
    }

    #[test]
    fn size_changing_effects_preview_the_new_canvas() {
        let mut p = Preview::new(|| {});
        let base = doc();
        p.request(&base, Effect::Border { width: 10, color: [0, 0, 0, 255] });
        assert!(p.wait_idle(Duration::from_secs(10)));
        assert_eq!(p.doc().unwrap().image_size(), (84, 68));
    }
}
