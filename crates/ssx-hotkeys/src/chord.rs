//! [`Chord`]: a validated modifier set plus one key, with a stable text form.
//!
//! The text form is what users write in settings files and what is shown in the UI:
//! `Ctrl+Shift+S`, `Print`, `Super+Alt+R`. Parsing is deliberately forgiving (case,
//! spaces around `+`, common aliases like `Control`, `Win`, `Cmd`, `PgUp`) while formatting
//! is canonical, so `parse(format(c)) == c` and `format(parse(s))` is a normalisation.

use std::{fmt, str::FromStr};

use crate::key::Key;

/// Modifier keys. A tiny bit-set (no dependency for four flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Modifiers = Modifiers(0);
    /// Control.
    pub const CTRL: Modifiers = Modifiers(1);
    /// Alt (Option on macOS).
    pub const ALT: Modifiers = Modifiers(2);
    /// Shift.
    pub const SHIFT: Modifiers = Modifiers(4);
    /// The logo key: Windows key, Command on macOS, "Meta"/"Super" on Linux.
    pub const SUPER: Modifiers = Modifiers(8);

    /// Canonical order used for formatting: Ctrl, Alt, Shift, Super.
    const ORDER: [(Modifiers, &'static str); 4] = [
        (Modifiers::CTRL, "Ctrl"),
        (Modifiers::ALT, "Alt"),
        (Modifiers::SHIFT, "Shift"),
        (Modifiers::SUPER, "Super"),
    ];

    /// `true` if no modifier is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `true` if every flag in `other` is set.
    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union of two sets.
    pub const fn union(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 | other.0)
    }

    /// The set flags in canonical order.
    pub fn iter(self) -> impl Iterator<Item = Modifiers> {
        Self::ORDER.into_iter().map(|(m, _)| m).filter(move |m| self.contains(*m))
    }

    /// Parses one modifier name (case-insensitive, with aliases).
    pub fn parse_one(s: &str) -> Option<Modifiers> {
        match s.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => Some(Modifiers::CTRL),
            "alt" | "option" | "opt" | "mod1" => Some(Modifiers::ALT),
            "shift" => Some(Modifiers::SHIFT),
            "super" | "win" | "windows" | "meta" | "cmd" | "command" | "logo" | "mod4" => {
                Some(Modifiers::SUPER)
            }
            _ => None,
        }
    }

    /// Canonical name of a single-flag set.
    pub fn name(self) -> &'static str {
        Self::ORDER.iter().find(|(m, _)| *m == self).map_or("", |(_, n)| n)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        self.union(rhs)
    }
}

/// Why a chord string or combination was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChordError {
    /// The string was empty or only whitespace.
    #[error("empty shortcut; expected something like \"Ctrl+Shift+S\" or \"Print\"")]
    Empty,
    /// Two `+` in a row, or a leading/trailing `+`.
    #[error("empty part in shortcut {0:?}; parts are separated by a single '+'")]
    EmptyPart(String),
    /// A part before the last is not a modifier.
    #[error(
        "{0:?} is not a modifier (Ctrl, Alt, Shift, Super); a shortcut has exactly one non-modifier key, last"
    )]
    NotAModifier(String),
    /// The last part is not a known key.
    #[error(
        "unknown key {0:?}; use a letter, digit, F1-F24, or a name such as Print, Space, PageUp"
    )]
    UnknownKey(String),
    /// Only modifiers were given.
    #[error("shortcut {0:?} has no key, only modifiers")]
    MissingKey(String),
    /// The same modifier twice.
    #[error("modifier {0} given twice")]
    DuplicateModifier(&'static str),
    /// A typing key without a modifier would be unusable everywhere.
    #[error(
        "{0} needs at least one modifier (Ctrl, Alt, Shift or Super); a bare typing key cannot be a global shortcut"
    )]
    NeedsModifier(String),
}

/// A global shortcut: zero or more modifiers and exactly one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    modifiers: Modifiers,
    key: Key,
}

impl Chord {
    /// Builds a chord, rejecting bare typing keys (see [`Key::allows_bare`]).
    pub fn new(modifiers: Modifiers, key: Key) -> Result<Chord, ChordError> {
        if modifiers.is_empty() && !key.allows_bare() {
            return Err(ChordError::NeedsModifier(key.name()));
        }
        Ok(Chord { modifiers, key })
    }

    /// The modifier set.
    pub fn modifiers(&self) -> Modifiers {
        self.modifiers
    }

    /// The key.
    pub fn key(&self) -> Key {
        self.key
    }

    /// Canonical text form (same as `Display`).
    pub fn normalized(&self) -> String {
        self.to_string()
    }

    /// The `global-hotkey` crate's hotkey for this chord.
    #[cfg(any(windows, target_os = "macos", all(unix, not(target_vendor = "apple"))))]
    pub fn to_hotkey(&self) -> global_hotkey::hotkey::HotKey {
        use global_hotkey::hotkey::Modifiers as M;
        let mut m = M::empty();
        for flag in self.modifiers.iter() {
            m |= match flag {
                Modifiers::CTRL => M::CONTROL,
                Modifiers::ALT => M::ALT,
                Modifiers::SHIFT => M::SHIFT,
                _ => M::SUPER,
            };
        }
        global_hotkey::hotkey::HotKey::new((!m.is_empty()).then_some(m), self.key.code())
    }

    /// The chord in a target's syntax, joined by `sep`, with each modifier named by `modifier`.
    pub(crate) fn join_with(
        &self,
        sep: &str,
        modifier: impl Fn(Modifiers) -> String,
        key: String,
    ) -> String {
        let mut parts: Vec<String> = self.modifiers.iter().map(modifier).collect();
        parts.push(key);
        parts.join(sep)
    }

    /// XDG GlobalShortcuts `preferred_trigger`: `CTRL+SHIFT+s`, `LOGO+ALT+r`.
    pub fn to_portal_trigger(&self) -> String {
        self.join_with(
            "+",
            |m| {
                match m {
                    Modifiers::CTRL => "CTRL",
                    Modifiers::ALT => "ALT",
                    Modifiers::SHIFT => "SHIFT",
                    _ => "LOGO",
                }
                .to_owned()
            },
            self.key.xkb_name(),
        )
    }

    /// GTK/GNOME accelerator: `<Control><Shift>s`.
    pub fn to_gnome_accelerator(&self) -> String {
        self.join_with(
            "",
            |m| {
                match m {
                    Modifiers::CTRL => "<Control>",
                    Modifiers::ALT => "<Alt>",
                    Modifiers::SHIFT => "<Shift>",
                    _ => "<Super>",
                }
                .to_owned()
            },
            self.key.xkb_name(),
        )
    }

    /// Qt key sequence for KDE (`Ctrl+Shift+S`, `Meta+Alt+R`); `None` when KDE has no name
    /// for the key.
    pub fn to_qt_sequence(&self) -> Option<String> {
        let key = self.key.qt_name()?;
        Some(self.join_with(
            "+",
            |m| {
                match m {
                    Modifiers::CTRL => "Ctrl",
                    Modifiers::ALT => "Alt",
                    Modifiers::SHIFT => "Shift",
                    _ => "Meta",
                }
                .to_owned()
            },
            key,
        ))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in self.modifiers.iter() {
            write!(f, "{}+", m.name())?;
        }
        f.write_str(&self.key.name())
    }
}

impl FromStr for Chord {
    type Err = ChordError;

    fn from_str(s: &str) -> Result<Chord, ChordError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ChordError::Empty);
        }
        // "Ctrl+Shift+S" and "Ctrl + Shift + S" both work. A key that *is* '+' is not
        // supported (write "Equal"): it is Shift+Equal on most layouts anyway.
        let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(ChordError::EmptyPart(trimmed.to_owned()));
        }
        let (last, mods) = parts.split_last().ok_or(ChordError::Empty)?;
        let mut modifiers = Modifiers::NONE;
        for part in mods {
            let Some(m) = Modifiers::parse_one(part) else {
                return Err(ChordError::NotAModifier((*part).to_owned()));
            };
            if modifiers.contains(m) {
                return Err(ChordError::DuplicateModifier(m.name()));
            }
            modifiers = modifiers | m;
        }
        let key = match Key::parse(last) {
            Some(k) => k,
            None if Modifiers::parse_one(last).is_some() => {
                return Err(ChordError::MissingKey(trimmed.to_owned()));
            }
            None => return Err(ChordError::UnknownKey((*last).to_owned())),
        };
        Chord::new(modifiers, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Named;

    fn c(s: &str) -> Chord {
        s.parse().unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn parses_the_documented_examples() {
        assert_eq!(c("Ctrl+Shift+S").modifiers(), Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(c("Ctrl+Shift+S").key(), Key::Letter('S'));
        assert_eq!(c("Print").key(), Key::Named(Named::Print));
        assert!(c("Print").modifiers().is_empty());
        assert_eq!(c("Super+Alt+R").modifiers(), Modifiers::SUPER | Modifiers::ALT);
    }

    #[test]
    fn parsing_is_forgiving_and_formatting_canonical() {
        for s in ["ctrl+shift+s", "CTRL + SHIFT + S", " Shift+Control+s ", "shift+ctl+S"] {
            assert_eq!(c(s).to_string(), "Ctrl+Shift+S", "{s}");
        }
        assert_eq!(c("win+alt+r").to_string(), "Alt+Super+R");
        assert_eq!(c("cmd+shift+4").to_string(), "Shift+Super+4");
        assert_eq!(c("Meta+PgUp").to_string(), "Super+PageUp");
        assert_eq!(c("ctrl+alt+shift+super+f12").to_string(), "Ctrl+Alt+Shift+Super+F12");
        assert_eq!(c("prtsc").to_string(), "Print");
        assert_eq!(c("Ctrl+-").to_string(), "Ctrl+Minus");
    }

    #[test]
    fn rejects_bad_input_with_specific_errors() {
        assert_eq!("".parse::<Chord>(), Err(ChordError::Empty));
        assert_eq!("   ".parse::<Chord>(), Err(ChordError::Empty));
        assert!(matches!("Ctrl++S".parse::<Chord>(), Err(ChordError::EmptyPart(_))));
        assert!(matches!("+S".parse::<Chord>(), Err(ChordError::EmptyPart(_))));
        assert!(matches!("Ctrl+".parse::<Chord>(), Err(ChordError::EmptyPart(_))));
        assert!(matches!("Ctrl+Shift".parse::<Chord>(), Err(ChordError::MissingKey(_))));
        assert!(matches!("Ctrl".parse::<Chord>(), Err(ChordError::MissingKey(_))));
        assert!(
            matches!("Ctrl+Nope".parse::<Chord>(), Err(ChordError::UnknownKey(k)) if k == "Nope")
        );
        assert!(matches!("A+B".parse::<Chord>(), Err(ChordError::NotAModifier(m)) if m == "A"));
        assert!(matches!(
            "Ctrl+Ctrl+S".parse::<Chord>(),
            Err(ChordError::DuplicateModifier("Ctrl"))
        ));
        assert!(matches!("Control+Ctl+S".parse::<Chord>(), Err(ChordError::DuplicateModifier(_))));
        assert!(matches!("S".parse::<Chord>(), Err(ChordError::NeedsModifier(_))));
        assert!(matches!("Space".parse::<Chord>(), Err(ChordError::NeedsModifier(_))));
        assert!(matches!("Escape".parse::<Chord>(), Err(ChordError::NeedsModifier(_))));
        assert!("F5".parse::<Chord>().is_ok(), "function keys may be bare");
        assert!("VolumeMute".parse::<Chord>().is_ok(), "media keys may be bare");
        assert!("Ctrl+Shift+S+".parse::<Chord>().is_err());
    }

    #[test]
    fn target_syntaxes() {
        let ch = c("Ctrl+Shift+S");
        assert_eq!(ch.to_portal_trigger(), "CTRL+SHIFT+s");
        assert_eq!(ch.to_gnome_accelerator(), "<Control><Shift>s");
        assert_eq!(ch.to_qt_sequence().as_deref(), Some("Ctrl+Shift+S"));
        let ch = c("Super+Alt+R");
        assert_eq!(ch.to_portal_trigger(), "ALT+LOGO+r");
        assert_eq!(ch.to_gnome_accelerator(), "<Alt><Super>r");
        assert_eq!(ch.to_qt_sequence().as_deref(), Some("Alt+Meta+R"));
        let ch = c("Print");
        assert_eq!(ch.to_portal_trigger(), "Print");
        assert_eq!(ch.to_gnome_accelerator(), "Print");
        assert_eq!(c("Ctrl+Numpad1").to_qt_sequence(), None);
    }

    #[test]
    fn every_valid_combination_round_trips() {
        for key in Key::all() {
            for bits in 0..16u8 {
                let mut m = Modifiers::NONE;
                for (i, flag) in
                    [Modifiers::CTRL, Modifiers::ALT, Modifiers::SHIFT, Modifiers::SUPER]
                        .into_iter()
                        .enumerate()
                {
                    if bits & (1 << i) != 0 {
                        m = m | flag;
                    }
                }
                match Chord::new(m, key) {
                    Ok(ch) => {
                        let text = ch.to_string();
                        assert_eq!(text.parse::<Chord>(), Ok(ch), "{text}");
                        assert_eq!(text.parse::<Chord>().map(|c| c.to_string()), Ok(text));
                    }
                    Err(e) => assert!(matches!(e, ChordError::NeedsModifier(_))),
                }
            }
        }
    }

    #[cfg(any(windows, target_os = "macos", all(unix, not(target_vendor = "apple"))))]
    #[test]
    fn maps_to_global_hotkey() {
        use global_hotkey::hotkey::{Code, HotKey, Modifiers as M};
        assert_eq!(
            c("Ctrl+Shift+S").to_hotkey(),
            HotKey::new(Some(M::CONTROL | M::SHIFT), Code::KeyS)
        );
        assert_eq!(c("Print").to_hotkey(), HotKey::new(None, Code::PrintScreen));
        assert_eq!(c("Super+Alt+F9").to_hotkey(), HotKey::new(Some(M::SUPER | M::ALT), Code::F9));
        // Every key maps to some code without panicking, and ids differ per chord.
        let mut ids = std::collections::HashSet::new();
        for key in Key::all() {
            let ch = Chord::new(Modifiers::CTRL, key).unwrap();
            assert!(ids.insert(ch.to_hotkey().id()), "id collision for {ch}");
        }
    }
}
