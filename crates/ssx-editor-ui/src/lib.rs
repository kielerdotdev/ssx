//! The ssx image-editor window: an egui/eframe application that drives
//! [`ssx_editor::EditorSession`].
//!
//! # Layout of the crate
//!
//! * Pure logic, testable without a display: [`viewport`] (zoom/pan maths), [`tiles`] (what to
//!   render), [`keymap`] + [`shortcuts`] (input routing and the cheat sheet), [`state`]
//!   ([`state::AppState`]: tool, dialogs, action queue), [`props`] (properties-bar model),
//!   [`forms`], [`export`], [`prefs`], [`history_log`], [`effects`], [`preview`].
//! * Drawing: [`icons`] (vector icons in code) and [`ui`] (toolbar, bars, canvas, dialogs).
//! * [`app::EditorApp`] ties them together; [`run`] opens the window.
//!
//! # Entry point
//!
//! ```no_run
//! use ssx_editor_ui::{EditorInput, EditorRequest, run};
//! let outcome = run(EditorRequest::new(EditorInput::Path("shot.png".into()))).unwrap();
//! println!("{}", outcome.to_json());
//! ```
//!
//! # Rendering backend
//!
//! eframe with the `wgpu` renderer. On machines without a GPU it runs on the Vulkan software
//! rasteriser (lavapipe); see the crate README for the backends that were verified.

#![forbid(unsafe_code)]

pub mod action;
pub mod app;
pub mod bench;
pub mod document;
pub mod effects;
pub mod export;
pub mod forms;
pub mod history_log;
pub mod icons;
pub mod keymap;
pub mod prefs;
pub mod preview;
pub mod props;
pub mod request;
pub mod services;
pub mod shortcuts;
pub mod state;
pub mod tiles;
pub mod tools;
pub mod ui;
pub mod viewport;

use std::sync::{Arc, Mutex};

pub use request::{
    BenchMode, DevOptions, EditorInput, EditorOutcome, EditorRequest, OutcomeAction, RunError,
};

use crate::{app::EditorApp, prefs::Prefs, services::Services};

/// The eframe shell: forwards to [`EditorApp`] and hands the outcome back after the loop ends.
struct Shell {
    app: EditorApp,
    slot: Arc<Mutex<Option<EditorOutcome>>>,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.show(ui);
    }

    fn on_exit(&mut self) {
        self.app.save_prefs();
        if let Ok(mut s) = self.slot.lock() {
            *s = self.app.outcome().cloned();
        }
    }
}

/// Opens the editor window for `request` and blocks until it is closed.
///
/// The input is loaded *before* any window exists, so a bad path or an empty clipboard is
/// reported as an error instead of flashing a window. Closing the window with unsaved
/// changes asks first; the outcome says how the session ended.
pub fn run(request: EditorRequest) -> Result<EditorOutcome, RunError> {
    let prefs = if request.dev.ephemeral_state { Prefs::default() } else { Prefs::load() };
    let mut probe_services = Services::system(|| {});
    let doc = EditorApp::load_input(&request, &mut probe_services)?;
    let win = prefs.window;
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([win.width, win.height])
        .with_min_inner_size([760.0, 480.0])
        .with_title(doc.title())
        .with_app_id("ssx-editor")
        .with_maximized(win.maximized);
    if let (Some(x), Some(y)) = (win.x, win.y) {
        viewport = viewport.with_position([x, y]);
    }
    let options = eframe::NativeOptions { viewport, ..eframe::NativeOptions::default() };
    let slot: Arc<Mutex<Option<EditorOutcome>>> = Arc::new(Mutex::new(None));
    let slot2 = slot.clone();
    let output = request.output.clone();
    let dev = request.dev.clone();
    eframe::run_native(
        "ssx editor",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let wake = {
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            };
            let services = Services::system(wake.clone());
            let app = EditorApp::new(doc, output, prefs, services, dev, wake);
            Ok(Box::new(Shell { app, slot: slot2 }))
        }),
    )
    .map_err(|e| RunError::Window(e.to_string()))?;
    let outcome = slot.lock().ok().and_then(|mut s| s.take());
    Ok(outcome.unwrap_or_else(EditorOutcome::cancelled))
}
