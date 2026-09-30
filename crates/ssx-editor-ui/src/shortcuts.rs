//! The keyboard shortcut table: one place that both the key handler and the in-app cheat sheet
//! read, so the two can never disagree.
//!
//! Conventions follow `ShareX` and the common editor habits: `Ctrl+Z/Y` undo/redo, `Ctrl+C/V`
//! copy/paste, `Ctrl+S` save, `Ctrl+O` open, `Ctrl+D` duplicate; single letters pick tools.
//! `Ctrl` means `Cmd` on macOS (egui's `command` modifier).

use std::sync::LazyLock;

use egui::Key;
use ssx_editor::object::Orient;

use crate::{
    action::{Action, DialogKind, Finish},
    tools::ToolId,
};

/// A key plus modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    /// The key.
    pub key: Key,
    /// Ctrl (Cmd on macOS).
    pub ctrl: bool,
    /// Shift.
    pub shift: bool,
    /// Alt.
    pub alt: bool,
}

impl Chord {
    /// A bare key.
    pub const fn key(key: Key) -> Self {
        Self { key, ctrl: false, shift: false, alt: false }
    }

    /// Ctrl + key.
    pub const fn ctrl(key: Key) -> Self {
        Self { key, ctrl: true, shift: false, alt: false }
    }

    /// Shift + key.
    pub const fn shift(key: Key) -> Self {
        Self { key, ctrl: false, shift: true, alt: false }
    }

    /// Alt + key.
    pub const fn alt(key: Key) -> Self {
        Self { key, ctrl: false, shift: false, alt: true }
    }

    /// Ctrl + Shift + key.
    pub const fn ctrl_shift(key: Key) -> Self {
        Self { key, ctrl: true, shift: true, alt: false }
    }

    /// Ctrl + Alt + key.
    pub const fn ctrl_alt(key: Key) -> Self {
        Self { key, ctrl: true, shift: false, alt: true }
    }

    /// Human-readable form for tooltips and the cheat sheet, e.g. `Ctrl+Shift+S`.
    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str(if cfg!(target_os = "macos") { "Cmd+" } else { "Ctrl+" });
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(key_label(self.key));
        s
    }
}

fn key_label(k: Key) -> &'static str {
    match k {
        Key::Plus => "+",
        Key::Minus => "-",
        Key::Equals => "=",
        Key::OpenBracket => "[",
        Key::CloseBracket => "]",
        Key::Quote => "'",
        Key::Comma => ",",
        Key::Period => ".",
        Key::Delete => "Del",
        Key::Backspace => "Backspace",
        Key::Escape => "Esc",
        Key::Enter => "Enter",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::PageUp => "PgUp",
        Key::PageDown => "PgDn",
        other => other.name(),
    }
}

/// One row of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct Shortcut {
    /// The key combination.
    pub chord: Chord,
    /// What it does.
    pub action: Action,
    /// Section in the cheat sheet.
    pub category: &'static str,
    /// Description in the cheat sheet.
    pub label: &'static str,
}

fn sc(chord: Chord, action: Action, category: &'static str, label: &'static str) -> Shortcut {
    Shortcut { chord, action, category, label }
}

/// The full table.
pub static SHORTCUTS: LazyLock<Vec<Shortcut>> = LazyLock::new(build);

fn tool(chord: Chord, t: ToolId) -> Shortcut {
    sc(chord, Action::SetTool(t), "Tools", t.label())
}

fn build() -> Vec<Shortcut> {
    use Key as K;
    let mut v = vec![
        // File
        sc(Chord::ctrl(K::N), Action::NewFromClipboard, "File", "New from clipboard"),
        sc(Chord::ctrl(K::O), Action::Open, "File", "Open image or project"),
        sc(Chord::ctrl(K::S), Action::Save, "File", "Save"),
        sc(Chord::ctrl_shift(K::S), Action::SaveAs, "File", "Save as..."),
        sc(Chord::ctrl_alt(K::S), Action::SaveProject, "File", "Save editable project (.ssxe)"),
        sc(Chord::ctrl_shift(K::C), Action::CopyImage, "File", "Copy image to clipboard"),
        sc(Chord::ctrl_shift(K::U), Action::Upload, "File", "Upload"),
        sc(Chord::ctrl(K::Enter), Action::Done(Finish::Save), "File", "Done (save and close)"),
        sc(Chord::ctrl(K::Q), Action::RequestClose, "File", "Close editor"),
        // Edit
        sc(Chord::ctrl(K::Z), Action::Undo, "Edit", "Undo"),
        sc(Chord::ctrl(K::Y), Action::Redo, "Edit", "Redo"),
        sc(Chord::ctrl_shift(K::Z), Action::Redo, "Edit", "Redo (alternative)"),
        sc(Chord::ctrl(K::A), Action::SelectAll, "Edit", "Select all objects"),
        sc(Chord::ctrl(K::D), Action::Duplicate, "Edit", "Duplicate selection"),
        sc(
            Chord::ctrl(K::C),
            Action::Copy,
            "Edit",
            "Copy selection (or the image if nothing is selected)",
        ),
        sc(Chord::ctrl(K::X), Action::Cut, "Edit", "Cut selection"),
        sc(Chord::ctrl(K::V), Action::Paste, "Edit", "Paste objects or clipboard image"),
        sc(Chord::ctrl_shift(K::V), Action::PasteImage, "Edit", "Paste clipboard image as object"),
        sc(Chord::key(K::Delete), Action::DeleteSelection, "Edit", "Delete selection"),
        sc(
            Chord::key(K::Backspace),
            Action::DeleteSelection,
            "Edit",
            "Delete selection (alternative)",
        ),
        sc(Chord::ctrl(K::G), Action::Group, "Edit", "Group"),
        sc(Chord::ctrl_shift(K::G), Action::Ungroup, "Edit", "Ungroup"),
        sc(Chord::key(K::CloseBracket), Action::Raise, "Edit", "Bring forward"),
        sc(Chord::key(K::OpenBracket), Action::Lower, "Edit", "Send backward"),
        sc(Chord::ctrl(K::CloseBracket), Action::BringToFront, "Edit", "Bring to front"),
        sc(Chord::ctrl(K::OpenBracket), Action::SendToBack, "Edit", "Send to back"),
        // View
        sc(Chord::ctrl(K::Num0), Action::ZoomFit, "View", "Fit to window"),
        sc(Chord::ctrl(K::Num1), Action::ZoomActual, "View", "Actual size (100 %)"),
        sc(Chord::ctrl(K::Equals), Action::ZoomIn, "View", "Zoom in"),
        sc(Chord::ctrl(K::Plus), Action::ZoomIn, "View", "Zoom in (alternative)"),
        sc(Chord::ctrl_shift(K::Equals), Action::ZoomIn, "View", "Zoom in (alternative)"),
        sc(Chord::ctrl(K::Minus), Action::ZoomOut, "View", "Zoom out"),
        sc(Chord::ctrl(K::Quote), Action::TogglePixelGrid, "View", "Toggle pixel grid"),
        sc(Chord::ctrl(K::L), Action::ToggleLayers, "View", "Show or hide the object list"),
        sc(
            Chord::key(K::F1),
            Action::OpenDialog(DialogKind::Shortcuts),
            "View",
            "Keyboard shortcuts",
        ),
        sc(Chord::ctrl(K::Comma), Action::OpenDialog(DialogKind::Settings), "View", "Settings"),
        // Image
        sc(
            Chord::ctrl_shift(K::R),
            Action::Orient(Orient::Rotate90),
            "Image",
            "Rotate 90 degrees clockwise",
        ),
        sc(
            Chord::ctrl_alt(K::R),
            Action::Orient(Orient::Rotate270),
            "Image",
            "Rotate 90 degrees counter-clockwise",
        ),
        sc(Chord::ctrl_shift(K::H), Action::Orient(Orient::FlipH), "Image", "Flip horizontally"),
        sc(Chord::ctrl_shift(K::J), Action::Orient(Orient::FlipV), "Image", "Flip vertically"),
        sc(
            Chord::ctrl_shift(K::I),
            Action::OpenDialog(DialogKind::Resize),
            "Image",
            "Resize image...",
        ),
        sc(
            Chord::ctrl_shift(K::K),
            Action::OpenDialog(DialogKind::Canvas),
            "Image",
            "Canvas size...",
        ),
        sc(
            Chord::ctrl_shift(K::X),
            Action::OpenDialog(DialogKind::Crop),
            "Image",
            "Crop numerically...",
        ),
        sc(Chord::ctrl_shift(K::E), Action::AutoCrop(8), "Image", "Auto-crop borders"),
    ];
    v.extend([
        tool(Chord::key(K::V), ToolId::Select),
        tool(Chord::key(K::C), ToolId::CropRect),
        tool(Chord::shift(K::C), ToolId::CropEllipse),
        tool(Chord::alt(K::C), ToolId::CropFree),
        tool(Chord::key(K::R), ToolId::Rectangle),
        tool(Chord::key(K::E), ToolId::Ellipse),
        tool(Chord::key(K::P), ToolId::Freehand),
        tool(Chord::key(K::L), ToolId::Line),
        tool(Chord::key(K::A), ToolId::Arrow),
        tool(Chord::shift(K::A), ToolId::FreehandArrow),
        tool(Chord::key(K::T), ToolId::Text),
        tool(Chord::shift(K::T), ToolId::TextBoxed),
        tool(Chord::key(K::B), ToolId::Balloon),
        tool(Chord::key(K::N), ToolId::Step),
        tool(Chord::key(K::M), ToolId::Magnify),
        tool(Chord::key(K::S), ToolId::Spotlight),
        tool(Chord::key(K::I), ToolId::Image),
        tool(Chord::key(K::J), ToolId::Sticker),
        tool(Chord::key(K::K), ToolId::Cursor),
        tool(Chord::key(K::X), ToolId::Eraser),
        tool(Chord::key(K::U), ToolId::Blur),
        tool(Chord::shift(K::U), ToolId::Pixelate),
        tool(Chord::key(K::G), ToolId::Grid),
        tool(Chord::key(K::H), ToolId::Highlight),
        tool(Chord::shift(K::H), ToolId::HighlightPen),
    ]);
    v
}

/// Mouse and modifier tricks that are not key chords: `(gesture, effect)`.
pub const GESTURES: &[(&str, &str)] = &[
    ("Mouse wheel", "Zoom about the pointer"),
    ("Ctrl+wheel / pinch", "Zoom about the pointer"),
    ("Shift+wheel", "Scroll horizontally"),
    ("Space+drag / middle-drag", "Pan the image"),
    ("Shift while drawing", "Constrain: square, circle, 45 degree lines"),
    ("Alt while drawing", "Draw from the centre"),
    ("Ctrl while drawing", "Snap to edges and centres"),
    ("Shift+click", "Add to the selection"),
    ("Arrow keys / Shift+arrows", "Nudge the selection by 1 / 10 px"),
    ("Double-click text", "Edit the text"),
    ("Enter / Esc", "Apply / cancel a crop, commit / leave text"),
    ("Drop a file on the window", "Open it (hold Shift to insert it as an image)"),
];

/// The table grouped by category, in a stable order, for the cheat sheet.
pub fn grouped() -> Vec<(&'static str, Vec<&'static Shortcut>)> {
    let mut out: Vec<(&'static str, Vec<&'static Shortcut>)> = Vec::new();
    for s in SHORTCUTS.iter() {
        match out.iter_mut().find(|(c, _)| *c == s.category) {
            Some((_, list)) => list.push(s),
            None => out.push((s.category, vec![s])),
        }
    }
    out
}

/// The first (primary) shortcut of an action, e.g. for tooltips.
pub fn primary_for(action: &Action) -> Option<Chord> {
    SHORTCUTS.iter().find(|s| &s.action == action).map(|s| s.chord)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn chords_are_unique() {
        let mut seen = HashSet::new();
        for s in SHORTCUTS.iter() {
            assert!(seen.insert(s.chord), "{} is bound twice", s.chord.display());
        }
    }

    #[test]
    fn every_toolbar_tool_has_a_letter() {
        for t in ToolId::ALL {
            if t == ToolId::CutOut {
                continue;
            }
            let c = primary_for(&Action::SetTool(t)).unwrap_or_else(|| panic!("{t:?}"));
            assert!(!c.ctrl, "tool keys must not need Ctrl");
        }
    }

    #[test]
    fn documented_conventions_hold() {
        let chord = |a: Action| primary_for(&a).map(|c| c.display());
        let ctrl = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };
        assert_eq!(chord(Action::Undo), Some(format!("{ctrl}+Z")));
        assert_eq!(chord(Action::Redo), Some(format!("{ctrl}+Y")));
        assert_eq!(chord(Action::Save), Some(format!("{ctrl}+S")));
        assert_eq!(chord(Action::SaveAs), Some(format!("{ctrl}+Shift+S")));
        assert_eq!(chord(Action::Open), Some(format!("{ctrl}+O")));
        assert_eq!(chord(Action::Duplicate), Some(format!("{ctrl}+D")));
        assert_eq!(chord(Action::DeleteSelection), Some("Del".into()));
        assert_eq!(chord(Action::ZoomFit), Some(format!("{ctrl}+0")));
        assert_eq!(chord(Action::ZoomActual), Some(format!("{ctrl}+1")));
    }

    #[test]
    fn grouping_keeps_every_row_once() {
        let g = grouped();
        let n: usize = g.iter().map(|(_, l)| l.len()).sum();
        assert_eq!(n, SHORTCUTS.len());
        let cats: Vec<_> = g.iter().map(|(c, _)| *c).collect();
        assert_eq!(cats, ["File", "Edit", "View", "Image", "Tools"]);
    }

    #[test]
    fn labels_are_filled_in() {
        for s in SHORTCUTS.iter() {
            assert!(!s.label.is_empty());
            assert!(!s.chord.display().is_empty());
        }
        for (g, e) in GESTURES {
            assert!(!g.is_empty() && !e.is_empty());
        }
    }
}
