//! Escaping for every syntax we embed the ssx path or labels into.
//!
//! This is the security-critical module. Rules of the house:
//! * **User file names never pass through here.** They travel as argv (`%F`, `"$@"`, `%1`,
//!   Python lists). Only *our own* strings (the ssx executable path, labels) are escaped.
//! * Where a format re-parses text (shell, Desktop Entry `Exec=`, `GKeyFile`, XML, batch),
//!   there is exactly one function per syntax, unit-tested against a small reference parser
//!   with hostile input.

use crate::error::{Result, ShellError};

/// Appends formatted text to a `String` (writing to a `String` cannot fail).
macro_rules! push_fmt {
    ($dst:expr, $($arg:tt)*) => {{
        use std::fmt::Write as _;
        let _ = write!($dst, $($arg)*);
    }};
}
pub(crate) use push_fmt;

/// `a;b;c;` - the list syntax of Desktop Entry / Nemo keys (every item terminated).
pub fn semicolon_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut s = String::new();
    for i in items {
        s.push_str(i);
        s.push(';');
    }
    s
}

/// POSIX shell single-quoting: `'` becomes `'\''`. Safe for any string without NUL.
pub fn sh_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Escapes a string for use as a `GKeyFile` / Desktop Entry *string value*: backslash,
/// newline, tab and carriage return, and a leading space (`\s`).
pub fn keyfile_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' ' if i == 0 => out.push_str("\\s"),
            c => out.push(c),
        }
    }
    out
}

/// One argument of a Desktop Entry `Exec=` line, ready to be written after `Exec=`.
///
/// Implements the Desktop Entry Specification: an argument with reserved characters is put in
/// double quotes with `"`, `` ` ``, `$` and `\` backslash-escaped; a literal `%` becomes `%%`;
/// finally the whole thing is escaped as a key-file string (so each backslash doubles).
pub fn desktop_exec_arg(arg: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
    let mut word = String::with_capacity(arg.len() + 2);
    let needs_quotes = arg.is_empty() || arg.chars().any(|c| RESERVED.contains(c));
    if needs_quotes {
        word.push('"');
    }
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' if needs_quotes => {
                word.push('\\');
                word.push(c);
            }
            '%' => word.push_str("%%"),
            c => word.push(c),
        }
    }
    if needs_quotes {
        word.push('"');
    }
    keyfile_value(&word)
}

/// A shell word left bare when it only has safe characters, single-quoted otherwise.
pub fn sh_word(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.+/@:,=".contains(&b)) {
        s.to_owned()
    } else {
        sh_single_quote(s)
    }
}

/// A word for `g_shell_parse_argv`-style command strings (Thunar `<command>`, Nemo `Exec=`
/// before key-file escaping): quoted when needed, with `%` doubled so it is not a field code.
pub fn shell_word_with_field_codes(arg: &str) -> String {
    sh_word(arg).replace('%', "%%")
}

/// Escapes text for XML character data (element content): `&`, `<`, `>` only.
pub fn xml_escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Escapes text for XML attribute values (and, conservatively, anything else).
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// A Python 3 string literal (double-quoted, ASCII only) for `s`.
pub fn py_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (' '..='~').contains(&c) => out.push(c),
            c if u32::from(c) <= 0xffff => push_fmt!(out, "\\u{:04x}", u32::from(c)),
            c => push_fmt!(out, "\\U{:08x}", u32::from(c)),
        }
    }
    out.push('"');
    out
}

/// Shell `case` pattern matching `*.<ext>` case-insensitively: `*.[pP][nN][gG]`.
///
/// `ext` must be ASCII alphanumeric (validated by [`crate::Action::validate`]).
pub fn sh_case_ext_pattern(ext: &str) -> String {
    let mut out = String::from("*.");
    for c in ext.chars() {
        if c.is_ascii_alphabetic() {
            out.push('[');
            out.push(c.to_ascii_lowercase());
            out.push(c.to_ascii_uppercase());
            out.push(']');
        } else {
            out.push(c);
        }
    }
    out
}

/// Rejects strings that cannot be represented in any of our formats (NUL, control chars).
pub fn ensure_plain_text(what: &str, s: &str) -> Result<()> {
    if s.chars().any(char::is_control) {
        return Err(ShellError::InvalidAction {
            id: what.to_owned(),
            reason: "contains a control character".into(),
        });
    }
    Ok(())
}

/// Quotes a Windows path for a registry command line: `"C:\path\ssx.exe"`.
///
/// Windows file names cannot contain `"`, so plain quoting is exact; anything with a quote or
/// a control character is refused.
pub fn win_command_quote(path: &str) -> Result<String> {
    if path.contains('"') || path.chars().any(char::is_control) {
        return Err(ShellError::InvalidAction {
            id: "windows command line".into(),
            reason: format!("path {path:?} contains a quote or control character"),
        });
    }
    Ok(format!("\"{path}\""))
}

/// Quotes a Windows path for use inside a `.cmd` file: `"C:\path\ssx.exe"` with `%` doubled
/// (the batch parser expands `%VAR%` even inside quotes).
pub fn win_batch_quote(path: &str) -> Result<String> {
    Ok(win_command_quote(path)?.replace('%', "%%"))
}

/// Reference parsers used only by tests to prove the escapers above round-trip.
#[cfg(test)]
pub(crate) mod reference {
    /// Undoes `GKeyFile` string escaping.
    pub fn keyfile_unescape(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next() {
                Some('s') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                other => panic!("bad key-file escape {other:?} in {s:?}"),
            }
        }
        out
    }

    /// Splits a Desktop Entry `Exec` value (as written in the file) into arguments, applying
    /// the spec's quoting rules and turning `%%` into `%`. Field codes like `%F` stay as-is.
    pub fn parse_desktop_exec(file_value: &str) -> Vec<String> {
        let s = keyfile_unescape(file_value);
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_word = false;
        let mut quoted = false;
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if quoted {
                match c {
                    '"' => quoted = false,
                    '\\' => {
                        let n = it.next().expect("dangling backslash");
                        assert!("\"`$\\".contains(n), "bad escape \\{n} in {s:?}");
                        cur.push(n);
                    }
                    c => cur.push(c),
                }
            } else {
                match c {
                    '"' => {
                        quoted = true;
                        in_word = true;
                    }
                    ' ' | '\t' | '\n' => {
                        if in_word {
                            args.push(std::mem::take(&mut cur));
                            in_word = false;
                        }
                    }
                    c => {
                        assert!(
                            !"'\\><~|&;$*?#()`".contains(c),
                            "reserved char {c:?} outside quotes in {s:?}"
                        );
                        cur.push(c);
                        in_word = true;
                    }
                }
            }
        }
        assert!(!quoted, "unterminated quote in {s:?}");
        if in_word {
            args.push(cur);
        }
        // Field codes are expanded after quoting is undone, so `%%` -> `%` applies to all words.
        args.into_iter().map(|a| a.replace("%%", "%")).collect()
    }

    /// Minimal `g_shell_parse_argv` (POSIX word splitting, no expansion). `%%` is
    /// decoded afterwards by the caller if the consumer treats it as an escape.
    pub fn parse_shell_words(s: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_word = false;
        let mut it = s.chars();
        while let Some(c) = it.next() {
            match c {
                '\'' => {
                    in_word = true;
                    loop {
                        match it.next() {
                            Some('\'') => break,
                            Some(c) => cur.push(c),
                            None => panic!("unterminated single quote in {s:?}"),
                        }
                    }
                }
                '"' => {
                    in_word = true;
                    loop {
                        match it.next() {
                            Some('"') => break,
                            Some('\\') => cur.push(it.next().expect("dangling backslash")),
                            Some(c) => cur.push(c),
                            None => panic!("unterminated double quote in {s:?}"),
                        }
                    }
                }
                '\\' => {
                    in_word = true;
                    cur.push(it.next().expect("dangling backslash"));
                }
                c if c.is_whitespace() => {
                    if in_word {
                        args.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                c => {
                    cur.push(c);
                    in_word = true;
                }
            }
        }
        if in_word {
            args.push(cur);
        }
        args
    }

    /// Decodes a literal produced by `py_str`.
    pub fn parse_py_str(lit: &str) -> String {
        let inner = lit.strip_prefix('"').and_then(|s| s.strip_suffix('"')).expect("quotes");
        let mut out = String::new();
        let mut it = inner.chars();
        while let Some(c) = it.next() {
            assert!(c != '"', "unescaped quote");
            if c != '\\' {
                out.push(c);
                continue;
            }
            match it.next().expect("escape") {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let h: String = it.by_ref().take(4).collect();
                    out.push(
                        char::from_u32(u32::from_str_radix(&h, 16).expect("hex")).expect("char"),
                    );
                }
                'U' => {
                    let h: String = it.by_ref().take(8).collect();
                    out.push(
                        char::from_u32(u32::from_str_radix(&h, 16).expect("hex")).expect("char"),
                    );
                }
                other => panic!("unknown escape \\{other}"),
            }
        }
        out
    }

    /// Deterministic pseudo-random hostile strings.
    pub fn hostile_strings(n: usize) -> Vec<String> {
        const ALPHABET: &[char] = &[
            ' ', ' ', '"', '\'', '`', '$', '\\', '\n', '\t', '\r', '%', '&', ';', '|', '<', '>',
            '*', '?', '(', ')', '#', '~', '!', '-', '-', '=', '{', '}', '[', ']', 'a', 'b', 'Z',
            '0', 'é', '日', '🦀', '\u{202e}', '/', '.',
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut out: Vec<String> = [
            "",
            " ",
            "plain",
            "with space",
            "it's",
            "say \"hi\"",
            "$(rm -rf ~)",
            "`id`",
            "-rf",
            "--help",
            "a\nb",
            "100%",
            "%F",
            "%%",
            "back\\slash",
            "trailing\\",
            "~",
            "*",
            "C:\\Program Files\\ssx\\ssx.exe",
            "/usr/bin/ss x",
            "日本語.png",
            "🦀 crab.png",
            "'; touch /tmp/pwned; '",
            "\"; touch /tmp/pwned; \"",
            "$HOME",
            "${IFS}",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for _ in 0..n {
            let len = (next() % 24) as usize;
            let s: String =
                (0..len).map(|_| ALPHABET[(next() % ALPHABET.len() as u64) as usize]).collect();
            out.push(s);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::reference::*;
    use super::*;

    #[test]
    fn single_quote_roundtrips() {
        for s in hostile_strings(2000) {
            let q = sh_single_quote(&s);
            assert_eq!(parse_shell_words(&q), vec![s.clone()], "{s:?} -> {q}");
        }
    }

    #[test]
    fn desktop_exec_arg_roundtrips() {
        for s in hostile_strings(2000) {
            let line = format!("{} post-file -- %F", desktop_exec_arg(&s));
            let args = parse_desktop_exec(&line);
            assert_eq!(args.first(), Some(&s), "{s:?} -> {line}");
            assert_eq!(&args[1..], ["post-file", "--", "%F"], "{s:?} -> {line}");
        }
    }

    #[test]
    fn desktop_exec_examples() {
        assert_eq!(desktop_exec_arg("/usr/bin/ssx"), "/usr/bin/ssx");
        assert_eq!(desktop_exec_arg("/opt/my apps/ssx"), "\"/opt/my apps/ssx\"");
        assert_eq!(desktop_exec_arg("/opt/100%/ssx"), "/opt/100%%/ssx");
        // `$` inside quotes: escaped once for the Exec quoting, doubled for the key file.
        assert_eq!(desktop_exec_arg("/o p/$x"), "\"/o p/\\\\$x\"");
    }

    #[test]
    fn shell_word_with_field_codes_roundtrips() {
        for s in hostile_strings(1000) {
            let w = shell_word_with_field_codes(&s);
            let parsed = parse_shell_words(&w);
            assert_eq!(parsed.len(), 1, "{s:?}");
            assert_eq!(parsed[0].replace("%%", "%"), s, "{s:?}");
        }
    }

    #[test]
    fn keyfile_value_roundtrips() {
        for s in hostile_strings(1000) {
            assert_eq!(keyfile_unescape(&keyfile_value(&s)), s, "{s:?}");
        }
    }

    #[test]
    fn python_literals_roundtrip_and_are_ascii() {
        for s in hostile_strings(1000) {
            let lit = py_str(&s);
            assert!(lit.is_ascii() && !lit.contains('\n'), "{lit}");
            assert_eq!(parse_py_str(&lit), s);
        }
    }

    #[test]
    fn xml_escape_examples() {
        assert_eq!(xml_escape(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&apos;");
    }

    #[test]
    fn case_pattern() {
        assert_eq!(sh_case_ext_pattern("png"), "*.[pP][nN][gG]");
        assert_eq!(sh_case_ext_pattern("mp4"), "*.[mM][pP]4");
    }

    #[test]
    fn windows_quoting() {
        assert_eq!(
            win_command_quote(r"C:\Program Files\ssx\ssx.exe").expect("ok"),
            r#""C:\Program Files\ssx\ssx.exe""#
        );
        assert_eq!(win_batch_quote(r"C:\100%\ssx.exe").expect("ok"), r#""C:\100%%\ssx.exe""#);
        assert!(win_command_quote("C:\\a\"b\\ssx.exe").is_err());
        assert!(win_command_quote("C:\\a\nb").is_err());
    }
}
