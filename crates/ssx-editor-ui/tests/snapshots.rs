//! Golden-image tests of the real UI, rendered headlessly through wgpu (lavapipe on CI).
//!
//! Regenerate after an intentional visual change with `UPDATE_SNAPSHOTS=1 cargo test -p
//! ssx-editor-ui --test snapshots`. Comparison is tolerance-based (per-pixel colour threshold
//! plus a small budget of differing pixels) so different Vulkan drivers' anti-aliasing does not
//! cause false alarms.

mod annotate;
mod common;

use common::*;
use egui::{Key, Modifiers, vec2};
use egui_kittest::{Harness, SnapshotOptions};
use ssx_editor_ui::{
    action::{Action, DialogKind},
    app::EditorApp,
    effects::EffectKind,
    tools::ToolId,
};

fn opts(budget: usize) -> SnapshotOptions {
    SnapshotOptions::new().threshold(0.9).failed_pixel_count_threshold(budget)
}

fn bars(
    app: EditorApp,
    size: egui::Vec2,
    rows: &'static [&'static str],
) -> Harness<'static, EditorApp> {
    Harness::builder().with_size(size).with_max_steps(40).wgpu().build_ui_state(
        move |ui, app: &mut EditorApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            for r in rows {
                match *r {
                    "menu" => app.show_menubar(ui),
                    "tools" => app.show_toolbar(ui),
                    _ => app.show_props(ui),
                }
            }
        },
        app,
    )
}

#[test]
fn toolbar_matches_the_reference_layout() {
    let mut app = app_for(dashboard());
    app.select_tool(ToolId::CropRect);
    let mut h = bars(app, vec2(1280.0, 84.0), &["menu", "tools"]);
    settle(&mut h);
    h.snapshot_options("toolbar", &opts(200));
}

#[test]
fn toolbar_highlights_the_active_tool() {
    let mut app = app_for(dashboard());
    app.select_tool(ToolId::Arrow);
    let mut h = bars(app, vec2(1280.0, 84.0), &["menu", "tools"]);
    settle(&mut h);
    h.snapshot_options("toolbar-arrow-active", &opts(200));
}

#[test]
fn properties_bar_for_a_rectangle() {
    let mut app = app_for(dashboard());
    app.select_tool(ToolId::Rectangle);
    let mut h = bars(app, vec2(1280.0, 44.0), &["props"]);
    settle(&mut h);
    h.snapshot_options("props-rectangle", &opts(150));
}

#[test]
fn properties_bar_for_text() {
    let mut app = app_for(dashboard());
    app.select_tool(ToolId::TextBoxed);
    let mut h = bars(app, vec2(1280.0, 44.0), &["props"]);
    settle(&mut h);
    h.snapshot_options("props-text", &opts(150));
}

#[test]
fn full_window_with_an_annotated_screenshot() {
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    annotate::annotate(&mut h);
    h.key_press(Key::V);
    settle(&mut h);
    // Deselect and move the pointer away so no hover state leaks into the golden.
    h.state_mut().state.push(Action::Deselect);
    settle(&mut h);
    h.remove_cursor();
    settle(&mut h);
    assert_eq!(h.state().doc.doc().objects().len(), 9);
    h.snapshot_options("window-annotated", &opts(4000));
}

#[test]
fn selection_handles_and_object_list() {
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    annotate::annotate(&mut h);
    h.key_press(Key::V);
    settle(&mut h);
    h.state_mut().state.push(Action::ToggleLayers);
    settle(&mut h);
    click_image(&mut h, (400.0, 78.0));
    h.remove_cursor();
    settle(&mut h);
    h.snapshot_options("window-selection-layers", &opts(4000));
}

#[test]
fn resize_dialog() {
    let mut h = window(app_for(flat(640, 400)), [960.0, 600.0]);
    settle(&mut h);
    h.state_mut().state.push(Action::OpenDialog(DialogKind::Resize));
    settle(&mut h);
    h.remove_cursor();
    settle(&mut h);
    h.snapshot_options("dialog-resize", &opts(1500));
}

#[test]
fn effect_preview_window() {
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    h.state_mut().state.push(Action::OpenEffect(EffectKind::GaussianBlur));
    settle(&mut h);
    let _ = h.state_mut().preview.wait_idle(std::time::Duration::from_secs(20));
    settle(&mut h);
    h.remove_cursor();
    settle(&mut h);
    h.snapshot_options("effect-preview", &opts(4000));
}

#[test]
fn text_editing_caret() {
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    h.key_press(Key::T);
    settle(&mut h);
    click_image(&mut h, (300.0, 620.0));
    type_text(&mut h, "Typing here");
    h.remove_cursor();
    // Freeze the blink phase: kittest time is deterministic, but be explicit.
    settle(&mut h);
    assert!(h.state().doc.session.text_edit_state().is_some());
    h.snapshot_options("window-text-editing", &opts(4000));
    let _ = Modifiers::NONE;
}
