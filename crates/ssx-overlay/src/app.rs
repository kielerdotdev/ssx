//! The glue every backend shares: model + renderer + damage tracking + outcome resolution.
//!
//! A backend's whole job is then: translate native events into [`InputEvent`]s, call
//! [`OverlayApp::begin_frame`] once per batch of events, render the returned damage into
//! whatever buffers it owns with [`OverlayApp::render`], and stop when
//! [`OverlayApp::outcome`] is `Some`.

use std::time::{Duration, Instant};

use ssx_types::{Monitor, Rect, WindowInfo};

use crate::error::OverlayError;
use crate::model::damage;
use crate::model::events::InputEvent;
use crate::model::scene::{CursorHint, Scene};
use crate::model::state::{Finish, FinishShape, ModelConfig, SelectionModel};
use crate::render::{PixelView, Renderer, TargetBuf};
use crate::types::{
    OverlayInput, OverlayOptions, OverlayOutcome, PickedColor, Selection, SelectionShape, UiScale,
};

/// One overlay session.
#[derive(Debug)]
pub struct OverlayApp {
    model: SelectionModel,
    renderer: Renderer,
    windows: Vec<WindowInfo>,
    current: Scene,
    prev: Option<Scene>,
    options: OverlayOptions,
    started: Instant,
    first_frame: Option<Instant>,
}

impl OverlayApp {
    /// Builds the session; consumes the frame (its pixels move into the renderer's
    /// pre-dimmed buffers and the original is dropped).
    pub fn new(input: OverlayInput) -> Result<Self, OverlayError> {
        let OverlayInput { desktop, monitors, windows, options } = input;
        Self::from_view(PixelView::of(&desktop), monitors, windows, options)
    }

    /// Builds the session from borrowed pixels (helper process: memory-mapped request).
    pub fn from_view(
        view: PixelView<'_>,
        monitors: Vec<Monitor>,
        windows: Vec<WindowInfo>,
        options: OverlayOptions,
    ) -> Result<Self, OverlayError> {
        let renderer = Renderer::from_view(view, options.dim)?;
        let scale = if view.scale_factor.is_finite() { view.scale_factor as f32 } else { 1.0 };
        // `Auto`: a frame that carries a desktop scale above 1 (Wayland's model, where desktop
        // pixels are the logical layout times one global factor) uses it everywhere; a frame
        // at 1.0 (Windows, X11: desktop pixels are physical) follows each monitor's own DPI.
        let mut model_options = options.clone();
        if model_options.ui_scale == UiScale::Auto && scale <= 1.0 + f32::EPSILON {
            model_options.ui_scale = UiScale::PerMonitor;
        }
        let cfg = ModelConfig::new(view.rect(), monitors, windows.clone(), model_options, scale);
        let model = SelectionModel::new(cfg);
        let current = model.scene();
        let started = Instant::now();
        Ok(Self {
            model,
            renderer,
            windows,
            current,
            prev: None,
            options,
            started,
            first_frame: None,
        })
    }

    /// Marks the moment the first frame reached the screen (for start-up timing).
    pub fn mark_first_frame(&mut self) {
        self.first_frame.get_or_insert_with(Instant::now);
    }

    /// Time from session creation to the first presented frame, if it happened.
    pub fn first_frame_after(&self) -> Option<Duration> {
        self.first_frame.map(|t| t.duration_since(self.started))
    }

    /// When the session should give up, from `options.timeout_ms`.
    pub fn deadline(&self) -> Option<Instant> {
        self.options.timeout_ms.map(|ms| self.started + Duration::from_millis(ms))
    }

    /// Options the session runs with.
    pub fn options(&self) -> &OverlayOptions {
        &self.options
    }

    /// Desktop bounds.
    pub fn bounds(&self) -> Rect {
        self.model.bounds()
    }

    /// Monitors (never empty).
    pub fn monitors(&self) -> &[Monitor] {
        self.model.monitors()
    }

    /// Feeds an event.
    pub fn handle(&mut self, ev: InputEvent) {
        self.model.handle(ev);
    }

    /// Ends the session as cancelled (timeout, window closed by the compositor).
    pub fn cancel(&mut self) {
        self.model.cancel();
    }

    /// Cursor shape the backend should show.
    pub fn cursor_hint(&self) -> CursorHint {
        self.current.cursor
    }

    /// Snapshots the scene and returns the desktop-space rectangles that changed since the
    /// previous call (the whole desktop on the first call). Render them with
    /// [`OverlayApp::render`].
    pub fn begin_frame(&mut self) -> Vec<Rect> {
        self.current = self.model.scene();
        let dirty = damage::between(self.prev.as_ref(), &self.current);
        self.prev = Some(self.current.clone());
        dirty
    }

    /// Renders the current scene into `area` of `target`.
    pub fn render(&mut self, area: Rect, target: &mut TargetBuf<'_>) {
        self.renderer.render(&self.current, area, target);
    }

    /// The renderer (pixel lookups, benchmarks).
    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// The result, once the interaction has ended.
    pub fn outcome(&self) -> Option<OverlayOutcome> {
        let f = self.model.finished()?;
        Some(match f {
            Finish::Cancelled => OverlayOutcome::Cancelled,
            Finish::Selected { rect, shape, window } => OverlayOutcome::Selected(Selection {
                rect: *rect,
                shape: match shape {
                    FinishShape::Rect => SelectionShape::Rect,
                    FinishShape::Ellipse => SelectionShape::Ellipse,
                    FinishShape::Freeform(p) => SelectionShape::Freeform(p.clone()),
                },
                snapped_window: window.and_then(|i| self.windows.get(i).cloned()),
            }),
            Finish::Window(i) => match self.windows.get(*i) {
                Some(w) => OverlayOutcome::Window(w.clone()),
                None => OverlayOutcome::Cancelled,
            },
            Finish::Monitor(i) => match self.model.monitors().get(*i) {
                Some(m) => OverlayOutcome::Monitor(m.clone()),
                None => OverlayOutcome::Cancelled,
            },
            Finish::PickColor(p) => match self.renderer.pixel(*p) {
                Some(rgb) => OverlayOutcome::ColorPicked(PickedColor { point: *p, rgb }),
                None => OverlayOutcome::Cancelled,
            },
        })
    }
}
