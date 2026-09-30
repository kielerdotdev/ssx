//! Exploration helper: dumps renderings of several states to `SSX_UI_DUMP` for a human to look
//! at. Not a golden test (see `snapshots.rs`).

mod annotate;
mod common;

use common::*;
use egui::{Key, Modifiers};

#[test]
fn dump_states() {
    if dump_dir().is_none() {
        eprintln!("SSX_UI_DUMP not set; nothing to dump");
        return;
    }
    let mut h = window(app_for(dashboard()), [1280.0, 800.0]);
    settle(&mut h);
    annotate::annotate(&mut h);
    settle(&mut h);
    dump(&mut h, "annotated");

    // Select the rectangle: handles and the properties bar.
    h.key_press(Key::V);
    settle(&mut h);
    click_image(&mut h, (400.0, 78.0));
    settle(&mut h);
    dump(&mut h, "selected-rect");

    // Text tool properties.
    h.key_press(Key::T);
    settle(&mut h);
    dump(&mut h, "text-tool");
    h.state_mut().state.push(ssx_editor_ui::action::Action::ToggleLayers);
    settle(&mut h);
    dump(&mut h, "layers");
    h.key_press(Key::F1);
    settle(&mut h);
    dump(&mut h, "shortcuts");
    h.key_press(Key::Escape);
    settle(&mut h);
    h.state_mut().state.push(ssx_editor_ui::action::Action::OpenEffect(
        ssx_editor_ui::effects::EffectKind::DropShadow,
    ));
    settle(&mut h);
    h.state_mut().preview.wait_idle(std::time::Duration::from_secs(10));
    settle(&mut h);
    dump(&mut h, "effect");
    h.state_mut().state.push(ssx_editor_ui::action::Action::CloseDialog);
    settle(&mut h);
    h.key_press(Key::C);
    settle(&mut h);
    drag_image(&mut h, (300.0, 200.0), (900.0, 500.0));
    dump(&mut h, "crop");
    h.key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    settle(&mut h);
    dump(&mut h, "zoom100");

    h.key_press(Key::Escape);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::S);
    settle(&mut h);
    dump(&mut h, "saveas");
    h.key_press(Key::Escape);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::I);
    settle(&mut h);
    dump(&mut h, "resize");
    h.key_press(Key::Escape);
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    settle(&mut h);
    dump(&mut h, "settings");
}
