//! The shortcut editor: a display of the current shortcut, modifier toggles, a key list, and
//! a **Record** button that turns the next key combination pressed into the shortcut.
//!
//! The translation of key presses lives in [`crate::hotkey_keys`] (pure and tested); this file
//! only handles focus and drawing. Why not *only* recording: egui cannot see PrintScreen,
//! Pause or the Windows key as key events, and PrintScreen is the classic screenshot key, so
//! the modifier toggles and the key list are first-class, not a fallback.

use egui::{Color32, EventFilter, Id, RichText, Ui, WidgetInfo, WidgetType};
use ssx_editor_ui::ui::theme;
use ssx_hotkeys::Modifiers;

use crate::{
    hotkey_keys::{HotkeyDraft, Translated, display_key, is_super_key, key_groups, translate},
    ui_kit,
};

#[derive(Debug, Clone, Default)]
struct Memory {
    recording: bool,
    super_held: bool,
    error: Option<String>,
    /// An incomplete edit (modifiers without a key) that is not stored yet.
    draft: Option<HotkeyDraft>,
}

/// Draws the editor for `value` (the stored text, or `None`). Returns `true` when it changed.
///
/// `label` names the widget for screen readers ("Hotkey of Capture region").
pub fn hotkey_field(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    label: &str,
    value: &mut Option<String>,
) -> bool {
    let id = ui.make_persistent_id(id_salt);
    let mut mem: Memory = ui.data(|d| d.get_temp(id)).unwrap_or_default();
    let mut changed = false;

    // What to show: an unfinished draft, else the stored value.
    let stored = value.as_deref().map(HotkeyDraft::from_setting);
    let mut draft = match (&mem.draft, &stored) {
        (Some(d), _) => *d,
        (None, Some(Ok(d))) => *d,
        _ => HotkeyDraft::default(),
    };
    let stored_unreadable = matches!(stored, Some(Err(_))) && mem.draft.is_none();

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ssx_editor_ui::ui::widgets::input_style(ui);
        // The current shortcut.
        let text = match (value.as_deref(), mem.draft.as_ref()) {
            (_, Some(d)) => d.display(),
            (Some(v), None) if stored_unreadable => v.to_owned(),
            (Some(_), None) => draft.display(),
            (None, None) => String::new(),
        };
        egui::Frame::new()
            .fill(theme::CANVAS_BG)
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(70, 73, 82)))
            .corner_radius(egui::CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(9, 3))
            .show(ui, |ui| {
                ui.set_min_width(136.0);
                let r = if text.is_empty() {
                    ui.label(RichText::new("Not set").color(ui_kit::DIM_TEXT))
                } else {
                    ui.label(RichText::new(&text).monospace().color(Color32::WHITE))
                };
                r.widget_info(|| {
                    WidgetInfo::labeled(
                        WidgetType::Label,
                        true,
                        format!("{label}: {}", if text.is_empty() { "not set" } else { &text }),
                    )
                });
            });

        // Modifiers.
        for (m, name) in [
            (Modifiers::CTRL, "Ctrl"),
            (Modifiers::ALT, "Alt"),
            (Modifiers::SHIFT, "Shift"),
            (Modifiers::SUPER, "Super"),
        ] {
            let on = draft.mods.contains(m);
            let tip = format!("{label}: {name} key");
            let r = ui_kit::chip_compact(ui, name, on);
            r.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, on, tip.clone()));
            if r.clicked() {
                draft.toggle(m);
                commit(&draft, value, &mut mem, &mut changed);
            }
        }

        // Key list.
        let key_label = draft.key.map_or_else(|| "Key".to_owned(), display_key);
        let menu = ui.menu_button(key_label.clone(), |ui| {
            ui.set_max_width(400.0);
            for (group, keys) in key_groups() {
                ui.label(RichText::new(group).strong().size(12.0));
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                    for k in keys {
                        let name = display_key(k);
                        if ui.selectable_label(draft.key == Some(k), name).clicked() {
                            draft.key = Some(k);
                            commit(&draft, value, &mut mem, &mut changed);
                            ui.close();
                        }
                    }
                });
                ui.add_space(4.0);
            }
        });
        menu.response.widget_info(|| {
            WidgetInfo::labeled(
                WidgetType::Button,
                true,
                format!("{label}: choose the key from a list"),
            )
        });

        // Record.
        let rec_text =
            if mem.recording { "Press the shortcut...  (Esc cancels)" } else { "Record" };
        let rec = ui_kit::chip_compact(ui, rec_text, mem.recording);
        rec.widget_info(|| {
            WidgetInfo::selected(
                WidgetType::Button,
                true,
                mem.recording,
                format!("{label}: record a shortcut by pressing it"),
            )
        });
        if rec.clicked() {
            mem.recording = !mem.recording;
            mem.error = None;
            if mem.recording {
                rec.request_focus();
                mem.super_held = false;
            }
        }
        if mem.recording {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    rec.id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
            if rec.lost_focus() && !rec.clicked() {
                mem.recording = false;
            }
            let events = ui.input(|i| i.events.clone());
            for ev in events {
                let egui::Event::Key { key, physical_key, pressed, repeat, modifiers } = ev else {
                    continue;
                };
                let k = physical_key.unwrap_or(key);
                if is_super_key(k) {
                    mem.super_held = pressed;
                }
                if !pressed || repeat {
                    continue;
                }
                match translate(k, modifiers, mem.super_held) {
                    Translated::Chord(c) => {
                        draft = HotkeyDraft::from_chord(&c);
                        commit(&draft, value, &mut mem, &mut changed);
                        mem.recording = false;
                        ui.memory_mut(|m| m.surrender_focus(rec.id));
                    }
                    Translated::ModifiersHeld(m) => {
                        mem.draft = Some(HotkeyDraft { mods: m, key: None });
                        mem.error = None;
                    }
                    Translated::Cancel => {
                        mem.recording = false;
                        mem.draft = None;
                        ui.memory_mut(|m| m.surrender_focus(rec.id));
                    }
                    Translated::Clear => {
                        *value = None;
                        changed = true;
                        mem.recording = false;
                        mem.draft = None;
                        mem.error = None;
                        ui.memory_mut(|m| m.surrender_focus(rec.id));
                    }
                    Translated::Rejected(why) => mem.error = Some(why),
                }
            }
            ui.ctx().request_repaint();
        }

        // Clear.
        if value.is_some() || mem.draft.is_some() {
            let r = ui_kit::mini_icon_button(
                ui,
                ssx_editor_ui::icons::Icon::Close,
                &format!("{label}: remove the shortcut"),
                true,
            );
            if r.clicked() {
                *value = None;
                changed = true;
                mem.draft = None;
                mem.error = None;
                mem.recording = false;
            }
        }
    });
    if mem.recording {
        ui_kit::hint(
            ui,
            "PrintScreen, Pause and the Windows key cannot be recorded on every system: choose the key from the Key list and switch modifiers on with the buttons.",
        );
    }
    if let Some(e) = &mem.error {
        ui.horizontal_top(|ui| {
            ui_kit::severity_icon(ui, ssx_core::settings::Severity::Error);
            ui.add(egui::Label::new(RichText::new(e).size(12.0).color(ui_kit::ERROR_TEXT)).wrap());
        });
    }
    ui.data_mut(|d| d.insert_temp(id, mem));
    changed
}

/// Stores `draft` if it is a complete, storable shortcut; otherwise keeps it as an unfinished
/// draft and remembers why.
fn commit(draft: &HotkeyDraft, value: &mut Option<String>, mem: &mut Memory, changed: &mut bool) {
    match draft.to_setting() {
        Ok(s) => {
            if value.as_deref() != Some(s.as_str()) {
                *value = Some(s);
                *changed = true;
            }
            mem.draft = None;
            mem.error = None;
        }
        Err(why) => {
            mem.draft = Some(*draft);
            // "choose a key" is a prompt, not an error.
            mem.error = (draft.key.is_some()).then_some(why);
        }
    }
}

/// The id used by the widget's persistent memory (tests reset it).
pub fn memory_id(ui: &Ui, id_salt: impl std::hash::Hash + std::fmt::Debug) -> Id {
    ui.make_persistent_id(id_salt)
}
