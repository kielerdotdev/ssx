//! Shared helpers for the UI tests: a sandboxed window, harness builders, and input helpers
//! that go through egui's real event path.
#![allow(dead_code)] // each test binary uses a different subset

pub mod demo;
pub mod tools;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use egui::{Event, Modifiers, PointerButton, Pos2, Vec2, pos2};
use egui_kittest::{Harness, kittest::Queryable};
use ssx_core::settings::{Paths, Settings};
use ssx_settings_ui::{SettingsApp, host::Host, model::SettingsModel, nav::Page, task::no_wake};

/// A window in a folder (a temp folder, kept alive, unless a fixed one was asked for).
pub struct Fixture {
    root: PathBuf,
    _guard: Option<tempfile::TempDir>,
}

impl Fixture {
    /// A fresh temp folder.
    pub fn temp() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self { root: dir.path().to_path_buf(), _guard: Some(dir) }
    }

    /// A fixed, emptied folder: for golden images, which show paths.
    pub fn fixed(name: &str) -> Self {
        let root = std::env::temp_dir().join("ssx-settings-ui-golden").join(name);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root, _guard: None }
    }
}

impl Fixture {
    /// The settings file.
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("config").join("settings.toml")
    }

    /// The paths of the host.
    pub fn paths(&self) -> Paths {
        Paths::rooted_at(self.root.join("config"))
    }

    /// Reloads the file with the core.
    pub fn load(&self) -> Settings {
        Settings::load(&self.settings_file()).unwrap().settings
    }

    /// The root of the sandbox.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// A sandboxed app on `page`, with default settings that are already on disk.
pub fn app(page: Page) -> (SettingsApp, Fixture) {
    app_with(page, Settings::default())
}

/// Like [`app`] with the given settings written to disk first.
pub fn app_with(page: Page, settings: Settings) -> (SettingsApp, Fixture) {
    app_custom(page, settings, |_, _| {})
}

/// Like [`app_with`], letting the test replace parts of the (sandboxed) host first.
pub fn app_custom(
    page: Page,
    settings: Settings,
    customise: impl FnOnce(&mut Host, &Path),
) -> (SettingsApp, Fixture) {
    let fx = Fixture::temp();
    let mut host = Host::sandboxed(fx.root());
    customise(&mut host, fx.root());
    std::fs::create_dir_all(&host.paths.config_dir).unwrap();
    // written without validation: some tests start from settings that have problems
    std::fs::write(fx.settings_file(), settings.to_toml_string().unwrap()).unwrap();
    let model = SettingsModel::load(fx.settings_file()).unwrap();
    let mut a = SettingsApp::new(model, host, page, no_wake());
    a.set_now(chrono::DateTime::parse_from_rfc3339("2025-03-09T14:05:06+01:00").unwrap());
    (a, fx)
}

/// A lived-in app: uploaders, a history, detected file managers, a diagnostics report.
pub fn demo_app(page: Page) -> (SettingsApp, Fixture) {
    demo_app_custom(page, |_| {}, |_| {})
}

/// Like [`demo_app`], letting the test change the settings and the host first.
pub fn demo_app_custom(
    page: Page,
    edit_settings: impl FnOnce(&mut Settings),
    edit_host: impl FnOnce(&mut Host),
) -> (SettingsApp, Fixture) {
    demo_app_in(Fixture::temp(), page, edit_settings, edit_host)
}

/// Like [`demo_app_custom`] in the given fixture.
pub fn demo_app_in(
    fx: Fixture,
    page: Page,
    edit_settings: impl FnOnce(&mut Settings),
    edit_host: impl FnOnce(&mut Host),
) -> (SettingsApp, Fixture) {
    let mut host = demo::demo_host(fx.root());
    let vault = ssx_settings_ui::secrets::MemoryVault::keyring();
    for n in ["my-s3-access-key-id", "my-s3-secret-access-key", "work-dropbox-auth-secret"] {
        ssx_settings_ui::secrets::SecretVault::set(&vault, n, "demo-value").unwrap();
    }
    host.vault = Arc::new(vault);
    edit_host(&mut host);
    std::fs::create_dir_all(&host.paths.config_dir).unwrap();
    let mut settings = demo::lived_in_settings();
    edit_settings(&mut settings);
    std::fs::write(fx.settings_file(), settings.to_toml_string().unwrap()).unwrap();
    let model = SettingsModel::load(fx.settings_file()).unwrap();
    let mut a = SettingsApp::new(model, host, page, no_wake());
    a.set_now(chrono::DateTime::parse_from_rfc3339("2025-03-09T14:05:06+01:00").unwrap());
    (a, fx)
}

/// Runs frames until every background task of the app is done (or `secs` pass).
pub fn wait_busy(h: &mut Harness<'_, SettingsApp>, secs: u64) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    for _ in 0..4 {
        h.step();
    }
    while h.state().busy() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(15));
        h.step();
    }
    settle(h);
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

fn text_box<'h>(h: &'h Harness<'_, SettingsApp>, label: &'h str) -> egui_kittest::Node<'h> {
    h.get_by_role_and_label(egui::accesskit::Role::TextInput, label)
}

/// Replaces the text of the text box called `label`.
pub fn set_text(h: &mut Harness<'_, SettingsApp>, label: &str, text: &str) {
    text_box(h, label).focus();
    h.step();
    h.key_press_modifiers(Modifiers::COMMAND, egui::Key::A);
    h.step();
    if text.is_empty() {
        h.key_press(egui::Key::Backspace);
    } else {
        text_box(h, label).type_text(text);
    }
    settle(h);
}

/// The value of the text box called `label`.
pub fn text_of(h: &Harness<'_, SettingsApp>, label: &str) -> String {
    text_box(h, label).value().unwrap_or_default()
}

/// Every accessible label currently on screen (for debugging a failing test).
pub fn labels(h: &Harness<'_, SettingsApp>) -> Vec<String> {
    use egui_kittest::kittest::NodeT;
    h.root().children_recursive().filter_map(|n| n.accesskit_node().label()).collect()
}

/// Whether some widget's label contains `text` (never panics on several matches).
pub fn has(h: &Harness<'_, SettingsApp>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

/// Whether some widget is labelled exactly `text`.
pub fn has_exact(h: &Harness<'_, SettingsApp>, text: &str) -> bool {
    h.query_all_by_label(text).next().is_some()
}

/// Clicks `label` and returns what was put on the clipboard while the click was processed.
pub fn click_and_copied(h: &mut Harness<'_, SettingsApp>, label: &str) -> Vec<String> {
    h.get_by_label(label).click();
    let mut out = Vec::new();
    for _ in 0..4 {
        h.step();
        for c in &h.output().platform_output.commands {
            if let egui::OutputCommand::CopyText(t) = c {
                out.push(t.clone());
            }
        }
    }
    settle(h);
    out
}

/// Presses at `from`, moves in a few steps, releases at `to`.
pub fn drag(h: &mut Harness<'_, SettingsApp>, from: Pos2, to: Pos2) {
    h.hover_at(from);
    h.step();
    h.event(Event::PointerButton {
        pos: from,
        button: PointerButton::Primary,
        pressed: true,
        modifiers: Modifiers::NONE,
    });
    h.step();
    for i in 1..=6 {
        let t = i as f32 / 6.0;
        h.hover_at(pos2(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t));
        h.step();
    }
    h.event(Event::PointerButton {
        pos: to,
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    });
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
