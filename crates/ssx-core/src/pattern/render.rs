//! Evaluating a parsed [`Pattern`] against a [`PatternContext`].

use std::path::PathBuf;

use chrono::{Datelike, Timelike};

use super::{
    PatternError,
    context::{PatternContext, PatternKind, UnknownTokens},
    parse::{Node, Pattern, Token},
    sanitize::{
        SanitizeOptions, remove_illegal, sanitize_file_name, sanitize_relative_path,
        truncate_graphemes,
    },
    words::{ADJECTIVES, ANIMALS, EMOJI},
};

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] =
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

impl Pattern {
    /// Renders the pattern.
    ///
    /// * [`PatternKind::FileName`]: result is a valid single file name (stem; see
    ///   [`render_file_name`](super::render_file_name) for the extension).
    /// * [`PatternKind::FilePath`]: result is a `/`-joined relative path.
    /// * [`PatternKind::Text`]: raw text, `%n` is a newline.
    ///
    /// The counter is only touched (and so only advances) when the pattern contains an
    /// `%i` token, and advances **once per render** even if several `%i` tokens appear
    /// (ShareX increments per token; one value per name is what users expect).
    pub fn render(
        &self,
        ctx: &PatternContext<'_>,
        kind: PatternKind,
    ) -> Result<String, PatternError> {
        let raw = self.render_raw(ctx, kind)?;
        let opts = SanitizeOptions::default();
        Ok(match kind {
            PatternKind::Text => raw,
            PatternKind::FileName => {
                let mut name = sanitize_file_name(&raw, &opts);
                if let Some(max) = ctx.options.max_name_len.filter(|m| *m > 0) {
                    let cut = truncate_graphemes(&name, max);
                    if cut.len() != name.len() {
                        name = sanitize_file_name(cut, &opts);
                    }
                }
                name
            }
            PatternKind::FilePath => {
                let p = sanitize_relative_path(&raw, &opts);
                p.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            }
        })
    }

    /// Renders as a relative folder path (empty if the pattern renders to nothing usable).
    pub fn render_folder(&self, ctx: &PatternContext<'_>) -> Result<PathBuf, PatternError> {
        let raw = self.render_raw(ctx, PatternKind::FilePath)?;
        Ok(sanitize_relative_path(&raw, &SanitizeOptions::default()))
    }

    fn render_raw(
        &self,
        ctx: &PatternContext<'_>,
        kind: PatternKind,
    ) -> Result<String, PatternError> {
        let now = ctx.clock.now();
        let mut out = String::new();
        let mut counter: Option<u64> = None;
        for node in &self.nodes {
            match node {
                Node::Literal(s) => out.push_str(s),
                Node::Unknown(raw) => match ctx.options.unknown {
                    UnknownTokens::Keep => out.push_str(raw),
                    UnknownTokens::Remove => {}
                    UnknownTokens::Error => {
                        return Err(PatternError::UnknownToken(raw.clone()));
                    }
                },
                Node::Token(tok) => match tok {
                    Token::Percent => out.push('%'),
                    Token::Year => out.push_str(&now.year().to_string()),
                    Token::Year2 => out.push_str(&format!("{:02}", now.year().rem_euclid(100))),
                    Token::Month => out.push_str(&format!("{:02}", now.month())),
                    Token::MonthName => out.push_str(MONTHS[now.month0() as usize % 12]),
                    Token::Day => out.push_str(&format!("{:02}", now.day())),
                    Token::Hour => {
                        let h = now.hour();
                        let h = if self.has_ampm {
                            match h % 12 {
                                0 => 12,
                                x => x,
                            }
                        } else {
                            h
                        };
                        out.push_str(&format!("{h:02}"));
                    }
                    Token::Minute => out.push_str(&format!("{:02}", now.minute())),
                    Token::Second => out.push_str(&format!("{:02}", now.second().min(59))),
                    // chrono encodes a leap second as 1000..1999 ms.
                    Token::Millis => {
                        out.push_str(&format!("{:03}", now.timestamp_subsec_millis().min(999)));
                    }
                    Token::AmPm => out.push_str(if now.hour() >= 12 { "PM" } else { "AM" }),
                    Token::DayName => {
                        out.push_str(DAYS[now.weekday().num_days_from_monday() as usize % 7])
                    }
                    Token::WeekOfYear => out.push_str(&now.iso_week().week().to_string()),
                    Token::Unix => out.push_str(&now.timestamp().to_string()),
                    Token::WindowTitle => {
                        let max = ctx.options.max_title_len.filter(|m| *m > 0);
                        out.push_str(&window_text(ctx.inputs.window_title.as_deref(), max, kind));
                    }
                    Token::ProcessName => {
                        out.push_str(&window_text(ctx.inputs.process_name.as_deref(), None, kind));
                    }
                    Token::UserName => out.push_str(&ctx.env.user_name()),
                    Token::UserDomain => out.push_str(&ctx.env.user_domain()),
                    Token::MachineName => out.push_str(&ctx.env.machine_name()),
                    Token::Newline => match kind {
                        PatternKind::Text => out.push('\n'),
                        // Not meaningful in names/paths: behave like any unrecognised token.
                        _ => match ctx.options.unknown {
                            UnknownTokens::Keep => out.push_str("%n"),
                            UnknownTokens::Remove => {}
                            UnknownTokens::Error => {
                                return Err(PatternError::UnknownToken("%n".to_owned()));
                            }
                        },
                    },
                    Token::Width => {
                        if let Some(w) = ctx.inputs.width.filter(|w| *w > 0) {
                            out.push_str(&w.to_string());
                        }
                    }
                    Token::Height => {
                        if let Some(h) = ctx.inputs.height.filter(|h| *h > 0) {
                            out.push_str(&h.to_string());
                        }
                    }
                    Token::Counter { chars, base, width } => {
                        let value = match counter {
                            Some(v) => v,
                            None => {
                                let v = ctx.counter.next().map_err(PatternError::Counter)?;
                                counter = Some(v);
                                v
                            }
                        };
                        out.push_str(&to_base(value, chars, *base, *width));
                    }
                    Token::Random { chars, count } => {
                        let alphabet: Vec<char> = chars.chars().collect();
                        for _ in 0..*count {
                            let i = ctx.rng.below(alphabet.len() as u64) as usize;
                            out.push(alphabet.get(i).copied().unwrap_or('0'));
                        }
                    }
                    Token::Emoji(n) => {
                        for _ in 0..*n {
                            out.push_str(pick(EMOJI, ctx));
                        }
                    }
                    Token::Adjective => out.push_str(&title_case(pick(ADJECTIVES, ctx))),
                    Token::Animal => out.push_str(&title_case(pick(ANIMALS, ctx))),
                    Token::RandomFile(path) => {
                        let lines =
                            ctx.env.read_lines(std::path::Path::new(path)).map_err(|source| {
                                PatternError::RandomFile { path: path.clone(), source }
                            })?;
                        let lines: Vec<&str> =
                            lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
                        if lines.is_empty() {
                            return Err(PatternError::EmptyRandomFile(path.clone()));
                        }
                        let i = ctx.rng.below(lines.len() as u64) as usize;
                        out.push_str(lines.get(i).copied().unwrap_or_default());
                    }
                    Token::Guid { upper } => {
                        let g = guid(ctx.rng.next_u64(), ctx.rng.next_u64());
                        out.push_str(&if *upper { g.to_uppercase() } else { g });
                    }
                },
            }
        }
        Ok(out)
    }
}

fn pick<'a>(list: &'a [&'a str], ctx: &PatternContext<'_>) -> &'a str {
    let i = ctx.rng.below(list.len() as u64) as usize;
    list.get(i).copied().unwrap_or_default()
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// ShareX: window text is trimmed, spaces become underscores, and (for file names/paths)
/// illegal characters are removed so a title can never inject a path separator.
fn window_text(text: Option<&str>, max_graphemes: Option<usize>, kind: PatternKind) -> String {
    let Some(text) = text else { return String::new() };
    let mut s: String =
        text.trim().chars().map(|c| if c.is_whitespace() { '_' } else { c }).collect();
    if kind != PatternKind::Text {
        s = remove_illegal(&s);
    }
    match max_graphemes {
        Some(m) => truncate_graphemes(&s, m).to_owned(),
        None => s,
    }
}

/// Formats `value` in `base` using the first `base` characters of `chars`, left-padded with
/// the zero digit to `width`.
fn to_base(mut value: u64, chars: &str, base: usize, width: usize) -> String {
    let digits: Vec<char> = chars.chars().take(base).collect();
    let b = digits.len().max(2) as u64;
    let mut out = Vec::new();
    if value == 0 {
        out.push(digits.first().copied().unwrap_or('0'));
    }
    while value > 0 {
        let d = (value % b) as usize;
        out.push(digits.get(d).copied().unwrap_or('0'));
        value /= b;
    }
    let zero = digits.first().copied().unwrap_or('0');
    while out.len() < width {
        out.push(zero);
    }
    out.iter().rev().collect()
}

/// RFC 4122 version-4 layout from 128 random bits.
fn guid(a: u64, b: u64) -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_conversion() {
        assert_eq!(to_base(0, "0123456789", 10, 0), "0");
        assert_eq!(to_base(42, "0123456789", 10, 5), "00042");
        assert_eq!(to_base(255, "0123456789abcdef", 16, 0), "ff");
        assert_eq!(to_base(35, "0123456789abcdefghijklmnopqrstuvwxyz", 36, 0), "z");
        assert_eq!(to_base(36, "0123456789abcdefghijklmnopqrstuvwxyz", 36, 0), "10");
        assert_eq!(to_base(5, "0123456789abcdef", 2, 8), "00000101");
        assert_eq!(to_base(u64::MAX, "0123456789", 10, 0), "18446744073709551615");
    }

    #[test]
    fn guid_shape() {
        let g = guid(0xffff_ffff_ffff_ffff, 0xffff_ffff_ffff_ffff);
        assert_eq!(g, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(g.len(), 36);
    }
}
