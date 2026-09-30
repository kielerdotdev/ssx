//! Pieces used by more than one page.

use egui::{RichText, Ui};
use ssx_core::settings::{DestinationType, Severity};
use ssx_editor_ui::ui::widgets::input_style;

use crate::{
    ui_kit,
    uploader_registry::{Registry, Source},
};

/// A short description of one destination for a menu: `name  (S3, my config)`.
pub fn describe(registry: &Registry, name: &str) -> String {
    match registry.get(name) {
        None => format!("{name}  (unknown)"),
        Some(e) if e.error.is_some() => format!("{name}  (broken)"),
        Some(e) => {
            let origin = match Registry::source(e) {
                Source::Builtin => "built in",
                Source::Table => "settings",
                Source::File => ".sxcu file",
            };
            format!("{name}  ({}, {origin})", e.kind)
        }
    }
}

/// A combo box that picks an uploader for `ty`, or "not set" (`none_label`). Returns `true` when
/// the choice changed. A problem with the current choice is shown underneath.
pub fn destination_picker(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    label: &str,
    current: &mut Option<String>,
    ty: DestinationType,
    registry: &Registry,
    none_label: &str,
) -> bool {
    input_style(ui);
    let mut changed = false;
    let selected_text = match current.as_deref() {
        None => none_label.to_owned(),
        Some(n) => describe(registry, n),
    };
    let combo = egui::ComboBox::from_id_salt(id_salt).width(320.0).selected_text(selected_text).show_ui(ui, |ui| {
        if ui.selectable_label(current.is_none(), none_label).clicked() && current.is_some() {
            *current = None;
            changed = true;
        }
        for e in registry.choices_for(ty) {
            let text = describe(registry, &e.name);
            if ui.selectable_label(current.as_deref() == Some(e.name.as_str()), text).clicked()
                && current.as_deref() != Some(e.name.as_str())
            {
                *current = Some(e.name.clone());
                changed = true;
            }
        }
        // A name that is not in the registry (typed in the file, or its table was removed)
        // stays selectable, so it is visible and can be replaced.
        if let Some(n) = current.clone()
            && registry.get(&n).is_none()
        {
            let _ = ui.selectable_label(true, format!("{n}  (unknown)"));
        }
    });
    combo.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, label));
    if let Some(n) = current.as_deref()
        && let Some(p) = registry.problem_for(ty, n)
    {
        ui.horizontal_top(|ui| {
            ui_kit::severity_icon(ui, Severity::Warning);
            ui.add(egui::Label::new(RichText::new(p).size(12.0).color(ui_kit::WARN_TEXT)).wrap());
        });
    }
    changed
}

#[cfg(test)]
mod tests {
    use ssx_core::settings::Settings;
    use ssx_upload::InMemorySecretStore;

    use super::*;

    #[test]
    fn descriptions_name_the_kind_and_origin() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Settings::default();
        s.uploaders.insert("mine".into(), "type='local'".parse().unwrap());
        s.uploaders.insert("oops".into(), "type='http'".parse().unwrap());
        let r = Registry::build(&s, dir.path(), std::sync::Arc::new(InMemorySecretStore::new()));
        assert_eq!(describe(&r, "local"), "local  (local, built in)");
        assert_eq!(describe(&r, "mine"), "mine  (local, settings)");
        assert_eq!(describe(&r, "oops"), "oops  (broken)");
        assert_eq!(describe(&r, "ghost"), "ghost  (unknown)");
    }
}
