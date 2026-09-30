//! General: save folder, file names with a live preview, image format, autostart, history
//! retention.

use std::path::{Path, PathBuf};

use egui::{Color32, RichText, Ui};
use ssx_core::settings::{General, ImageFormatKind, Severity};
use ssx_editor_ui::ui::theme;

use super::Cx;
use crate::{
    autostart::AutostartError,
    pattern_info::{
        CHEAT_SHEET, EXAMPLE_FILE_PATTERNS, PatternKind, PatternReport, SampleInputs, TokenDoc,
        analyze, names_in, preview_paths, unsupported_tokens,
    },
    task::Slot,
    ui_kit::{self, Field},
};

/// Which pattern field a token chip inserts into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatternTarget {
    /// The file name pattern.
    #[default]
    File,
    /// The folder pattern.
    Folder,
}

/// State of the General page.
#[derive(Debug, Default)]
pub struct State {
    /// The pattern field that was focused last (where clicked tokens go).
    pub target: PatternTarget,
    /// The token cheat sheet is open.
    pub cheat_sheet_open: bool,
    /// Cached autostart state (`None` until read).
    pub autostart: Option<bool>,
    /// The last autostart error.
    pub autostart_error: Option<String>,
    /// A folder dialog is open.
    pub folder_dialog: Slot<Option<PathBuf>>,
    /// The next counter value to preview.
    pub sample: SampleInputs,
}

/// A note about a folder the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathNote {
    /// How serious.
    pub severity: Severity,
    /// What to tell the user.
    pub text: String,
}

/// Looks at a save folder and says what will happen with it, or `None` if it is fine.
pub fn save_dir_note(dir: &Path) -> Option<PathNote> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {
            if m.permissions().readonly() {
                Some(PathNote { severity: Severity::Warning, text: "this folder is read-only; saving will fail".to_owned() })
            } else {
                None
            }
        }
        Ok(_) => Some(PathNote {
            severity: Severity::Error,
            text: "this path is a file, not a folder; choose a folder".to_owned(),
        }),
        Err(_) => {
            // Walk up to the nearest existing ancestor: creation only works below a folder.
            let mut anc = dir.parent();
            while let Some(a) = anc {
                if a.as_os_str().is_empty() {
                    break;
                }
                if let Ok(m) = std::fs::metadata(a) {
                    return Some(if m.is_dir() {
                        PathNote {
                            severity: Severity::Warning,
                            text: "this folder does not exist yet; ssx creates it when it saves the first file"
                                .to_owned(),
                        }
                    } else {
                        PathNote {
                            severity: Severity::Error,
                            text: format!("{} is a file, so this folder cannot be created", a.display()),
                        }
                    });
                }
                anc = a.parent();
            }
            Some(PathNote {
                severity: Severity::Warning,
                text: "this folder does not exist yet; ssx creates it when it saves the first file".to_owned(),
            })
        }
    }
}

/// The text a token chip inserts: parameters get a sensible value (`%i{n}` becomes `%i{3}`).
pub fn insertable(spelling: &str) -> String {
    spelling
        .replace("{n}", "{3}")
        .replace("{base,len}", "{62,4}")
        .replace("{path}", "{names.txt}")
}

/// The individual spellings in a cheat-sheet row (`"%y  %yy"` gives `%y`, `%yy`).
pub fn spellings(doc: &TokenDoc) -> Vec<&'static str> {
    doc.token.split_whitespace().collect()
}

/// Appends `token` to `pattern`.
pub fn append_token(pattern: &mut String, token: &str) {
    pattern.push_str(&insertable(token));
}

impl State {
    /// Reads the autostart state (call when the page opens and after a change).
    pub fn refresh(&mut self, cx: &Cx<'_>) {
        match cx.host.autostart.is_enabled() {
            Ok(v) => {
                self.autostart = Some(v);
                self.autostart_error = None;
            }
            Err(e) => {
                self.autostart = None;
                self.autostart_error = Some(e.to_string());
            }
        }
    }

    /// Turns autostart on or off and records the outcome.
    pub fn set_autostart(&mut self, cx: &Cx<'_>, on: bool) -> Result<(), AutostartError> {
        let r = cx.host.autostart.set_enabled(on);
        match &r {
            Ok(()) => {
                self.autostart = Some(on);
                self.autostart_error = None;
            }
            Err(e) => self.autostart_error = Some(e.to_string()),
        }
        r
    }
}

fn number(ui: &mut Ui, label: &str, v: &mut u32, range: std::ops::RangeInclusive<u32>, suffix: &str) -> egui::Response {
    ssx_editor_ui::ui::widgets::input_style(ui);
    let r = ui.add(egui::DragValue::new(v).range(range).clamp_existing_to_range(false).suffix(suffix.to_owned()).speed(1.0));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::DragValue, true, label));
    r
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    // A folder chosen in the dialog.
    if let Some(Some(dir)) = st.folder_dialog.poll() {
        cx.settings.general.save_dir = Some(dir);
    }
    ui_kit::page_scroll(ui, "general", |ui| {
        save_location(ui, st, cx);
        file_names(ui, st, cx);
        images(ui, cx);
        startup(ui, st, cx);
        history(ui, cx);
    });
}

fn save_location(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Save location"), |ui| {
        let default_dir = General::default().resolve_save_dir();
        let mut text = cx.settings.general.save_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        Field::new("Save folder")
            .issues(cx.issues, "general.save_dir")
            .help(&format!("Empty means the default, {}.", default_dir.display()))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui_kit::text_input(ui, "Save folder", &mut text, &default_dir.display().to_string(), 360.0).changed() {
                        cx.settings.general.save_dir =
                            if text.is_empty() { None } else { Some(PathBuf::from(&text)) };
                    }
                    let busy = st.folder_dialog.running();
                    if ui_kit::button_if(ui, "Browse...", !busy, "A dialog is already open").clicked() {
                        let dialogs = cx.host.dialogs.clone();
                        let start = cx.settings.general.save_dir.clone();
                        st.folder_dialog.start(cx.wake, move || dialogs.pick_folder(start.as_deref()));
                    }
                    if cx.settings.general.save_dir.is_some()
                        && ui_kit::button(ui, "Use default").on_hover_text("Go back to the Pictures/ssx folder").clicked()
                    {
                        cx.settings.general.save_dir = None;
                    }
                });
                if let Some(dir) = cx.settings.general.save_dir.as_deref().filter(|d| !d.as_os_str().is_empty())
                    && let Some(n) = save_dir_note(dir)
                {
                    ui.horizontal_top(|ui| {
                        ui_kit::severity_icon(ui, n.severity);
                        ui.add(egui::Label::new(RichText::new(n.text).size(12.0).color(ui_kit::severity_color(n.severity))).wrap());
                    });
                }
            });
        ui.add_space(6.0);
        Field::new("Sub-folders by type").help("Screenshots, recordings, text and other files each get their own folder inside the save folder.")
            .show(ui, |ui| {
                ui_kit::switch(ui, "Use a folder per type", &mut cx.settings.general.use_type_subfolders);
            });
        if cx.settings.general.use_type_subfolders {
            ui.add_space(4.0);
            let g = &mut cx.settings.general.subfolders;
            for (label, path, value) in [
                ("Images", "general.subfolders.image", &mut g.image),
                ("Videos", "general.subfolders.video", &mut g.video),
                ("Text", "general.subfolders.text", &mut g.text),
                ("Other files", "general.subfolders.file", &mut g.file),
            ] {
                Field::new(label).issues(cx.issues, path).show(ui, |ui| {
                    ui_kit::text_input(ui, label, value, "folder name", 220.0);
                });
            }
        }
    });
}

fn pattern_row(
    ui: &mut Ui,
    st: &mut State,
    cx: &Cx<'_>,
    target: PatternTarget,
    label: &str,
    path: &str,
    value: &mut String,
    kind: PatternKind,
) -> PatternReport {
    let report = analyze(value, kind);
    Field::new(label).issues(cx.issues, path).show(ui, |ui| {
        let r = ui_kit::text_input(ui, label, value, "", 420.0);
        if r.has_focus() || r.gained_focus() {
            st.target = target;
        }
        // The core validator already reports unknown / unsupported tokens; only what it does
        // not check (illegal characters) is added here, to avoid saying things twice.
        if !report.illegal_chars.is_empty() {
            for m in report.messages(kind).into_iter().skip(report.unknown_tokens.len() + report.unsupported.len()) {
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, Severity::Warning);
                    ui.add(egui::Label::new(RichText::new(m).size(12.0).color(ui_kit::WARN_TEXT)).wrap());
                });
            }
        }
    });
    report
}

fn file_names(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("File names"), |ui| {
        let mut folder = cx.settings.general.folder_pattern.clone();
        let folder_report = pattern_row(ui, st, cx, PatternTarget::Folder, "Folder pattern", "general.folder_pattern", &mut folder, PatternKind::Folder);
        cx.settings.general.folder_pattern = folder;
        ui.add_space(4.0);
        let mut name = cx.settings.general.file_name_pattern.clone();
        let name_report = pattern_row(ui, st, cx, PatternTarget::File, "File name pattern", "general.file_name_pattern", &mut name, PatternKind::FileName);
        cx.settings.general.file_name_pattern = name;

        ui.add_space(2.0);
        Field::new("Examples").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (label, pattern) in EXAMPLE_FILE_PATTERNS {
                    if ui_kit::chip(ui, label, cx.settings.general.file_name_pattern == *pattern).on_hover_text(*pattern).clicked() {
                        cx.settings.general.file_name_pattern = (*pattern).to_owned();
                    }
                }
            });
        });
        ui.add_space(4.0);
        Field::new("Longest name").issues(cx.issues, "general.max_file_name_len")
            .help("Characters of the file name before the extension; 0 only applies the operating system's limit.")
            .show(ui, |ui| {
                let mut n = cx.settings.general.max_file_name_len as u32;
                if number(ui, "Longest name", &mut n, 0..=255, " characters").changed() {
                    cx.settings.general.max_file_name_len = n as usize;
                }
            });
        Field::new("Longest window title").issues(cx.issues, "general.max_title_len")
            .help("How much of the window title %t may use; 0 is unlimited.")
            .show(ui, |ui| {
                let mut n = cx.settings.general.max_title_len as u32;
                if number(ui, "Longest window title", &mut n, 0..=255, " characters").changed() {
                    cx.settings.general.max_title_len = n as usize;
                }
            });

        // Live preview.
        st.sample.next_counter = ssx_core::pattern::FileCounter::new(cx.host.paths.counter_file())
            .peek()
            .map_or(1, |n| n + 1);
        let p = preview_paths(&cx.settings.general, &*cx.host.clock, &*cx.host.env, &st.sample);
        ui.add_space(6.0);
        egui::Frame::new()
            .fill(theme::CANVAS_BG)
            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(52, 54, 60)))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Preview: where the next screenshot goes").size(11.5).color(theme::TEXT_DIM));
                ui.add_space(2.0);
                let dir = p.full.parent().map(|d| d.display().to_string()).unwrap_or_default();
                let sep = std::path::MAIN_SEPARATOR;
                let job = {
                    let mut j = egui::text::LayoutJob::default();
                    let mono = egui::FontId::monospace(13.0);
                    j.append(&format!("{dir}{sep}"), 0.0, egui::TextFormat::simple(mono.clone(), theme::TEXT_DIM));
                    j.append(&p.file_name, 0.0, egui::TextFormat::simple(mono, Color32::WHITE));
                    j.wrap.max_width = ui.available_width();
                    j
                };
                let r = ui.add(egui::Label::new(job).wrap().selectable(true));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("Preview: {}", p.full.display())));
                if let Some(e) = &p.error {
                    ui.label(RichText::new(e).size(12.0).color(ui_kit::ERROR_TEXT));
                }
                let mut notes: Vec<String> = Vec::new();
                if name_report.uses_window {
                    notes.push(format!("%t is shown with the sample window title \"{}\"", st.sample.window_title));
                }
                if name_report.uses_counter || folder_report.uses_counter {
                    notes.push(format!("%i shows the next counter value ({})", st.sample.next_counter));
                }
                if !notes.is_empty() {
                    ui_kit::hint(ui, &notes.join(". "));
                }
            });

        ui.add_space(8.0);
        let label = if st.cheat_sheet_open { "Hide the token list" } else { "Show the token list" };
        if ui_kit::link(ui, label).clicked() {
            st.cheat_sheet_open = !st.cheat_sheet_open;
        }
        if st.cheat_sheet_open {
            cheat_sheet(ui, st, cx);
        }
    });
}

fn cheat_sheet(ui: &mut Ui, st: &State, cx: &mut Cx<'_>) {
    ui.add_space(6.0);
    let target_name = match st.target {
        PatternTarget::File => "file name pattern",
        PatternTarget::Folder => "folder pattern",
    };
    ui_kit::hint(ui, &format!("Click a token to append it to the {target_name} (the field you used last)."));
    ui.add_space(4.0);
    let mut group = "";
    for doc in CHEAT_SHEET {
        if doc.group != group {
            group = doc.group;
            ui.add_space(4.0);
            ui.label(RichText::new(group).strong().size(12.5).color(Color32::WHITE));
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for s in super::general::spellings(doc) {
                if s == "%%" {
                    ui.label(RichText::new(s).monospace().color(theme::TEXT));
                    continue;
                }
                if ui_kit::chip(ui, s, false).on_hover_text(format!("Insert {}", insertable(s))).clicked() {
                    match st.target {
                        PatternTarget::File => append_token(&mut cx.settings.general.file_name_pattern, s),
                        PatternTarget::Folder => append_token(&mut cx.settings.general.folder_pattern, s),
                    }
                }
            }
            ui.add_space(6.0);
            ui.label(RichText::new(doc.meaning).size(12.5).color(theme::TEXT_DIM));
        });
    }
    ui.add_space(8.0);
    ui.label(RichText::new("Not supported").strong().size(12.5).color(Color32::WHITE));
    for (tok, why) in unsupported_tokens() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(*tok).monospace().color(ui_kit::WARN_TEXT));
            ui.label(RichText::new(*why).size(12.5).color(theme::TEXT_DIM));
        });
    }
    let _ = names_in;
}

fn images(ui: &mut Ui, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Images"), |ui| {
        Field::new("Format").issues(cx.issues, "general.image_format").show(ui, |ui| {
            let cur = cx.settings.general.image_format;
            if let Some(f) = ui_kit::segmented_row(
                ui,
                cur,
                &[(ImageFormatKind::Png, "PNG"), (ImageFormatKind::Jpg, "JPEG"), (ImageFormatKind::Webp, "WebP")],
            ) {
                cx.settings.general.image_format = f;
            }
        });
        ui.add_space(4.0);
        let lossy = cx.settings.general.image_format == ImageFormatKind::Jpg;
        Field::new("Quality")
            .issues(cx.issues, "general.image_quality")
            .help(if lossy { "Lower is smaller and blurrier." } else { "PNG and WebP are lossless; quality applies to JPEG only." })
            .show(ui, |ui| {
                ui.add_enabled_ui(lossy, |ui| {
                    ssx_editor_ui::ui::widgets::input_style(ui);
                    let mut q = u32::from(cx.settings.general.image_quality);
                    let r = ui.add(egui::Slider::new(&mut q, 1..=100).trailing_fill(true).clamping(egui::SliderClamping::Edits));
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "Quality"));
                    if r.changed() {
                        cx.settings.general.image_quality = q.clamp(1, 100) as u8;
                    }
                });
            });
        ui.add_space(4.0);
        Field::new("Notifications").show(ui, |ui| {
            ui_kit::switch(ui, "Show a notification when a workflow finishes", &mut cx.settings.general.show_notifications);
        });
    });
}

fn startup(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    if st.autostart.is_none() && st.autostart_error.is_none() {
        st.refresh(cx);
    }
    ui_kit::card(ui, Some("Startup"), |ui| {
        Field::new("Start with the computer")
            .help(&format!(
                "Applied immediately; it is a setting of your operating system, not of settings.toml ({}).",
                cx.host.autostart.location()
            ))
            .show(ui, |ui| {
                let mut on = st.autostart.unwrap_or(false);
                if ui_kit::switch(ui, "Start ssx when I log in", &mut on).changed() {
                    let t = cx.time(ui.ctx());
                    match st.set_autostart(cx, on) {
                        Ok(()) => cx.toasts.success(t, if on { "ssx will start when you log in" } else { "ssx will no longer start at login" }),
                        Err(e) => cx.toasts.error(t, e.to_string()),
                    }
                }
                if !cx.host.autostart_command.program_exists() {
                    ui_kit::hint(ui, &format!(
                        "{} was not found next to this program; it will be started once it is installed.",
                        cx.host.autostart_command.program.display()
                    ));
                }
                if let Some(e) = &st.autostart_error {
                    ui.horizontal_top(|ui| {
                        ui_kit::severity_icon(ui, Severity::Error);
                        ui.add(egui::Label::new(RichText::new(e).size(12.0).color(ui_kit::ERROR_TEXT)).wrap());
                    });
                }
            });
    });
}

fn history(ui: &mut Ui, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("History"), |ui| {
        Field::new("Record history").show(ui, |ui| {
            ui_kit::switch(ui, "Remember what was captured and uploaded", &mut cx.settings.history.enabled);
        });
        ui.add_space(4.0);
        Field::new("Keep at most").help("Older entries are removed first. 0 keeps everything.").show(ui, |ui| {
            number(ui, "Keep at most", &mut cx.settings.history.max_entries, 0..=1_000_000, " entries");
        });
        Field::new("Keep for").help("Entries older than this are removed. 0 keeps them forever.").show(ui, |ui| {
            number(ui, "Keep for", &mut cx.settings.history.max_age_days, 0..=36_500, " days");
        });
        Field::new("Thumbnail size").issues(cx.issues, "history.thumbnail_max_edge")
            .help("Longest edge of the previews stored in the history (16 to 1024).")
            .show(ui, |ui| {
                number(ui, "Thumbnail size", &mut cx.settings.history.thumbnail_max_edge, 16..=1024, " px");
            });
        ui_kit::hint(ui, "Deleting entries now, or pruning to these limits, is done on the History page. Files on disk are never removed by retention.");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pattern_info::{PatternKind, analyze};

    #[test]
    fn missing_save_folders_are_a_warning_files_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(save_dir_note(dir.path()), None);
        let n = save_dir_note(&dir.path().join("a/b/c")).unwrap();
        assert_eq!(n.severity, Severity::Warning);
        assert!(n.text.contains("does not exist yet"));
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let n = save_dir_note(&file).unwrap();
        assert_eq!(n.severity, Severity::Error);
        let n = save_dir_note(&file.join("sub")).unwrap();
        assert_eq!(n.severity, Severity::Error);
        assert!(n.text.contains("is a file"));
    }

    #[test]
    fn read_only_folders_are_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm.clone()).unwrap();
        let n = save_dir_note(&ro);
        perm.set_readonly(false);
        std::fs::set_permissions(&ro, perm).unwrap();
        assert!(n.is_some_and(|n| n.text.contains("read-only")));
    }

    #[test]
    fn inserted_tokens_are_always_valid_tokens() {
        for doc in CHEAT_SHEET {
            for s in spellings(doc) {
                let inserted = insertable(s);
                let r = analyze(&inserted, PatternKind::FileName);
                assert!(r.unknown_tokens.is_empty(), "{s} -> {inserted}: {r:?}");
                assert!(r.unsupported.is_empty(), "{s} -> {inserted}");
                assert!(r.illegal_chars.is_empty(), "{s} -> {inserted}: {:?}", r.illegal_chars);
            }
        }
    }

    #[test]
    fn appending_puts_the_token_at_the_end() {
        let mut p = "shot-".to_owned();
        append_token(&mut p, "%i{n}");
        append_token(&mut p, "%y");
        assert_eq!(p, "shot-%i{3}%y");
    }

    #[test]
    fn parameter_placeholders_are_replaced() {
        assert_eq!(insertable("%ra{n}"), "%ra{3}");
        assert_eq!(insertable("%ib{base,len}"), "%ib{62,4}");
        assert_eq!(insertable("%rf{path}"), "%rf{names.txt}");
        assert_eq!(insertable("%y"), "%y");
    }
}
