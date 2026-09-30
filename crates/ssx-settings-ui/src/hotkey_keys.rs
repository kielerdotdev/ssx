//! From key presses to a stored hotkey, without any drawing.
//!
//! Three vocabularies meet here and must agree:
//!
//! * `egui::Key` / `egui::Modifiers`, what the window receives,
//! * `ssx_hotkeys::Chord`, what the hotkey backends register and the generators render,
//! * the string in `settings.toml`, parsed by both `ssx_core::settings::Hotkey` (validation,
//!   duplicate detection) and `Chord` (the hotkey manager).
//!
//! [`translate`] maps a press to a [`Translated`] outcome; [`HotkeyDraft`] is the editable
//! "modifiers + key" pair the widget shows; [`settings_string`] is the only way a chord is
//! turned into text, and refuses keys that the two parsers would read differently.
//!
//! **Physical keys.** Hotkeys are registered by key *position* (`global-hotkey` uses W3C
//! codes), so [`translate`] takes the physical key when egui has one; Shift+2 is `2` with
//! Shift, not `@`.
//!
//! **What egui cannot see.** `egui::Key` has no `PrintScreen`, Pause, `ScrollLock` or Menu, and
//! `egui::Modifiers` has no Windows/Super key. The widget therefore also offers a key list
//! ([`key_groups`]) and tracks the Super key from its own key events.

use ssx_core::settings::Hotkey;
use ssx_hotkeys::{Chord, ChordError, Key, Modifiers, Named};

/// What a key press means for the capture widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Translated {
    /// A complete, valid shortcut.
    Chord(Chord),
    /// Only a modifier is down; keep waiting (the widget shows what is held).
    ModifiersHeld(Modifiers),
    /// Escape on its own: stop capturing, change nothing.
    Cancel,
    /// Backspace or Delete on its own: remove the shortcut.
    Clear,
    /// The press cannot be a hotkey; the text says why and what to do.
    Rejected(String),
}

/// `egui` modifiers as a hotkey modifier set. `super_held` comes from the widget's own
/// tracking of the Windows / Super key (egui does not report it); on macOS the command key is
/// `mac_cmd`, which is the Super of a chord.
pub fn modifiers_from_egui(m: egui::Modifiers, super_held: bool) -> Modifiers {
    let mut out = Modifiers::NONE;
    if m.ctrl {
        out = out | Modifiers::CTRL;
    }
    if m.alt {
        out = out | Modifiers::ALT;
    }
    if m.shift {
        out = out | Modifiers::SHIFT;
    }
    if m.mac_cmd || super_held {
        out = out | Modifiers::SUPER;
    }
    out
}

/// The hotkey key for an egui key, or the reason there is none.
pub fn key_from_egui(k: egui::Key) -> Result<Key, String> {
    use egui::Key as E;
    let named = |n| Ok(Key::Named(n));
    Ok(match k {
        E::A => Key::Letter('A'),
        E::B => Key::Letter('B'),
        E::C => Key::Letter('C'),
        E::D => Key::Letter('D'),
        E::E => Key::Letter('E'),
        E::F => Key::Letter('F'),
        E::G => Key::Letter('G'),
        E::H => Key::Letter('H'),
        E::I => Key::Letter('I'),
        E::J => Key::Letter('J'),
        E::K => Key::Letter('K'),
        E::L => Key::Letter('L'),
        E::M => Key::Letter('M'),
        E::N => Key::Letter('N'),
        E::O => Key::Letter('O'),
        E::P => Key::Letter('P'),
        E::Q => Key::Letter('Q'),
        E::R => Key::Letter('R'),
        E::S => Key::Letter('S'),
        E::T => Key::Letter('T'),
        E::U => Key::Letter('U'),
        E::V => Key::Letter('V'),
        E::W => Key::Letter('W'),
        E::X => Key::Letter('X'),
        E::Y => Key::Letter('Y'),
        E::Z => Key::Letter('Z'),
        E::Num0 => Key::Digit(0),
        E::Num1 => Key::Digit(1),
        E::Num2 => Key::Digit(2),
        E::Num3 => Key::Digit(3),
        E::Num4 => Key::Digit(4),
        E::Num5 => Key::Digit(5),
        E::Num6 => Key::Digit(6),
        E::Num7 => Key::Digit(7),
        E::Num8 => Key::Digit(8),
        E::Num9 => Key::Digit(9),
        E::F1 => Key::F(1),
        E::F2 => Key::F(2),
        E::F3 => Key::F(3),
        E::F4 => Key::F(4),
        E::F5 => Key::F(5),
        E::F6 => Key::F(6),
        E::F7 => Key::F(7),
        E::F8 => Key::F(8),
        E::F9 => Key::F(9),
        E::F10 => Key::F(10),
        E::F11 => Key::F(11),
        E::F12 => Key::F(12),
        E::F13 => Key::F(13),
        E::F14 => Key::F(14),
        E::F15 => Key::F(15),
        E::F16 => Key::F(16),
        E::F17 => Key::F(17),
        E::F18 => Key::F(18),
        E::F19 => Key::F(19),
        E::F20 => Key::F(20),
        E::F21 => Key::F(21),
        E::F22 => Key::F(22),
        E::F23 => Key::F(23),
        E::F24 => Key::F(24),
        E::Space => return named(Named::Space),
        E::Enter => return named(Named::Enter),
        E::Tab => return named(Named::Tab),
        E::Escape => return named(Named::Escape),
        E::Backspace => return named(Named::Backspace),
        E::Delete => return named(Named::Delete),
        E::Insert => return named(Named::Insert),
        E::Home => return named(Named::Home),
        E::End => return named(Named::End),
        E::PageUp => return named(Named::PageUp),
        E::PageDown => return named(Named::PageDown),
        E::ArrowUp => return named(Named::Up),
        E::ArrowDown => return named(Named::Down),
        E::ArrowLeft => return named(Named::Left),
        E::ArrowRight => return named(Named::Right),
        E::Minus => return named(Named::Minus),
        E::Equals => return named(Named::Equal),
        E::Comma => return named(Named::Comma),
        E::Period => return named(Named::Period),
        E::Slash => return named(Named::Slash),
        E::Backslash => return named(Named::Backslash),
        E::Semicolon => return named(Named::Semicolon),
        E::Quote => return named(Named::Quote),
        E::Backtick => return named(Named::Backquote),
        E::OpenBracket => return named(Named::BracketLeft),
        E::CloseBracket => return named(Named::BracketRight),
        E::Copy | E::Cut | E::Paste => {
            return Err(
                "the operating system reports Copy/Cut/Paste as commands, not keys; press the letter key instead"
                    .to_owned(),
            );
        }
        E::Plus
        | E::Colon
        | E::Pipe
        | E::Questionmark
        | E::Exclamationmark
        | E::OpenCurlyBracket
        | E::CloseCurlyBracket => {
            return Err(
                "that symbol needs Shift on most keyboards; hold Shift together with the key underneath it, or pick the key from the list"
                    .to_owned(),
            );
        }
        other => return Err(format!("{other:?} cannot be used as a global shortcut key")),
    })
}

/// `true` for the modifier keys egui reports as physical key events.
pub fn is_modifier_key(k: egui::Key) -> bool {
    use egui::Key as E;
    matches!(
        k,
        E::ShiftLeft
            | E::ShiftRight
            | E::ControlLeft
            | E::ControlRight
            | E::AltLeft
            | E::AltRight
            | E::SuperLeft
            | E::SuperRight
    )
}

/// `true` for the Windows / Super / Command keys (which `egui::Modifiers` does not carry).
pub fn is_super_key(k: egui::Key) -> bool {
    matches!(k, egui::Key::SuperLeft | egui::Key::SuperRight)
}

/// Interprets a key press. `key` should be the physical key when the event has one.
pub fn translate(key: egui::Key, mods: egui::Modifiers, super_held: bool) -> Translated {
    let m = modifiers_from_egui(mods, super_held);
    if is_modifier_key(key) {
        return Translated::ModifiersHeld(m);
    }
    if m.is_empty() {
        match key {
            egui::Key::Escape => return Translated::Cancel,
            egui::Key::Backspace | egui::Key::Delete => return Translated::Clear,
            _ => {}
        }
    }
    let k = match key_from_egui(key) {
        Ok(k) => k,
        Err(why) => return Translated::Rejected(why),
    };
    match Chord::new(m, k) {
        Ok(c) => match settings_string(&c) {
            Ok(_) => Translated::Chord(c),
            Err(why) => Translated::Rejected(why),
        },
        Err(e) => Translated::Rejected(chord_error_text(&e)),
    }
}

/// A chord error as one actionable sentence.
pub fn chord_error_text(e: &ChordError) -> String {
    match e {
        ChordError::NeedsModifier(k) => format!(
            "{k} on its own would stop you typing that key everywhere; hold Ctrl, Alt, Shift or Super as well"
        ),
        other => other.to_string(),
    }
}

/// The text stored in `settings.toml` for `chord` (the canonical spelling of
/// `ssx_core::settings::Hotkey`, e.g. `Ctrl+PrintScreen`).
///
/// Fails for keys that `ssx-core` cannot parse (media keys, the numeric pad, `=`): a value it
/// rejects would block saving, and one the two parsers read differently would silently bind
/// the wrong key.
pub fn settings_string(chord: &Chord) -> Result<String, String> {
    let text = chord.to_string();
    let hk: Hotkey = text.parse().map_err(|_| {
        format!("{} cannot be stored in settings.toml yet; choose another key", chord.key().name())
    })?;
    let canonical = hk.to_string();
    match canonical.parse::<Chord>() {
        Ok(back) if back == *chord => Ok(canonical),
        _ => Err(format!(
            "{} is read differently by different parts of ssx; choose another key",
            chord.key().name()
        )),
    }
}

/// The modifiers and key of a stored hotkey, editable piece by piece.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HotkeyDraft {
    /// The held modifiers.
    pub mods: Modifiers,
    /// The key, once chosen.
    pub key: Option<Key>,
}

impl HotkeyDraft {
    /// Reads a stored hotkey (any spelling the manager accepts).
    pub fn from_setting(text: &str) -> Result<Self, ChordError> {
        let c: Chord = text.parse()?;
        Ok(Self { mods: c.modifiers(), key: Some(c.key()) })
    }

    /// A draft that holds exactly `chord`.
    pub fn from_chord(chord: &Chord) -> Self {
        Self { mods: chord.modifiers(), key: Some(chord.key()) }
    }

    /// Flips one modifier.
    pub fn toggle(&mut self, m: Modifiers) {
        self.mods = if self.mods.contains(m) {
            Modifiers::iter(self.mods).filter(|x| *x != m).fold(Modifiers::NONE, |a, b| a | b)
        } else {
            self.mods | m
        };
    }

    /// The chord, or why the draft is not complete yet.
    pub fn to_chord(&self) -> Result<Chord, String> {
        let key = self.key.ok_or_else(|| "choose a key".to_owned())?;
        Chord::new(self.mods, key).map_err(|e| chord_error_text(&e))
    }

    /// The text to store, or why the draft cannot be stored.
    pub fn to_setting(&self) -> Result<String, String> {
        settings_string(&self.to_chord()?)
    }

    /// Text for the widget: `Ctrl+Shift+PrintScreen`, or `Ctrl+…` while the key is missing.
    pub fn display(&self) -> String {
        let mut parts: Vec<String> = self.mods.iter().map(|m| m.name().to_owned()).collect();
        parts.push(self.key.map_or_else(|| "\u{2026}".to_owned(), display_key));
        parts.join("+")
    }
}

/// The name of a key as stored (`PrintScreen` rather than the chord's `Print`).
pub fn display_key(k: Key) -> String {
    Chord::new(Modifiers::CTRL, k)
        .ok()
        .and_then(|c| settings_string(&c).ok())
        .and_then(|s| s.rsplit('+').next().map(str::to_owned))
        .unwrap_or_else(|| k.name())
}

/// Whether `key` can be stored in settings (with a modifier).
pub fn storable(key: Key) -> bool {
    Chord::new(Modifiers::CTRL, key).is_ok_and(|c| settings_string(&c).is_ok())
}

/// The keys that can be picked from the list, grouped; only ones that can be stored.
pub fn key_groups() -> Vec<(&'static str, Vec<Key>)> {
    let pick = |keys: Vec<Key>| keys.into_iter().filter(|k| storable(*k)).collect::<Vec<_>>();
    let special: Vec<Key> = [
        Named::Print,
        Named::Pause,
        Named::ScrollLock,
        Named::Insert,
        Named::Delete,
        Named::Home,
        Named::End,
        Named::PageUp,
        Named::PageDown,
        Named::Up,
        Named::Down,
        Named::Left,
        Named::Right,
        Named::Space,
        Named::Enter,
        Named::Tab,
        Named::Backspace,
        Named::Escape,
    ]
    .into_iter()
    .map(Key::Named)
    .collect();
    let punctuation: Vec<Key> = [
        Named::Minus,
        Named::Equal,
        Named::Comma,
        Named::Period,
        Named::Slash,
        Named::Backslash,
        Named::Semicolon,
        Named::Quote,
        Named::Backquote,
        Named::BracketLeft,
        Named::BracketRight,
    ]
    .into_iter()
    .map(Key::Named)
    .collect();
    vec![
        ("Special", pick(special)),
        ("Function", pick((1..=Key::MAX_F).map(Key::F).collect())),
        ("Letters", pick(('A'..='Z').map(Key::Letter).collect())),
        ("Digits", pick((0..=9).map(Key::Digit).collect())),
        ("Punctuation", pick(punctuation)),
    ]
}

#[cfg(test)]
mod tests {
    use egui::Key as E;

    use super::*;

    fn mods(ctrl: bool, alt: bool, shift: bool) -> egui::Modifiers {
        egui::Modifiers { ctrl, alt, shift, command: ctrl, ..egui::Modifiers::NONE }
    }

    fn chord(s: &str) -> Chord {
        s.parse().unwrap()
    }

    #[test]
    fn letters_digits_function_keys_translate() {
        assert_eq!(
            translate(E::S, mods(true, false, true), false),
            Translated::Chord(chord("Ctrl+Shift+S"))
        );
        assert_eq!(
            translate(E::Num5, mods(false, true, false), false),
            Translated::Chord(chord("Alt+5"))
        );
        assert_eq!(translate(E::F9, egui::Modifiers::NONE, false), Translated::Chord(chord("F9")));
        assert_eq!(
            translate(E::F24, mods(true, false, false), false),
            Translated::Chord(chord("Ctrl+F24"))
        );
    }

    #[test]
    fn super_comes_from_the_tracked_key_or_the_mac_command_key() {
        assert_eq!(
            translate(E::R, mods(false, false, false), true),
            Translated::Chord(chord("Super+R"))
        );
        let cmd = egui::Modifiers { mac_cmd: true, command: true, ..egui::Modifiers::NONE };
        assert_eq!(translate(E::R, cmd, false), Translated::Chord(chord("Super+R")));
        assert_eq!(modifiers_from_egui(mods(true, true, true), true).iter().count(), 4);
    }

    #[test]
    fn modifier_keys_alone_keep_waiting_and_report_what_is_held() {
        for k in [E::ShiftLeft, E::ControlRight, E::AltLeft, E::SuperLeft, E::SuperRight] {
            assert!(is_modifier_key(k));
            assert!(matches!(
                translate(k, mods(true, false, false), false),
                Translated::ModifiersHeld(_)
            ));
        }
        match translate(E::ControlLeft, mods(true, false, false), false) {
            Translated::ModifiersHeld(m) => assert_eq!(m, Modifiers::CTRL),
            other => panic!("{other:?}"),
        }
        assert!(is_super_key(E::SuperLeft) && !is_super_key(E::ShiftLeft));
    }

    #[test]
    fn escape_cancels_and_backspace_clears_only_without_modifiers() {
        assert_eq!(translate(E::Escape, egui::Modifiers::NONE, false), Translated::Cancel);
        assert_eq!(translate(E::Backspace, egui::Modifiers::NONE, false), Translated::Clear);
        assert_eq!(translate(E::Delete, egui::Modifiers::NONE, false), Translated::Clear);
        // with a modifier they are ordinary keys (Ctrl+Escape, Ctrl+Delete)
        assert!(matches!(
            translate(E::Delete, mods(true, false, false), false),
            Translated::Chord(_)
        ));
    }

    #[test]
    fn a_typing_key_without_a_modifier_is_rejected_with_advice() {
        match translate(E::S, egui::Modifiers::NONE, false) {
            Translated::Rejected(why) => {
                assert!(why.contains("Ctrl") && why.contains("typing"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            translate(E::Space, egui::Modifiers::NONE, false),
            Translated::Rejected(_)
        ));
    }

    #[test]
    fn keys_without_a_hotkey_meaning_are_rejected() {
        for k in [
            E::Copy,
            E::Cut,
            E::Paste,
            E::Plus,
            E::Colon,
            E::Pipe,
            E::BrowserBack,
            E::F30,
            E::IntlBackslash,
        ] {
            assert!(
                matches!(translate(k, mods(true, false, false), false), Translated::Rejected(_)),
                "{k:?}"
            );
        }
    }

    #[test]
    fn keys_that_settings_cannot_store_are_rejected_not_silently_changed() {
        // `=` is a real key for the hotkey manager but unknown to the settings validator.
        match translate(E::Equals, mods(true, false, false), false) {
            Translated::Rejected(why) => assert!(why.contains("settings.toml"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn settings_strings_use_the_core_spelling_and_round_trip_through_both_parsers() {
        assert_eq!(settings_string(&chord("Ctrl+Print")).unwrap(), "Ctrl+PrintScreen");
        assert_eq!(settings_string(&chord("ctrl+shift+s")).unwrap(), "Ctrl+Shift+S");
        assert_eq!(settings_string(&chord("Print")).unwrap(), "PrintScreen");
        for key in Key::all() {
            let Ok(c) = Chord::new(Modifiers::CTRL | Modifiers::SHIFT, key) else { continue };
            if let Ok(text) = settings_string(&c) {
                let core: Hotkey = text.parse().unwrap_or_else(|e| panic!("{text}: {e}"));
                assert_eq!(core.to_string(), text, "canonical");
                assert_eq!(text.parse::<Chord>().unwrap(), c, "{text}");
            }
        }
    }

    #[test]
    fn storable_keys_cover_what_people_bind() {
        for k in [
            Key::Named(Named::Print),
            Key::Named(Named::Pause),
            Key::F(1),
            Key::F(24),
            Key::Letter('A'),
            Key::Digit(0),
            Key::Named(Named::Space),
            Key::Named(Named::Minus),
        ] {
            assert!(storable(k), "{k:?}");
        }
        assert!(!storable(Key::Named(Named::VolumeUp)));
        assert!(!storable(Key::Named(Named::Equal)));
    }

    #[test]
    fn key_list_offers_print_screen_and_only_storable_keys() {
        let groups = key_groups();
        let special = &groups[0].1;
        assert!(special.contains(&Key::Named(Named::Print)));
        for (_, keys) in &groups {
            assert!(!keys.is_empty());
            assert!(keys.iter().all(|k| storable(*k)));
        }
        assert!(!groups[4].1.contains(&Key::Named(Named::Equal)));
        assert_eq!(groups[1].1.len(), 24);
    }

    #[test]
    fn draft_round_trips_and_edits() {
        let d = HotkeyDraft::from_setting("ctrl + shift + prtsc").unwrap();
        assert_eq!(d.to_setting().unwrap(), "Ctrl+Shift+PrintScreen");
        assert_eq!(d.display(), "Ctrl+Shift+PrintScreen");
        let mut d = d;
        d.toggle(Modifiers::SHIFT);
        assert_eq!(d.to_setting().unwrap(), "Ctrl+PrintScreen");
        d.toggle(Modifiers::ALT);
        d.toggle(Modifiers::ALT);
        assert_eq!(d.to_setting().unwrap(), "Ctrl+PrintScreen");
        d.toggle(Modifiers::SUPER);
        assert_eq!(d.to_setting().unwrap(), "Ctrl+Super+PrintScreen");
    }

    #[test]
    fn incomplete_or_invalid_drafts_explain_themselves() {
        let d = HotkeyDraft { mods: Modifiers::CTRL, key: None };
        assert_eq!(d.to_setting().unwrap_err(), "choose a key");
        assert_eq!(d.display(), "Ctrl+\u{2026}");
        let d = HotkeyDraft { mods: Modifiers::NONE, key: Some(Key::Letter('A')) };
        assert!(d.to_setting().unwrap_err().contains("Ctrl"));
        assert!(HotkeyDraft::from_setting("banana").is_err());
        assert_eq!(HotkeyDraft::default().display(), "\u{2026}");
    }

    #[test]
    fn draft_from_chord_matches_the_chord() {
        let c = chord("Alt+F4");
        let d = HotkeyDraft::from_chord(&c);
        assert_eq!(d.to_chord().unwrap(), c);
    }

    #[test]
    fn every_egui_key_translates_without_panicking() {
        for k in E::ALL {
            for m in [egui::Modifiers::NONE, mods(true, false, false), mods(true, true, true)] {
                let _ = translate(*k, m, false);
            }
        }
    }

    #[test]
    fn every_letter_digit_and_function_key_that_egui_has_is_translatable() {
        for k in E::ALL {
            let name = format!("{k:?}");
            let expect_ok = name.len() == 1
                || (name.starts_with("Num") && name.len() == 4)
                || (name.starts_with('F') && name[1..].parse::<u8>().is_ok_and(|n| n <= 24));
            if expect_ok {
                assert!(key_from_egui(*k).is_ok(), "{name}");
            }
        }
    }
}
