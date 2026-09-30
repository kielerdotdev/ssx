//! Live preview and diagnostics for filename / folder patterns.
//!
//! Everything comes from `ssx_core::pattern`, so the preview is exactly what the engine will
//! produce: the same parser, the same sanitising, the same folder composition as
//! `workflow::steps` (save folder, then the per-type sub-folder, then the rendered folder
//! pattern). Only the *inputs* are made up: a sample window title and image size, a fixed
//! random seed (so the preview does not flicker on every frame) and a counter that is read but
//! never advanced.

use std::path::PathBuf;

use ssx_core::{
    pattern::{
        Clock, Env, MemoryCounter, NameInputs, Pattern, PatternContext, RenderOptions, SeededRng,
        UNSUPPORTED_TOKENS, known_token_names, render_file_name, render_folder, suspicious_tokens,
    },
    settings::General,
};

/// Which kind of pattern is being analysed; the rules for `/` and `\` differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// A single file name (no path separators).
    FileName,
    /// A relative folder path (`/` separates folders).
    Folder,
}

/// Findings about a pattern's text that do not need the clock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternReport {
    /// `%tokens` the parser does not know (they would appear literally).
    pub unknown_tokens: Vec<String>,
    /// Tokens people expect that do not exist, with the fix.
    pub unsupported: Vec<(&'static str, &'static str)>,
    /// Characters in the literal text that are illegal in file names and will be dropped.
    pub illegal_chars: Vec<char>,
    /// The pattern uses the window title or process name.
    pub uses_window: bool,
    /// The pattern uses the auto-increment counter.
    pub uses_counter: bool,
    /// The pattern uses the image size.
    pub uses_size: bool,
}

impl PatternReport {
    /// `true` if there is anything worth a warning line.
    pub fn has_warnings(&self) -> bool {
        !self.unknown_tokens.is_empty()
            || !self.unsupported.is_empty()
            || !self.illegal_chars.is_empty()
    }

    /// The warnings as sentences, in the order they should be shown.
    pub fn messages(&self, kind: PatternKind) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.unknown_tokens {
            out.push(format!(
                "{t} is not a token; it will appear literally (write %% for a percent sign)"
            ));
        }
        for (t, hint) in &self.unsupported {
            out.push(format!("{t} {hint}"));
        }
        if !self.illegal_chars.is_empty() {
            let list: String =
                self.illegal_chars.iter().map(|c| display_char(*c)).collect::<Vec<_>>().join(" ");
            out.push(match kind {
                PatternKind::FileName => format!(
                    "{list} cannot be used in a file name and will be removed; for sub-folders use the folder pattern"
                ),
                PatternKind::Folder => {
                    format!("{list} cannot be used in a folder name and will be removed")
                }
            });
        }
        out
    }
}

fn display_char(c: char) -> String {
    if c.is_control() { format!("U+{:04X}", c as u32) } else { format!("'{c}'") }
}

fn illegal_in(kind: PatternKind, c: char) -> bool {
    let sep = matches!(c, '/' | '\\');
    (sep && kind == PatternKind::FileName)
        || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        || c.is_control()
}

/// The literal (non-token) text of `source`: `%tokens` and their `{...}` parameters removed,
/// `%%` kept as one `%`.
pub fn literal_text(source: &str) -> String {
    let names: Vec<&str> = known_token_names().collect();
    let mut out = String::new();
    let mut rest = source;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(r) = after.strip_prefix('%') {
            out.push('%');
            rest = r;
            continue;
        }
        // Longest known token name, else the alphanumeric run (an unknown token).
        let name_len =
            names.iter().filter(|n| after.starts_with(**n)).map(|n| n.len()).max().unwrap_or_else(
                || after.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(after.len()),
            );
        let mut tail = &after[name_len..];
        if name_len == 0 {
            // A lone percent sign is literal text.
            out.push('%');
        } else if tail.starts_with('{')
            && let Some(close) = tail.find('}')
        {
            tail = &tail[close + 1..];
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Analyses the text of a pattern.
pub fn analyze(source: &str, kind: PatternKind) -> PatternReport {
    let p = Pattern::parse(source);
    let mut illegal = Vec::new();
    for c in literal_text(source).chars() {
        if illegal_in(kind, c) && !illegal.contains(&c) {
            illegal.push(c);
        }
    }
    PatternReport {
        unknown_tokens: p.unknown_tokens().into_iter().map(str::to_owned).collect(),
        unsupported: suspicious_tokens(source),
        illegal_chars: illegal,
        uses_window: p.uses_window_info(),
        uses_counter: p.uses_counter(),
        uses_size: p.uses_dimensions(),
    }
}

/// The made-up inputs of a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleInputs {
    /// Window title for `%t`.
    pub window_title: &'static str,
    /// Process name for `%pn`.
    pub process_name: &'static str,
    /// Image width for `%width`.
    pub width: u32,
    /// Image height for `%height`.
    pub height: u32,
    /// The value the next `%i` would take (the counter is not advanced).
    pub next_counter: u64,
}

impl Default for SampleInputs {
    fn default() -> Self {
        Self {
            window_title: "Example Domain - Firefox",
            process_name: "firefox",
            width: 1920,
            height: 1080,
            next_counter: 1,
        }
    }
}

/// Where a file would be saved, with the pieces that make it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPreview {
    /// The complete path (`<save dir>/<type folder>/<dated folder>/<name>.<ext>`).
    pub full: PathBuf,
    /// The rendered folder pattern (empty when the pattern is empty).
    pub folder: String,
    /// The rendered file name with extension.
    pub file_name: String,
    /// The resolved save folder (the configured one or `<Pictures>/ssx`).
    pub save_dir: PathBuf,
    /// `Some(reason)` if rendering failed.
    pub error: Option<String>,
}

/// Renders both patterns of `general` the way the workflow engine will.
pub fn preview_paths(
    general: &General,
    clock: &dyn Clock,
    env: &dyn Env,
    sample: &SampleInputs,
) -> PathPreview {
    let inputs = NameInputs {
        window_title: Some(sample.window_title.to_owned()),
        process_name: Some(sample.process_name.to_owned()),
        width: Some(sample.width),
        height: Some(sample.height),
    };
    let rng = SeededRng::new(0x55AA_1234);
    let counter = MemoryCounter::starting_after(sample.next_counter.saturating_sub(1));
    let ctx = PatternContext {
        clock,
        rng: &rng,
        env,
        counter: &counter,
        inputs: &inputs,
        options: RenderOptions {
            unknown: ssx_core::pattern::UnknownTokens::Keep,
            max_name_len: (general.max_file_name_len > 0).then_some(general.max_file_name_len),
            max_title_len: (general.max_title_len > 0).then_some(general.max_title_len),
        },
    };
    let save_dir = general.resolve_save_dir();
    let mut dir = save_dir.clone();
    if general.use_type_subfolders {
        dir.push(&general.subfolders.image);
    }
    let mut error = None;
    let mut folder = String::new();
    if !general.folder_pattern.trim().is_empty() {
        match render_folder(&general.folder_pattern, &ctx) {
            Ok(p) => {
                folder = p.display().to_string();
                dir.push(p);
            }
            Err(e) => error = Some(e.to_string()),
        }
    }
    let file_name = match render_file_name(
        &general.file_name_pattern,
        general.image_format.extension(),
        &ctx,
    ) {
        Ok(n) => n,
        Err(e) => {
            error.get_or_insert(e.to_string());
            String::new()
        }
    };
    PathPreview { full: dir.join(&file_name), folder, file_name, save_dir, error }
}

/// One row of the token cheat sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenDoc {
    /// The token as typed (`%i{n}`); several spellings may share a row.
    pub token: &'static str,
    /// What it produces.
    pub meaning: &'static str,
    /// The heading it is listed under.
    pub group: &'static str,
}

const fn t(group: &'static str, token: &'static str, meaning: &'static str) -> TokenDoc {
    TokenDoc { token, meaning, group }
}

/// Every supported token, documented. A test checks it covers `known_token_names()`, so a
/// token added to the core cannot be missing from the UI unnoticed.
pub const CHEAT_SHEET: &[TokenDoc] = &[
    t("Date", "%y  %yy", "year (4 / 2 digits)"),
    t("Date", "%mo", "month number (01-12)"),
    t("Date", "%mon  %mon2", "month name (English)"),
    t("Date", "%d", "day of month (01-31)"),
    t("Date", "%w  %w2", "weekday name (English)"),
    t("Date", "%wy", "ISO week of the year"),
    t("Date", "%unix", "Unix time in seconds"),
    t("Time", "%h", "hour (24 h, or 12 h when %pm is used)"),
    t("Time", "%mi  %s  %ms", "minute, second, millisecond"),
    t("Time", "%pm", "AM / PM"),
    t("Capture", "%t", "window title (trimmed, spaces to _)"),
    t("Capture", "%pn", "process name of the window"),
    t("Capture", "%width  %height", "image size in pixels"),
    t("Machine", "%un  %uln  %cn", "user name, user domain, computer name"),
    t("Counter", "%i  %i{n}", "auto-increment, zero-padded to n digits"),
    t("Counter", "%ix  %iX", "auto-increment in hex (lower / upper case)"),
    t("Counter", "%ia  %iA  %iAa  %iaA", "auto-increment in base 36 / base 62"),
    t("Counter", "%ib{base,len}  %iB{base,len}", "auto-increment in a custom base"),
    t("Random", "%ra{n}", "n random letters and digits"),
    t("Random", "%rn  %rna", "random digits / random letters without look-alikes"),
    t("Random", "%rx  %rX", "random hex digits (lower / upper case)"),
    t("Random", "%remoji  %radjective  %ranimal", "a random emoji / adjective / animal"),
    t("Random", "%rf{path}", "a random line of a text file"),
    t("Random", "%guid  %GUID", "random GUID (lower / upper case)"),
    t("Other", "%n", "new line (text patterns only)"),
    t("Other", "%%", "a literal percent sign"),
];

/// The token names a cheat-sheet row covers (`%i{n}` covers `i`; `%%` covers nothing).
pub fn names_in(doc: &TokenDoc) -> Vec<String> {
    doc.token
        .split_whitespace()
        .filter_map(|w| {
            let w = w.strip_prefix('%')?;
            let w = w.split('{').next().unwrap_or(w);
            w.starts_with(|c: char| c.is_ascii_alphabetic()).then(|| w.to_owned())
        })
        .collect()
}

/// The tokens people often expect that do not exist, with the fix (from the core).
pub fn unsupported_tokens() -> &'static [(&'static str, &'static str)] {
    UNSUPPORTED_TOKENS
}

/// Ready-made patterns offered as one-click examples.
pub const EXAMPLE_FILE_PATTERNS: &[(&str, &str)] = &[
    ("Default", "Screenshot_%y-%mo-%d_%h-%mi-%s"),
    ("With window title", "%y%mo%d_%h%mi%s_%t"),
    ("Counter", "shot-%i{4}"),
    ("Random", "%y-%mo-%d_%ra{6}"),
];

#[cfg(test)]
mod tests {
    use ssx_core::pattern::{FixedClock, StaticEnv};

    use super::*;

    fn clock() -> FixedClock {
        FixedClock::from_rfc3339("2024-03-09T14:05:06.789+01:00").unwrap()
    }

    fn env() -> StaticEnv {
        StaticEnv {
            user: "marius".into(),
            domain: "WORK".into(),
            machine: "laptop".into(),
            files: std::collections::BTreeMap::default(),
        }
    }

    #[test]
    fn default_general_previews_like_the_engine() {
        let g = General { save_dir: Some(PathBuf::from("/data/shots")), ..General::default() };
        let p = preview_paths(&g, &clock(), &env(), &SampleInputs::default());
        assert_eq!(p.file_name, "Screenshot_2024-03-09_14-05-06.png");
        assert_eq!(p.folder, "2024-03");
        assert_eq!(
            p.full,
            PathBuf::from("/data/shots/Screenshots/2024-03/Screenshot_2024-03-09_14-05-06.png")
        );
        assert_eq!(p.save_dir, PathBuf::from("/data/shots"));
        assert!(p.error.is_none());
    }

    #[test]
    fn subfolders_and_folder_pattern_can_be_switched_off() {
        let g = General {
            save_dir: Some(PathBuf::from("/d")),
            use_type_subfolders: false,
            folder_pattern: String::new(),
            file_name_pattern: "a".into(),
            ..General::default()
        };
        let p = preview_paths(&g, &clock(), &env(), &SampleInputs::default());
        assert_eq!(p.full, PathBuf::from("/d/a.png"));
        assert_eq!(p.folder, "");
    }

    #[test]
    fn extension_follows_the_format() {
        let g = General {
            save_dir: Some(PathBuf::from("/d")),
            image_format: ssx_core::settings::ImageFormatKind::Jpg,
            file_name_pattern: "x".into(),
            ..General::default()
        };
        assert_eq!(
            preview_paths(&g, &clock(), &env(), &SampleInputs::default()).file_name,
            "x.jpg"
        );
    }

    #[test]
    fn sample_inputs_feed_the_context_tokens() {
        let g = General {
            save_dir: Some(PathBuf::from("/d")),
            file_name_pattern: "%un@%cn_%pn_%t_%width x %height_%i{3}".into(),
            ..General::default()
        };
        let s = SampleInputs { next_counter: 7, ..SampleInputs::default() };
        let p = preview_paths(&g, &clock(), &env(), &s);
        assert_eq!(
            p.file_name,
            "marius@laptop_firefox_Example_Domain_-_Firefox_1920 x 1080_007.png"
        );
    }

    #[test]
    fn preview_is_stable_between_calls() {
        let g = General {
            save_dir: Some(PathBuf::from("/d")),
            file_name_pattern: "%ra{8}-%guid".into(),
            ..General::default()
        };
        let a = preview_paths(&g, &clock(), &env(), &SampleInputs::default());
        let b = preview_paths(&g, &clock(), &env(), &SampleInputs::default());
        assert_eq!(a, b, "a fixed seed keeps the preview from flickering");
        assert!(a.file_name.len() > 20);
    }

    #[test]
    fn name_length_limit_applies() {
        let g = General {
            save_dir: Some(PathBuf::from("/d")),
            file_name_pattern: "abcdefghijklmnopqrstuvwxyz".into(),
            max_file_name_len: 5,
            ..General::default()
        };
        assert_eq!(
            preview_paths(&g, &clock(), &env(), &SampleInputs::default()).file_name,
            "abcde.png"
        );
    }

    #[test]
    fn unknown_and_unsupported_tokens_are_reported() {
        let r = analyze("%y-%foo-%hh-%date", PatternKind::FileName);
        assert_eq!(r.unknown_tokens, ["%foo"]);
        assert!(r.unsupported.iter().any(|(t, _)| *t == "%hh"));
        assert!(r.unsupported.iter().any(|(t, _)| *t == "%date"));
        assert!(r.has_warnings());
        assert!(r.messages(PatternKind::FileName).len() >= 3);
    }

    #[test]
    fn clean_patterns_have_no_warnings() {
        for src in
            ["Screenshot_%y-%mo-%d_%h-%mi-%s", "%i{4}", "%%literal", "%rf{C:\\lines.txt}", ""]
        {
            let r = analyze(src, PatternKind::FileName);
            assert!(!r.has_warnings(), "{src}: {r:?}");
        }
    }

    #[test]
    fn illegal_characters_are_found_in_literal_text_only() {
        let r = analyze("a/b:c*d?e\"f<g>h|i", PatternKind::FileName);
        assert_eq!(r.illegal_chars, ['/', ':', '*', '?', '"', '<', '>', '|']);
        let msg = r.messages(PatternKind::FileName).join("\n");
        assert!(msg.contains("folder pattern"), "{msg}");
        // separators are fine in a folder pattern
        let r = analyze("%y/%mo\\%d", PatternKind::Folder);
        assert!(r.illegal_chars.is_empty());
        let r = analyze("a:b", PatternKind::Folder);
        assert_eq!(r.illegal_chars, [':']);
        // a token parameter may contain anything
        assert!(analyze("%rf{/tmp/a:b}", PatternKind::FileName).illegal_chars.is_empty());
        // control characters are illegal too
        assert_eq!(analyze("a\tb", PatternKind::FileName).illegal_chars, ['\t']);
        assert!(
            analyze("a\tb", PatternKind::FileName).messages(PatternKind::FileName)[0]
                .contains("U+0009")
        );
    }

    #[test]
    fn literal_text_skips_tokens_and_keeps_escaped_percent() {
        assert_eq!(literal_text("Screenshot_%y-%mo"), "Screenshot_-");
        assert_eq!(literal_text("100%%"), "100%");
        assert_eq!(literal_text("%i{4}x%ib{62,4}y"), "xy");
        assert_eq!(literal_text("50% off"), "50% off");
        assert_eq!(literal_text("%foo-bar"), "-bar");
    }

    #[test]
    fn detects_what_a_pattern_needs() {
        let r = analyze("%t-%i-%width", PatternKind::FileName);
        assert!(r.uses_window && r.uses_counter && r.uses_size);
        let r = analyze("%y", PatternKind::FileName);
        assert!(!r.uses_window && !r.uses_counter && !r.uses_size);
    }

    #[test]
    fn cheat_sheet_covers_every_token_the_core_knows() {
        let documented: Vec<String> = CHEAT_SHEET.iter().flat_map(names_in).collect();
        for name in known_token_names() {
            assert!(
                documented.iter().any(|d| d == name),
                "token %{name} is not in the cheat sheet"
            );
        }
        for d in &documented {
            assert!(
                known_token_names().any(|n| n == d),
                "cheat sheet documents %{d}, which the core does not know"
            );
        }
    }

    #[test]
    fn cheat_sheet_examples_all_parse_as_known_tokens() {
        for row in CHEAT_SHEET {
            for w in row.token.split_whitespace() {
                let sample = w
                    .replace("{n}", "{4}")
                    .replace("{base,len}", "{62,4}")
                    .replace("{path}", "{/x}");
                let r = analyze(&sample, PatternKind::FileName);
                assert!(r.unknown_tokens.is_empty(), "{w}: {r:?}");
            }
        }
    }

    #[test]
    fn example_patterns_are_clean() {
        for (_, p) in EXAMPLE_FILE_PATTERNS {
            assert!(!analyze(p, PatternKind::FileName).has_warnings(), "{p}");
        }
        assert!(!unsupported_tokens().is_empty());
    }
}
