//! Shared helpers for the UI tests: a sandboxed window, harness builders, and input helpers
//! that go through egui's real event path.
#![allow(dead_code)] // each test binary uses a different subset

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2, pos2};
use egui_kittest::{Harness, kittest::Queryable};
use ssx_core::settings::{Paths, Settings};
use ssx_settings_ui::{
    SettingsApp,
    host::Host,
    model::SettingsModel,
    nav::Page,
    task::no_wake,
};

/// A window in a temp folder, with the temp folder kept alive.
pub struct Fixture {
    /// Owns the folder.
    pub dir: tempfile::TempDir,
}

impl Fixture {
    /// The settings file.
    pub fn settings_file(&self) -> PathBuf {
        self.dir.path().join("config").join("settings.toml")
    }

    /// The paths of the host.
    pub fn paths(&self) -> Paths {
        Paths::rooted_at(self.dir.path().join("config"))
    }

    /// Reloads the file with the core.
    pub fn load(&self) -> Settings {
        Settings::load(&self.settings_file()).unwrap().settings
    }

    /// The root of the sandbox.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }
}

/// A sandboxed app on `page`, with default settings that are already on disk.
pub fn app(page: Page) -> (SettingsApp, Fixture) {
    app_with(page, Settings::default())
}

/// Like [`app`] with the given settings written to disk first.
pub fn app_with(page: Page, settings: Settings) -> (SettingsApp, Fixture) {
    let fx = Fixture { dir: tempfile::tempdir().unwrap() };
    let host = Host::sandboxed(fx.root());
    std::fs::create_dir_all(&host.paths.config_dir).unwrap();
    settings.save(&fx.settings_file()).unwrap();
    let model = SettingsModel::load(fx.settings_file()).unwrap();
    let mut a = SettingsApp::new(model, host, page, no_wake());
    a.set_now(chrono::DateTime::parse_from_rfc3339("2025-03-09T14:05:06+01:00").unwrap());
    (a, fx)
}

/// A harness running the whole window (`logic` + `show`) at `size` points.
pub fn window(app: SettingsApp, size: impl Into<Vec2>) -> Harness<'static, SettingsApp> {
    let mut h = Harness::builder().with_size(size).with_max_steps(60).wgpu().build_ui_state(
        |ui, app: &mut SettingsApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.show(ui);
        },
        app,
    );
    ssx_editor_ui::ui::theme::apply(&h.ctx);
    h.step();
    h
}

/// Runs frames until the UI is idle. Continuous repaint requests (the external-change poll,
/// spinners) are normal here, so hitting the step limit is fine.
pub fn settle<S>(h: &mut Harness<'_, S>) {
    let _ = h.try_run();
}

/// Clicks the widget called `label` (must be unique).
pub fn click(h: &mut Harness<'_, SettingsApp>, label: &str) {
    h.get_by_label(label).click();
    settle(h);
}

/// Clicks the widget whose label starts with `label` and is the only such one.
pub fn click_contains(h: &mut Harness<'_, SettingsApp>, label: &str) {
    h.get_by_label_contains(label).click();
    settle(h);
}

/// Replaces the text of the text box called `label`.
pub fn set_text(h: &mut Harness<'_, SettingsApp>, label: &str, text: &str) {
    let n = h.get_by_label(label);
    n.focus();
    h.step();
    h.key_press_modifiers(Modifiers::COMMAND, egui::Key::A);
    h.step();
    h.get_by_label(label).type_text(text);
    settle(h);
}

/// The value of the text box called `label`.
pub fn text_of(h: &Harness<'_, SettingsApp>, label: &str) -> String {
    h.get_by_label(label).value().unwrap_or_default()
}

/// Presses at `from`, moves in a few steps, releases at `to`.
pub fn drag(h: &mut Harness<'_, SettingsApp>, from: Pos2, to: Pos2) {
    h.hover_at(from);
    h.step();
    h.event(Event::PointerButton { pos: from, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        h.hover_at(pos2(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t));
        h.step();
    }
    h.event(Event::PointerButton { pos: to, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.step();
    settle(h);
}

/// Presses a key with modifiers.
pub fn key(h: &mut Harness<'_, SettingsApp>, m: Modifiers, k: egui::Key) {
    h.key_press_modifiers(m, k);
    settle(h);
}

/// Where snapshot PNGs are written for inspection (`SSX_UI_DUMP`).
pub fn dump_dir() -> Option<PathBuf> {
    std::env::var_os("SSX_UI_DUMP").map(PathBuf::from)
}

/// Saves a rendering of the harness for a human to look at, when `SSX_UI_DUMP` is set.
pub fn dump(h: &mut Harness<'_, SettingsApp>, name: &str) {
    if let Some(dir) = dump_dir() {
        let _ = std::fs::create_dir_all(&dir);
        match h.render() {
            Ok(img) => {
                let _ = img.save(dir.join(format!("{name}.png")));
            }
            Err(e) => eprintln!("cannot render {name}: {e}"),
        }
    }
}

/// A shared handle for asserting on the working settings.
pub fn working(h: &Harness<'_, SettingsApp>) -> Settings {
    h.state().model.working().clone()
}

/// Wait for the HDR preview to finish (the worker is off-thread).
pub fn wait_preview(h: &mut Harness<'_, SettingsApp>) {
    let _ = h.state_mut().capture.engine().wait_idle(std::time::Duration::from_secs(60));
    settle(h);
}

/// A no-op `Arc` waker.
pub fn waker() -> ssx_settings_ui::task::Waker {
    Arc::new(|| {})
}
