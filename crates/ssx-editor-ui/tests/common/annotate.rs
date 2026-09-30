//! Shared "annotate the dashboard through the UI" script used by the snapshot tests and the
//! documentation screenshot.
#![allow(dead_code)]

use super::*;
use egui::{Key, Modifiers};
use egui_kittest::Harness;
use ssx_editor_ui::app::EditorApp;

/// Draws a realistic set of annotations with the real tools and pointer events.
pub fn annotate(h: &mut Harness<'_, EditorApp>) {
    let key = |h: &mut Harness<'_, EditorApp>, k: Key, m: Modifiers| {
        h.key_press_modifiers(m, k);
        settle(h);
    };
    // 1. Red rectangle around the first KPI card.
    key(h, Key::R, Modifiers::NONE);
    drag_image(h, (232.0, 78.0), (570.0, 210.0));
    // 2. Arrow + text pointing at the newest bar.
    key(h, Key::A, Modifiers::NONE);
    drag_image(h, (700.0, 262.0), (824.0, 290.0));
    key(h, Key::T, Modifiers::SHIFT);
    click_image(h, (560.0, 246.0));
    type_text(h, "Best week yet");
    key(h, Key::Escape, Modifiers::NONE);
    // 3. Numbered steps.
    key(h, Key::N, Modifiers::NONE);
    click_image(h, (236.0, 84.0));
    click_image(h, (846.0, 268.0));
    click_image(h, (1236.0, 384.0));
    key(h, Key::Escape, Modifiers::NONE);
    // 4. Hide the API key.
    key(h, Key::U, Modifiers::NONE);
    drag_image(h, (960.0, 384.0), (1220.0, 416.0));
    // 5. Highlight a table row.
    key(h, Key::H, Modifiers::NONE);
    drag_image(h, (250.0, 624.0), (1240.0, 652.0));
    // 6. A speech balloon.
    key(h, Key::B, Modifiers::NONE);
    drag_image(h, (830.0, 660.0), (1000.0, 706.0));
    type_text(h, "Fix this page");
    key(h, Key::Escape, Modifiers::NONE);
    key(h, Key::Escape, Modifiers::NONE);
}
