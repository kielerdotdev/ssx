//! Translating egui input into editor commands.
//!
//! [`route`] decides, for one key press, who handles it:
//!
//! * the **app** (an [`Action`] from the shortcut table),
//! * the **session** (arrows nudge, Enter applies a crop, text-caret movement...), or
//! * nobody.
//!
//! The important rule is *text editing wins*: while a text object is being edited, single
//! letters are typing (they arrive as `Event::Text`, never as tool shortcuts) and caret keys
//! belong to the session; only Ctrl-chords that are not text commands (Save, Open...) still
//! reach the app.

use std::collections::HashMap;
use std::sync::LazyLock;

use egui::{CursorIcon, Key};
use ssx_editor::{CursorHint, Key as SKey, Modifiers as SMods};

use crate::{
    action::Action,
    shortcuts::{Chord, SHORTCUTS},
};

static LOOKUP: LazyLock<HashMap<Chord, Action>> =
    LazyLock::new(|| SHORTCUTS.iter().map(|s| (s.chord, s.action.clone())).collect());

/// Who handles a key press.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    /// An application command.
    App(Action),
    /// Forward to `EditorSession::key_down`.
    Session(SKey, SMods),
    /// Not ours.
    Ignore,
}

/// Looks a chord up in the shortcut table.
pub fn resolve(chord: Chord) -> Option<&'static Action> {
    LOOKUP.get(&chord)
}

/// egui modifiers → chord parts. `command` is Ctrl on Linux/Windows and Cmd on macOS.
pub fn chord_of(key: Key, m: &egui::Modifiers) -> Chord {
    Chord { key, ctrl: m.command, shift: m.shift, alt: m.alt }
}

/// egui modifiers → the engine's modifiers.
pub fn session_mods(m: &egui::Modifiers) -> SMods {
    SMods { shift: m.shift, ctrl: m.command, alt: m.alt }
}

/// egui key → the engine's key, for the keys the session understands.
pub fn session_key(key: Key, ctrl: bool) -> Option<SKey> {
    Some(match key {
        Key::ArrowLeft => SKey::Left,
        Key::ArrowRight => SKey::Right,
        Key::ArrowUp => SKey::Up,
        Key::ArrowDown => SKey::Down,
        Key::Home => SKey::Home,
        Key::End => SKey::End,
        Key::Backspace => SKey::Backspace,
        Key::Delete => SKey::Delete,
        Key::Enter => SKey::Enter,
        Key::Escape => SKey::Escape,
        Key::Tab => SKey::Tab,
        other if ctrl => {
            let name = other.name();
            let mut it = name.chars();
            match (it.next(), it.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => SKey::Char(c.to_ascii_lowercase()),
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// Ctrl-chords the text editor owns while a text object is being edited.
fn is_text_command(key: Key, ctrl: bool) -> bool {
    ctrl && matches!(key, Key::Z | Key::Y | Key::A | Key::C | Key::X | Key::V)
}

/// Decides who handles a key press. See the module docs.
pub fn route(key: Key, m: &egui::Modifiers, text_editing: bool) -> Routed {
    let chord = chord_of(key, m);
    if text_editing {
        if is_text_command(key, chord.ctrl) {
            // Clipboard text comes through egui's Copy/Cut/Paste events instead.
            return match key {
                Key::C | Key::X | Key::V => Routed::Ignore,
                _ => session_key(key, true)
                    .map_or(Routed::Ignore, |k| Routed::Session(k, session_mods(m))),
            };
        }
        if chord.ctrl {
            return resolve(chord).map_or(Routed::Ignore, |a| Routed::App(a.clone()));
        }
        return session_key(key, false)
            .map_or(Routed::Ignore, |k| Routed::Session(k, session_mods(m)));
    }
    if let Some(a) = resolve(chord) {
        return Routed::App(a.clone());
    }
    if !chord.ctrl
        && !chord.alt
        && let Some(k) = session_key(key, false)
    {
        return Routed::Session(k, session_mods(m));
    }
    Routed::Ignore
}

/// Mouse cursor for a hint from the session.
pub fn cursor_icon(h: CursorHint) -> CursorIcon {
    match h {
        CursorHint::Default => CursorIcon::Default,
        CursorHint::Crosshair => CursorIcon::Crosshair,
        CursorHint::Text => CursorIcon::Text,
        CursorHint::Move => CursorIcon::Move,
        CursorHint::Grabbing => CursorIcon::Grabbing,
        CursorHint::ResizeNs => CursorIcon::ResizeVertical,
        CursorHint::ResizeEw => CursorIcon::ResizeHorizontal,
        CursorHint::ResizeNwse => CursorIcon::ResizeNwSe,
        CursorHint::ResizeNesw => CursorIcon::ResizeNeSw,
        // egui has no rotate cursor; a cell cursor reads as "special handle".
        CursorHint::Rotate => CursorIcon::Alias,
        CursorHint::Eraser => CursorIcon::Cell,
        CursorHint::NotAllowed => CursorIcon::NotAllowed,
    }
}

#[cfg(test)]
mod tests {
    use egui::Modifiers;

    use super::*;
    use crate::tools::ToolId;

    fn ctrl() -> Modifiers {
        Modifiers::COMMAND
    }

    #[test]
    fn letters_pick_tools_outside_text_editing() {
        assert_eq!(
            route(Key::R, &Modifiers::NONE, false),
            Routed::App(Action::SetTool(ToolId::Rectangle))
        );
        assert_eq!(
            route(Key::A, &Modifiers::SHIFT, false),
            Routed::App(Action::SetTool(ToolId::FreehandArrow))
        );
        assert_eq!(route(Key::Q, &Modifiers::NONE, false), Routed::Ignore);
    }

    #[test]
    fn letters_never_pick_tools_while_typing() {
        for k in [Key::R, Key::T, Key::A, Key::S, Key::V] {
            assert_eq!(route(k, &Modifiers::NONE, true), Routed::Ignore, "{k:?}");
        }
    }

    #[test]
    fn caret_keys_go_to_the_session_while_typing() {
        assert_eq!(
            route(Key::ArrowLeft, &Modifiers::SHIFT, true),
            Routed::Session(SKey::Left, SMods::SHIFT)
        );
        assert_eq!(
            route(Key::Enter, &Modifiers::NONE, true),
            Routed::Session(SKey::Enter, SMods::NONE)
        );
        assert_eq!(
            route(Key::Escape, &Modifiers::NONE, true),
            Routed::Session(SKey::Escape, SMods::NONE)
        );
        assert_eq!(
            route(Key::Backspace, &Modifiers::NONE, true),
            Routed::Session(SKey::Backspace, SMods::NONE),
            "backspace edits text instead of deleting the object"
        );
    }

    #[test]
    fn text_commands_belong_to_the_session_and_file_commands_to_the_app() {
        assert_eq!(route(Key::Z, &ctrl(), true), Routed::Session(SKey::Char('z'), SMods::CTRL));
        assert_eq!(route(Key::A, &ctrl(), true), Routed::Session(SKey::Char('a'), SMods::CTRL));
        // Clipboard text arrives via egui events, so the key itself is ignored.
        assert_eq!(route(Key::V, &ctrl(), true), Routed::Ignore);
        assert_eq!(route(Key::S, &ctrl(), true), Routed::App(Action::Save));
        assert_eq!(route(Key::S, &ctrl(), false), Routed::App(Action::Save));
    }

    #[test]
    fn arrows_nudge_and_delete_deletes_outside_text() {
        assert_eq!(
            route(Key::ArrowUp, &Modifiers::NONE, false),
            Routed::Session(SKey::Up, SMods::NONE)
        );
        assert_eq!(
            route(Key::Delete, &Modifiers::NONE, false),
            Routed::App(Action::DeleteSelection)
        );
        assert_eq!(
            route(Key::Enter, &Modifiers::NONE, false),
            Routed::Session(SKey::Enter, SMods::NONE)
        );
        assert_eq!(
            route(Key::Enter, &ctrl(), false),
            Routed::App(Action::Done(crate::action::Finish::Save))
        );
    }

    #[test]
    fn modifiers_are_translated() {
        let m = Modifiers { shift: true, alt: true, ..Modifiers::COMMAND };
        assert_eq!(session_mods(&m), SMods { shift: true, ctrl: true, alt: true });
        assert_eq!(session_mods(&Modifiers::NONE), SMods::NONE);
        let c = chord_of(Key::Z, &m);
        assert!(c.ctrl && c.shift && c.alt);
    }

    #[test]
    fn every_table_entry_routes_back_to_itself() {
        for s in SHORTCUTS.iter() {
            let m = Modifiers {
                alt: s.chord.alt,
                ctrl: false,
                shift: s.chord.shift,
                mac_cmd: false,
                command: s.chord.ctrl,
            };
            assert_eq!(
                route(s.chord.key, &m, false),
                Routed::App(s.action.clone()),
                "{}",
                s.chord.display()
            );
        }
    }

    #[test]
    fn session_keys_cover_ctrl_letters_only_with_ctrl() {
        assert_eq!(session_key(Key::G, true), Some(SKey::Char('g')));
        assert_eq!(session_key(Key::G, false), None);
        assert_eq!(session_key(Key::F5, true), None);
    }

    #[test]
    fn every_cursor_hint_maps() {
        for h in [
            CursorHint::Default,
            CursorHint::Crosshair,
            CursorHint::Text,
            CursorHint::Move,
            CursorHint::Grabbing,
            CursorHint::ResizeNs,
            CursorHint::ResizeEw,
            CursorHint::ResizeNwse,
            CursorHint::ResizeNesw,
            CursorHint::Rotate,
            CursorHint::Eraser,
            CursorHint::NotAllowed,
        ] {
            let _ = cursor_icon(h);
        }
        assert_eq!(cursor_icon(CursorHint::ResizeNwse), CursorIcon::ResizeNwSe);
    }
}
