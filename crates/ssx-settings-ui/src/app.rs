//! The window: navigation rail, page area, the Apply / Revert / Save bar, banners, and the
//! life cycle of the settings model around them.
//!
//! **Apply, Save, Revert.** *Apply* validates and writes `settings.toml` and keeps the window
//! open; *Save* does the same and closes it; *Revert* throws the unsaved edits away. Nothing
//! else writes the file. There is no "restart": the running ssx watches the file and reloads
//! it, so a successful Apply is all it takes.

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local};
use egui::{Align, Color32, Context, Layout, RichText, Sense, Ui, vec2};
use ssx_editor_ui::{
    icons::{self, IconColors},
    ui::theme,
};

use crate::{
    host::Host,
    model::{External, SaveError, SettingsModel},
    nav::Page,
    pages::{self, Cx},
    task::Waker,
    ui_kit::{self, Toasts},
    uploader_registry::RegistryCache,
    validation::Issues,
};

/// How often the settings file is checked for outside changes.
pub const POLL_EVERY: Duration = Duration::from_millis(1500);

/// How the window ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Settings were written at least once.
    pub saved: bool,
    /// How many times.
    pub writes: u32,
    /// Unsaved edits were discarded on exit.
    pub discarded: bool,
    /// The page that was showing.
    pub page: Option<Page>,
}

impl Outcome {
    /// One line of JSON for `--json`.
    pub fn to_json(&self, path: &std::path::Path) -> String {
        serde_json::json!({
            "saved": self.saved,
            "writes": self.writes,
            "discarded": self.discarded,
            "page": self.page.map(Page::slug),
            "settings_file": path.display().to_string(),
        })
        .to_string()
    }
}

/// A modal the shell itself owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialog {
    /// The window is being closed with unsaved edits.
    Unsaved,
}

/// The kind of thing the footer says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Nothing to worry about.
    Ok,
    /// There are unsaved changes.
    Pending,
    /// Something blocks saving.
    Blocked,
}

/// The footer sentence for the current state, and its tone.
pub fn footer_status(dirty: &[Page], errors: usize, warnings: usize, conflict: bool) -> (String, Tone) {
    if conflict {
        return ("settings.toml changed on disk: choose Reload or Overwrite above".to_owned(), Tone::Blocked);
    }
    if errors > 0 {
        let s = if errors == 1 { "1 problem must be fixed before saving".to_owned() } else { format!("{errors} problems must be fixed before saving") };
        return (s, Tone::Blocked);
    }
    if dirty.is_empty() {
        let s = if warnings > 0 {
            format!("All changes saved; {warnings} warning{}", if warnings == 1 { "" } else { "s" })
        } else {
            "All changes saved".to_owned()
        };
        return (s, Tone::Ok);
    }
    let names: Vec<&str> = dirty.iter().map(|p| p.label()).collect();
    let mut s = format!("Unsaved changes in {}", names.join(", "));
    if warnings > 0 {
        s.push_str(&format!("; {warnings} warning{}", if warnings == 1 { "" } else { "s" }));
    }
    (s, Tone::Pending)
}

/// The settings window.
pub struct SettingsApp {
    /// The settings being edited.
    pub model: SettingsModel,
    /// The machine.
    pub host: Host,
    /// The page showing.
    pub page: Page,
    /// Messages.
    pub toasts: Toasts,
    /// A modal owned by the shell.
    pub dialog: Option<Dialog>,
    /// The last save error, shown in a banner.
    pub save_error: Option<String>,
    /// General page.
    pub general: pages::general::State,
    /// Capture page.
    pub capture: pages::capture::State,
    /// Workflows page.
    pub workflows: pages::workflows::State,
    /// Hotkeys page.
    pub hotkeys: pages::hotkeys::State,
    /// Uploaders page.
    pub uploaders: pages::uploaders::State,
    /// History page.
    pub history: pages::history::State,
    /// Integration page.
    pub integration: pages::integration::State,
    registry: RegistryCache,
    /// How the window ended so far.
    pub outcome: Outcome,
    wake: Waker,
    closing: bool,
    force_close: bool,
    next_poll: f64,
    last_snapshot: Option<ssx_core::settings::Settings>,
    entered: Option<Page>,
    now_override: Option<DateTime<FixedOffset>>,
}

impl std::fmt::Debug for SettingsApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsApp").field("page", &self.page).field("outcome", &self.outcome).finish_non_exhaustive()
    }
}

impl SettingsApp {
    /// Builds the window over `model` and `host`. `wake` repaints the window from threads.
    pub fn new(model: SettingsModel, host: Host, page: Page, wake: Waker) -> Self {
        Self {
            model,
            host,
            page,
            toasts: Toasts::default(),
            dialog: None,
            save_error: None,
            general: pages::general::State::default(),
            capture: pages::capture::State::new(&wake),
            workflows: pages::workflows::State::default(),
            hotkeys: pages::hotkeys::State::default(),
            uploaders: pages::uploaders::State::default(),
            history: pages::history::State::default(),
            integration: pages::integration::State::default(),
            registry: RegistryCache::default(),
            outcome: Outcome::default(),
            wake,
            closing: false,
            force_close: false,
            next_poll: 0.0,
            last_snapshot: None,
            entered: None,
            now_override: None,
        }
    }

    /// Pins "now" (tests and screenshots).
    pub fn set_now(&mut self, now: DateTime<FixedOffset>) {
        self.now_override = Some(now);
    }

    fn now(&self) -> DateTime<FixedOffset> {
        self.now_override.unwrap_or_else(|| Local::now().fixed_offset())
    }

    /// Whether any page has background work running (tests wait for this to become `false`).
    pub fn busy(&self) -> bool {
        self.uploaders.busy() || self.history.busy() || self.integration.busy()
    }

    /// Whether the window has asked to close.
    pub fn closing(&self) -> bool {
        self.closing
    }

    /// The finished outcome (with the page that was showing).
    pub fn outcome(&self) -> Outcome {
        Outcome { page: Some(self.page), discarded: self.outcome.discarded || (self.model.is_dirty() && self.force_close), ..self.outcome.clone() }
    }

    // ---- actions -----------------------------------------------------------------------

    /// Writes the settings. Returns `true` on success.
    pub fn apply(&mut self, ctx: &Context) -> bool {
        let now = ctx.input(|i| i.time);
        match self.model.save() {
            Ok(()) => {
                self.outcome.saved = true;
                self.outcome.writes += 1;
                self.save_error = None;
                self.toasts.success(now, "Saved. The running ssx picks the new settings up by itself.");
                true
            }
            Err(SaveError::Blocked { count, first }) => {
                if let Some(p) = self.model.issues().first_page_with_error() {
                    self.page = p;
                }
                self.toasts.error(now, format!("Not saved: {count} problem(s) to fix first ({first})"));
                false
            }
            Err(SaveError::Conflict) => {
                self.save_error = None;
                false
            }
            Err(e) => {
                self.save_error = Some(e.to_string());
                false
            }
        }
    }

    /// Writes the settings and closes the window.
    pub fn save_and_close(&mut self, ctx: &Context) {
        if self.apply(ctx) {
            self.request_close(ctx);
        }
    }

    /// Discards unsaved edits.
    pub fn revert(&mut self, ctx: &Context) {
        self.model.revert();
        self.save_error = None;
        let now = ctx.input(|i| i.time);
        self.toasts.info(now, "Changes discarded.");
    }

    fn request_close(&mut self, ctx: &Context) {
        self.closing = true;
        self.force_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Switches page.
    pub fn go(&mut self, page: Page) {
        self.page = page;
    }

    // ---- per frame ---------------------------------------------------------------------

    /// Per-frame work that is not drawing: close requests, external-change polling.
    pub fn logic(&mut self, ctx: &Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.force_close {
            if self.model.is_dirty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.dialog = Some(Dialog::Unsaved);
            } else {
                self.closing = true;
            }
        }
        let t = ctx.input(|i| i.time);
        if t >= self.next_poll {
            self.next_poll = t + POLL_EVERY.as_secs_f64();
            match self.model.check_external() {
                External::Reloaded => {
                    if let Some(n) = self.model.take_notice() {
                        self.toasts.info(t, n);
                    }
                }
                External::Conflict | External::Unchanged => {}
            }
        }
        ctx.request_repaint_after(POLL_EVERY);
        self.shortcuts(ctx);
    }

    fn shortcuts(&mut self, ctx: &Context) {
        if self.dialog.is_some() {
            return;
        }
        let (apply, next, prev, jump) = ctx.input_mut(|i| {
            let apply = i.consume_key(egui::Modifiers::COMMAND, egui::Key::S);
            let next = i.consume_key(egui::Modifiers::COMMAND, egui::Key::PageDown);
            let prev = i.consume_key(egui::Modifiers::COMMAND, egui::Key::PageUp);
            let keys = [
                egui::Key::Num1, egui::Key::Num2, egui::Key::Num3, egui::Key::Num4,
                egui::Key::Num5, egui::Key::Num6, egui::Key::Num7, egui::Key::Num8,
            ];
            let jump = keys.iter().position(|k| i.consume_key(egui::Modifiers::COMMAND, *k));
            (apply, next, prev, jump)
        });
        if apply {
            self.apply(ctx);
        }
        if next {
            self.page = self.page.step(1);
        }
        if prev {
            self.page = self.page.step(-1);
        }
        if let Some(i) = jump
            && let Some(p) = Page::ALL.get(i)
        {
            self.page = *p;
        }
    }

    /// Draws the whole window.
    pub fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let registry = self.registry.get(self.model.working(), &self.host.paths.config_dir, &self.host.vault.store());
        let issues = self.model.issues().clone().without_known_uploader_hints(|n| registry.get(n).is_some());
        let dirty_pages = self.model.dirty_pages();

        egui::Panel::left("nav")
            .exact_size(200.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(theme::BAR_BG).inner_margin(egui::Margin::symmetric(10, 12)))
            .show(ui, |ui| self.nav(ui, &issues, &dirty_pages));

        egui::Panel::bottom("footer")
            .frame(egui::Frame::new().fill(theme::BAR_BG).inner_margin(egui::Margin::symmetric(14, 8)))
            .show_separator_line(true)
            .show(ui, |ui| self.footer(ui, &issues, &dirty_pages));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(ui_kit::PAGE_BG).inner_margin(egui::Margin::symmetric(18, 10)))
            .show(ui, |ui| {
                self.banners(ui);
                ui_kit::page_header(ui, self.page.label(), self.page.blurb());
                self.page_ui(ui, &issues);
            });

        self.dialogs(&ctx);
        ui_kit::show_toasts(&ctx, &mut self.toasts);
    }

    fn page_ui(&mut self, ui: &mut Ui, issues: &Issues) {
        let ctx = ui.ctx().clone();
        let before = self.model.working().clone();
        let mut go_to = None;
        let now = self.now();
        let registry = self.registry.get(self.model.working(), &self.host.paths.config_dir, &self.host.vault.store());
        {
            let settings = self.model.working_mut();
            let mut cx = Cx {
                settings,
                issues,
                host: &self.host,
                registry,
                wake: &self.wake,
                toasts: &mut self.toasts,
                now,
                go_to: &mut go_to,
            };
            if self.entered != Some(self.page) {
                match self.page {
                    Page::General => self.general.refresh(&cx),
                    Page::Integration => {
                        self.integration.refresh_states();
                        self.integration.rerun_doctor();
                    }
                    Page::History => self.history.reset_source(),
                    _ => {}
                }
                if self.entered == Some(Page::History) {
                    self.history.reset_source();
                }
                self.entered = Some(self.page);
            }
            match self.page {
                Page::General => pages::general::ui(ui, &mut self.general, &mut cx),
                Page::Capture => pages::capture::ui(ui, &mut self.capture, &mut cx),
                Page::Workflows => pages::workflows::ui(ui, &mut self.workflows, &mut cx),
                Page::Hotkeys => pages::hotkeys::ui(ui, &mut self.hotkeys, &mut cx),
                Page::Uploaders => pages::uploaders::ui(ui, &mut self.uploaders, &mut cx),
                Page::History => pages::history::ui(ui, &mut self.history, &mut cx),
                Page::Integration => pages::integration::ui(ui, &mut self.integration, &mut cx),
                Page::About => pages::about::ui(ui, &mut cx),
            }
        }
        if let Some(p) = go_to {
            self.page = p;
        }
        if *self.model.working() != before {
            ctx.request_repaint();
        }
        self.last_snapshot = Some(before);
    }

    // ---- navigation --------------------------------------------------------------------

    fn nav(&mut self, ui: &mut Ui, issues: &Issues, dirty: &[Page]) {
        ui.horizontal(|ui| {
            pages::about::logo(ui, 28.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("ssx").strong().size(15.0).color(Color32::WHITE));
                ui.label(RichText::new("Settings").size(11.5).color(theme::TEXT_DIM));
            });
        });
        ui.add_space(14.0);
        for (n, page) in Page::ALL.into_iter().enumerate() {
            let (errors, warnings) = issues.counts_for(page);
            let r = nav_item(ui, page, self.page == page, dirty.contains(&page), errors, warnings, n + 1);
            if r.clicked() {
                self.page = page;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(2.0);
            ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).size(11.0).color(theme::TEXT_DIM));
        });
    }

    // ---- banners -----------------------------------------------------------------------

    fn banners(&mut self, ui: &mut Ui) {
        if let Some(c) = self.model.conflict().cloned() {
            let text = if c.deleted {
                "settings.toml was deleted by another program while you had unsaved edits.".to_owned()
            } else if let Some(e) = &c.disk_error {
                format!("settings.toml was changed by another program and can no longer be read ({e}).")
            } else {
                "settings.toml was changed by another program while you had unsaved edits.".to_owned()
            };
            let mut reload = false;
            let mut overwrite = false;
            banner(ui, ui_kit::WARN_TEXT, &text, |ui| {
                let can_reload = !c.deleted && c.disk_error.is_none();
                if ui_kit::button_if(ui, "Reload from disk", can_reload, "The file on disk cannot be read").on_hover_text("Discard your edits and load the file").clicked() {
                    reload = true;
                }
                if ui_kit::danger(ui, "Overwrite the file").on_hover_text("Keep your edits; the next Apply replaces the file on disk").clicked() {
                    overwrite = true;
                }
            });
            if reload && let Err(e) = self.model.reload_from_disk() {
                self.save_error = Some(e.to_string());
            }
            if overwrite {
                self.model.keep_edits_and_overwrite();
            }
        }
        if let Some(e) = self.save_error.clone() {
            let mut dismiss = false;
            banner(ui, ui_kit::ERROR_TEXT, &format!("Could not save: {e}"), |ui| {
                if ui_kit::button(ui, "Dismiss").clicked() {
                    dismiss = true;
                }
            });
            if dismiss {
                self.save_error = None;
            }
        }
        if !self.model.warnings().is_empty() {
            let text = self.model.warnings().join("\n");
            let mut dismiss = false;
            banner(ui, ui_kit::WARN_TEXT, &text, |ui| {
                if ui_kit::button(ui, "Dismiss").clicked() {
                    dismiss = true;
                }
            });
            if dismiss {
                self.model.clear_warnings();
            }
        }
    }

    // ---- footer ------------------------------------------------------------------------

    fn footer(&mut self, ui: &mut Ui, issues: &Issues, dirty: &[Page]) {
        let ctx = ui.ctx().clone();
        let (errors, warnings) = issues.totals();
        let conflict = self.model.conflict().is_some();
        let (text, tone) = footer_status(dirty, errors, warnings, conflict);
        let is_dirty = self.model.is_dirty();
        let blocked = errors > 0 || conflict;
        ui.horizontal(|ui| {
            let color = match tone {
                Tone::Ok => ui_kit::OK_TEXT,
                Tone::Pending => theme::ACCENT,
                Tone::Blocked => ui_kit::ERROR_TEXT,
            };
            let (rect, _) = ui.allocate_exact_size(vec2(10.0, 18.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.label(RichText::new(&text).color(theme::TEXT));
            if errors > 0
                && let Some(p) = issues.first_page_with_error()
                && ui_kit::link(ui, "Show the first problem").clicked()
            {
                self.page = p;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let why = if conflict { "Resolve the change on disk first" } else { "Fix the problems first" };
                if ui_kit::primary(ui, "Save", !blocked).on_hover_text("Write settings.toml and close (Ctrl+S applies without closing)").clicked() && !blocked {
                    self.save_and_close(&ctx);
                }
                let apply = ui_kit::button_if(ui, "Apply", is_dirty && !blocked, if is_dirty { why } else { "Nothing to apply" });
                if apply.on_hover_text("Write settings.toml now and keep this window open").clicked() {
                    self.apply(&ctx);
                }
                let revert = ui_kit::button_if(ui, "Revert", is_dirty, "No unsaved changes");
                if revert.on_hover_text("Discard the changes made since the last save").clicked() {
                    self.revert(&ctx);
                }
                let _ = why;
            });
        });
    }

    // ---- dialogs -----------------------------------------------------------------------

    fn dialogs(&mut self, ctx: &Context) {
        match self.dialog {
            Some(Dialog::Unsaved) => {
                let blocked = self.model.issues().blocks_save() || self.model.conflict().is_some();
                let mut choice = None;
                let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
                let m = egui::Modal::new(egui::Id::new("ssx-unsaved")).frame(frame).show(ctx, |ui| {
                    ssx_editor_ui::ui::widgets::input_style(ui);
                    ui.set_width(440.0);
                    ui.label(RichText::new("Save your changes?").heading().strong());
                    ui.add_space(6.0);
                    let pages: Vec<&str> = self.model.dirty_pages().iter().map(|p| p.label()).collect();
                    ui.label(format!("You changed settings on: {}.", pages.join(", ")));
                    if blocked {
                        ui.label(RichText::new("They cannot be saved yet: fix the problems (or resolve the change on disk) first, or discard them.").color(ui_kit::WARN_TEXT));
                    }
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui_kit::primary(ui, "Save", !blocked).clicked() && !blocked {
                            choice = Some(0);
                        }
                        if ui_kit::danger(ui, "Discard").clicked() {
                            choice = Some(1);
                        }
                        if ui_kit::button(ui, "Cancel").clicked() {
                            choice = Some(2);
                        }
                    });
                });
                if choice.is_none() && m.should_close() {
                    choice = Some(2);
                }
                match choice {
                    Some(0) => {
                        self.dialog = None;
                        self.save_and_close(ctx);
                    }
                    Some(1) => {
                        self.dialog = None;
                        self.outcome.discarded = true;
                        self.request_close(ctx);
                    }
                    Some(_) => self.dialog = None,
                    None => {}
                }
            }
            None => {}
        }
    }
}

fn banner(ui: &mut Ui, color: Color32, text: &str, buttons: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.add(egui::Label::new(RichText::new(text).color(theme::TEXT)).wrap());
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| buttons(ui));
        });
    ui.add_space(8.0);
}

/// One row of the navigation rail.
fn nav_item(ui: &mut Ui, page: Page, selected: bool, dirty: bool, errors: usize, warnings: usize, number: usize) -> egui::Response {
    let size = vec2(ui.available_width(), 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let mut label = page.label().to_owned();
    if dirty {
        label.push_str(", unsaved changes");
    }
    if errors > 0 {
        label.push_str(&format!(", {errors} problem{}", if errors == 1 { "" } else { "s" }));
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, label.clone()));
    if ui.is_rect_visible(rect) {
        let r = egui::CornerRadius::same(6);
        if selected {
            ui.painter().rect_filled(rect, r, theme::ACTIVE_BG);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(rect.min + vec2(0.0, 6.0), vec2(3.0, rect.height() - 12.0)),
                egui::CornerRadius::same(2),
                theme::ACCENT,
            );
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, r, theme::HOVER_BG.gamma_multiply(0.7));
        }
        if resp.has_focus() {
            ui.painter().rect_stroke(rect, r, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
        }
        let ink = if selected || resp.hovered() { Color32::WHITE } else { theme::TEXT };
        let icon_rect = egui::Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), vec2(20.0, 20.0));
        icons::paint(ui.painter(), page.icon(), icon_rect, IconColors::with_ink(ink));
        ui.painter().text(
            egui::pos2(rect.left() + 38.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            page.label(),
            egui::FontId::proportional(13.5),
            ink,
        );
        let mut x = rect.right() - 12.0;
        if errors > 0 {
            ui.painter().circle_filled(egui::pos2(x, rect.center().y), 4.5, ui_kit::ERROR_TEXT);
            x -= 13.0;
        } else if warnings > 0 {
            ui.painter().circle_filled(egui::pos2(x, rect.center().y), 4.5, ui_kit::WARN_TEXT);
            x -= 13.0;
        }
        if dirty {
            ui.painter().circle_filled(egui::pos2(x, rect.center().y), 3.5, theme::ACCENT);
        }
        if resp.hovered() || resp.has_focus() {
            resp.clone().on_hover_text(format!("{}  (Ctrl+{number})", page.blurb()));
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_says_what_is_going_on() {
        assert_eq!(footer_status(&[], 0, 0, false), ("All changes saved".to_owned(), Tone::Ok));
        assert_eq!(footer_status(&[], 0, 2, false).0, "All changes saved; 2 warnings");
        assert_eq!(footer_status(&[], 0, 1, false).0, "All changes saved; 1 warning");
        let (t, tone) = footer_status(&[Page::General, Page::Capture], 0, 0, false);
        assert_eq!((t.as_str(), tone), ("Unsaved changes in General, Capture & HDR", Tone::Pending));
        assert!(footer_status(&[Page::General], 0, 1, false).0.ends_with("; 1 warning"));
        assert_eq!(footer_status(&[Page::General], 1, 0, false), ("1 problem must be fixed before saving".to_owned(), Tone::Blocked));
        assert_eq!(footer_status(&[], 3, 0, false).0, "3 problems must be fixed before saving");
        assert_eq!(footer_status(&[Page::General], 3, 0, true).1, Tone::Blocked);
        assert!(footer_status(&[], 0, 0, true).0.contains("Reload"));
    }

    #[test]
    fn outcome_json_is_one_line_with_the_documented_keys() {
        let o = Outcome { saved: true, writes: 2, discarded: false, page: Some(Page::Hotkeys) };
        let j = o.to_json(std::path::Path::new("/c/settings.toml"));
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["saved"], true);
        assert_eq!(v["writes"], 2);
        assert_eq!(v["page"], "hotkeys");
        assert_eq!(v["settings_file"], "/c/settings.toml");
        assert!(!j.contains('\n'));
    }
}
