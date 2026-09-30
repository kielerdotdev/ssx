//! Terminal output helpers: colour policy, simple tables, human-readable values.
//!
//! Colour follows the usual rules: never when `NO_COLOR` is set (any value), never when the
//! stream is not a terminal or `TERM=dumb`, and overridable with `--color always|never`.
//! Data goes to **stdout**, everything chatty (progress, warnings, errors) to **stderr**, so
//! `ssx upload a.png | xclip` and `ssx capture ... --json | jq` stay clean.

use std::{
    fmt::Write as _,
    io::{IsTerminal, Write},
};

use clap::ValueEnum;

/// `--color` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum ColorChoice {
    /// Colour when writing to a terminal and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always colour.
    Always,
    /// Never colour.
    Never,
}

/// Decides whether to colour, from the choice and the environment.
pub fn use_color(choice: ColorChoice, is_terminal: bool, no_color: bool, term_dumb: bool) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => is_terminal && !no_color && !term_dumb,
    }
}

/// ANSI styling for one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    on: bool,
}

impl Style {
    /// Styling for stdout or stderr, per `choice` and the environment.
    pub fn for_stream(choice: ColorChoice, stderr: bool) -> Self {
        let tty = if stderr { std::io::stderr().is_terminal() } else { std::io::stdout().is_terminal() };
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
        Self { on: use_color(choice, tty, no_color, dumb) }
    }

    /// No styling (tests, pipes).
    pub const fn plain() -> Self {
        Self { on: false }
    }

    /// Always styled.
    pub const fn colored() -> Self {
        Self { on: true }
    }

    fn wrap(self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }

    /// Bold.
    pub fn bold(self, t: &str) -> String {
        self.wrap("1", t)
    }
    /// Dim.
    pub fn dim(self, t: &str) -> String {
        self.wrap("2", t)
    }
    /// Red.
    pub fn red(self, t: &str) -> String {
        self.wrap("1;31", t)
    }
    /// Yellow.
    pub fn yellow(self, t: &str) -> String {
        self.wrap("33", t)
    }
    /// Green.
    pub fn green(self, t: &str) -> String {
        self.wrap("32", t)
    }
    /// Cyan.
    pub fn cyan(self, t: &str) -> String {
        self.wrap("36", t)
    }
}

/// A left-aligned text table. Column widths are measured in characters, which is right for
/// the ASCII-and-Latin data shown here and degrades gracefully for the rest.
#[derive(Debug, Clone, Default)]
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with the given column titles.
    pub fn new<S: Into<String>>(header: impl IntoIterator<Item = S>) -> Self {
        Self { header: header.into_iter().map(Into::into).collect(), rows: Vec::new() }
    }

    /// Adds a row (missing cells are blank, extra cells dropped).
    pub fn row<S: Into<String>>(&mut self, cells: impl IntoIterator<Item = S>) {
        let mut r: Vec<String> = cells.into_iter().map(Into::into).collect();
        r.resize(self.header.len(), String::new());
        self.rows.push(r);
    }

    /// Number of data rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// `true` if there are no data rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Renders the table; the header is bold in `style`. Trailing spaces are trimmed.
    pub fn render(&self, style: Style) -> String {
        let cols = self.header.len();
        let widths: Vec<usize> = (0..cols)
            .map(|c| {
                self.rows
                    .iter()
                    .map(|r| r[c].chars().count())
                    .chain(std::iter::once(self.header[c].chars().count()))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let mut out = String::new();
        let mut line = |cells: &[String], head: bool| {
            let mut s = String::new();
            for (i, cell) in cells.iter().enumerate() {
                let pad = widths[i].saturating_sub(cell.chars().count());
                let text = if head { style.bold(cell) } else { cell.clone() };
                s.push_str(&text);
                if i + 1 < cols {
                    s.push_str(&" ".repeat(pad + 2));
                }
            }
            let _ = writeln!(out, "{}", s.trim_end());
        };
        line(&self.header, true);
        for r in &self.rows {
            line(r, false);
        }
        out
    }
}

/// `1234567` -> `1.2 MB` (decimal units, like file managers on most platforms).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Local time `2026-09-30 12:34:56` for Unix milliseconds.
pub fn local_time(ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(|| format!("@{ms}"), |t| t.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// RFC 3339 UTC time for Unix milliseconds (JSON output).
pub fn rfc3339(ms: i64) -> String {
    use chrono::TimeZone as _;
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map_or_else(|| ms.to_string(), |t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// Prints a line to stdout, ignoring a closed pipe (`ssx history list | head`).
pub fn out_line(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
}

/// Prints text to stdout without adding a newline, ignoring a closed pipe.
pub fn out_text(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "{text}");
}

/// Prints a line to stderr.
pub fn err_line(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{text}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_policy() {
        // (choice, tty, NO_COLOR, TERM=dumb) -> colour?
        assert!(use_color(ColorChoice::Auto, true, false, false));
        assert!(!use_color(ColorChoice::Auto, false, false, false), "pipes are never coloured");
        assert!(!use_color(ColorChoice::Auto, true, true, false), "NO_COLOR wins");
        assert!(!use_color(ColorChoice::Auto, true, false, true), "TERM=dumb");
        assert!(use_color(ColorChoice::Always, false, true, true), "--color always is explicit");
        assert!(!use_color(ColorChoice::Never, true, false, false));
    }

    #[test]
    fn styles_wrap_only_when_on() {
        assert_eq!(Style::plain().red("x"), "x");
        assert_eq!(Style::colored().red("x"), "\x1b[1;31mx\x1b[0m");
        assert_eq!(Style::colored().dim("x"), "\x1b[2mx\x1b[0m");
    }

    #[test]
    fn tables_align_and_trim() {
        let mut t = Table::new(["ID", "NAME"]);
        t.row(["1", "alpha"]);
        t.row(["1234", "b"]);
        t.row(["7"]); // short row: padded
        assert_eq!(t.len(), 3);
        assert_eq!(t.render(Style::plain()), "ID    NAME\n1     alpha\n1234  b\n7\n");
        assert!(Table::new(["A"]).is_empty());
    }

    #[test]
    fn byte_sizes_are_human() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1000), "1.0 kB");
        assert_eq!(human_bytes(1_234_567), "1.2 MB");
        assert_eq!(human_bytes(u64::MAX).ends_with("TB"), true);
    }

    #[test]
    fn times_render_in_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(local_time(0).len(), "1970-01-01 00:00:00".len());
    }
}
