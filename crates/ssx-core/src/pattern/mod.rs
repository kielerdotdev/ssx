//! ShareX-compatible filename and folder patterns.
//!
//! A pattern such as `Screenshot_%y-%mo-%d_%h-%mi-%s` is parsed once into a [`Pattern`]
//! and rendered against a [`PatternContext`]. The context carries every non-deterministic
//! input (clock, RNG, environment, counter) as an injectable trait, so rendering is
//! deterministic in tests; see [`context`].
//!
//! # Tokens
//!
//! Source of truth: ShareX `NameParser.cs` / `Helpers.cs` (fetched from the ShareX
//! repository while writing this module).
//!
//! | Token | Meaning |
//! |---|---|
//! | `%y` `%yy` | year (4 / 2 digits) |
//! | `%mo` `%mon` `%mon2` | month number (2 digits) / month name / month name (invariant) |
//! | `%d` | day of month (2 digits) |
//! | `%h` `%mi` `%s` `%ms` | hour, minute, second (2 digits), millisecond (3 digits) |
//! | `%pm` | `AM` / `PM`; if present anywhere in the pattern `%h` becomes 12-hour |
//! | `%w` `%w2` | weekday name / weekday name (invariant) |
//! | `%wy` | ISO-8601 week of the year |
//! | `%unix` | Unix time in seconds |
//! | `%t` `%pn` | window title / process name (trimmed, spaces to `_`, illegal chars removed) |
//! | `%un` `%uln` `%cn` | user name / user domain / machine name |
//! | `%width` `%height` | image size (empty when unknown) |
//! | `%i` `%i{n}` | auto-increment decimal, zero-padded to `n` |
//! | `%ix` `%iX` | ... lower / upper hex |
//! | `%ia` `%iA` | ... base 36 lower / upper |
//! | `%iAa` `%iaA` | ... base 62 (`0-9A-Za-z` / `0-9a-zA-Z`) |
//! | `%ib{base,len}` `%iB{base,len}` | ... custom base over `0-9a-zA-Z` / `0-9A-Za-z` |
//! | `%ra` `%rn` `%rna` `%rx` `%rX` (+ `{n}`) | random alphanumeric / digit / base-56 / lower hex / upper hex |
//! | `%remoji` (+ `{n}`) | random emoji |
//! | `%radjective` `%ranimal` | random word, title-cased |
//! | `%rf{path}` | random non-empty line of a text file |
//! | `%guid` `%GUID` | random (v4) GUID, lower / upper case |
//! | `%n` | newline, only in [`PatternKind::Text`] |
//! | `%%` | literal `%` (ssx extension) |
//!
//! # Deliberate differences from ShareX
//!
//! * `%mon`, `%w` are always **English** (identical to `%mon2` / `%w2`); ssx has no
//!   localisation layer in the core. Use `%mo` for locale-independent months.
//! * `%wy` is the ISO-8601 week, not the current culture's calendar week.
//! * All `%i…` tokens in one pattern share **one** counter increment; ShareX increments
//!   once per token.
//! * `%rf{path}` reads through [`Env`], so it can be sandboxed; the built-in adjective /
//!   animal / emoji lists are much smaller than ShareX's.
//! * Unknown `%tokens` are kept verbatim by default (as in ShareX) but can be removed or
//!   turned into an error ([`UnknownTokens`]).
//!
//! # Not supported
//!
//! See [`UNSUPPORTED_TOKENS`]; these are tokens people commonly *assume* exist (they are not
//! ShareX tokens either). Several of them parse as a shorter real token followed by literal
//! text (`%hh` is the hour followed by `h`), so they are not diagnosed by
//! [`Pattern::unknown_tokens`]; [`suspicious_tokens`] finds them for settings validation.

mod context;
mod parse;
mod render;
mod sanitize;
mod unique;
mod words;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

pub use context::{
    Clock, CounterStore, Env, FileCounter, FixedClock, MemoryCounter, NameInputs, PatternContext,
    PatternKind, RenderOptions, Rng, SeededRng, StaticEnv, SystemClock, SystemEnv, SystemRng,
    UnknownTokens,
};
pub use parse::{Pattern, known_token_names};
pub use sanitize::{
    MAX_FILE_NAME_BYTES, SanitizeOptions, is_valid_file_name, is_windows_reserved,
    sanitize_file_name, sanitize_relative_path, truncate_bytes, truncate_graphemes,
};
pub use unique::{candidate_name, create_unique, write_unique};

/// Tokens that users often expect but that neither ShareX nor ssx implements, with a hint.
pub const UNSUPPORTED_TOKENS: &[(&str, &str)] = &[
    ("%hh", "not a ShareX token; use %h (24-hour, or 12-hour when the pattern also has %pm)"),
    ("%ras", "not a ShareX token; use %ra{n} for n random alphanumeric characters"),
    ("%date", "not a ShareX token; compose it from %y-%mo-%d"),
    ("%time", "not a ShareX token; compose it from %h-%mi-%s"),
    ("%uid", "not a ShareX token; use %guid or %ra{n}"),
];

/// Entries of [`UNSUPPORTED_TOKENS`] that occur in `pattern_source`, with their hints.
pub fn suspicious_tokens(pattern_source: &str) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    let mut rest = pattern_source;
    // Skip escaped percent signs so "%%date" is not flagged.
    while let Some(i) = rest.find('%') {
        rest = &rest[i..];
        if rest.starts_with("%%") {
            rest = &rest[2..];
            continue;
        }
        if let Some(hit) = UNSUPPORTED_TOKENS.iter().find(|(t, _)| rest.starts_with(t)) {
            out.push(*hit);
        }
        rest = &rest[1..];
    }
    out
}

/// Failure while rendering a pattern.
#[derive(Debug, thiserror::Error)]
pub enum PatternError {
    /// An unknown token was found and [`UnknownTokens::Error`] is set.
    #[error("unknown filename token {0:?}; see the pattern documentation for the supported tokens")]
    UnknownToken(String),
    /// The `%i` counter file could not be read or updated.
    #[error("cannot update the auto-increment counter file: {0}")]
    Counter(#[source] std::io::Error),
    /// `%rf{path}` could not read its file.
    #[error("cannot read the file {path:?} used by %rf: {source}")]
    RandomFile {
        /// The path given in the pattern.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// `%rf{path}` pointed at a file without any non-empty line.
    #[error("the file {0:?} used by %rf has no non-empty lines")]
    EmptyRandomFile(String),
}

/// Normalises a user-supplied extension: leading dots removed, non-alphanumerics dropped.
fn clean_extension(ext: &str) -> String {
    ext.trim_start_matches('.').chars().filter(char::is_ascii_alphanumeric).collect()
}

/// Renders `pattern` as a complete, valid file name with extension `ext` (`"png"`, `".png"`
/// or `""`). The stem is truncated to [`RenderOptions::max_name_len`] graphemes; the total
/// never exceeds [`MAX_FILE_NAME_BYTES`] and the extension is always preserved.
pub fn render_file_name(
    pattern: &str,
    ext: &str,
    ctx: &PatternContext<'_>,
) -> Result<String, PatternError> {
    let stem = Pattern::parse(pattern).render(ctx, PatternKind::FileName)?;
    let ext = clean_extension(ext);
    let full = if ext.is_empty() { stem } else { format!("{stem}.{ext}") };
    Ok(sanitize_file_name(&full, &SanitizeOptions::default()))
}

/// Renders a folder pattern (`%y-%mo/%d`) as a safe relative path.
pub fn render_folder(pattern: &str, ctx: &PatternContext<'_>) -> Result<PathBuf, PatternError> {
    Pattern::parse(pattern).render_folder(ctx)
}
