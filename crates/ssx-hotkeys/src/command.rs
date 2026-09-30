//! [`Command`]: the program a hotkey launches, and how to spell it safely for each target.
//!
//! A command is *structured* (program + argument vector), never a shell string, because
//! every target parses its command text differently:
//!
//! | target | who parses the text | quoting used |
//! |---|---|---|
//! | sway | sway's command parser (with an extra unescape for a `bindsym`), then `sh -c` | POSIX line with double-quoted words; see [`sway_exec_arg`], [`sway_bindsym_exec_arg`] |
//! | Hyprland | config parser (`$var`, `#`), then `sh -c` | POSIX line, `$` and `#` neutralised; see [`hyprland_exec_arg`] |
//! | GNOME | GLib `g_shell_parse_argv` (no shell) | POSIX single quotes inside a GVariant string |
//! | KDE | Desktop Entry `Exec=` parser (no shell) | `Exec` double-quote rules + string escapes |
//!
//! The sway rules were found empirically against a real sway (it splits commands at `;` and
//! `,`, expands `$name`, and protects only double-quoted text); the test-suite replays a
//! corpus of hostile arguments through it, including real key presses.

use std::fmt::Write as _;

/// Why a [`Command`] cannot be rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// The program name is empty.
    #[error("command has an empty program name")]
    EmptyProgram,
    /// NUL, newline or carriage return cannot be expressed on one config line.
    #[error("command contains a {0} character, which cannot be written on one config line")]
    ControlCharacter(&'static str),
}

/// A program and its arguments, plus an optional human label.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Command {
    program: String,
    args: Vec<String>,
    label: Option<String>,
}

impl Command {
    /// A command running `program` (looked up in `PATH` by the desktop).
    pub fn new(program: impl Into<String>) -> Self {
        Self { program: program.into(), args: Vec::new(), label: None }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets the label shown in settings UIs (GNOME/KDE), e.g. `"Capture region"`.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The program.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments.
    pub fn arguments(&self) -> &[String] {
        &self.args
    }

    /// Program followed by arguments.
    pub fn words(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.program.as_str()).chain(self.args.iter().map(String::as_str))
    }

    /// The label, or the POSIX command line when none was set.
    pub fn display_name(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.shell_line())
    }

    /// Checks the command can be written on a single config line.
    pub fn validate(&self) -> Result<(), CommandError> {
        if self.program.is_empty() {
            return Err(CommandError::EmptyProgram);
        }
        for w in self.words() {
            for (c, what) in [('\0', "NUL"), ('\n', "newline"), ('\r', "carriage return")] {
                if w.contains(c) {
                    return Err(CommandError::ControlCharacter(what));
                }
            }
        }
        Ok(())
    }

    /// The command as a POSIX shell line (each word quoted only if needed).
    pub fn shell_line(&self) -> String {
        self.words().map(quote_posix).collect::<Vec<_>>().join(" ")
    }
}

fn is_posix_safe(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '%' | '+' | ':' | ',' | '.' | '/' | '@' | '-')
}

/// Quotes one word for a POSIX shell (`sh`, `dash`, `bash`, `zsh`): bare when every
/// character is harmless, otherwise single-quoted with `'` written as `'\''`.
pub fn quote_posix(word: &str) -> String {
    quote_with(word, |c| match c {
        '\'' => Some("'\\''"),
        _ => None,
    })
}

/// Single-quote `word` (unless it is entirely safe), replacing chosen characters by the
/// given replacement *inside* the quotes (each replacement must itself close and reopen
/// the quotes).
fn quote_with(word: &str, replace: impl Fn(char) -> Option<&'static str>) -> String {
    if !word.is_empty() && word.chars().all(is_posix_safe) {
        return word.to_owned();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    for c in word.chars() {
        match replace(c) {
            Some(r) => out.push_str(r),
            None => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The text after `exec` for a sway command that is parsed once by sway (`exec_always`,
/// `swaymsg exec`, a runtime `exec`): a POSIX shell line whose words are double-quoted
/// wherever needed.
///
/// Why double quotes: sway splits a command at `;` and `,` and expands `$variables`, and it
/// only protects `;`/`,` inside *double*-quoted strings (its single-quote handling is
/// parity-based and breaks on the `'\''` idiom). Inside `"..."` sway leaves the text alone
/// and hands it to `sh -c`, where POSIX double quotes are equally valid, so a word is
/// written as `"..."` with `\`, `"`, `$` and `` ` `` backslash-escaped (also stopping sway
/// expanding `$name`), and only words made purely of harmless characters stay bare.
pub fn sway_exec_arg(cmd: &Command) -> String {
    cmd.words().map(|w| quote_sway_word(w, false)).collect::<Vec<_>>().join(" ")
}

/// The text after `exec` in a sway **`bindsym`** line.
///
/// A `bindsym` line goes through one extra unescaping step compared with
/// [`sway_exec_arg`]: inside a double-quoted string sway turns `\\` into `\` and `\"` into
/// `"` before storing the command (found by injecting real key presses into a headless
/// sway and reading the `sh -c` argument with `strace`; the single-pass form works for
/// `exec_always` but loses the backslash of any escaped `"` or `\`). So the escapes for
/// `\` and `"` are escaped once more. `\$` and `` \` `` are left alone by that unescaping,
/// and the backslash in front of `$` is what stops sway expanding `$name` at load time.
pub fn sway_bindsym_exec_arg(cmd: &Command) -> String {
    cmd.words().map(|w| quote_sway_word(w, true)).collect::<Vec<_>>().join(" ")
}

fn quote_sway_word(word: &str, bindsym: bool) -> String {
    if !word.is_empty() && word.chars().all(|c| is_posix_safe(c) && !matches!(c, ',' | ';')) {
        return word.to_owned();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    for c in word.chars() {
        match c {
            // The shell needs `\\` and `\"` inside double quotes. For a `bindsym`, sway's
            // extra unescaping then eats one level of both characters, so that two-character
            // sequence is escaped again (`\\` -> `\\\\`, `\"` -> `\\\"`).
            '\\' | '"' => {
                if bindsym {
                    out.push_str("\\\\");
                }
                out.push('\\');
                out.push(c);
            }
            '$' | '`' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `params` field of a Hyprland `bind = MODS, key, exec, <params>` line.
///
/// Hyprland's config parser substitutes `$variables` and starts a comment at `#` (written
/// `##` for a literal one) before `exec` runs the text with `sh -c`. `$` is emitted as
/// `'"$"'`, which the shell reads as a literal `$` but which never forms `$name` for the
/// parser; `#` is doubled.
pub fn hyprland_exec_arg(cmd: &Command) -> String {
    let line = cmd
        .words()
        .map(|w| {
            quote_with(w, |c| match c {
                '\'' => Some("'\\''"),
                '$' => Some("'\"$\"'"),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join(" ");
    line.replace('#', "##")
}

/// A GVariant text-format string literal: `'it\'s'`.
pub fn gvariant_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// A GVariant array of strings, e.g. `['a', 'b']` (`@as []` when empty).
pub fn gvariant_string_array<S: AsRef<str>>(items: &[S]) -> String {
    if items.is_empty() {
        return "@as []".to_owned();
    }
    let inner: Vec<String> = items.iter().map(|s| gvariant_string(s.as_ref())).collect();
    format!("[{}]", inner.join(", "))
}

/// The value of a Desktop Entry `Exec=` key for `cmd`.
///
/// Follows the Desktop Entry Specification: arguments containing reserved characters are
/// double-quoted with `"`, `` ` ``, `$` and `\` backslash-escaped, `%` is doubled (field
/// codes), and the whole value is then string-escaped (`\` becomes `\\`) because `Exec`
/// is a `string` key.
pub fn desktop_exec_value(cmd: &Command) -> String {
    cmd.words().map(desktop_exec_word).collect::<Vec<_>>().join(" ")
}

fn desktop_exec_word(word: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`";
    let word = word.replace('%', "%%");
    let quoted = if word.is_empty() || word.chars().any(|c| RESERVED.contains(c)) {
        let mut q = String::with_capacity(word.len() + 2);
        q.push('"');
        for c in word.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q
    } else {
        word
    };
    quoted.replace('\\', "\\\\")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Awkward words every quoting function must survive.
    pub(crate) const CORPUS: &[&str] = &[
        "plain",
        "with space",
        "  leading and trailing  ",
        "it's",
        "two'quotes'here",
        "\"double\"",
        "mixed ' and \"",
        "$HOME",
        "${HOME}",
        "$(whoami)",
        "`whoami`",
        "semi;colon",
        "com,ma",
        "a;b,c;d",
        "&& ||",
        "pipe|pipe",
        "redir > out < in",
        "glob*?[x]",
        "tilde ~ and ~/x",
        "hash # not comment",
        "##",
        "back\\slash",
        "trailing\\",
        "\\\"",
        "\\\\\"",
        "\"\\",
        "percent %s %%",
        "{brace}",
        "!bang",
        "=equals",
        "FOO=bar",
        "-dash",
        "--opt=val ue",
        "",
        "ünïcödé 日本語 🙂",
        "tab\there",
        "$x $mod $term",
        "a=b c",
    ];

    #[test]
    fn posix_quoting_basics() {
        assert_eq!(quote_posix("plain"), "plain");
        assert_eq!(quote_posix("/usr/bin/ssx"), "/usr/bin/ssx");
        assert_eq!(quote_posix("a b"), "'a b'");
        assert_eq!(quote_posix("it's"), "'it'\\''s'");
        assert_eq!(quote_posix(""), "''");
        assert_eq!(quote_posix("$x"), "'$x'");
        assert_eq!(quote_posix("a;b"), "'a;b'");
        assert_eq!(quote_posix("FOO=bar"), "'FOO=bar'", "env-assignment lookalikes are quoted");
        assert_eq!(quote_posix("日本"), "'日本'");
    }

    #[test]
    fn command_builder_and_validation() {
        let c = Command::new("ssx").arg("capture").args(["region", "--copy"]).label("Region");
        assert_eq!(c.shell_line(), "ssx capture region --copy");
        assert_eq!(c.display_name(), "Region");
        assert_eq!(Command::new("ssx").arg("a b").display_name(), "ssx 'a b'");
        assert_eq!(Command::new("").validate(), Err(CommandError::EmptyProgram));
        assert!(matches!(
            Command::new("x").arg("a\nb").validate(),
            Err(CommandError::ControlCharacter("newline"))
        ));
        assert!(matches!(
            Command::new("x\0").validate(),
            Err(CommandError::ControlCharacter("NUL"))
        ));
        assert!(
            Command::new("x")
                .args(CORPUS.iter().copied().filter(|s| !s.contains('\n')))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn gvariant_escaping() {
        assert_eq!(gvariant_string("plain"), "'plain'");
        assert_eq!(gvariant_string("it's"), "'it\\'s'");
        assert_eq!(gvariant_string("a\\b"), "'a\\\\b'");
        assert_eq!(gvariant_string("l1\nl2"), "'l1\\nl2'");
        assert_eq!(gvariant_string("\u{1}"), "'\\u0001'");
        assert_eq!(gvariant_string("日本 🙂"), "'日本 🙂'");
        assert_eq!(gvariant_string_array::<&str>(&[]), "@as []");
        assert_eq!(gvariant_string_array(&["a", "b'c"]), "['a', 'b\\'c']");
    }

    #[test]
    fn hyprland_neutralises_variables_and_comments() {
        let c = Command::new("ssx").arg("$mainMod").arg("a # b");
        assert_eq!(hyprland_exec_arg(&c), "ssx ''\"$\"'mainMod' 'a ## b'");
        // No `$` directly followed by a name character survives.
        for w in CORPUS {
            let out = hyprland_exec_arg(&Command::new("x").arg(*w));
            assert!(
                !out.as_bytes().windows(2).any(|p| p[0] == b'$'
                    && (p[1].is_ascii_alphanumeric() || p[1] == b'_' || p[1] == b'{')),
                "{w:?} -> {out}"
            );
            assert!(!out.replace("##", "").contains('#'), "single # left in {out}");
        }
    }

    /// Minimal Desktop Entry `Exec` parser per the spec, to check our output round-trips.
    fn parse_desktop_exec(value: &str) -> Vec<String> {
        // 1. string-value unescape
        let mut s = String::new();
        let mut it = value.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('\\') | None => s.push('\\'),
                    Some(o) => {
                        s.push('\\');
                        s.push(o);
                    }
                }
            } else {
                s.push(c);
            }
        }
        // 2. split on spaces outside double quotes, handling \ escapes inside quotes, %%.
        let mut args = Vec::new();
        let mut cur = String::new();
        let (mut in_q, mut have) = (false, false);
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '"' => {
                    in_q = !in_q;
                    have = true;
                }
                '\\' if in_q => {
                    if let Some(n) = it.next() {
                        cur.push(n);
                    }
                }
                '%' if it.peek() == Some(&'%') => {
                    it.next();
                    cur.push('%');
                }
                ' ' if !in_q => {
                    if have || !cur.is_empty() {
                        args.push(std::mem::take(&mut cur));
                        have = false;
                    }
                }
                c => cur.push(c),
            }
        }
        if have || !cur.is_empty() {
            args.push(cur);
        }
        args
    }

    #[test]
    fn desktop_exec_round_trips_the_corpus() {
        for w in CORPUS {
            let cmd = Command::new("ssx").arg(*w);
            let value = desktop_exec_value(&cmd);
            assert_eq!(
                parse_desktop_exec(&value),
                vec!["ssx".to_owned(), (*w).to_owned()],
                "{w:?} -> {value}"
            );
        }
        let all = Command::new("/opt/my app/ssx").args(CORPUS.iter().copied());
        let mut want = vec!["/opt/my app/ssx".to_owned()];
        want.extend(CORPUS.iter().map(|s| (*s).to_owned()));
        assert_eq!(parse_desktop_exec(&desktop_exec_value(&all)), want);
    }

    #[test]
    fn desktop_exec_literal_examples() {
        assert_eq!(
            desktop_exec_value(&Command::new("ssx").arg("capture").arg("region")),
            "ssx capture region"
        );
        assert_eq!(desktop_exec_value(&Command::new("ssx").arg("a b")), "ssx \"a b\"");
        assert_eq!(desktop_exec_value(&Command::new("ssx").arg("100%")), "ssx 100%%");
        // A backslash inside quotes is escaped twice: once for Exec, once for the string.
        assert_eq!(desktop_exec_value(&Command::new("ssx").arg("a b\\c")), "ssx \"a b\\\\\\\\c\"");
    }

    #[cfg(unix)]
    #[test]
    fn posix_quoting_round_trips_through_a_real_shell() {
        let words: Vec<&str> = CORPUS.to_vec();
        let cmd = Command::new("printed").args(words.iter().copied());
        let script = format!("printf '%s\\0' {}", cmd.shell_line());
        let out = std::process::Command::new("sh").arg("-c").arg(&script).output();
        let Ok(out) = out else {
            eprintln!("SKIP: no sh");
            return;
        };
        let got: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
        let mut want: Vec<&str> = vec!["printed"];
        want.extend(words.iter().copied());
        assert_eq!(got.len(), want.len() + 1, "trailing NUL yields one empty tail");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(*g, w.as_bytes(), "word {w:?}");
        }
    }
}
