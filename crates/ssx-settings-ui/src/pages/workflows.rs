//! Workflows: create, duplicate, delete and reorder workflows, and edit what each one does.

use egui::{Color32, RichText, Ui};
use ssx_core::settings::{
    AfterCapture, AfterUpload, Settings, Severity, Workflow, builtin_workflows,
};
use ssx_editor_ui::{
    icons::Icon,
    ui::{theme, widgets::input_style},
};

use super::{Cx, shared::destination_picker};
use crate::{
    hotkey_plan::{Owner, others_using},
    hotkey_widget::hotkey_field,
    reorder_ui,
    ui_kit::{self, Answer, Field},
    validation::{Issues, workflow_path},
    workflow_edit::{
        CAPTURE_STEPS, DESTINATION_ROWS, INPUT_KINDS, RUN_COMMAND_PLACEHOLDERS, addable_capture_steps,
        addable_upload_steps, capture_step_blurb, capture_step_label, delete, duplicate, input_blurb,
        input_label, instantiate, set_destination, set_input, templates, upload_step_blurb,
        upload_step_label,
    },
};

/// A destructive action waiting for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Delete the workflow at this index.
    Delete(usize),
    /// Replace every workflow with the built-in ones.
    Reset,
}

/// State of the Workflows page.
#[derive(Debug, Default)]
pub struct State {
    /// The selected workflow.
    pub selected: usize,
    /// A confirmation dialog.
    pub confirm: Option<Confirm>,
    /// The id field is unlocked for editing.
    pub id_unlocked: bool,
}

impl State {
    /// Keeps the selection inside the list.
    pub fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    /// Selects the workflow at `index` (and locks the id field again).
    pub fn select(&mut self, index: usize) {
        if self.selected != index {
            self.selected = index;
            self.id_unlocked = false;
        }
    }

    /// Follows a list item that moved from `from` to `to` so the selection stays on it.
    pub fn follow_move(&mut self, from: usize, to: usize) {
        self.selected = follow(self.selected, from, to);
    }
}

/// Where the item at `sel` ends up when the item at `from` is moved to position `to`.
pub fn follow(sel: usize, from: usize, to: usize) -> usize {
    if sel == from {
        to
    } else if from < to && sel > from && sel <= to {
        sel - 1
    } else if from > to && sel >= to && sel < from {
        sel + 1
    } else {
        sel
    }
}

/// Replaces the workflows with the built-in ones (the "Reset to defaults" button).
pub fn reset_workflows(settings: &mut Settings) {
    settings.workflows = builtin_workflows();
}

/// The one-line summary under a workflow's name in the list.
pub fn subtitle(w: &Workflow) -> String {
    match w.trigger.hotkey.as_deref() {
        Some(h) if !h.is_empty() => format!("{}  \u{b7}  {h}", input_label(w.input)),
        _ => input_label(w.input).to_owned(),
    }
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    st.clamp(cx.settings.workflows.len());
    let ctx = ui.ctx().clone();
    ui_kit::page_scroll(ui, "workflows", |ui| {
        let total = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            ui.vertical(|ui| {
                ui.set_width(300.0);
                list_panel(ui, st, cx);
            });
            ui.vertical(|ui| {
                ui.set_width((total - 314.0).max(300.0));
                if cx.settings.workflows.is_empty() {
                    ui_kit::card(ui, None, |ui| {
                        ui.label("There are no workflows. Create one with New workflow, or reset to the defaults.");
                    });
                } else {
                    detail_panel(ui, st, cx);
                }
            });
        });
    });
    dialogs(&ctx, st, cx);
}

fn list_panel(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        input_style(ui);
        let menu = ui.menu_button("New workflow", |ui| {
            ui.set_min_width(300.0);
            ui.label(RichText::new("Start from").size(12.0).color(theme::TEXT_DIM));
            for t in templates() {
                if ui.button(&t.label).clicked() {
                    let w = instantiate(&t.workflow, &cx.settings.workflows);
                    cx.settings.workflows.push(w);
                    st.select(cx.settings.workflows.len() - 1);
                    ui.close();
                }
            }
        });
        menu.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "New workflow"));
        let has = !cx.settings.workflows.is_empty();
        if ui_kit::button_if(ui, "Duplicate", has, "No workflow selected").clicked()
            && let Some(i) = duplicate(&mut cx.settings.workflows, st.selected)
        {
            st.select(i);
        }
        if ui_kit::button_if(ui, "Delete", has, "No workflow selected").clicked() {
            st.confirm = Some(Confirm::Delete(st.selected));
        }
    });
    ui.add_space(8.0);
    let names: Vec<String> = cx.settings.workflows.iter().map(|w| w.name.clone()).collect();
    ui_kit::card(ui, None, |ui| {
        if names.is_empty() {
            ui_kit::hint(ui, "No workflows yet.");
        }
        let n = names.len();
        let moved = reorder_ui::list(
            ui,
            "workflow-list",
            n,
            |i| names[i].clone(),
            |ui, i| {
                let w = &cx.settings.workflows[i];
                ui.set_min_width(240.0);
                let title = if w.name.trim().is_empty() { "(unnamed)".to_owned() } else { w.name.clone() };
                let marker = cx.issues.worst_at(&workflow_path(i));
                let r = ui_kit::list_item(ui, st.selected == i, &title, &subtitle(w), marker, &format!("Workflow {title}"));
                if r.clicked() {
                    st.select(i);
                }
                r.on_hover_text(&title);
            },
        );
        if let Some(m) = moved {
            let sel_before = st.selected;
            if let Some(pos) = reorder_ui::apply(ui, "workflow-list", &mut cx.settings.workflows, m) {
                st.selected = follow(sel_before, m.mv.from, pos);
            }
        }
    });
    ui_kit::hint(ui, "Drag the dots to reorder, or focus them and press Alt+Up / Alt+Down. The order only matters for menus and the list here.");
    ui.add_space(10.0);
    if ui_kit::button(ui, "Reset to defaults...").on_hover_text("Replace all workflows with the ones ssx ships").clicked() {
        st.confirm = Some(Confirm::Reset);
    }
}

fn detail_panel(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let i = st.selected;
    let p = workflow_path(i);
    let registry = cx.registry.clone();
    let issues: Issues = cx.issues.clone();
    let others = {
        let w = &cx.settings.workflows[i];
        others_using(cx.settings, &Owner::Workflow { index: i, name: w.name.clone() })
    };
    let (defaults, workflow) = {
        let s = &mut *cx.settings;
        (s.destinations.clone(), &mut s.workflows[i])
    };

    ui_kit::card(ui, Some("Workflow"), |ui| {
        Field::new("Name").issues(&issues, &format!("{p}.name")).show(ui, |ui| {
            ui_kit::text_input(ui, "Workflow name", &mut workflow.name, "", 320.0);
        });
        ui.add_space(3.0);
        Field::new("Id")
            .issues(&issues, &format!("{p}.id"))
            .help("Identifies the workflow in the history and in scripts. Changing it can break anything that refers to the old one.")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if st.id_unlocked {
                        ui_kit::text_input(ui, "Workflow id", &mut workflow.id, "", 260.0);
                    } else {
                        ui.label(RichText::new(&workflow.id).monospace());
                        if ui_kit::link(ui, "Change").clicked() {
                            st.id_unlocked = true;
                        }
                    }
                });
            });
        ui.add_space(3.0);
        Field::new("Command name")
            .issues(&issues, &format!("{p}.trigger.cli_name"))
            .help("Run it from a terminal or a launcher with `ssx run <name>`. Lower-case letters, digits and dashes.")
            .show(ui, |ui| {
                let mut c = workflow.trigger.cli_name.clone().unwrap_or_default();
                if ui_kit::text_input(ui, "Command name", &mut c, "for example region", 220.0).changed() {
                    workflow.trigger.cli_name = if c.is_empty() { None } else { Some(c) };
                }
            });
        ui.add_space(3.0);
        Field::new("Hotkey").issues(&issues, &format!("{p}.trigger.hotkey")).show(ui, |ui| {
            hotkey_field(ui, ("wf-hotkey", i), &format!("Hotkey of {}", workflow.name), &mut workflow.trigger.hotkey);
            if !others.is_empty() {
                let names: Vec<String> = others.iter().map(Owner::label).collect();
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, Severity::Error);
                    ui.add(egui::Label::new(RichText::new(format!("Also used by {}. A hotkey can start only one thing.", names.join(", "))).size(12.0).color(ui_kit::ERROR_TEXT)).wrap());
                });
            }
        });
        ui.add_space(3.0);
        Field::new("Starts with").help(input_blurb(workflow.input)).show(ui, |ui| {
            input_style(ui);
            let mut chosen = None;
            let combo = egui::ComboBox::from_id_salt(("wf-input", i)).width(300.0).selected_text(input_label(workflow.input)).show_ui(ui, |ui| {
                for (kind, label, blurb) in INPUT_KINDS {
                    if ui.selectable_label(workflow.input == kind, label).on_hover_text(blurb).clicked() {
                        chosen = Some(kind);
                    }
                }
            });
            combo.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Input"));
            if let Some(k) = chosen {
                let dropped = set_input(workflow, k);
                if !dropped.is_empty() {
                    let names: Vec<&str> = dropped.iter().map(|s| capture_step_label(*s)).collect();
                    let t = ui.input(|inp| inp.time);
                    cx.toasts.info(t, format!("Removed {} (they only work on images)", names.join(", ")));
                }
            }
        });
    });

    capture_steps_card(ui, i, &p, workflow, &issues);
    upload_steps_card(ui, i, &p, workflow, &issues);

    ui_kit::card(ui, Some("Where it uploads"), |ui| {
        ui_kit::hint(ui, "Each kind of content goes to the destination set on the Uploaders page. Pick one here to send this workflow's content somewhere else.");
        ui.add_space(6.0);
        for (ty, label, help) in DESTINATION_ROWS {
            let mut cur = workflow.destination.get(ty).map(str::to_owned);
            let global = defaults.default_for(ty).map(str::to_owned);
            let none_label = match (ty, &global) {
                (ssx_core::settings::DestinationType::Video, None) => match defaults.file.as_deref() {
                    Some(f) => format!("Same as the file destination ({f})"),
                    None => "Same as the file destination (not set)".to_owned(),
                },
                (_, Some(g)) => format!("Default ({g})"),
                (_, None) => "Default (not set)".to_owned(),
            };
            Field::new(label).help(help).issues(&issues, &format!("{p}.destination.{}", dest_key(ty))).show(ui, |ui| {
                if destination_picker(ui, ("wf-dest", i, label), &format!("{label} destination"), &mut cur, ty, &registry, &none_label) {
                    set_destination(workflow, ty, cur.clone());
                }
            });
            ui.add_space(2.0);
        }
    });
}

fn dest_key(ty: ssx_core::settings::DestinationType) -> &'static str {
    use ssx_core::settings::DestinationType as D;
    match ty {
        D::Image => "image",
        D::Text => "text",
        D::File => "file",
        D::Video => "video",
        D::UrlShortener => "url_shortener",
        D::UrlSharing => "url_sharing",
    }
}

fn capture_steps_card(ui: &mut Ui, i: usize, p: &str, w: &mut Workflow, issues: &Issues) {
    ui_kit::card(ui, Some("After capture"), |ui| {
        ui_kit::hint(ui, "These run in this order, top to bottom. Drag a step by its dots to change the order.");
        ui.add_space(4.0);
        let labels: Vec<String> = w.after_capture.iter().map(|s| capture_step_label(*s).to_owned()).collect();
        let mut remove = None;
        let mut nudge = None;
        let n = labels.len();
        let steps = w.after_capture.clone();
        let moved = reorder_ui::list(
            ui,
            ("ac", i),
            n,
            |k| labels[k].clone(),
            |ui, k| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(capture_step_label(steps[k])).strong().color(Color32::WHITE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui_kit::mini_icon_button(ui, Icon::Close, &format!("Remove {}", labels[k]), true).clicked() {
                            remove = Some(k);
                        }
                        if ui_kit::mini_icon_button(ui, Icon::ChevronDown, &format!("Move {} down", labels[k]), k + 1 < n).clicked() {
                            nudge = Some(crate::reorder::Move::step(k, 1));
                        }
                        if ui_kit::mini_icon_button(ui, Icon::ChevronUp, &format!("Move {} up", labels[k]), k > 0).clicked() {
                            nudge = Some(crate::reorder::Move::step(k, -1));
                        }
                    });
                });
                ui_kit::hint(ui, capture_step_blurb(steps[k]));
                ui_kit::issue_lines(ui, issues, &format!("{p}.after_capture[{k}]"));
            },
        );
        if let Some(m) = moved {
            reorder_ui::apply(ui, ("ac", i), &mut w.after_capture, m);
        }
        if let Some(mv) = nudge {
            mv.apply(&mut w.after_capture);
        }
        if let Some(k) = remove {
            w.after_capture.remove(k);
        }
        ui_kit::issue_lines_exact(ui, issues, &format!("{p}.after_capture"));
        if w.after_capture.is_empty() {
            ui_kit::hint(ui, "No steps: the workflow does nothing with what it captures.");
        }
        ui.add_space(6.0);
        let addable = addable_capture_steps(w);
        ui.horizontal(|ui| {
            input_style(ui);
            let menu = ui.add_enabled_ui(!addable.is_empty(), |ui| {
                ui.menu_button("Add step", |ui| {
                    ui.set_min_width(260.0);
                    for s in &addable {
                        if ui.button(capture_step_label(*s)).on_hover_text(capture_step_blurb(*s)).clicked() {
                            w.after_capture.push(*s);
                            ui.close();
                        }
                    }
                })
            });
            menu.inner.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Add after-capture step"));
        });
    });
    let _ = CAPTURE_STEPS;
    let _ = AfterCapture::Upload;
}

fn upload_steps_card(ui: &mut Ui, i: usize, p: &str, w: &mut Workflow, issues: &Issues) {
    ui_kit::card(ui, Some("After upload"), |ui| {
        ui_kit::hint(ui, "These run once the upload succeeded and there is a URL.");
        ui.add_space(4.0);
        let labels: Vec<String> = w.after_upload.iter().map(|s| upload_step_label(s).to_owned()).collect();
        let n = labels.len();
        let mut remove = None;
        let mut nudge = None;
        let steps = &mut w.after_upload;
        let moved = reorder_ui::list(
            ui,
            ("au", i),
            n,
            |k| labels[k].clone(),
            |ui, k| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(upload_step_label(&steps[k])).strong().color(Color32::WHITE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui_kit::mini_icon_button(ui, Icon::Close, &format!("Remove {}", labels[k]), true).clicked() {
                            remove = Some(k);
                        }
                        if ui_kit::mini_icon_button(ui, Icon::ChevronDown, &format!("Move {} down", labels[k]), k + 1 < n).clicked() {
                            nudge = Some(crate::reorder::Move::step(k, 1));
                        }
                        if ui_kit::mini_icon_button(ui, Icon::ChevronUp, &format!("Move {} up", labels[k]), k > 0).clicked() {
                            nudge = Some(crate::reorder::Move::step(k, -1));
                        }
                    });
                });
                ui_kit::hint(ui, upload_step_blurb(&steps[k]));
                if let AfterUpload::RunCommand { program, args } = &mut steps[k] {
                    run_command_editor(ui, (i, k), program, args, issues, &format!("{p}.after_upload[{k}]"));
                }
                ui_kit::issue_lines(ui, issues, &format!("{p}.after_upload[{k}]"));
            },
        );
        if let Some(m) = moved {
            reorder_ui::apply(ui, ("au", i), &mut w.after_upload, m);
        }
        if let Some(mv) = nudge {
            mv.apply(&mut w.after_upload);
        }
        if let Some(k) = remove {
            w.after_upload.remove(k);
        }
        if w.after_upload.is_empty() {
            ui_kit::hint(ui, "No steps.");
        }
        ui.add_space(6.0);
        let addable = addable_upload_steps(w);
        ui.horizontal(|ui| {
            input_style(ui);
            let menu = ui.add_enabled_ui(!addable.is_empty(), |ui| {
                ui.menu_button("Add step", |ui| {
                    ui.set_min_width(260.0);
                    for s in addable {
                        if ui.button(upload_step_label(&s)).on_hover_text(upload_step_blurb(&s)).clicked() {
                            w.after_upload.push(s);
                            ui.close();
                        }
                    }
                })
            });
            menu.inner.response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Add after-upload step"));
        });
    });
}

fn run_command_editor(
    ui: &mut Ui,
    id: (usize, usize),
    program: &mut String,
    args: &mut Vec<String>,
    _issues: &Issues,
    _path: &str,
) {
    ui.add_space(2.0);
    egui::Frame::new()
        .fill(theme::CANVAS_BG)
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            Field::new("Program").show(ui, |ui| {
                ui_kit::text_input(ui, "Program to run", program, "/usr/bin/notify-send", 300.0);
            });
            let mut remove = None;
            for (k, a) in args.iter_mut().enumerate() {
                Field::new(&format!("Argument {}", k + 1)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui_kit::text_input(ui, &format!("Argument {}", k + 1), a, "", 300.0);
                        if ui_kit::mini_icon_button(ui, Icon::Close, &format!("Remove argument {}", k + 1), true).clicked() {
                            remove = Some(k);
                        }
                    });
                });
            }
            if let Some(k) = remove {
                args.remove(k);
            }
            ui.horizontal(|ui| {
                if ui_kit::button(ui, "Add argument").clicked() {
                    args.push(String::new());
                }
            });
            let ph: Vec<String> = RUN_COMMAND_PLACEHOLDERS.iter().map(|(p, d)| format!("{p} {d}")).collect();
            ui_kit::hint(ui, &format!("Each argument is passed as it is; no shell runs. Placeholders: {}.", ph.join(", ")));
            let _ = id;
        });
}

fn dialogs(ctx: &egui::Context, st: &mut State, cx: &mut Cx<'_>) {
    match st.confirm.clone() {
        Some(Confirm::Delete(i)) => {
            let name = cx.settings.workflows.get(i).map(|w| w.name.clone()).unwrap_or_default();
            match ui_kit::confirm(
                ctx,
                "wf-delete",
                "Delete this workflow?",
                &format!("\"{name}\" will be removed from the settings when you save. Its history entries stay."),
                "Delete",
                true,
            ) {
                Some(Answer::Confirm) => {
                    if let Some(sel) = delete(&mut cx.settings.workflows, i) {
                        st.selected = sel;
                    }
                    st.confirm = None;
                }
                Some(Answer::Cancel) => st.confirm = None,
                None => {}
            }
        }
        Some(Confirm::Reset) => {
            match ui_kit::confirm(
                ctx,
                "wf-reset",
                "Reset workflows to the defaults?",
                "Every workflow you created or changed is replaced by the workflows ssx ships. Other settings are not touched. Nothing is written until you save, and Revert brings your workflows back.",
                "Reset workflows",
                true,
            ) {
                Some(Answer::Confirm) => {
                    reset_workflows(cx.settings);
                    st.selected = 0;
                    st.confirm = None;
                }
                Some(Answer::Cancel) => st.confirm = None,
                None => {}
            }
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selection_follows_a_moved_item() {
        // list [a b c d e]; move c (2) to position 0 -> [c a b d e]
        assert_eq!(follow(2, 2, 0), 0, "the moved item");
        assert_eq!(follow(0, 2, 0), 1);
        assert_eq!(follow(1, 2, 0), 2);
        assert_eq!(follow(3, 2, 0), 3);
        // move a (0) to position 3 -> [b c d a e]
        assert_eq!(follow(0, 0, 3), 3);
        assert_eq!(follow(1, 0, 3), 0);
        assert_eq!(follow(3, 0, 3), 2);
        assert_eq!(follow(4, 0, 3), 4);
    }

    #[test]
    fn the_selection_follows_for_every_move_of_every_list_length() {
        for n in 1..8 {
            for from in 0..n {
                for to in 0..n {
                    for sel in 0..n {
                        let mut v: Vec<usize> = (0..n).collect();
                        if crate::reorder::move_to(&mut v, from, to).is_some() {
                            assert_eq!(v[follow(sel, from, to)], sel, "n={n} from={from} to={to} sel={sel}");
                        } else {
                            assert_eq!(follow(sel, from, to.min(n - 1)), sel);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn clamp_and_select() {
        let mut st = State { selected: 9, id_unlocked: true, ..State::default() };
        st.clamp(3);
        assert_eq!(st.selected, 2);
        st.clamp(0);
        assert_eq!(st.selected, 0);
        st.id_unlocked = true;
        st.select(0);
        assert!(st.id_unlocked, "selecting the same one changes nothing");
        st.select(1);
        assert!(!st.id_unlocked);
        assert_eq!(st.selected, 1);
    }

    #[test]
    fn reset_restores_the_builtins() {
        let mut s = Settings::default();
        s.workflows.clear();
        reset_workflows(&mut s);
        assert_eq!(s.workflows, builtin_workflows());
    }

    #[test]
    fn subtitles_show_the_input_and_the_hotkey() {
        let w = &builtin_workflows()[0];
        assert_eq!(subtitle(w), "Capture region  \u{b7}  Ctrl+PrintScreen");
        assert_eq!(subtitle(&builtin_workflows()[4]), "Capture monitor");
    }
}
