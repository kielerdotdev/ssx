//! The small state file: window size, last tool, per-tool styles and recent colours survive a
//! restart.

#![allow(clippy::float_cmp)] // the values are stored and read back verbatim

mod common;

use common::*;
use egui::Key;
use ssx_editor::{Color, Tool};
use ssx_editor_ui::{
    action::Action, app::EditorApp, document::EditorDoc, prefs::Prefs, props::PropEdit,
    request::DevOptions, services::Services, tools::ToolId,
};

#[test]
fn tool_styles_tool_window_and_colours_persist_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("editor-ui.json");

    // Session 1: pick a tool, restyle it, use a colour, resize the window, quit.
    let mut app = EditorApp::new(
        EditorDoc::from_frame(flat(300, 200)).unwrap(),
        None,
        Prefs::load_from(&file).unwrap(),
        Services::fake(),
        DevOptions::default(),
        || {},
    );
    app.set_prefs_path(Some(file.clone()));
    let mut h = window(app, [1200.0, 800.0]);
    settle(&mut h);
    h.key_press(Key::A);
    settle(&mut h);
    h.state_mut().state.push(Action::Prop(PropEdit::StrokeWidth(11.0)));
    h.state_mut().state.push(Action::Prop(PropEdit::Stroke(Color::rgb(1, 2, 3))));
    settle(&mut h);
    h.state_mut().state.note_color(Color::rgb(9, 8, 7));
    h.state_mut().state.prefs.window.width = 1111.0;
    h.state_mut().state.prefs.window.height = 777.0;
    h.state_mut().save_prefs();
    assert!(file.exists(), "the state file is written to the config dir");

    // Session 2: everything is back.
    let loaded = Prefs::load_from(&file).unwrap();
    assert_eq!(loaded.last_tool, ToolId::Arrow);
    assert_eq!((loaded.window.width, loaded.window.height), (1111.0, 777.0));
    assert!(loaded.recent_colors.contains(&Color::rgb(9, 8, 7)));
    let app2 = EditorApp::new(
        EditorDoc::from_frame(flat(300, 200)).unwrap(),
        None,
        loaded,
        Services::fake(),
        DevOptions::default(),
        || {},
    );
    assert_eq!(app2.state.tool, ToolId::Arrow, "the last tool is selected again");
    assert_eq!(app2.doc.session.tool(), Tool::Arrow);
    let preset = app2.doc.session.styles().get(Tool::Arrow).unwrap();
    assert_eq!(preset.style.stroke_width, 11.0);
    assert_eq!(preset.style.stroke, Color::rgb(1, 2, 3));
}
