//! Uploaders: the destinations that exist, which one each kind of content goes to, importing
//! ShareX `.sxcu` files, editing the built-in kinds, and a test upload.
//!
//! Secrets never pass through the settings: a secret field shows only *whether* a value is
//! stored and where, takes a new value in a password box, and hands it to the credential
//! store on a worker thread; the settings hold the `keyring:<name>` reference.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use egui::{Color32, RichText, Ui};
use ssx_core::{
    settings::{DestinationType, FolderPolicy, Severity},
    workflow::{CancelToken, UploadOutcome},
};
use ssx_editor_ui::{
    icons::Icon,
    ui::{theme, widgets::input_style},
};

use super::{Cx, shared::destination_picker};
use crate::{
    secrets::{SecretLocation, SecretVault, default_secret_name, reference_for, reference_name},
    task::Slot,
    ui_kit::{self, Answer, Field},
    uploader_forms::{
        FieldKind, FieldSpec, UploaderKind, check, fields, free_name, get_map, get_text,
        has_plaintext_secret, name_problem, new_table, set_map, set_required_text, set_text,
    },
    uploader_registry::{
        Collision, ImportPreview, Registry, Source, TestJob, import, import_collision, preview_import,
        remove_file, type_word,
    },
};

/// The two halves of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// The list of destinations and their settings.
    #[default]
    Destinations,
    /// Which destination each kind of content goes to.
    Defaults,
}

/// A destructive action waiting for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Remove the `[uploaders.<name>]` table.
    RemoveTable(String),
    /// Delete the imported `.sxcu` file.
    RemoveFile(String),
}

/// The "New destination" dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDialog {
    /// What kind.
    pub kind: UploaderKind,
    /// Its name.
    pub name: String,
}

/// The import dialog: a checked file and the name to import it under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDialog {
    /// What checking the file found.
    pub preview: ImportPreview,
    /// The name to import as.
    pub name: String,
    /// The user agreed to replace an existing import of that name.
    pub overwrite: bool,
    /// An error from the last attempt.
    pub error: Option<String>,
}

/// The result of a secret operation on a worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretMsg {
    /// Which of the names have a value.
    Presence(Vec<(String, Result<bool, String>)>),
    /// `set` finished for `field` of `uploader`.
    Stored {
        /// The uploader.
        uploader: String,
        /// The field.
        field: String,
        /// The secret's name.
        name: String,
        /// How it went.
        result: Result<(), String>,
    },
    /// `delete` finished.
    Deleted {
        /// The uploader.
        uploader: String,
        /// The field.
        field: String,
        /// The secret's name.
        name: String,
        /// How it went.
        result: Result<(), String>,
    },
}

/// A test upload.
#[derive(Debug, Default)]
pub struct TestState {
    /// Which destination is being or was tested.
    pub name: String,
    slot: Slot<Result<UploadOutcome, String>>,
    progress: Arc<Mutex<Option<(u64, Option<u64>)>>>,
    cancel: Option<CancelToken>,
    /// The finished result.
    pub result: Option<Result<UploadOutcome, String>>,
}

impl TestState {
    /// Whether an upload is in flight.
    pub fn running(&self) -> bool {
        self.slot.running()
    }

    /// The latest progress `(sent, total)`.
    pub fn progress(&self) -> Option<(u64, Option<u64>)> {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// State of the Uploaders page.
#[derive(Debug, Default)]
pub struct State {
    /// Which half is showing.
    pub tab: Tab,
    /// The destination selected in the list.
    pub selected: Option<String>,
    /// A confirmation dialog.
    pub confirm: Option<Confirm>,
    /// The "New destination" dialog.
    pub new_dialog: Option<NewDialog>,
    /// The import dialog.
    pub import: Option<ImportDialog>,
    /// The "Advanced" fields are open.
    pub advanced_open: bool,
    /// Text typed into secret fields, by field key. Never written anywhere but the vault.
    pub secret_inputs: HashMap<String, String>,
    /// Whether secrets have a value, by secret name.
    pub presence: HashMap<String, Result<bool, String>>,
    /// An extension being added to the overrides.
    pub new_extension: String,
    /// The last test upload.
    pub test: TestState,
    file_dialog: Slot<Option<PathBuf>>,
    folder_dialog: Slot<Option<PathBuf>>,
    folder_target: Option<(String, &'static str)>,
    secret_slot: Slot<SecretMsg>,
    presence_wanted: Vec<String>,
}

impl State {
    /// Whether any background work is running (tests wait for this to become `false`).
    pub fn busy(&self) -> bool {
        self.test.running() || self.file_dialog.running() || self.folder_dialog.running() || self.secret_slot.running()
    }

    /// Selects a destination and forgets what was typed for the previous one.
    pub fn select(&mut self, name: Option<String>) {
        if self.selected != name {
            self.selected = name;
            self.secret_inputs.clear();
            self.presence.clear();
            self.presence_wanted.clear();
            self.test.result = None;
        }
    }

    /// Starts an import from `path`: checks it and opens the dialog.
    pub fn begin_import(&mut self, path: &std::path::Path, registry: &Registry, config_dir: &std::path::Path) {
        let preview = preview_import(path);
        let name = preview.suggested_name.clone();
        let _ = (registry, config_dir);
        self.import = Some(ImportDialog { preview, name, overwrite: false, error: None });
    }
}

/// What a secret field says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLine {
    /// The sentence.
    pub text: String,
    /// `true` for the "there is a value" states.
    pub ok: bool,
    /// The user can remove the value from here.
    pub removable: bool,
}

/// Works out what to say about a secret field.
///
/// `reference` is the field's value in the settings; `presence` what the vault answered for
/// that name (`None` = not asked yet).
pub fn secret_line(
    reference: &str,
    presence: Option<&Result<bool, String>>,
    from_env: Option<&str>,
    location: &SecretLocation,
) -> SecretLine {
    if !reference.is_empty() && !reference.starts_with(ssx_core::settings::KEYRING_PREFIX) {
        return SecretLine {
            text: "This field holds a plain-text value typed into settings.toml. It is not shown; enter it below to move it into the keyring.".to_owned(),
            ok: false,
            removable: false,
        };
    }
    if reference.is_empty() {
        return SecretLine { text: location.empty_text(), ok: false, removable: false };
    }
    if let Some(var) = from_env {
        return SecretLine {
            text: format!("provided by the environment variable {var}; it cannot be changed here"),
            ok: true,
            removable: false,
        };
    }
    match presence {
        None => SecretLine { text: "checking...".to_owned(), ok: false, removable: false },
        Some(Err(e)) => SecretLine { text: format!("cannot read the credential store: {e}"), ok: false, removable: false },
        Some(Ok(true)) => SecretLine { text: location.stored_text(), ok: true, removable: true },
        Some(Ok(false)) => SecretLine {
            text: "the settings refer to a secret that is not in the credential store; enter it below".to_owned(),
            ok: false,
            removable: true,
        },
    }
}

/// The fraction (0-1) of an upload that is done, if the total is known.
pub fn progress_fraction(sent: u64, total: Option<u64>) -> Option<f32> {
    total.filter(|t| *t > 0).map(|t| (sent as f64 / t as f64).clamp(0.0, 1.0) as f32)
}

/// `1.2 KB of 3.4 KB`.
pub fn progress_text(sent: u64, total: Option<u64>) -> String {
    use crate::history_view::human_bytes;
    match total {
        Some(t) => format!("{} of {}", human_bytes(sent), human_bytes(t)),
        None => human_bytes(sent),
    }
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let ctx = ui.ctx().clone();
    poll(st, cx, &ctx);
    handle_drops(st, cx, &ctx);
    ui.horizontal(|ui| {
        let options = [(Tab::Destinations, "Destinations"), (Tab::Defaults, "Defaults")];
        if let Some(t) = ui_kit::segmented_row(ui, st.tab, &options) {
            st.tab = t;
        }
    });
    ui.add_space(8.0);
    match st.tab {
        Tab::Destinations => ui_kit::page_scroll(ui, "uploaders-dest", |ui| destinations(ui, st, cx)),
        Tab::Defaults => ui_kit::page_scroll(ui, "uploaders-defaults", |ui| defaults(ui, st, cx)),
    }
    dialogs(&ctx, st, cx);
    drop_overlay(&ctx);
}

fn poll(st: &mut State, cx: &mut Cx<'_>, ctx: &egui::Context) {
    let t = ctx.input(|i| i.time);
    if let Some(Some(path)) = st.file_dialog.poll() {
        st.begin_import(&path, &cx.registry, &cx.host.paths.config_dir);
    }
    if let Some(Some(dir)) = st.folder_dialog.poll()
        && let Some((uploader, key)) = st.folder_target.take()
        && let Some(table) = cx.settings.uploaders.get_mut(&uploader)
    {
        set_text(table, key, &dir.display().to_string());
    }
    if let Some(r) = st.test.slot.poll() {
        st.test.result = Some(r);
        st.test.cancel = None;
    }
    if let Some(msg) = st.secret_slot.poll() {
        match msg {
            SecretMsg::Presence(list) => {
                for (n, r) in list {
                    st.presence.insert(n, r);
                }
            }
            SecretMsg::Stored { uploader, field, name, result } => match result {
                Ok(()) => {
                    if let Some(table) = cx.settings.uploaders.get_mut(&uploader) {
                        set_text(table, &field, &reference_for(&name));
                    }
                    st.presence.insert(name, Ok(true));
                    st.secret_inputs.remove(&field);
                    cx.toasts.success(t, "Secret stored. The settings only refer to it by name.");
                }
                Err(e) => cx.toasts.error(t, format!("The secret was not stored: {e}")),
            },
            SecretMsg::Deleted { uploader, field, name, result } => match result {
                Ok(()) => {
                    if let Some(table) = cx.settings.uploaders.get_mut(&uploader) {
                        set_text(table, &field, "");
                    }
                    st.presence.remove(&name);
                    cx.toasts.info(t, "Secret removed from the credential store.");
                }
                Err(e) => cx.toasts.error(t, format!("The secret was not removed: {e}")),
            },
        }
    }
}

fn handle_drops(st: &mut State, cx: &mut Cx<'_>, ctx: &egui::Context) {
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    for f in dropped {
        let Some(path) = f.path else { continue };
        let is_sxcu = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sxcu"));
        if is_sxcu {
            st.tab = Tab::Destinations;
            st.begin_import(&path, &cx.registry, &cx.host.paths.config_dir);
        } else {
            let t = ctx.input(|i| i.time);
            cx.toasts.error(t, format!("{} is not a .sxcu file", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())));
        }
    }
}

fn drop_overlay(ctx: &egui::Context) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop-overlay")));
    painter.rect_filled(screen, 0.0, Color32::from_black_alpha(150));
    painter.text(screen.center(), egui::Align2::CENTER_CENTER, "Drop a .sxcu file to import it", egui::FontId::proportional(22.0), Color32::WHITE);
}

// ---- destinations tab ---------------------------------------------------------------------

fn destinations(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let total = ui.available_width();
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 14.0;
        ui.vertical(|ui| {
            ui.set_width(290.0);
            list_panel(ui, st, cx);
        });
        ui.vertical(|ui| {
            ui.set_width((total - 304.0).max(320.0));
            detail_panel(ui, st, cx);
        });
    });
}

fn list_panel(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        input_style(ui);
        let busy = st.file_dialog.running();
        if ui_kit::button_if(ui, "Import .sxcu...", !busy, "A dialog is already open").on_hover_text("Import a ShareX custom uploader (or drop the file on this window)").clicked() {
            let dialogs = cx.host.dialogs.clone();
            st.file_dialog.start(cx.wake, move || dialogs.pick_file("Import a ShareX custom uploader", &["sxcu"]));
        }
        let menu = ui.menu_button("New destination", |ui| {
            ui.set_min_width(260.0);
            for k in UploaderKind::ALL {
                if ui.button(k.label()).on_hover_text(k.blurb()).clicked() {
                    let taken = cx.registry.names();
                    st.new_dialog = Some(NewDialog { kind: k, name: free_name(k.type_name(), &taken) });
                    ui.close();
                }
            }
        });
        menu.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "New destination"));
    });
    ui.add_space(8.0);
    let registry = cx.registry.clone();
    ui_kit::card(ui, None, |ui| {
        if registry.entries.is_empty() {
            ui_kit::hint(ui, "No destinations.");
        }
        for e in &registry.entries {
            let selected = st.selected.as_deref() == Some(e.name.as_str());
            let origin = match Registry::source(e) {
                Source::Builtin => "built in".to_owned(),
                Source::Table => "settings".to_owned(),
                Source::File => std::path::Path::new(&e.origin).file_name().map_or_else(|| e.origin.clone(), |n| n.to_string_lossy().into_owned()),
            };
            let r = ui_kit::list_item(ui, selected, &e.name, &format!("{}  \u{b7}  {origin}", e.kind), e.error.as_ref().map(|_| Severity::Error), &format!("Destination {}", e.name));
            if r.clicked() {
                st.select(Some(e.name.clone()));
            }
            ui.add_space(2.0);
        }
    });
    ui_kit::hint(ui, "Drop a .sxcu file anywhere on this window to import it.");
}

fn detail_panel(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    if st.selected.is_none()
        && let Some(first) = cx.registry.entries.iter().find(|e| Registry::source(e) != Source::Builtin).or(cx.registry.entries.first())
    {
        st.select(Some(first.name.clone()));
    }
    let Some(name) = st.selected.clone() else {
        ui_kit::card(ui, None, |ui| {
            ui_kit::hint(ui, "Select a destination to see its settings, or import a ShareX .sxcu file, or create a new one.");
        });
        return;
    };
    let registry = cx.registry.clone();
    let Some(info) = registry.get(&name).cloned() else {
        st.select(None);
        return;
    };
    ui_kit::card(ui, Some(&name), |ui| {
        ui.horizontal(|ui| {
            ui_kit::badge(ui, &info.kind, theme::ACCENT);
            let origin = match Registry::source(&info) {
                Source::Builtin => "built in".to_owned(),
                Source::Table => "defined in settings.toml".to_owned(),
                Source::File => format!("imported file {}", std::path::Path::new(&info.origin).file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())),
            };
            ui.label(RichText::new(origin).color(theme::TEXT_DIM));
        });
        ui.add_space(4.0);
        let mut accepts: Vec<&str> = info.uploads.clone();
        if info.shortens {
            accepts.push("URL shortening");
        }
        if !accepts.is_empty() {
            ui_kit::hint(ui, &format!("Accepts: {}", accepts.join(", ")));
        }
        if let Some(e) = &info.error {
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                ui_kit::severity_icon(ui, Severity::Error);
                ui.add(egui::Label::new(RichText::new(e).color(ui_kit::ERROR_TEXT)).wrap());
            });
        }
        let refs = Registry::references_to(cx.settings, &name);
        if !refs.is_empty() {
            ui.add_space(4.0);
            ui_kit::hint(ui, &format!("Used by: {}.", refs.join("; ")));
        }
    });

    if Registry::source(&info) == Source::Table {
        form_card(ui, st, cx, &name);
    }

    test_card(ui, st, cx, &name, info.error.is_none());

    match Registry::source(&info) {
        Source::Table => {
            if ui_kit::danger(ui, "Remove this destination...").clicked() {
                st.confirm = Some(Confirm::RemoveTable(name));
            }
        }
        Source::File => {
            if ui_kit::danger(ui, "Delete the imported file...").clicked() {
                st.confirm = Some(Confirm::RemoveFile(name));
            }
        }
        Source::Builtin => ui_kit::hint(ui, "Built-in destinations need no setup and cannot be removed."),
    }
}

fn form_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>, name: &str) {
    let Some(table) = cx.settings.uploaders.get(name).cloned() else { return };
    let Some(kind) = UploaderKind::of(&table) else {
        ui_kit::card(ui, Some("Settings"), |ui| {
            ui_kit::hint(ui, "This table has a type the window cannot edit; change it in settings.toml.");
        });
        return;
    };
    // The real builder decides whether the table works.
    let verdict = check(name, &table, &cx.host.paths.config_dir);
    let mut edited = table.clone();
    let issues = cx.issues.clone();
    ui_kit::card(ui, Some(&format!("{} settings", kind.label())), |ui| {
        match &verdict {
            Ok(()) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
                    ui.label(RichText::new("These settings are complete.").color(theme::TEXT));
                });
            }
            Err(e) => {
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, Severity::Warning);
                    ui.add(egui::Label::new(RichText::new(e).color(ui_kit::WARN_TEXT)).wrap());
                });
            }
        }
        ui.add_space(6.0);
        let base = format!("uploaders.{name}");
        for spec in fields(kind).iter().filter(|s| !s.advanced) {
            field_row(ui, st, cx, name, spec, &mut edited, &issues, &base);
        }
        let advanced: Vec<&FieldSpec> = fields(kind).iter().filter(|s| s.advanced).collect();
        if !advanced.is_empty() {
            ui.add_space(4.0);
            let label = if st.advanced_open { "Hide advanced settings" } else { "Show advanced settings" };
            if ui_kit::link(ui, label).clicked() {
                st.advanced_open = !st.advanced_open;
            }
            if st.advanced_open {
                ui.add_space(4.0);
                for spec in advanced {
                    field_row(ui, st, cx, name, spec, &mut edited, &issues, &base);
                }
            }
        }
        ui_kit::issue_lines_exact(ui, &issues, &base);
    });
    if edited != table {
        cx.settings.uploaders.insert(name.to_owned(), edited);
    }
}

#[allow(clippy::too_many_arguments)] // one call site per field kind; a struct would only rename the arguments
fn field_row(
    ui: &mut Ui,
    st: &mut State,
    cx: &mut Cx<'_>,
    uploader: &str,
    spec: &FieldSpec,
    table: &mut toml::Table,
    issues: &crate::validation::Issues,
    base: &str,
) {
    let path = format!("{base}.{}", spec.key);
    let mut f = Field::new(spec.label).issues(issues, &path).help(spec.help);
    if spec.required {
        f = f.required();
    }
    f.show(ui, |ui| match spec.kind {
        FieldKind::Text => {
            let mut v = get_text(table, spec.key).to_owned();
            if ui_kit::text_input(ui, spec.label, &mut v, "", 360.0).changed() {
                if spec.required {
                    set_required_text(table, spec.key, &v);
                } else {
                    set_text(table, spec.key, &v);
                }
            }
        }
        FieldKind::Folder => {
            let mut v = get_text(table, spec.key).to_owned();
            ui.horizontal(|ui| {
                if ui_kit::text_input(ui, spec.label, &mut v, "", 300.0).changed() {
                    set_text(table, spec.key, &v);
                }
                if ui_kit::button_if(ui, "Browse...", !st.folder_dialog.running(), "A dialog is open").clicked() {
                    let dialogs = cx.host.dialogs.clone();
                    st.folder_target = Some((uploader.to_owned(), spec.key));
                    st.folder_dialog.start(cx.wake, move || dialogs.pick_folder(None));
                }
            });
        }
        FieldKind::Choice(options) => {
            input_style(ui);
            let cur = get_text(table, spec.key).to_owned();
            let shown = if cur.is_empty() { "(default)".to_owned() } else { cur.clone() };
            let combo = egui::ComboBox::from_id_salt((uploader, spec.key)).width(220.0).selected_text(shown).show_ui(ui, |ui| {
                if ui.selectable_label(cur.is_empty(), "(default)").clicked() {
                    set_text(table, spec.key, "");
                }
                for o in options {
                    if ui.selectable_label(cur == *o, *o).clicked() {
                        set_text(table, spec.key, o);
                    }
                }
            });
            combo.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, spec.label));
        }
        FieldKind::Map => {
            let mut entries = get_map(table, spec.key);
            let mut remove = None;
            for (k, (name, value)) in entries.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui_kit::text_input(ui, &format!("{} name {}", spec.label, k + 1), name, "name", 140.0);
                    ui_kit::text_input(ui, &format!("{} value {}", spec.label, k + 1), value, "value", 200.0);
                    if ui_kit::mini_icon_button(ui, Icon::Close, &format!("Remove {} {}", spec.label, k + 1), true).clicked() {
                        remove = Some(k);
                    }
                });
            }
            if let Some(k) = remove {
                entries.remove(k);
            }
            if ui_kit::button(ui, "Add").clicked() {
                entries.push((format!("Name{}", entries.len() + 1), String::new()));
            }
            set_map(table, spec.key, &entries);
        }
        FieldKind::Secret => secret_field(ui, st, cx, uploader, spec, table),
    });
    ui.add_space(3.0);
}

fn secret_field(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>, uploader: &str, spec: &FieldSpec, table: &mut toml::Table) {
    let reference = get_text(table, spec.key).to_owned();
    let secret_name = reference_name(&reference).map(str::to_owned);
    let vault: Arc<dyn SecretVault> = cx.host.vault.clone();
    let location = vault.location();
    // Ask the vault (off-thread) whether the referenced secret has a value.
    if let Some(n) = &secret_name
        && !st.presence.contains_key(n)
        && !st.secret_slot.running()
    {
        let (v, n2) = (vault.clone(), n.clone());
        st.secret_slot.start(cx.wake, move || SecretMsg::Presence(vec![(n2.clone(), v.exists(&n2))]));
    }
    let env_var = secret_name.as_deref().filter(|n| vault.from_environment(n)).map(ssx_services::secrets::env_var_name);
    let line = secret_line(&reference, secret_name.as_ref().and_then(|n| st.presence.get(n)), env_var.as_deref(), &location);
    ui.horizontal_top(|ui| {
        if line.ok {
            ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
        }
        ui.add(egui::Label::new(RichText::new(&line.text).size(12.5).color(if line.ok { ui_kit::OK_TEXT } else { theme::TEXT_DIM })).wrap());
    });
    if env_var.is_none() {
        let input = st.secret_inputs.entry(spec.key.to_owned()).or_default();
        ui.horizontal(|ui| {
            input_style(ui);
            let r = ui.add(
                egui::TextEdit::singleline(input)
                    .password(true)
                    .hint_text("enter a new value")
                    .desired_width(240.0)
                    .margin(egui::Margin::symmetric(6, 4)),
            );
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, format!("{} (new value)", spec.label)));
            let can = !input.is_empty() && !st.secret_slot.running();
            if ui_kit::button_if(ui, "Store secret", can, "Type the value first").clicked() {
                let name = secret_name.clone().unwrap_or_else(|| default_secret_name(uploader, spec.key));
                let (v, value) = (vault.clone(), std::mem::take(input));
                let (u, f) = (uploader.to_owned(), spec.key.to_owned());
                st.secret_slot.start(cx.wake, move || {
                    let result = v.set(&name, &value);
                    SecretMsg::Stored { uploader: u, field: f, name, result }
                });
            }
            if line.removable
                && let Some(n) = &secret_name
                && ui_kit::button_if(ui, "Remove", !st.secret_slot.running(), "Busy").clicked()
            {
                let (v, n) = (vault.clone(), n.clone());
                let (u, f) = (uploader.to_owned(), spec.key.to_owned());
                st.secret_slot.start(cx.wake, move || {
                    let result = v.delete(&n);
                    SecretMsg::Deleted { uploader: u, field: f, name: n, result }
                });
            }
        });
    }
    let _ = has_plaintext_secret;
}

fn test_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>, name: &str, usable: bool) {
    ui_kit::card(ui, Some("Test"), |ui| {
        ui_kit::hint(ui, "Uploads a tiny generated image (or shortens example.com) with the settings as they are on this page, saved or not.");
        ui.add_space(6.0);
        let running = st.test.running() && st.test.name == name;
        ui.horizontal(|ui| {
            if running {
                if ui_kit::button(ui, "Cancel").clicked()
                    && let Some(c) = &st.test.cancel
                {
                    c.cancel();
                }
            } else if ui_kit::button_if(ui, "Test upload", usable && !st.test.running(), if usable { "Another test is running" } else { "Fix the settings first" }).clicked() {
                start_test(st, cx, name);
            }
        });
        if running {
            ui.add_space(6.0);
            let (sent, total) = st.test.progress().unwrap_or((0, None));
            let bar = match progress_fraction(sent, total) {
                Some(f) => egui::ProgressBar::new(f).text(progress_text(sent, total)),
                None => egui::ProgressBar::new(0.0).animate(true).text(if sent == 0 { "connecting...".to_owned() } else { progress_text(sent, total) }),
            };
            ui.add(bar.desired_width(360.0));
        }
        if !running && st.test.name == name {
            match &st.test.result {
                Some(Ok(o)) => {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
                        ui.label(RichText::new("Uploaded").color(ui_kit::OK_TEXT));
                    });
                    url_row(ui, cx, "URL", &o.url);
                    if let Some(d) = &o.deletion_url {
                        url_row(ui, cx, "Delete URL", d);
                    }
                }
                Some(Err(e)) => {
                    ui.add_space(6.0);
                    ui.horizontal_top(|ui| {
                        ui_kit::severity_icon(ui, Severity::Error);
                        ui.add(egui::Label::new(RichText::new(e).color(ui_kit::ERROR_TEXT)).wrap());
                    });
                }
                None => {}
            }
        }
    });
}

fn url_row(ui: &mut Ui, cx: &mut Cx<'_>, label: &str, url: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).color(theme::TEXT_DIM));
        ui.add(egui::Label::new(RichText::new(url).monospace().size(12.5)).selectable(true));
        if ui_kit::button(ui, "Copy").on_hover_text("Copy the URL").clicked() {
            ui.ctx().copy_text(url.to_owned());
        }
        if url.starts_with("http") && ui_kit::button(ui, "Open").clicked() {
            let t = ui.input(|i| i.time);
            if let Err(e) = cx.host.opener.open_url(url) {
                cx.toasts.error(t, e);
            }
        }
    });
}

/// Starts a test upload of `name` on a worker thread.
pub fn start_test(st: &mut State, cx: &Cx<'_>, name: &str) {
    let cancel = CancelToken::new();
    let progress = Arc::new(Mutex::new(None));
    let job = TestJob {
        settings: cx.settings.clone(),
        config_dir: cx.host.paths.config_dir.clone(),
        name: name.to_owned(),
        secrets: cx.host.vault.store(),
    };
    let uploads = cx.host.uploads.clone();
    let (c, p) = (cancel.clone(), progress.clone());
    st.test.name = name.to_owned();
    st.test.result = None;
    st.test.cancel = Some(cancel);
    st.test.progress = progress;
    st.test.slot.start(cx.wake, move || {
        uploads.test(&job, &c, &|pr| {
            *p.lock().unwrap_or_else(PoisonError::into_inner) = Some((pr.sent, pr.total));
        })
    });
}

// ---- defaults tab -------------------------------------------------------------------------

fn defaults(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let registry = cx.registry.clone();
    let issues = cx.issues.clone();
    ui_kit::card(ui, Some("Where content goes"), |ui| {
        ui_kit::hint(ui, "The destination for each kind of content. A workflow can override these on the Workflows page.");
        ui.add_space(6.0);
        for (ty, label, key) in [
            (DestinationType::Image, "Images", "image"),
            (DestinationType::Text, "Text", "text"),
            (DestinationType::File, "Files", "file"),
            (DestinationType::Video, "Videos", "video"),
            (DestinationType::UrlShortener, "URL shortener", "url_shortener"),
            (DestinationType::UrlSharing, "URL sharing", "url_sharing"),
        ] {
            let d = &mut cx.settings.destinations;
            let file_default = d.file.clone();
            let slot = match ty {
                DestinationType::Image => &mut d.image,
                DestinationType::Text => &mut d.text,
                DestinationType::File => &mut d.file,
                DestinationType::Video => &mut d.video,
                DestinationType::UrlShortener => &mut d.url_shortener,
                DestinationType::UrlSharing => &mut d.url_sharing,
            };
            let none_label = if ty == DestinationType::Video {
                match &file_default {
                    Some(f) => format!("Same as files ({f})"),
                    None => "Same as files (not set)".to_owned(),
                }
            } else {
                "Not set".to_owned()
            };
            Field::new(label).issues(&issues, &format!("destinations.{key}")).show(ui, |ui| {
                destination_picker(ui, ("default-dest", key), &format!("{label} destination"), slot, ty, &registry, &none_label);
            });
            ui.add_space(3.0);
        }
    });
    extension_card(ui, st, cx, &registry, &issues);
    ui_kit::card(ui, Some("Uploading files"), |ui| {
        Field::new("Edit images first").help("Images passed to Upload files go through the editor first, if the workflow has that step.").show(ui, |ui| {
            ui_kit::switch(ui, "Open the editor for image files", &mut cx.settings.post_file.images_through_editor);
        });
        ui.add_space(3.0);
        Field::new("Folders").help("What to do when a folder is uploaded.").show(ui, |ui| {
            let opts = [(FolderPolicy::Zip, "Zip and upload"), (FolderPolicy::Error, "Refuse")];
            if let Some(p) = ui_kit::segmented_row(ui, cx.settings.post_file.folders, &opts) {
                cx.settings.post_file.folders = p;
            }
        });
        ui.add_space(3.0);
        Field::new("At the same time").issues(&issues, "post_file.max_parallel_uploads").help("How many files are uploaded at once (1 to 16).").show(ui, |ui| {
            input_style(ui);
            let mut n = cx.settings.post_file.max_parallel_uploads;
            let r = ui.add(egui::DragValue::new(&mut n).range(1..=16).clamp_existing_to_range(false).suffix(" files"));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::DragValue, true, "Parallel uploads"));
            if r.changed() {
                cx.settings.post_file.max_parallel_uploads = n;
            }
        });
    });
}

fn extension_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>, registry: &Registry, issues: &crate::validation::Issues) {
    ui_kit::card(ui, Some("By file type"), |ui| {
        ui_kit::hint(ui, "Send files with a given extension to a specific destination, whatever kind of content they are (for example zip to your own server).");
        ui.add_space(6.0);
        let exts: Vec<String> = cx.settings.destinations.extension_overrides.keys().cloned().collect();
        let mut remove = None;
        for ext in &exts {
            let path = format!("destinations.extension_overrides.{ext}");
            Field::new(&format!(".{ext}")).issues(issues, &path).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut cur = cx.settings.destinations.extension_overrides.get(ext).cloned();
                    if destination_picker(ui, ("ext-dest", ext), &format!("Destination for .{ext}"), &mut cur, DestinationType::File, registry, "Choose...")
                        && let Some(c) = cur
                    {
                        cx.settings.destinations.extension_overrides.insert(ext.clone(), c);
                    }
                    if ui_kit::mini_icon_button(ui, Icon::Close, &format!("Remove the override for .{ext}"), true).clicked() {
                        remove = Some(ext.clone());
                    }
                });
            });
        }
        if let Some(e) = remove {
            cx.settings.destinations.extension_overrides.remove(&e);
        }
        ui.horizontal(|ui| {
            ui_kit::text_input(ui, "New extension", &mut st.new_extension, "zip", 90.0);
            let ext = st.new_extension.trim().trim_start_matches('.').to_ascii_lowercase();
            let free = !ext.is_empty() && !cx.settings.destinations.extension_overrides.contains_key(&ext);
            if ui_kit::button_if(ui, "Add", free, if ext.is_empty() { "Type an extension such as zip" } else { "That extension already has a rule" }).clicked() {
                let default = registry.choices_for(DestinationType::File).first().map(|e| e.name.clone()).unwrap_or_else(|| "local".to_owned());
                cx.settings.destinations.extension_overrides.insert(ext, default);
                st.new_extension.clear();
            }
        });
    });
}

// ---- dialogs ------------------------------------------------------------------------------

fn dialogs(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    new_dialog(ctx, st, cx);
    import_dialog(ctx, st, cx);
    confirm_dialog(ctx, st, cx);
}

fn new_dialog(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    let Some(mut d) = st.new_dialog.take() else { return };
    let taken = cx.registry.names();
    let problem = name_problem(&d.name, &taken);
    let mut answer = None;
    let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
    let m = egui::Modal::new(egui::Id::new("up-new")).frame(frame).show(ctx, |ui| {
        input_style(ui);
        ui.set_width(440.0);
        ui.label(RichText::new("New destination").heading().strong());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for k in UploaderKind::ALL {
                if ui_kit::chip(ui, k.label(), d.kind == k).clicked() {
                    let was_default = d.name == free_name(d.kind.type_name(), &taken);
                    d.kind = k;
                    if was_default {
                        d.name = free_name(k.type_name(), &taken);
                    }
                }
            }
        });
        ui_kit::hint(ui, d.kind.blurb());
        ui.add_space(6.0);
        Field::new("Name").show(ui, |ui| {
            ui_kit::text_input(ui, "Destination name", &mut d.name, "", 240.0);
            if let Some(p) = &problem {
                ui.label(RichText::new(p).size(12.0).color(ui_kit::ERROR_TEXT));
            }
        });
        ui.add_space(10.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui_kit::primary(ui, "Create", problem.is_none()).clicked() && problem.is_none() {
                answer = Some(Answer::Confirm);
            }
            if ui_kit::button(ui, "Cancel").clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if answer.is_none() && m.should_close() {
        answer = Some(Answer::Cancel);
    }
    match answer {
        Some(Answer::Confirm) => {
            cx.settings.uploaders.insert(d.name.clone(), new_table(d.kind));
            st.select(Some(d.name));
            st.tab = Tab::Destinations;
        }
        Some(Answer::Cancel) => {}
        None => st.new_dialog = Some(d),
    }
}

fn import_dialog(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    let Some(mut d) = st.import.take() else { return };
    let cfg = cx.host.paths.config_dir.clone();
    let taken: Vec<String> = cx.registry.names();
    let collision = import_collision(&cx.registry, &cfg, &d.name);
    let name_err = if d.name.is_empty() || !ssx_services::upload::sxcu_files::valid_uploader_name(&d.name) {
        name_problem(&d.name, &[])
    } else {
        None
    };
    let mut answer = None;
    let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
    let m = egui::Modal::new(egui::Id::new("up-import")).frame(frame).show(ctx, |ui| {
        input_style(ui);
        ui.set_width(500.0);
        ui.label(RichText::new("Import a ShareX custom uploader").heading().strong());
        ui.add_space(4.0);
        let file = d.preview.source.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        ui_kit::hint(ui, &format!("File: {file}"));
        ui.add_space(6.0);
        if let Some(e) = &d.preview.error {
            ui.horizontal_top(|ui| {
                ui_kit::severity_icon(ui, Severity::Error);
                ui.add(egui::Label::new(RichText::new(format!("This file cannot be imported: {e}")).color(ui_kit::ERROR_TEXT)).wrap());
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
                ui.label(format!("Valid: \"{}\"", d.preview.display_name));
            });
            for w in &d.preview.warnings {
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, Severity::Warning);
                    ui.add(egui::Label::new(RichText::new(w).size(12.5).color(ui_kit::WARN_TEXT)).wrap());
                });
            }
            ui.add_space(8.0);
            Field::new("Import as").show(ui, |ui| {
                ui_kit::text_input(ui, "Destination name", &mut d.name, "", 240.0);
                if let Some(p) = &name_err {
                    ui.label(RichText::new(p).size(12.0).color(ui_kit::ERROR_TEXT));
                }
                match &collision {
                    Collision::ReplacesFile => {
                        ui_kit::switch(ui, "Replace the file I imported earlier under this name", &mut d.overwrite);
                    }
                    Collision::Shadowed(what) => {
                        ui.horizontal_top(|ui| {
                            ui_kit::severity_icon(ui, Severity::Warning);
                            ui.add(egui::Label::new(RichText::new(format!("This name is already used by {what}; the imported file would never be used. Choose another name.")).size(12.0).color(ui_kit::WARN_TEXT)).wrap());
                        });
                    }
                    Collision::None => {}
                }
            });
        }
        if let Some(e) = &d.error {
            ui.label(RichText::new(e).color(ui_kit::ERROR_TEXT));
        }
        ui.add_space(10.0);
        let can = d.preview.importable()
            && name_err.is_none()
            && !matches!(collision, Collision::Shadowed(_))
            && (collision != Collision::ReplacesFile || d.overwrite);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui_kit::primary(ui, "Import", can).clicked() && can {
                answer = Some(Answer::Confirm);
            }
            if ui_kit::button(ui, if d.preview.importable() { "Cancel" } else { "Close" }).clicked() {
                answer = Some(Answer::Cancel);
            }
        });
    });
    if answer.is_none() && m.should_close() {
        answer = Some(Answer::Cancel);
    }
    let _ = taken;
    match answer {
        Some(Answer::Confirm) => match import(&cfg, &d.preview.source, &d.name, d.overwrite) {
            Ok(done) => {
                let t = ctx.input(|i| i.time);
                cx.toasts.success(t, format!("Imported {:?} as {}. It is used like any other destination.", done.display_name, done.name));
                st.select(Some(done.name));
                st.tab = Tab::Destinations;
            }
            Err(e) => {
                d.error = Some(e.to_string());
                st.import = Some(d);
            }
        },
        Some(Answer::Cancel) => {}
        None => st.import = Some(d),
    }
}

fn confirm_dialog(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    let Some(c) = st.confirm.clone() else { return };
    let (name, title, verb, what) = match &c {
        Confirm::RemoveTable(n) => (n.clone(), "Remove this destination?", "Remove", "The [uploaders] table is removed from the settings when you save."),
        Confirm::RemoveFile(n) => (n.clone(), "Delete the imported file?", "Delete file", "The imported .sxcu file is deleted from disk right away."),
    };
    let refs = Registry::references_to(cx.settings, &name);
    let mut body = format!("\"{name}\": {what}");
    if !refs.is_empty() {
        body.push_str(&format!("\n\nStill used by: {}. They will report that the destination does not exist until you change them.", refs.join("; ")));
    }
    match ui_kit::confirm(ctx, "up-confirm", title, &body, verb, true) {
        Some(Answer::Confirm) => {
            let t = ctx.input(|i| i.time);
            match &c {
                Confirm::RemoveTable(n) => {
                    cx.settings.uploaders.remove(n);
                    cx.toasts.info(t, format!("Removed {n}. Save to make it final."));
                }
                Confirm::RemoveFile(n) => match remove_file(&cx.host.paths.config_dir, n) {
                    Ok(true) => cx.toasts.info(t, format!("Deleted the imported file for {n}.")),
                    Ok(false) => cx.toasts.error(t, "There was no imported file to delete."),
                    Err(e) => cx.toasts.error(t, format!("Could not delete it: {e}")),
                },
            }
            st.select(None);
            st.confirm = None;
        }
        Some(Answer::Cancel) => st.confirm = None,
        None => {}
    }
    let _ = type_word;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring() -> SecretLocation {
        SecretLocation::Keyring("Secret Service".into())
    }

    #[test]
    fn secret_lines_cover_every_state() {
        let l = secret_line("", None, None, &keyring());
        assert_eq!(l.text, "not set");
        assert!(!l.ok && !l.removable);
        let l = secret_line("keyring:x", Some(&Ok(true)), None, &keyring());
        assert!(l.ok && l.removable && l.text.contains("keyring"), "{l:?}");
        let l = secret_line("keyring:x", Some(&Ok(true)), None, &SecretLocation::MemoryOnly("no bus".into()));
        assert!(l.text.contains("memory only"), "{l:?}");
        let l = secret_line("keyring:x", Some(&Ok(false)), None, &keyring());
        assert!(!l.ok && l.removable && l.text.contains("not in the credential store"));
        let l = secret_line("keyring:x", None, None, &keyring());
        assert_eq!(l.text, "checking...");
        let l = secret_line("keyring:x", Some(&Err("locked".into())), None, &keyring());
        assert!(l.text.contains("locked"));
        let l = secret_line("keyring:x", Some(&Ok(true)), Some("SSX_SECRET_X"), &keyring());
        assert!(l.ok && !l.removable && l.text.contains("SSX_SECRET_X"));
        let l = secret_line("hunter2", None, None, &keyring());
        assert!(!l.text.contains("hunter2"), "a plain-text value is never echoed");
        assert!(l.text.contains("plain-text"));
    }

    #[test]
    fn progress_maths() {
        assert_eq!(progress_fraction(50, Some(100)), Some(0.5));
        assert_eq!(progress_fraction(500, Some(100)), Some(1.0));
        assert_eq!(progress_fraction(5, Some(0)), None);
        assert_eq!(progress_fraction(5, None), None);
        assert_eq!(progress_text(512, Some(2048)), "512 B of 2.0 KB");
        assert_eq!(progress_text(1536, None), "1.5 KB");
    }

    #[test]
    fn selecting_another_destination_forgets_secret_input() {
        let mut st = State::default();
        st.select(Some("a".into()));
        st.secret_inputs.insert("access_key_id".into(), "AKIA...".into());
        st.presence.insert("a-key".into(), Ok(true));
        st.select(Some("a".into()));
        assert!(!st.secret_inputs.is_empty(), "same selection keeps it");
        st.select(Some("b".into()));
        assert!(st.secret_inputs.is_empty() && st.presence.is_empty());
    }

    #[test]
    fn begin_import_checks_the_file_and_suggests_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.sxcu");
        std::fs::write(&f, r#"{"Version":"13.7.0","Name":"My Host","DestinationType":"ImageUploader","RequestMethod":"POST","RequestURL":"https://e.example.com/up","Body":"MultipartFormData","FileFormName":"f","URL":"{json:u}"}"#).unwrap();
        let mut st = State::default();
        st.begin_import(&f, &Registry::default(), dir.path());
        let d = st.import.unwrap();
        assert!(d.preview.importable());
        assert_eq!(d.name, "My-Host");
        assert!(!d.overwrite && d.error.is_none());
    }
}
