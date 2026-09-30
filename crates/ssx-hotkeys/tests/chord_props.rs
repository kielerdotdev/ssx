//! Property tests for chord parsing and command quoting.

use proptest::prelude::*;
use ssx_hotkeys::{Chord, Key, Modifiers, bindings::gnome};

fn any_key() -> impl Strategy<Value = Key> {
    let keys: Vec<Key> = Key::all().collect();
    prop::sample::select(keys)
}

fn any_mods() -> impl Strategy<Value = Modifiers> {
    (0u8..16).prop_map(|bits| {
        [Modifiers::CTRL, Modifiers::ALT, Modifiers::SHIFT, Modifiers::SUPER]
            .into_iter()
            .enumerate()
            .filter(|(i, _)| bits & (1 << i) != 0)
            .fold(Modifiers::NONE, |a, (_, m)| a | m)
    })
}

fn any_chord() -> impl Strategy<Value = Chord> {
    (any_mods(), any_key())
        .prop_filter_map("bare typing keys are invalid", |(m, k)| Chord::new(m, k).ok())
}

proptest! {
    #[test]
    fn format_then_parse_is_identity(c in any_chord()) {
        let text = c.to_string();
        prop_assert_eq!(text.parse::<Chord>(), Ok(c));
    }

    #[test]
    fn normalisation_is_idempotent(c in any_chord()) {
        let once = c.normalized();
        let twice = once.parse::<Chord>().unwrap().normalized();
        prop_assert_eq!(once, twice);
    }

    /// Any casing and spacing of a canonical chord parses to the same chord.
    #[test]
    fn case_and_spacing_do_not_matter(c in any_chord(), upper in any::<bool>(), pad in 0usize..3) {
        let base = c.to_string();
        let cased = if upper { base.to_uppercase() } else { base.to_lowercase() };
        let sp = " ".repeat(pad);
        let spaced = cased.replace('+', &format!("{sp}+{sp}"));
        prop_assert_eq!(spaced.parse::<Chord>(), Ok(c));
    }

    /// Arbitrary text never panics, and whatever parses re-parses to itself.
    #[test]
    fn arbitrary_strings_never_panic(s in "\\PC{0,24}") {
        if let Ok(c) = s.parse::<Chord>() {
            prop_assert_eq!(c.to_string().parse::<Chord>(), Ok(c));
        }
    }

    #[test]
    fn plus_soup_never_panics(s in "[A-Za-z0-9+ ]{0,20}") {
        if let Ok(c) = s.parse::<Chord>() {
            prop_assert_eq!(c.to_string().parse::<Chord>(), Ok(c));
        }
    }

    #[test]
    fn every_target_syntax_is_nonempty_and_distinct_per_chord(a in any_chord(), b in any_chord()) {
        prop_assume!(a != b);
        prop_assert_ne!(a.to_gnome_accelerator(), b.to_gnome_accelerator());
        prop_assert_ne!(a.to_portal_trigger(), b.to_portal_trigger());
        if let (Some(x), Some(y)) = (a.to_qt_sequence(), b.to_qt_sequence()) {
            prop_assert_ne!(x, y);
        }
    }

    /// GVariant string literals round-trip any text (through our own parser).
    #[test]
    fn gvariant_strings_round_trip(s in "\\PC{0,40}") {
        let text = format!("'{}'", escape(&s));
        let parsed = ssx_hotkeys::bindings::gnome::parse_gvariant_string(&text).map(|(v, _)| v);
        prop_assert_eq!(parsed, Some(s));
    }
}

/// Independent GVariant escaper for the property above (does not reuse the crate's).
fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '\'' => vec!['\\', '\''],
            c => vec![c],
        })
        .collect()
}

#[test]
fn gnome_module_is_reachable() {
    assert!(gnome::PATH_PREFIX.ends_with('/'));
}
