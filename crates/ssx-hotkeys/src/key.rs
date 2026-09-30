//! The portable key vocabulary and its per-desktop spellings.
//!
//! Every desktop names keys differently: sway, Hyprland, GNOME and the XDG portal all use
//! XKB keysym names (`Print`, `Return`, `grave`, `XF86AudioPlay`), KDE uses Qt key names
//! (`Print`, `Return`, `` ` ``, `Media Play`), and `global-hotkey` uses W3C `Code`s. Instead
//! of translating between them pairwise, each [`Key`] carries all of its spellings in one
//! table, so adding a key is one row and a golden test catches mistakes.
//!
//! The XKB names in the table are validated against a real compositor in the test-suite
//! (headless sway rejects a `bindsym` with an unknown keysym), so they are not guesses.

/// A non-alphanumeric, non-function key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Named {
    Space,
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    Print,
    Pause,
    ScrollLock,
    Menu,
    Minus,
    Equal,
    Comma,
    Period,
    Slash,
    Backslash,
    Semicolon,
    Quote,
    Backquote,
    BracketLeft,
    BracketRight,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    NumpadEnter,
    PlayPause,
    MediaStop,
    MediaNext,
    MediaPrevious,
    VolumeUp,
    VolumeDown,
    VolumeMute,
    BrightnessUp,
    BrightnessDown,
}

/// One row of the table: every spelling of a [`Named`] key.
struct NamedDef {
    key: Named,
    /// Canonical portable name, used when formatting.
    name: &'static str,
    /// Extra accepted spellings when parsing (matched case-insensitively).
    aliases: &'static [&'static str],
    /// XKB keysym name (sway, Hyprland, GNOME, XDG portal).
    xkb: &'static str,
    /// Qt key name for KDE, if KDE can express it.
    qt: Option<&'static str>,
    /// May be used without a modifier (never a key people type text with).
    bare_ok: bool,
}

const fn def(
    key: Named,
    name: &'static str,
    aliases: &'static [&'static str],
    xkb: &'static str,
    qt: Option<&'static str>,
    bare_ok: bool,
) -> NamedDef {
    NamedDef { key, name, aliases, xkb, qt, bare_ok }
}

#[rustfmt::skip]
static NAMED: &[NamedDef] = &[
    def(Named::Space,          "Space",      &[],          "space",                 Some("Space"), false),
    def(Named::Enter,          "Enter",      &["Return"],          "Return",                Some("Return"), false),
    def(Named::Tab,            "Tab",        &[],            "Tab",                   Some("Tab"), false),
    def(Named::Escape,         "Escape",     &["Esc"],         "Escape",                Some("Esc"), false),
    def(Named::Backspace,      "Backspace",  &["BS"],      "BackSpace",             Some("Backspace"), false),
    def(Named::Delete,         "Delete",     &["Del"],         "Delete",                Some("Del"), false),
    def(Named::Insert,         "Insert",     &["Ins"],         "Insert",                Some("Ins"), false),
    def(Named::Home,           "Home",       &[],           "Home",                  Some("Home"), false),
    def(Named::End,            "End",        &[],            "End",                   Some("End"), false),
    def(Named::PageUp,         "PageUp",     &["PgUp", "Prior"],         "Prior",                 Some("PgUp"), false),
    def(Named::PageDown,       "PageDown",   &["PgDn", "PgDown", "Next"],       "Next",                  Some("PgDown"), false),
    def(Named::Up,             "Up",         &["ArrowUp"],        "Up",                    Some("Up"), false),
    def(Named::Down,           "Down",       &["ArrowDown"],      "Down",                  Some("Down"), false),
    def(Named::Left,           "Left",       &["ArrowLeft"],      "Left",                  Some("Left"), false),
    def(Named::Right,          "Right",      &["ArrowRight"],     "Right",                 Some("Right"), false),
    def(Named::Print,          "Print",      &["PrintScreen", "PrtSc", "PrtScr"],    "Print",                 Some("Print"), true),
    def(Named::Pause,          "Pause",      &["Break"],          "Pause",                 Some("Pause"), true),
    def(Named::ScrollLock,     "ScrollLock", &["Scroll_Lock"],     "Scroll_Lock",           Some("ScrollLock"), true),
    def(Named::Menu,           "Menu",       &["ContextMenu", "Apps"],    "Menu",                  Some("Menu"), true),
    def(Named::Minus,          "Minus",      &["-", "Hyphen"],          "minus",                 Some("-"), false),
    def(Named::Equal,          "Equal",      &["=", "Equals"],          "equal",                 Some("="), false),
    def(Named::Comma,          "Comma",      &[","],          "comma",                 Some(","), false),
    def(Named::Period,         "Period",     &[".", "Dot"],         "period",                Some("."), false),
    def(Named::Slash,          "Slash",      &["/"],          "slash",                 Some("/"), false),
    def(Named::Backslash,      "Backslash",  &["\\"],      "backslash",             Some("\\"), false),
    def(Named::Semicolon,      "Semicolon",  &[";"],      "semicolon",             Some(";"), false),
    def(Named::Quote,          "Quote",      &["'", "Apostrophe"],          "apostrophe",            Some("'"), false),
    def(Named::Backquote,      "Backquote",  &["`", "Grave", "Backtick"],      "grave",                 Some("`"), false),
    def(Named::BracketLeft,    "BracketLeft",  &["["],    "bracketleft",           Some("["), false),
    def(Named::BracketRight,   "BracketRight", &["]"],   "bracketright",          Some("]"), false),
    def(Named::NumpadAdd,      "NumpadAdd",      &["KpAdd"],      "KP_Add",                None, false),
    def(Named::NumpadSubtract, "NumpadSubtract", &["KpSubtract"], "KP_Subtract",           None, false),
    def(Named::NumpadMultiply, "NumpadMultiply", &["KpMultiply"], "KP_Multiply",           None, false),
    def(Named::NumpadDivide,   "NumpadDivide",   &["KpDivide"],   "KP_Divide",             None, false),
    def(Named::NumpadDecimal,  "NumpadDecimal",  &["KpDecimal"],  "KP_Decimal",            None, false),
    def(Named::NumpadEnter,    "NumpadEnter",    &["KpEnter"],    "KP_Enter",              None, false),
    def(Named::PlayPause,      "PlayPause",  &["MediaPlayPause", "MediaPlay"], "XF86AudioPlay",         Some("Media Play"), true),
    def(Named::MediaStop,      "MediaStop",  &["Stop"],      "XF86AudioStop",         Some("Media Stop"), true),
    def(Named::MediaNext,      "MediaNext",  &["MediaNextTrack", "NextTrack"], "XF86AudioNext",         Some("Media Next"), true),
    def(Named::MediaPrevious,  "MediaPrevious", &["MediaPrev", "MediaPrevTrack", "PrevTrack"], "XF86AudioPrev", Some("Media Previous"), true),
    def(Named::VolumeUp,       "VolumeUp",   &["AudioVolumeUp", "AudioRaiseVolume"], "XF86AudioRaiseVolume",  Some("Volume Up"), true),
    def(Named::VolumeDown,     "VolumeDown", &["AudioVolumeDown", "AudioLowerVolume"], "XF86AudioLowerVolume", Some("Volume Down"), true),
    def(Named::VolumeMute,     "VolumeMute", &["Mute", "AudioMute", "AudioVolumeMute"], "XF86AudioMute",   Some("Volume Mute"), true),
    def(Named::BrightnessUp,   "BrightnessUp",   &["MonBrightnessUp"],   "XF86MonBrightnessUp",   Some("Monitor Brightness Up"), true),
    def(Named::BrightnessDown, "BrightnessDown", &["MonBrightnessDown"], "XF86MonBrightnessDown", Some("Monitor Brightness Down"), true),
];

impl Named {
    fn def(self) -> &'static NamedDef {
        // Every variant has a row (checked by a unit test); the fallback keeps this
        // panic-free should a future variant be added without one.
        NAMED.iter().find(|d| d.key == self).unwrap_or(&NAMED[0])
    }

    /// All named keys, for exhaustive tests and pickers.
    pub fn all() -> impl Iterator<Item = Named> {
        NAMED.iter().map(|d| d.key)
    }
}

/// A single non-modifier key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    /// `A`..=`Z` (stored uppercase).
    Letter(char),
    /// `0`..=`9` on the main row.
    Digit(u8),
    /// `F1`..=`F24`.
    F(u8),
    /// `Numpad0`..=`Numpad9`.
    Numpad(u8),
    /// Everything else.
    Named(Named),
}

impl Key {
    /// Highest function key accepted.
    pub const MAX_F: u8 = 24;

    /// Parses a key name, case-insensitively (`s`, `F5`, `Print`, `PgUp`, `-`, `Numpad3`).
    pub fn parse(s: &str) -> Option<Key> {
        let mut chars = s.chars();
        let first = chars.next()?;
        if chars.next().is_none() {
            if first.is_ascii_alphabetic() {
                return Some(Key::Letter(first.to_ascii_uppercase()));
            }
            if let Some(d) = first.to_digit(10) {
                return Some(Key::Digit(d as u8));
            }
        }
        let lower = s.to_ascii_lowercase();
        if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            if (1..=Self::MAX_F).contains(&n) && !lower[1..].starts_with('+') {
                return Some(Key::F(n));
            }
        }
        for prefix in ["numpad", "kp", "num"] {
            if let Some(n) = lower.strip_prefix(prefix).and_then(|n| n.parse::<u8>().ok()) {
                if n <= 9 && lower.len() == prefix.len() + 1 {
                    return Some(Key::Numpad(n));
                }
            }
        }
        NAMED
            .iter()
            .find(|d| {
                d.name.eq_ignore_ascii_case(s) || d.aliases.iter().any(|a| a.eq_ignore_ascii_case(s))
            })
            .map(|d| Key::Named(d.key))
    }

    /// Parses an XKB keysym name as used by sway/Hyprland/GNOME configs
    /// (case-insensitive; also accepts the portable names).
    pub fn from_xkb(name: &str) -> Option<Key> {
        if let Some(d) = NAMED.iter().find(|d| d.xkb.eq_ignore_ascii_case(name)) {
            return Some(Key::Named(d.key));
        }
        if let Some(n) = name.to_ascii_lowercase().strip_prefix("kp_").and_then(|n| n.parse::<u8>().ok())
        {
            if n <= 9 {
                return Some(Key::Numpad(n));
            }
        }
        match name {
            "Page_Up" => return Some(Key::Named(Named::PageUp)),
            "Page_Down" => return Some(Key::Named(Named::PageDown)),
            _ => {}
        }
        Key::parse(name)
    }

    /// Canonical portable name (what `Display` for a chord prints).
    pub fn name(self) -> String {
        match self {
            Key::Letter(c) => c.to_string(),
            Key::Digit(d) => d.to_string(),
            Key::F(n) => format!("F{n}"),
            Key::Numpad(n) => format!("Numpad{n}"),
            Key::Named(k) => k.def().name.to_owned(),
        }
    }

    /// XKB keysym name (`s`, `Print`, `XF86AudioPlay`).
    pub fn xkb_name(self) -> String {
        match self {
            Key::Letter(c) => c.to_ascii_lowercase().to_string(),
            Key::Digit(d) => d.to_string(),
            Key::F(n) => format!("F{n}"),
            Key::Numpad(n) => format!("KP_{n}"),
            Key::Named(k) => k.def().xkb.to_owned(),
        }
    }

    /// Qt key name for KDE, or `None` if KDE has no portable spelling (keypad keys).
    pub fn qt_name(self) -> Option<String> {
        match self {
            Key::Letter(c) => Some(c.to_string()),
            Key::Digit(d) => Some(d.to_string()),
            Key::F(n) => Some(format!("F{n}")),
            Key::Numpad(_) => None,
            Key::Named(k) => k.def().qt.map(str::to_owned),
        }
    }

    /// May the key be a hotkey on its own? Typing keys may not: a global `S` would make
    /// the letter unusable in every application.
    pub fn allows_bare(self) -> bool {
        match self {
            Key::F(_) => true,
            Key::Named(k) => k.def().bare_ok,
            _ => false,
        }
    }

    /// Every key, for exhaustive tests.
    pub fn all() -> impl Iterator<Item = Key> {
        ('A'..='Z')
            .map(Key::Letter)
            .chain((0..=9).map(Key::Digit))
            .chain((1..=Self::MAX_F).map(Key::F))
            .chain((0..=9).map(Key::Numpad))
            .chain(Named::all().map(Key::Named))
    }
}

#[cfg(any(windows, target_os = "macos", all(unix, not(target_vendor = "apple"))))]
mod code_map {
    use global_hotkey::hotkey::Code;

    use super::{Key, Named};

    impl Key {
        /// The W3C `Code` that `global-hotkey` registers.
        pub fn code(self) -> Code {
            match self {
                Key::Letter(c) => match c {
                    'A' => Code::KeyA,
                    'B' => Code::KeyB,
                    'C' => Code::KeyC,
                    'D' => Code::KeyD,
                    'E' => Code::KeyE,
                    'F' => Code::KeyF,
                    'G' => Code::KeyG,
                    'H' => Code::KeyH,
                    'I' => Code::KeyI,
                    'J' => Code::KeyJ,
                    'K' => Code::KeyK,
                    'L' => Code::KeyL,
                    'M' => Code::KeyM,
                    'N' => Code::KeyN,
                    'O' => Code::KeyO,
                    'P' => Code::KeyP,
                    'Q' => Code::KeyQ,
                    'R' => Code::KeyR,
                    'S' => Code::KeyS,
                    'T' => Code::KeyT,
                    'U' => Code::KeyU,
                    'V' => Code::KeyV,
                    'W' => Code::KeyW,
                    'X' => Code::KeyX,
                    'Y' => Code::KeyY,
                    _ => Code::KeyZ,
                },
                Key::Digit(d) => [
                    Code::Digit0,
                    Code::Digit1,
                    Code::Digit2,
                    Code::Digit3,
                    Code::Digit4,
                    Code::Digit5,
                    Code::Digit6,
                    Code::Digit7,
                    Code::Digit8,
                    Code::Digit9,
                ][usize::from(d.min(9))],
                Key::F(n) => [
                    Code::F1,
                    Code::F2,
                    Code::F3,
                    Code::F4,
                    Code::F5,
                    Code::F6,
                    Code::F7,
                    Code::F8,
                    Code::F9,
                    Code::F10,
                    Code::F11,
                    Code::F12,
                    Code::F13,
                    Code::F14,
                    Code::F15,
                    Code::F16,
                    Code::F17,
                    Code::F18,
                    Code::F19,
                    Code::F20,
                    Code::F21,
                    Code::F22,
                    Code::F23,
                    Code::F24,
                ][usize::from(n.clamp(1, 24)) - 1],
                Key::Numpad(n) => [
                    Code::Numpad0,
                    Code::Numpad1,
                    Code::Numpad2,
                    Code::Numpad3,
                    Code::Numpad4,
                    Code::Numpad5,
                    Code::Numpad6,
                    Code::Numpad7,
                    Code::Numpad8,
                    Code::Numpad9,
                ][usize::from(n.min(9))],
                Key::Named(k) => match k {
                    Named::Space => Code::Space,
                    Named::Enter => Code::Enter,
                    Named::Tab => Code::Tab,
                    Named::Escape => Code::Escape,
                    Named::Backspace => Code::Backspace,
                    Named::Delete => Code::Delete,
                    Named::Insert => Code::Insert,
                    Named::Home => Code::Home,
                    Named::End => Code::End,
                    Named::PageUp => Code::PageUp,
                    Named::PageDown => Code::PageDown,
                    Named::Up => Code::ArrowUp,
                    Named::Down => Code::ArrowDown,
                    Named::Left => Code::ArrowLeft,
                    Named::Right => Code::ArrowRight,
                    Named::Print => Code::PrintScreen,
                    Named::Pause => Code::Pause,
                    Named::ScrollLock => Code::ScrollLock,
                    Named::Menu => Code::ContextMenu,
                    Named::Minus => Code::Minus,
                    Named::Equal => Code::Equal,
                    Named::Comma => Code::Comma,
                    Named::Period => Code::Period,
                    Named::Slash => Code::Slash,
                    Named::Backslash => Code::Backslash,
                    Named::Semicolon => Code::Semicolon,
                    Named::Quote => Code::Quote,
                    Named::Backquote => Code::Backquote,
                    Named::BracketLeft => Code::BracketLeft,
                    Named::BracketRight => Code::BracketRight,
                    Named::NumpadAdd => Code::NumpadAdd,
                    Named::NumpadSubtract => Code::NumpadSubtract,
                    Named::NumpadMultiply => Code::NumpadMultiply,
                    Named::NumpadDivide => Code::NumpadDivide,
                    Named::NumpadDecimal => Code::NumpadDecimal,
                    Named::NumpadEnter => Code::NumpadEnter,
                    Named::PlayPause => Code::MediaPlayPause,
                    Named::MediaStop => Code::MediaStop,
                    Named::MediaNext => Code::MediaTrackNext,
                    Named::MediaPrevious => Code::MediaTrackPrevious,
                    Named::VolumeUp => Code::AudioVolumeUp,
                    Named::VolumeDown => Code::AudioVolumeDown,
                    Named::VolumeMute => Code::AudioVolumeMute,
                    Named::BrightnessUp => Code::BrightnessUp,
                    Named::BrightnessDown => Code::BrightnessDown,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_variant_has_a_row_and_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for d in NAMED {
            assert!(seen.insert(d.key), "duplicate row for {:?}", d.key);
        }
        assert_eq!(NAMED.len(), 45, "update this count when adding a key");
        let mut spellings = std::collections::HashSet::new();
        for d in NAMED {
            for s in std::iter::once(&d.name).chain(d.aliases) {
                assert!(spellings.insert(s.to_ascii_lowercase()), "spelling {s:?} used twice");
            }
        }
    }

    #[test]
    fn canonical_names_round_trip_for_every_key() {
        for k in Key::all() {
            assert_eq!(Key::parse(&k.name()), Some(k), "{}", k.name());
            assert_eq!(Key::from_xkb(&k.xkb_name()), Some(k), "xkb {}", k.xkb_name());
        }
    }

    #[test]
    fn parsing_is_case_insensitive_and_accepts_aliases() {
        assert_eq!(Key::parse("s"), Some(Key::Letter('S')));
        assert_eq!(Key::parse("print"), Some(Key::Named(Named::Print)));
        assert_eq!(Key::parse("PRTSC"), Some(Key::Named(Named::Print)));
        assert_eq!(Key::parse("pgup"), Some(Key::Named(Named::PageUp)));
        assert_eq!(Key::parse("f12"), Some(Key::F(12)));
        assert_eq!(Key::parse("F24"), Some(Key::F(24)));
        assert_eq!(Key::parse("F25"), None);
        assert_eq!(Key::parse("F0"), None);
        assert_eq!(Key::parse("numpad5"), Some(Key::Numpad(5)));
        assert_eq!(Key::parse("-"), Some(Key::Named(Named::Minus)));
        assert_eq!(Key::parse("`"), Some(Key::Named(Named::Backquote)));
        assert_eq!(Key::parse("MediaPlayPause"), Some(Key::Named(Named::PlayPause)));
        assert_eq!(Key::parse(""), None);
        assert_eq!(Key::parse("nonsense"), None);
        assert_eq!(Key::parse("é"), None);
        assert_eq!(Key::parse("f+1"), None);
    }

    #[test]
    fn xkb_names_for_common_keys() {
        assert_eq!(Key::Letter('S').xkb_name(), "s");
        assert_eq!(Key::Named(Named::Print).xkb_name(), "Print");
        assert_eq!(Key::Named(Named::Enter).xkb_name(), "Return");
        assert_eq!(Key::Named(Named::Backquote).xkb_name(), "grave");
        assert_eq!(Key::Named(Named::PlayPause).xkb_name(), "XF86AudioPlay");
        assert_eq!(Key::Numpad(3).xkb_name(), "KP_3");
        assert_eq!(Key::from_xkb("Page_Up"), Some(Key::Named(Named::PageUp)));
        assert_eq!(Key::from_xkb("RETURN"), Some(Key::Named(Named::Enter)));
    }

    #[test]
    fn qt_names_and_missing_keypad_support() {
        assert_eq!(Key::Named(Named::PlayPause).qt_name().as_deref(), Some("Media Play"));
        assert_eq!(Key::Named(Named::PageDown).qt_name().as_deref(), Some("PgDown"));
        assert_eq!(Key::Numpad(1).qt_name(), None);
        assert_eq!(Key::Letter('Q').qt_name().as_deref(), Some("Q"));
    }

    #[test]
    fn bare_key_policy() {
        assert!(Key::F(5).allows_bare());
        assert!(Key::Named(Named::Print).allows_bare());
        assert!(Key::Named(Named::VolumeMute).allows_bare());
        assert!(!Key::Letter('S').allows_bare());
        assert!(!Key::Digit(1).allows_bare());
        assert!(!Key::Named(Named::Space).allows_bare());
        assert!(!Key::Named(Named::Escape).allows_bare());
        assert!(!Key::Numpad(0).allows_bare());
    }
}
