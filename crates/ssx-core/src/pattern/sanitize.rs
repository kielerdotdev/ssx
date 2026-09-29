//! Turning arbitrary text into a file name that is valid on Windows, macOS and Linux.
//!
//! Screenshots are routinely saved on one OS and synced to another (Dropbox, network
//! shares, USB sticks), and window titles are attacker/user-controlled text, so the
//! sanitiser targets the **intersection** of all three file systems instead of only the
//! current one:
//!
//! * the Windows-illegal set `< > : " / \ | ? *` and all control characters are removed
//!   (ShareX removes them too; the replacement is configurable),
//! * bidirectional-override characters are removed (they let `evilgpj.exe` display as
//!   `evilexe.jpg`),
//! * trailing dots and spaces are trimmed (Windows silently strips them, which breaks
//!   round-trips), and `.` / `..` never survive,
//! * Windows reserved device names (`CON`, `NUL`, `COM1`, … also with an extension, so
//!   `nul.txt` is reserved too) get a `_` prefix,
//! * the result is at most 255 **bytes** (the Linux/macOS limit; Windows counts UTF-16
//!   units, which are never more than bytes), truncated on grapheme-cluster boundaries so
//!   an emoji or combining sequence is never cut in half, keeping the extension.

use std::path::PathBuf;

use unicode_segmentation::UnicodeSegmentation;

/// Longest file name (bytes) accepted by ext4, APFS and NTFS (255).
pub const MAX_FILE_NAME_BYTES: usize = 255;

/// Longest extension (bytes) that is preserved verbatim when truncating.
const MAX_EXT_BYTES: usize = 32;

/// Options for [`sanitize_file_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeOptions {
    /// Text substituted for each illegal character (ShareX: empty).
    pub replacement: String,
    /// Maximum size of the result in bytes.
    pub max_bytes: usize,
    /// Result if nothing usable remains.
    pub fallback: String,
}

impl Default for SanitizeOptions {
    fn default() -> Self {
        Self {
            replacement: String::new(),
            max_bytes: MAX_FILE_NAME_BYTES,
            fallback: "file".to_owned(),
        }
    }
}

const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

fn is_illegal(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        || c.is_control()
        || matches!(
            c,
            // line/paragraph separators, BOM, bidi embeddings/overrides/isolates and marks
            '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{200e}' | '\u{200f}'
                | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// `true` if `name` (a whole file name, with extension) is a Windows reserved device name.
pub fn is_windows_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end_matches([' ', '\u{a0}']);
    let upper = base.to_uppercase();
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    // COM1..COM9 / LPT1..LPT9, plus the superscript digits Windows also treats as reserved.
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let (Some(d), None) = (chars.next(), chars.next()) {
                if matches!(d, '0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}') {
                    return true;
                }
            }
        }
    }
    false
}

/// `true` if `name` is acceptable as a single file name on Windows, macOS and Linux.
pub fn is_valid_file_name(name: &str, max_bytes: usize) -> bool {
    !name.is_empty()
        && name.len() <= max_bytes
        && !name.chars().any(is_illegal)
        && !name.ends_with(['.', ' '])
        && !name.starts_with(' ')
        && name != "."
        && name != ".."
        && !is_windows_reserved(name)
}

/// Splits `name` into (stem, extension-including-dot). A leading dot alone (`.png`) is a
/// stem, not an extension, so hidden-file style names survive.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= MAX_EXT_BYTES => name.split_at(i),
        _ => (name, ""),
    }
}

/// Truncates `s` to at most `max_graphemes` grapheme clusters.
pub fn truncate_graphemes(s: &str, max_graphemes: usize) -> &str {
    match s.grapheme_indices(true).nth(max_graphemes) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Truncates `s` to at most `max_bytes` bytes without splitting a grapheme cluster.
pub fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = 0;
    for (i, g) in s.grapheme_indices(true) {
        if i + g.len() > max_bytes {
            break;
        }
        end = i + g.len();
    }
    &s[..end]
}

fn strip_illegal(name: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.trim().chars() {
        if is_illegal(c) {
            out.push_str(replacement);
        } else {
            out.push(c);
        }
    }
    out
}

/// Removes every character that is illegal in a file name (no trimming, no other rules).
pub(super) fn remove_illegal(s: &str) -> String {
    s.chars().filter(|c| !is_illegal(*c)).collect()
}

/// Sanitises one file name (not a path). The result always satisfies
/// [`is_valid_file_name`].
pub fn sanitize_file_name(name: &str, opts: &SanitizeOptions) -> String {
    let max = opts.max_bytes.clamp(8, MAX_FILE_NAME_BYTES);
    let cleaned = strip_illegal(name, &opts.replacement);
    let mut s = cleaned.trim_matches([' ', '\u{a0}', '\t']).trim_end_matches(['.', ' ']).to_owned();
    if s.trim_start_matches('.').is_empty() {
        return fallback(opts, max);
    }
    if is_windows_reserved(&s) {
        s.insert(0, '_');
    }
    if s.len() > max {
        let (stem, ext) = split_ext(&s);
        let stem_budget = max.saturating_sub(ext.len());
        let shortened = truncate_bytes(stem, stem_budget).trim_end_matches([' ', '.']);
        s = if shortened.is_empty() {
            // e.g. a name that was only an over-long extension
            truncate_bytes(&s, max).to_owned()
        } else {
            format!("{shortened}{ext}")
        };
        s = s.trim_end_matches(['.', ' ']).to_owned();
        if is_windows_reserved(&s) {
            s.insert(0, '_');
        }
    }
    if is_valid_file_name(&s, max) { s } else { fallback(opts, max) }
}

fn fallback(opts: &SanitizeOptions, max: usize) -> String {
    if is_valid_file_name(&opts.fallback, max) { opts.fallback.clone() } else { "file".to_owned() }
}

/// Sanitises a relative folder path. Components are split on both `/` and `\`, each is
/// sanitised, empty ones and `.`/`..` are dropped (no traversal, no absolute paths, no
/// drive letters: `C:` becomes `C`).
pub fn sanitize_relative_path(path: &str, opts: &SanitizeOptions) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.split(['/', '\\']) {
        let comp = comp.trim();
        if comp.is_empty() || comp.trim_matches('.').is_empty() {
            continue;
        }
        let cleaned = strip_illegal(comp, &opts.replacement);
        if cleaned.trim_matches([' ', '.']).is_empty() {
            continue;
        }
        out.push(sanitize_file_name(&cleaned, opts));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn s(name: &str) -> String {
        sanitize_file_name(name, &SanitizeOptions::default())
    }

    #[test]
    fn strips_illegal_characters() {
        assert_eq!(s("a<b>c:d\"e/f\\g|h?i*j.png"), "abcdefghij.png");
        assert_eq!(s("tab\there\u{0}nul"), "tabherenul");
    }

    #[test]
    fn control_characters_removed() {
        assert_eq!(s("a\u{1}b\u{7f}c\u{85}d"), "abcd");
    }

    #[test]
    fn replacement_is_configurable() {
        let o = SanitizeOptions { replacement: "_".into(), ..SanitizeOptions::default() };
        assert_eq!(sanitize_file_name("a:b/c.png", &o), "a_b_c.png");
    }

    #[test]
    fn trailing_dots_and_spaces_trimmed() {
        assert_eq!(s("name. . "), "name");
        assert_eq!(s("  name  "), "name");
        assert_eq!(s("a..."), "a");
    }

    #[test]
    fn dot_names_fall_back() {
        assert_eq!(s("."), "file");
        assert_eq!(s(".."), "file");
        assert_eq!(s("..."), "file");
        assert_eq!(s(""), "file");
        assert_eq!(s("   "), "file");
        assert_eq!(s("<>"), "file");
    }

    #[test]
    fn hidden_style_names_survive() {
        assert_eq!(s(".png"), ".png");
        assert_eq!(s(".hidden"), ".hidden");
    }

    #[test]
    fn reserved_names_are_prefixed() {
        for n in ["CON", "con", "Nul", "PRN", "AUX", "COM1", "lpt9", "COM\u{b9}", "CONIN$"] {
            assert_eq!(s(n), format!("_{n}"), "{n}");
        }
        assert_eq!(s("nul.txt"), "_nul.txt");
        assert_eq!(s("CON.tar.gz"), "_CON.tar.gz");
        assert_eq!(s("con .png"), "_con .png");
    }

    #[test]
    fn near_reserved_names_untouched() {
        for n in ["CONSOLE", "COM10", "COM", "LPT", "NULL", "AUXILIARY", "com0x", "xCON"] {
            assert_eq!(s(n), n, "{n}");
        }
    }

    #[test]
    fn bidi_overrides_removed() {
        assert_eq!(s("evil\u{202e}gpj.exe"), "evilgpj.exe");
    }

    #[test]
    fn truncation_keeps_extension_and_graphemes() {
        let long = format!("{}.png", "a".repeat(400));
        let out = s(&long);
        assert_eq!(out.len(), 255);
        assert!(out.ends_with(".png"));

        // 4-byte emoji family (ZWJ sequence) must not be split.
        let family = "👨‍👩‍👧‍👦";
        let name = format!("{}.png", family.repeat(30));
        let out = s(&name);
        assert!(out.len() <= 255);
        assert!(out.ends_with(".png"));
        let stem = out.strip_suffix(".png").unwrap_or("");
        assert!(stem.len() % family.len() == 0, "cut inside a grapheme: {stem:?}");
    }

    #[test]
    fn truncation_on_unicode_boundaries() {
        let name = "é".repeat(300);
        let out = s(&name);
        assert!(out.len() <= 255);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn truncation_cannot_create_reserved_name() {
        let o = SanitizeOptions { max_bytes: 8, ..SanitizeOptions::default() };
        let out = sanitize_file_name("CONSOLE_LONG_NAME", &o);
        assert!(is_valid_file_name(&out, 8), "{out}");
    }

    #[test]
    fn truncate_helpers() {
        assert_eq!(truncate_graphemes("héllo", 2), "hé");
        assert_eq!(truncate_graphemes("e\u{301}x", 1), "e\u{301}");
        assert_eq!(truncate_graphemes("abc", 10), "abc");
        assert_eq!(truncate_graphemes("abc", 0), "");
        assert_eq!(truncate_bytes("ééé", 3), "é");
        assert_eq!(truncate_bytes("abc", 10), "abc");
    }

    #[test]
    fn relative_path_sanitising() {
        let o = SanitizeOptions::default();
        assert_eq!(sanitize_relative_path("2024-05/Shots", &o), PathBuf::from("2024-05/Shots"));
        assert_eq!(sanitize_relative_path("../../etc/passwd", &o), PathBuf::from("etc/passwd"));
        assert_eq!(sanitize_relative_path("/abs//dir/./x", &o), PathBuf::from("abs/dir/x"));
        assert_eq!(
            sanitize_relative_path("C:\\Windows\\System32", &o),
            PathBuf::from("C/Windows/System32")
        );
        assert_eq!(sanitize_relative_path("con/nul", &o), PathBuf::from("_con/_nul"));
        assert_eq!(sanitize_relative_path("", &o), PathBuf::new());
        assert_eq!(sanitize_relative_path("..", &o), PathBuf::new());
    }

    proptest! {
        #[test]
        fn output_is_always_a_valid_file_name(input in "\\PC*") {
            let out = s(&input);
            prop_assert!(is_valid_file_name(&out, 255), "{:?} -> {:?}", input, out);
            // idempotent
            prop_assert_eq!(s(&out), out);
        }

        #[test]
        fn output_valid_for_nasty_alphabet(input in "[ .<>:\"/\\\\|?*a-zA-Z0-9\u{0}-\u{1f}\u{202e}éあ👍]{0,400}") {
            let out = s(&input);
            prop_assert!(is_valid_file_name(&out, 255), "{:?} -> {:?}", input, out);
            prop_assert!(out.len() <= 255);
            prop_assert!(out.encode_utf16().count() <= 255);
        }

        #[test]
        fn small_limits_still_valid(input in "\\PC{0,60}", max in 8usize..64) {
            let o = SanitizeOptions { max_bytes: max, ..SanitizeOptions::default() };
            let out = sanitize_file_name(&input, &o);
            prop_assert!(is_valid_file_name(&out, max), "{:?} -> {:?}", input, out);
        }

        #[test]
        fn relative_paths_never_escape(input in "[a-z./\\\\: ]{0,60}") {
            let p = sanitize_relative_path(&input, &SanitizeOptions::default());
            prop_assert!(!p.is_absolute());
            for c in p.components() {
                prop_assert!(matches!(c, std::path::Component::Normal(_)), "{:?}", p);
            }
        }
    }
}
