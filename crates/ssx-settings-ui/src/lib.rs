//! The ssx settings and history window: an egui/eframe application.
//!
//! # Layout of the crate
//!
//! * **Pure logic**, testable without a display, one module per concern: [`model`] (the
//!   working copy: dirty tracking, atomic save, external-change detection), [`validation`]
//!   (validator findings mapped onto fields), [`nav`], [`autostart`], [`pattern_info`],
//!   [`hdr_scene`] + [`debounce`] + [`preview_engine`] (the HDR preview), [`hotkey_keys`] +
//!   [`hotkey_plan`], [`reorder`] + [`workflow_edit`], [`uploader_forms`] +
//!   [`uploader_registry`] + [`secrets`], [`history_view`] + [`thumbs`], [`task`].
//! * **Drawing**: [`ui_kit`] (widgets in the editor's dark look, reused from
//!   `ssx-editor-ui`'s theme) and [`pages`], where every page is a pure `State` plus a thin
//!   `ui()` function.
//! * [`host`]: everything the window needs from the machine (opener, file dialogs, secret
//!   store, autostart, file managers, diagnostics, history), so tests and screenshots run on
//!   fakes.
//! * [`app::SettingsApp`] ties it together; [`run`] opens the window.
//!
//! # Entry point
//!
//! ```no_run
//! use ssx_settings_ui::{RunOptions, nav::Page, run};
//! let outcome = run(RunOptions { page: Page::Hotkeys, ..RunOptions::default() }).unwrap();
//! println!("{}", outcome.saved);
//! ```
//!
//! # Rendering backend
//!
//! eframe with the `wgpu` renderer, like the editor; on machines without a GPU it runs on
//! the Vulkan software rasteriser (lavapipe).

#![forbid(unsafe_code)]

pub mod app;
pub mod autostart;
pub mod debounce;
pub mod hdr_scene;
pub mod history_view;
pub mod host;
pub mod hotkey_keys;
pub mod hotkey_plan;
pub mod hotkey_widget;
pub mod model;
pub mod nav;
pub mod pages;
pub mod pattern_info;
pub mod preview_engine;
pub mod reorder;
pub mod reorder_ui;
pub mod secrets;
pub mod task;
pub mod thumbs;
pub mod ui_kit;
pub mod uploader_forms;
pub mod uploader_registry;
pub mod validation;
pub mod workflow_edit;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use ssx_core::settings::{Paths, SettingsError};

pub use app::{Outcome, SettingsApp};

use crate::{host::Host, model::SettingsModel, nav::Page};

/// Why the window could not run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The settings or data directories could not be determined.
    #[error("{0}")]
    Paths(#[from] ssx_core::settings::PathsError),
    /// The settings file cannot be used (a file from a newer ssx, an I/O error).
    #[error("{0}")]
    Settings(#[from] SettingsError),
    /// The window could not be created.
    #[error("cannot open the window: {0}")]
    Window(String),
}

/// What to open.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// The page to start on.
    pub page: Page,
    /// Use this folder for settings and data instead of the default (like `SSX_CONFIG_DIR`).
    pub config_dir: Option<PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self { page: Page::General, config_dir: None }
    }
}

/// The eframe shell: forwards to [`SettingsApp`] and hands the outcome back after the loop.
struct Shell {
    app: SettingsApp,
    slot: Arc<Mutex<Option<Outcome>>>,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.show(ui);
    }

    fn on_exit(&mut self) {
        if let Ok(mut s) = self.slot.lock() {
            *s = Some(self.app.outcome());
        }
    }
}

/// Resolves the directories, honouring `config_dir` over `SSX_CONFIG_DIR` over the platform
/// default.
pub fn resolve_paths(config_dir: Option<PathBuf>) -> Result<Paths, RunError> {
    match config_dir {
        Some(dir) => Ok(Paths::rooted_at(dir)),
        None => Ok(Paths::discover()?),
    }
}

/// Opens the window and blocks until it is closed.
pub fn run(options: RunOptions) -> Result<(Outcome, PathBuf), RunError> {
    let paths = resolve_paths(options.config_dir)?;
    let model = SettingsModel::load(paths.settings_file())?;
    let file = model.path().to_path_buf();
    let host = Host::system(paths);
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1120.0, 760.0])
        .with_min_inner_size([900.0, 560.0])
        .with_title("ssx settings")
        .with_app_id("ssx-settings");
    let native = eframe::NativeOptions { viewport, ..eframe::NativeOptions::default() };
    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
    let slot2 = slot.clone();
    let page = options.page;
    eframe::run_native(
        "ssx settings",
        native,
        Box::new(move |cc| {
            ssx_editor_ui::ui::theme::apply(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let wake: task::Waker = Arc::new(move || ctx.request_repaint());
            let app = SettingsApp::new(model, host, page, wake);
            Ok(Box::new(Shell { app, slot: slot2 }))
        }),
    )
    .map_err(|e| RunError::Window(e.to_string()))?;
    let outcome = slot.lock().ok().and_then(|mut s| s.take()).unwrap_or_default();
    Ok((outcome, file))
}
