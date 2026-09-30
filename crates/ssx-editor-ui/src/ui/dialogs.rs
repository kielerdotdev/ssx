//! Modal dialogs: save, open, resize, canvas, crop, cut-out, unsaved changes, shortcuts, settings,
//! and the live-preview effect window. Each one edits a form from `crate::forms` and queues an
//! [`Action`] when confirmed; none touches the document itself.

use egui::{Align, Context, Id, Layout, Modal, RichText, Ui, vec2};
use ssx_editor::{Color, object::Axis};
use ssx_imgfx::ResizeFilter;

use super::{
    color::color_button,
    theme,
    widgets::{self, accent_button, caption, segmented},
};
use crate::{
    action::{Action, UnsavedAnswer},
    document::EditorDoc,
    effects::EffectKind,
    export::SaveFormat,
    forms::{
        CanvasForm, CropForm, CutForm, MAX_SIDE, MAX_SIDE_I32, OpenForm, ResizeForm, SaveForm,
    },
    preview::Preview,
    props::ColorField,
    shortcuts::{GESTURES, grouped},
    state::{AppState, Continuation, Dialog},
};

/// A right-aligned row that takes only one line of height (a bare `with_layout` would claim
/// all the vertical space the dialog has left).
fn right_aligned(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 30.0),
        Layout::right_to_left(Align::Center),
        add,
    );
}

fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).heading().strong());
    ui.add_space(6.0);
}

fn buttons(ui: &mut Ui, ok: &str, enabled: bool) -> Option<bool> {
    let mut out = None;
    ui.add_space(8.0);
    right_aligned(ui, |ui| {
        if widgets::accent_button_ex(ui, ok, None, enabled).clicked() && enabled {
            out = Some(true);
        }
        if ui.add(egui::Button::new("Cancel").min_size(vec2(70.0, 26.0))).clicked() {
            out = Some(false);
        }
    });
    out
}

fn row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(110.0, 22.0), egui::Sense::hover());
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            theme::TEXT_DIM,
        );
        add(ui);
    });
}

fn error_line(ui: &mut Ui, e: Option<&str>) {
    if let Some(e) = e {
        ui.label(RichText::new(e).color(theme::DANGER).size(12.0));
    }
}

/// Shows the open dialog, if any.
pub fn show(ctx: &Context, state: &mut AppState, doc: &EditorDoc, preview: &Preview) {
    let Some(mut dialog) = state.dialog.take() else { return };
    let close = match &mut dialog {
        Dialog::SaveAs(f) => save_as(ctx, state, f),
        Dialog::Open(f) => open(ctx, state, f),
        Dialog::Resize(f) => resize(ctx, state, f),
        Dialog::Canvas(f) => canvas(ctx, state, f),
        Dialog::Crop(f) => crop(ctx, state, f),
        Dialog::CutOut(f) => cut_out(ctx, state, f),
        Dialog::Shortcuts => shortcuts(ctx),
        Dialog::Settings => settings(ctx, state),
        Dialog::Unsaved(cont) => unsaved(ctx, state, doc, cont),
        Dialog::Error { title: t, message } => error(ctx, t, message),
        Dialog::Effect(f) => effect(ctx, state, f, preview),
    };
    if !close && state.dialog.is_none() {
        state.dialog = Some(dialog);
    }
}

fn modal(
    ctx: &Context,
    id: &str,
    width: f32,
    content: impl FnOnce(&mut Ui) -> Option<bool>,
) -> bool {
    let frame = egui::Frame::popup(&ctx.global_style()).inner_margin(egui::Margin::same(18));
    let m = Modal::new(Id::new(("ssx-dialog", id))).frame(frame).show(ctx, |ui| {
        widgets::input_style(ui);
        ui.set_width(width);
        content(ui)
    });
    // Any button (confirm or cancel) ends the dialog; otherwise only Esc / a click outside does.
    m.inner.is_some() || m.should_close()
}

fn save_as(ctx: &Context, state: &mut AppState, f: &mut SaveForm) -> bool {
    let mut confirmed = false;
    let closed = modal(ctx, "save", 480.0, |ui| {
        title(ui, "Save as");
        row(ui, "File", |ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut f.path).desired_width(300.0));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "File path")
            });
            if r.changed() {
                f.sync_format_from_path();
            }
            if ui.button("Browse...").clicked() {
                state.push(Action::BrowseSaveTarget);
            }
        });
        row(ui, "Format", |ui| {
            egui::ComboBox::from_id_salt("save-format")
                .selected_text(f.format.label())
                .width(220.0)
                .show_ui(ui, |ui| {
                    for fmt in SaveFormat::ALL {
                        if ui.selectable_label(f.format == fmt, fmt.label()).clicked() {
                            f.set_format(fmt);
                        }
                    }
                });
        });
        match f.format {
            SaveFormat::Image(ssx_types::ImageFormat::Jpeg) => {
                row(ui, "Quality", |ui| {
                    let mut q = f32::from(f.jpeg_quality);
                    if ui.add(egui::Slider::new(&mut q, 1.0..=100.0).max_decimals(0)).changed() {
                        f.jpeg_quality = q as u8;
                    }
                });
                caption(ui, "JPEG has no transparency: transparent areas become white.");
            }
            SaveFormat::Image(ssx_types::ImageFormat::Png) => {
                row(ui, "Compression", |ui| {
                    ui.checkbox(&mut f.png_fast, "Fast (larger file)");
                });
            }
            SaveFormat::Image(ssx_types::ImageFormat::WebP) => {
                caption(ui, "WebP is saved lossless.");
            }
            SaveFormat::Project => {
                caption(
                    ui,
                    "Keeps every annotation editable. Open it again in the editor to continue.",
                );
            }
            SaveFormat::Image(_) => {}
        }
        let target = f.target();
        error_line(ui, target.as_ref().err().map(String::as_str));
        buttons(ui, "Save", target.is_ok()).inspect(|ok| confirmed = *ok)
    });
    if confirmed && let Ok(path) = f.target() {
        state.prefs.jpeg_quality = f.jpeg_quality;
        state.prefs.png_fast = f.png_fast;
        state.push(Action::SaveTo { path, settings: f.settings() });
    }
    closed
}

fn open(ctx: &Context, state: &mut AppState, f: &mut OpenForm) -> bool {
    let mut confirmed = false;
    let closed = modal(ctx, "open", 480.0, |ui| {
        title(ui, "Open");
        row(ui, "File", |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut f.path)
                    .desired_width(300.0)
                    .hint_text("/path/to/image.png"),
            );
            if ui.button("Browse...").clicked() {
                state.push(Action::Open);
            }
        });
        let t = f.target();
        if !f.path.trim().is_empty() {
            error_line(ui, t.as_ref().err().map(String::as_str));
        }
        buttons(ui, "Open", t.is_ok()).inspect(|ok| confirmed = *ok)
    });
    if confirmed && let Ok(p) = f.target() {
        state.push(Action::OpenPath(p));
    }
    closed
}

fn number(ui: &mut Ui, v: &mut u32, max: u32) -> bool {
    ui.add(egui::DragValue::new(v).range(1..=max).speed(1.0).suffix(" px")).changed()
}

fn resize(ctx: &Context, state: &mut AppState, f: &mut ResizeForm) -> bool {
    let mut confirmed = false;
    let closed = modal(ctx, "resize", 400.0, |ui| {
        title(ui, "Resize image");
        caption(ui, &format!("Current size: {} x {} px", f.orig_w, f.orig_h));
        ui.checkbox(&mut f.lock_aspect, "Keep aspect ratio");
        row(ui, "Width", |ui| {
            let mut w = f.width;
            if number(ui, &mut w, MAX_SIDE) {
                f.set_width(w);
            }
        });
        row(ui, "Height", |ui| {
            let mut h = f.height;
            if number(ui, &mut h, MAX_SIDE) {
                f.set_height(h);
            }
        });
        row(ui, "Percent", |ui| {
            let mut p = f.percent();
            if ui
                .add(egui::DragValue::new(&mut p).range(1.0..=1000.0).speed(0.5).suffix(" %"))
                .changed()
            {
                f.set_percent(p);
            }
        });
        row(ui, "Filter", |ui| {
            for (flt, name) in [
                (ResizeFilter::Lanczos3, "Sharp (Lanczos)"),
                (ResizeFilter::Bilinear, "Smooth"),
                (ResizeFilter::Nearest, "Pixelated"),
            ] {
                if ui.selectable_label(f.filter == flt, name).clicked() {
                    f.filter = flt;
                }
            }
        });
        let t = f.target();
        error_line(ui, t.as_ref().err().map(String::as_str));
        buttons(ui, "Resize", t.is_ok()).inspect(|ok| confirmed = *ok)
    });
    if confirmed && let Ok((w, h)) = f.target() {
        state.push(Action::Resize { width: w, height: h, filter: f.filter });
    }
    closed
}

fn canvas(ctx: &Context, state: &mut AppState, f: &mut CanvasForm) -> bool {
    let mut confirmed = false;
    let closed = modal(ctx, "canvas", 400.0, |ui| {
        title(ui, "Canvas size");
        caption(
            ui,
            "Add (positive) or remove (negative) pixels around the picture. Annotations keep their place.",
        );
        for (label, v) in [
            ("Left", &mut f.left),
            ("Top", &mut f.top),
            ("Right", &mut f.right),
            ("Bottom", &mut f.bottom),
        ] {
            row(ui, label, |ui| {
                ui.add(
                    egui::DragValue::new(v)
                        .range(-MAX_SIDE_I32..=MAX_SIDE_I32)
                        .speed(1.0)
                        .suffix(" px"),
                );
            });
        }
        row(ui, "Fill", |ui| {
            let cur = f.background.unwrap_or(Color::TRANSPARENT);
            if let Some(c) = color_button(ui, "Canvas fill", cur, ColorField::Canvas, state, true) {
                f.background = (!c.is_transparent()).then_some(c);
            }
            caption(ui, if f.background.is_none() { "Transparent" } else { "Solid" });
        });
        let size = f.result_size();
        match &size {
            Ok((w, h)) => caption(ui, &format!("Result: {w} x {h} px")),
            Err(e) => error_line(ui, Some(e)),
        }
        buttons(ui, "Apply", size.is_ok() && !f.is_noop()).inspect(|ok| confirmed = *ok)
    });
    if confirmed {
        state.push(Action::ResizeCanvas {
            left: f.left,
            top: f.top,
            right: f.right,
            bottom: f.bottom,
            background: f.background,
        });
    }
    closed
}

fn crop(ctx: &Context, state: &mut AppState, f: &mut CropForm) -> bool {
    let mut confirmed = false;
    let mut auto = false;
    let closed = modal(ctx, "crop", 400.0, |ui| {
        title(ui, "Crop");
        caption(ui, &format!("Image: {} x {} px", f.image.0, f.image.1));
        row(ui, "Left", |ui| {
            ui.add(
                egui::DragValue::new(&mut f.x).range(-MAX_SIDE_I32..=MAX_SIDE_I32).suffix(" px"),
            );
        });
        row(ui, "Top", |ui| {
            ui.add(
                egui::DragValue::new(&mut f.y).range(-MAX_SIDE_I32..=MAX_SIDE_I32).suffix(" px"),
            );
        });
        row(ui, "Width", |ui| {
            number(ui, &mut f.width, MAX_SIDE);
        });
        row(ui, "Height", |ui| {
            number(ui, &mut f.height, MAX_SIDE);
        });
        ui.separator();
        row(ui, "Auto-crop", |ui| {
            let mut t = u32::from(f.tolerance);
            ui.add(egui::DragValue::new(&mut t).range(0..=255).prefix("tolerance "));
            f.tolerance = t as u8;
            if ui.button("Trim borders").on_hover_text("Remove uniform-coloured borders").clicked()
            {
                auto = true;
            }
        });
        let r = f.rect();
        error_line(ui, r.as_ref().err().map(String::as_str));
        buttons(ui, "Crop", r.is_ok()).inspect(|ok| confirmed = *ok)
    });
    if auto {
        state.push(Action::AutoCrop(f.tolerance));
        return true;
    }
    if confirmed && let Ok(r) = f.rect() {
        state.push(Action::CropTo(r));
    }
    closed
}

fn cut_out(ctx: &Context, state: &mut AppState, f: &mut CutForm) -> bool {
    let mut confirmed = false;
    let closed = modal(ctx, "cutout", 400.0, |ui| {
        title(ui, "Cut out");
        caption(
            ui,
            "Removes a strip and joins the two sides. Drag with the Cut out tool for a visual way.",
        );
        row(ui, "Direction", |ui| {
            if let Some(a) =
                segmented(ui, f.axis, &[(Axis::X, "Vertical strip"), (Axis::Y, "Horizontal strip")])
            {
                f.axis = a;
                let len = f.len();
                f.start = len * 45 / 100;
                f.end = len * 55 / 100;
            }
        });
        let len = f.len();
        row(ui, "From", |ui| {
            ui.add(egui::DragValue::new(&mut f.start).range(0..=len).suffix(" px"));
        });
        row(ui, "To", |ui| {
            ui.add(egui::DragValue::new(&mut f.end).range(0..=len).suffix(" px"));
        });
        let s = f.strip();
        error_line(ui, s.as_ref().err().map(String::as_str));
        buttons(ui, "Cut out", s.is_ok()).inspect(|ok| confirmed = *ok)
    });
    if confirmed && let Ok((axis, start, end)) = f.strip() {
        state.push(Action::CutOut { axis, start, end });
    }
    closed
}

fn shortcuts(ctx: &Context) -> bool {
    modal(ctx, "shortcuts", 620.0, |ui| {
        title(ui, "Keyboard shortcuts");
        egui::ScrollArea::vertical().max_height(460.0).show(ui, |ui| {
            egui::Grid::new("shortcut-grid")
                .num_columns(2)
                .spacing([28.0, 3.0])
                .min_col_width(280.0)
                .show(ui, |ui| {
                    for (cat, list) in grouped() {
                        ui.label(RichText::new(cat).strong().color(theme::ACCENT));
                        ui.label("");
                        ui.end_row();
                        for s in list {
                            ui.label(s.label);
                            ui.label(
                                RichText::new(s.chord.display()).monospace().color(theme::TEXT_DIM),
                            );
                            ui.end_row();
                        }
                    }
                    ui.label(RichText::new("Mouse").strong().color(theme::ACCENT));
                    ui.label("");
                    ui.end_row();
                    for (g, e) in GESTURES {
                        ui.label(*e);
                        ui.label(RichText::new(*g).monospace().color(theme::TEXT_DIM));
                        ui.end_row();
                    }
                });
        });
        ui.add_space(8.0);
        let mut out = None;
        right_aligned(ui, |ui| {
            if accent_button(ui, "Close", None).clicked() {
                out = Some(false);
            }
        });
        out
    })
}

fn settings(ctx: &Context, state: &mut AppState) -> bool {
    modal(ctx, "settings", 440.0, |ui| {
        title(ui, "Settings");
        ui.checkbox(&mut state.prefs.show_layers, "Show the object list");
        ui.checkbox(&mut state.prefs.pixel_grid, "Draw a pixel grid when zoomed in");
        ui.checkbox(&mut state.prefs.png_fast, "Fast PNG compression (larger files)");
        row(ui, "JPEG quality", |ui| {
            let mut q = f32::from(state.prefs.jpeg_quality);
            if ui.add(egui::Slider::new(&mut q, 1.0..=100.0).max_decimals(0)).changed() {
                state.prefs.jpeg_quality = q as u8;
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui
                .button("Reset tool styles")
                .on_hover_text("Forget the colours, widths and fonts each tool remembers")
                .clicked()
            {
                state.push(Action::ResetToolStyles);
            }
            if ui.button("Clear recent colours").clicked() {
                state.push(Action::ClearRecentColors);
            }
        });
        caption(ui, "Window size, tool styles and recent colours are remembered between sessions.");
        let mut out = None;
        right_aligned(ui, |ui| {
            if accent_button(ui, "Close", None).clicked() {
                out = Some(false);
            }
        });
        out
    })
}

fn unsaved(ctx: &Context, state: &mut AppState, doc: &EditorDoc, cont: &Continuation) -> bool {
    let name = doc.display_name();
    let what = match cont {
        Continuation::Close => "before closing",
        Continuation::Run(_) => "before replacing it",
    };
    let mut answer = None;
    let closed = modal(ctx, "unsaved", 420.0, |ui| {
        title(ui, "Unsaved changes");
        ui.label(format!("Save the changes to {name} {what}?"));
        ui.add_space(10.0);
        right_aligned(ui, |ui| {
            if accent_button(ui, "Save", None).clicked() {
                answer = Some(UnsavedAnswer::Save);
            }
            if ui.button("Discard").clicked() {
                answer = Some(UnsavedAnswer::Discard);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(UnsavedAnswer::Cancel);
            }
        });
        None
    });
    if let Some(a) = answer {
        state.push(Action::Unsaved(a));
        state.after_save = Some(cont.clone());
        return true;
    }
    if closed {
        // Esc or a click outside: same as Cancel.
        state.push(Action::Unsaved(UnsavedAnswer::Cancel));
    }
    closed
}

fn error(ctx: &Context, t: &str, message: &str) -> bool {
    modal(ctx, "error", 420.0, |ui| {
        ui.label(RichText::new(t).heading().color(theme::DANGER));
        ui.add_space(6.0);
        ui.label(message);
        ui.add_space(8.0);
        let mut out = None;
        right_aligned(ui, |ui| {
            if accent_button(ui, "OK", None).clicked() {
                out = Some(false);
            }
        });
        out
    })
}

/// The effect window: not modal, so the picture can still be zoomed and panned to judge the
/// preview.
fn effect(
    ctx: &Context,
    state: &mut AppState,
    f: &mut crate::effects::EffectForm,
    preview: &Preview,
) -> bool {
    let mut close = false;
    let kind: EffectKind = f.kind;
    egui::Window::new(kind.label())
        .id(Id::new("ssx-effect-window"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::RIGHT_TOP, vec2(-16.0, 140.0))
        .show(ctx, |ui| {
            widgets::input_style(ui);
            ui.set_width(280.0);
            caption(ui, "Live preview on the picture");
            for (i, spec) in kind.params().iter().enumerate() {
                if let Some(v) = f.values.get_mut(i) {
                    ui.horizontal(|ui| {
                        ui.label(spec.label);
                        right_aligned(ui, |ui| {
                            let mut slider = egui::Slider::new(v, spec.min..=spec.max)
                                .suffix(spec.suffix)
                                .trailing_fill(true);
                            if spec.integer {
                                slider = slider.integer();
                            } else {
                                slider = slider.max_decimals(2);
                            }
                            ui.add(slider);
                        });
                    });
                }
            }
            if kind.has_color() {
                row(ui, "Colour", |ui| {
                    let cur = Color::rgba(f.color[0], f.color[1], f.color[2], f.color[3]);
                    if let Some(c) =
                        color_button(ui, "Effect colour", cur, ColorField::Effect, state, false)
                    {
                        f.color = c.to_array();
                    }
                });
            }
            if kind.has_sides() {
                ui.horizontal(|ui| {
                    ui.label("Sides");
                    ui.checkbox(&mut f.sides.top, "Top");
                    ui.checkbox(&mut f.sides.right, "Right");
                    ui.checkbox(&mut f.sides.bottom, "Bottom");
                    ui.checkbox(&mut f.sides.left, "Left");
                });
            }
            if let Some(e) = preview.error() {
                error_line(ui, Some(e));
            } else if preview.is_busy() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    caption(ui, "Rendering preview...");
                });
            } else {
                caption(ui, "Preview is up to date.");
            }
            ui.add_space(4.0);
            right_aligned(ui, |ui| {
                if accent_button(ui, "Apply", Some(crate::icons::Icon::Check)).clicked() {
                    state.push(Action::ApplyEffect(f.effect()));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    let _ = widgets::BUTTON;
    close
}
