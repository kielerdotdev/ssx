//! Textual global-hotkey accelerators (`Ctrl+Shift+PrintScreen`).
//!
//! The core only *validates and normalises* accelerators so that duplicates can be detected
//! (`ctrl+shift+printscreen` and `Shift+Ctrl+PrtSc` are the same binding). Registering them
//! with the OS is the job of the `ssx-hotkeys` crate, which can parse the canonical form.

use std::{collections::BTreeSet, fmt, str::FromStr};

/// A modifier key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    /// Control.
    Ctrl,
    /// Alt / Option.
    Alt,
    /// Shift.
    Shift,
    /// Windows key / Command / Super.
    Super,
}

impl Modifier {
    fn name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "Ctrl",
            Modifier::Alt => "Alt",
            Modifier::Shift => "Shift",
            Modifier::Super => "Super",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Self::Ctrl),
            "alt" | "option" | "opt" => Some(Self::Alt),
            "shift" => Some(Self::Shift),
            "super" | "win" | "windows" | "meta" | "cmd" | "command" => Some(Self::Super),
            _ => None,
        }
    }
}

/// A parsed accelerator in canonical form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hotkey {
    /// Held modifiers (deduplicated, canonical order).
    pub modifiers: BTreeSet<Modifier>,
    /// The non-modifier key, canonical spelling (`A`, `F5`, `PrintScreen`, `Space`, …).
    pub key: String,
}

/// Why an accelerator string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    /// Empty string or only separators.
    #[error("the hotkey is empty; use something like \"Ctrl+Shift+PrintScreen\"")]
    Empty,
    /// Only modifiers were given.
    #[error("the hotkey {0:?} has no key, only modifiers; add a key such as \"F9\" or \"S\"")]
    NoKey(String),
    /// More than one non-modifier key.
    #[error("the hotkey {0:?} has more than one key; a hotkey is modifiers plus exactly one key")]
    TooManyKeys(String),
    /// A key name we do not know.
    #[error(
        "unknown key {key:?} in hotkey {hotkey:?}; use a letter, digit, F1-F24 or a name such as PrintScreen, Space, Enter"
    )]
    UnknownKey {
        /// The full hotkey text.
        hotkey: String,
        /// The offending part.
        key: String,
    },
}

const NAMED_KEYS: &[(&str, &str)] = &[
    ("printscreen", "PrintScreen"),
    ("prtsc", "PrintScreen"),
    ("prtscn", "PrintScreen"),
    ("print", "PrintScreen"),
    ("sysrq", "PrintScreen"),
    ("scrolllock", "ScrollLock"),
    ("pause", "Pause"),
    ("escape", "Escape"),
    ("esc", "Escape"),
    ("enter", "Enter"),
    ("return", "Enter"),
    ("space", "Space"),
    ("tab", "Tab"),
    ("backspace", "Backspace"),
    ("delete", "Delete"),
    ("del", "Delete"),
    ("insert", "Insert"),
    ("ins", "Insert"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "PageUp"),
    ("pgup", "PageUp"),
    ("pagedown", "PageDown"),
    ("pgdn", "PageDown"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("plus", "Plus"),
    ("minus", "Minus"),
    ("comma", "Comma"),
    ("period", "Period"),
    ("slash", "Slash"),
    ("backslash", "Backslash"),
    ("semicolon", "Semicolon"),
    ("quote", "Quote"),
    ("backquote", "Backquote"),
    ("bracketleft", "BracketLeft"),
    ("bracketright", "BracketRight"),
];

fn canonical_key(k: &str) -> Option<String> {
    let lower = k.to_ascii_lowercase();
    if let Some((_, canon)) = NAMED_KEYS.iter().find(|(n, _)| *n == lower) {
        return Some((*canon).to_owned());
    }
    // F1..F24
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=24).contains(&n)
    {
        return Some(format!("F{n}"));
    }
    // Numpad0..9
    if let Some(d) = lower.strip_prefix("numpad")
        && d.len() == 1
        && d.as_bytes()[0].is_ascii_digit()
    {
        return Some(format!("Numpad{d}"));
    }
    let mut chars = k.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some(c.to_ascii_uppercase().to_string()),
        _ => None,
    }
}

impl FromStr for Hotkey {
    type Err = HotkeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(HotkeyError::Empty);
        }
        let mut modifiers = BTreeSet::new();
        let mut key: Option<String> = None;
        for part in parts {
            if let Some(m) = Modifier::parse(part) {
                modifiers.insert(m);
                continue;
            }
            let canon = canonical_key(part).ok_or_else(|| HotkeyError::UnknownKey {
                hotkey: s.to_owned(),
                key: part.to_owned(),
            })?;
            if key.replace(canon).is_some() {
                return Err(HotkeyError::TooManyKeys(s.to_owned()));
            }
        }
        let key = key.ok_or_else(|| HotkeyError::NoKey(s.to_owned()))?;
        Ok(Self { modifiers, key })
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in &self.modifiers {
            write!(f, "{}+", m.name())?;
        }
        f.write_str(&self.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(s: &str) -> String {
        s.parse::<Hotkey>().unwrap().to_string()
    }

    #[test]
    fn canonicalisation() {
        assert_eq!(canon("ctrl+shift+printscreen"), "Ctrl+Shift+PrintScreen");
        assert_eq!(canon("Shift + Control + PrtSc"), "Ctrl+Shift+PrintScreen");
        assert_eq!(canon("cmd+s"), "Super+S");
        assert_eq!(canon("Alt+f12"), "Alt+F12");
        assert_eq!(canon("PrintScreen"), "PrintScreen");
        assert_eq!(canon("ctrl+ctrl+a"), "Ctrl+A");
        assert_eq!(canon("Ctrl+Numpad5"), "Ctrl+Numpad5");
        assert_eq!(canon("ctrl+1"), "Ctrl+1");
    }

    #[test]
    fn equivalent_spellings_compare_equal() {
        assert_eq!(
            "ctrl+shift+printscreen".parse::<Hotkey>().unwrap(),
            "SHIFT+CTRL+PrtSc".parse::<Hotkey>().unwrap()
        );
    }

    #[test]
    fn errors() {
        assert_eq!("".parse::<Hotkey>().unwrap_err(), HotkeyError::Empty);
        assert_eq!("+ +".parse::<Hotkey>().unwrap_err(), HotkeyError::Empty);
        assert!(matches!("ctrl+shift".parse::<Hotkey>(), Err(HotkeyError::NoKey(_))));
        assert!(matches!("ctrl+a+b".parse::<Hotkey>(), Err(HotkeyError::TooManyKeys(_))));
        assert!(matches!("ctrl+banana".parse::<Hotkey>(), Err(HotkeyError::UnknownKey { .. })));
        assert!(matches!("f25".parse::<Hotkey>(), Err(HotkeyError::UnknownKey { .. })));
        assert!(matches!("ctrl+é".parse::<Hotkey>(), Err(HotkeyError::UnknownKey { .. })));
        let msg = "ctrl+banana".parse::<Hotkey>().unwrap_err().to_string();
        assert!(msg.contains("banana") && msg.contains("PrintScreen"), "{msg}");
    }

    #[test]
    fn display_roundtrips() {
        for s in ["Ctrl+Alt+Shift+Super+F24", "Space", "Alt+Enter"] {
            assert_eq!(canon(s), s);
            assert_eq!(canon(&canon(s)), s);
        }
    }
}
