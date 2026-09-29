//! ShareX "name parser": the `%`-code mini language (`%y-%mo-%d`, `%rn{8}`, `%guid`...).
//!
//! ShareX applies it to the *values* of `Parameters`, `Headers` and `Arguments` and to
//! `Data` (never to `RequestURL` or response templates). Upstream implements it as a long
//! chain of ordered `string.Replace` calls; here a single left-to-right scan takes the
//! *longest* matching code at each `%`, which gives the same result for every ordering the
//! upstream chain relies on (`%mon2` before `%mon` before `%mo`, `%unix` before `%un`,
//! `%height` before `%h`...) without the quadratic rescans.
//!
//! Deliberate differences: month/day names are always English (upstream uses the OS
//! culture), repeat counts are capped at [`MAX_REPEAT`] to keep hostile patterns cheap, and
//! `%t`/`%pn` stay literal unless the caller supplies a window title / process name (same
//! as upstream when those are unset).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Datelike as _, FixedOffset, Local, Timelike as _};
use rand::Rng as _;

/// Upper bound for `%rn{n}`, `%ra{n}`... repeat counts.
pub const MAX_REPEAT: usize = 4096;

const NUMBERS: &str = "0123456789";
const ALPHANUMERIC: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const ALPHANUMERIC_INVERSE: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const BASE56: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz";
const HEX_UPPER: &str = "0123456789ABCDEF";
const HEX_LOWER: &str = "0123456789abcdef";

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const DAYS: [&str; 7] =
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

const ADJECTIVES: &[&str] = &[
    "Brave", "Calm", "Clever", "Cosmic", "Crimson", "Curious", "Daring", "Eager", "Fancy",
    "Fuzzy", "Gentle", "Golden", "Happy", "Jolly", "Kind", "Lively", "Lucky", "Mellow",
    "Mighty", "Nimble", "Noble", "Odd", "Peppy", "Proud", "Quick", "Quiet", "Silly", "Sunny",
    "Swift", "Witty",
];
const ANIMALS: &[&str] = &[
    "Badger", "Beaver", "Camel", "Cat", "Cobra", "Crane", "Dolphin", "Eagle", "Falcon", "Ferret",
    "Fox", "Gecko", "Heron", "Horse", "Koala", "Lemur", "Llama", "Lynx", "Otter", "Owl",
    "Panda", "Penguin", "Rabbit", "Raven", "Seal", "Shark", "Sloth", "Tiger", "Whale", "Wolf",
];
const EMOJIS: &[&str] = &[
    "😀", "😎", "🤖", "👻", "🐱", "🐶", "🦊", "🐼", "🦄", "🌈", "🔥", "⭐", "🍕", "🍩", "🚀", "🎲",
    "🎸", "🌵", "🍀", "💎",
];

/// Text handling mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NameParserKind {
    /// Plain text; `%n` is left alone.
    #[default]
    Default,
    /// Multi-line text: `%n` becomes a newline.
    Text,
}

/// Configuration for one parse. Cheap to build per call.
#[derive(Debug, Clone, Default)]
pub struct NameParser {
    /// Mode.
    pub kind: NameParserKind,
    /// Time used for date codes; `None` means "now, local time zone". Tests pin it.
    pub now: Option<DateTime<FixedOffset>>,
    /// Value for `%t` (window title); `None` leaves `%t` untouched.
    pub window_title: Option<String>,
    /// Value for `%pn` (process name); `None` leaves `%pn` untouched.
    pub process_name: Option<String>,
    /// Value for `%width` / `%height`; `None` yields empty strings.
    pub image_size: Option<(u32, u32)>,
    /// Value used by the `%i` family (already incremented by the caller).
    pub auto_increment: u64,
}

impl NameParser {
    /// A parser in [`NameParserKind::Text`] mode (what custom uploaders use).
    pub fn text() -> Self {
        Self { kind: NameParserKind::Text, ..Self::default() }
    }

    /// Use the next value of `counter` for the `%i` family.
    #[must_use]
    pub fn with_counter(mut self, counter: &AtomicU64) -> Self {
        self.auto_increment = counter.fetch_add(1, Ordering::Relaxed) + 1;
        self
    }

    /// Expand every `%` code in `pattern`.
    pub fn parse(&self, pattern: &str) -> String {
        let now = self.now.unwrap_or_else(|| Local::now().fixed_offset());
        let twelve_hour = pattern.contains("%pm");
        let mut out = String::with_capacity(pattern.len());
        let mut rest = pattern;
        while let Some(pos) = rest.find('%') {
            out.push_str(&rest[..pos]);
            rest = &rest[pos + 1..];
            match self.expand(rest, &now, twelve_hour) {
                Some((text, used)) => {
                    out.push_str(&text);
                    rest = &rest[used..];
                }
                None => out.push('%'),
            }
        }
        out.push_str(rest);
        out
    }

    /// Try to expand the code at the start of `s` (the text after `%`). Returns the
    /// replacement and the number of bytes consumed.
    fn expand(&self, s: &str, now: &DateTime<FixedOffset>, twelve_hour: bool) -> Option<(String, usize)> {
        // Longest first; see the module docs.
        const CODES: &[&str] = &[
            "radjective", "ranimal", "remoji", "height", "width", "mon2", "unix", "guid", "GUID",
            "rna", "iAa", "iaA", "uln", "mon", "yy", "mo", "mi", "ms", "wy", "w2", "pm", "pn",
            "un", "cn", "ia", "iA", "ib", "iB", "ix", "iX", "rn", "ra", "rx", "rX", "rf", "y",
            "d", "h", "s", "w", "t", "n", "i",
        ];
        let code = CODES.iter().find(|c| s.starts_with(**c))?;
        let after = &s[code.len()..];
        // Optional `{args}` directly after the code.
        let (args, arg_len) = match after.strip_prefix('{').and_then(|a| a.find('}').map(|e| &a[..e])) {
            Some(a) => (Some(a), a.len() + 2),
            None => (None, 0),
        };
        let with_args = |text: String| Some((text, code.len() + arg_len));
        let plain = |text: String| Some((text, code.len()));
        let count = || args.map(|a| a.trim().parse::<usize>().unwrap_or(0));
        let n1 = || args.map_or(0, |a| a.split(',').next().and_then(|p| p.trim().parse::<usize>().ok()).unwrap_or(0));

        match *code {
            "radjective" => plain(pick(ADJECTIVES).to_owned()),
            "ranimal" => plain(pick(ANIMALS).to_owned()),
            "remoji" => with_args(repeat(count(), || pick(EMOJIS).to_owned())),
            "height" => plain(self.image_size.map(|(_, h)| h.to_string()).unwrap_or_default()),
            "width" => plain(self.image_size.map(|(w, _)| w.to_string()).unwrap_or_default()),
            "mon2" | "mon" => plain(MONTHS[now.month0() as usize].to_owned()),
            "unix" => plain(chrono::Utc::now().timestamp().to_string()),
            "guid" => plain(random_guid().to_lowercase()),
            "GUID" => plain(random_guid().to_uppercase()),
            "rna" => with_args(repeat(count(), || random_char(BASE56))),
            "rn" => with_args(repeat(count(), || random_char(NUMBERS))),
            "ra" => with_args(repeat(count(), || random_char(ALPHANUMERIC))),
            "rx" => with_args(repeat(count(), || random_char(HEX_LOWER))),
            "rX" => with_args(repeat(count(), || random_char(HEX_UPPER))),
            "rf" => with_args(random_line_from_file(args.unwrap_or_default())),
            "uln" => plain(env_first(&["USERDOMAIN", "USERDNSDOMAIN"]).unwrap_or_else(machine_name)),
            "un" => plain(env_first(&["USERNAME", "USER", "LOGNAME"]).unwrap_or_default()),
            "cn" => plain(machine_name()),
            "yy" => plain(format!("{:02}", now.year().rem_euclid(100))),
            "y" => plain(now.year().to_string()),
            "mo" => plain(format!("{:02}", now.month())),
            "d" => plain(format!("{:02}", now.day())),
            "h" => {
                let h = if twelve_hour { hour_to_12(now.hour()) } else { now.hour() };
                plain(format!("{h:02}"))
            }
            "mi" => plain(format!("{:02}", now.minute())),
            "s" => plain(format!("{:02}", now.second())),
            "ms" => plain(format!("{:03}", now.timestamp_subsec_millis())),
            "wy" => plain(now.iso_week().week().to_string()),
            "w2" | "w" => plain(DAYS[now.weekday().num_days_from_monday() as usize].to_owned()),
            "pm" => plain(if now.hour() >= 12 { "PM" } else { "AM" }.to_owned()),
            "pn" => self.process_name.clone().and_then(plain),
            "t" => self.window_title.as_deref().map(|t| t.trim().replace(' ', "_")).and_then(plain),
            "n" if self.kind == NameParserKind::Text => plain("\n".to_owned()),
            "n" => None,
            "i" => with_args(pad(self.auto_increment.to_string(), n1())),
            "ix" => with_args(pad(format!("{:x}", self.auto_increment), n1())),
            "iX" => with_args(pad(format!("{:X}", self.auto_increment), n1())),
            "ia" => with_args(pad(to_base(self.auto_increment, 36, ALPHANUMERIC).to_lowercase(), n1())),
            "iA" => with_args(pad(to_base(self.auto_increment, 36, ALPHANUMERIC).to_uppercase(), n1())),
            "iAa" => with_args(pad(to_base(self.auto_increment, 62, ALPHANUMERIC), n1())),
            "iaA" => with_args(pad(to_base(self.auto_increment, 62, ALPHANUMERIC_INVERSE), n1())),
            "ib" | "iB" => {
                let alphabet = if *code == "ib" { ALPHANUMERIC_INVERSE } else { ALPHANUMERIC };
                let (base, width) = args
                    .map(|a| {
                        let mut it = a.split(',').map(|p| p.trim().parse::<usize>().ok());
                        (it.next().flatten(), it.next().flatten())
                    })
                    .unwrap_or((None, None));
                let base = base.unwrap_or(10).clamp(2, alphabet.len());
                with_args(pad(to_base(self.auto_increment, base as u64, alphabet), width.unwrap_or(0)))
            }
            _ => None,
        }
    }
}

fn hour_to_12(h: u32) -> u32 {
    match h % 12 {
        0 => 12,
        n => n,
    }
}

fn pad(s: String, width: usize) -> String {
    let width = width.min(MAX_REPEAT);
    if s.len() >= width { s } else { format!("{}{s}", "0".repeat(width - s.len())) }
}

fn to_base(mut value: u64, base: u64, alphabet: &str) -> String {
    let digits: Vec<char> = alphabet.chars().collect();
    let base = base.clamp(2, digits.len() as u64);
    if value == 0 {
        return digits.first().map(char::to_string).unwrap_or_default();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(digits[(value % base) as usize]);
        value /= base;
    }
    out.iter().rev().collect()
}

fn pick<'a>(list: &[&'a str]) -> &'a str {
    list[rand::rng().random_range(0..list.len())]
}

fn random_char(alphabet: &str) -> String {
    let chars: Vec<char> = alphabet.chars().collect();
    chars[rand::rng().random_range(0..chars.len())].to_string()
}

fn repeat(count: Option<usize>, f: impl Fn() -> String) -> String {
    (0..count.unwrap_or(1).min(MAX_REPEAT)).map(|_| f()).collect()
}

fn random_guid() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{byte:02x}");
    }
    s
}

fn random_line_from_file(path: &str) -> String {
    // Bounded read: a hostile `.sxcu` must not make us slurp a huge file.
    const LIMIT: u64 = 1024 * 1024;
    let Ok(file) = std::fs::File::open(path) else { return String::new() };
    let mut buf = String::new();
    let mut reader = std::io::Read::take(file, LIMIT);
    if std::io::Read::read_to_string(&mut reader, &mut buf).is_err() {
        return String::new();
    }
    let lines: Vec<&str> = buf.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    lines[rand::rng().random_range(0..lines.len())].to_owned()
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
}

fn machine_name() -> String {
    env_first(&["COMPUTERNAME", "HOSTNAME"])
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_owned()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn fixed() -> NameParser {
        let now = FixedOffset::east_opt(2 * 3600)
            .unwrap()
            .with_ymd_and_hms(2024, 3, 9, 15, 4, 5)
            .unwrap();
        NameParser { now: Some(now), auto_increment: 7, ..NameParser::text() }
    }

    #[test]
    fn date_time_codes() {
        let p = fixed();
        assert_eq!(p.parse("%y-%mo-%d_%h-%mi-%s"), "2024-03-09_15-04-05");
        assert_eq!(p.parse("%yy"), "24");
        assert_eq!(p.parse("%mon %mon2"), "March March");
        assert_eq!(p.parse("%w/%w2"), "Saturday/Saturday");
        assert_eq!(p.parse("%h %pm"), "03 PM");
        assert_eq!(p.parse("%ms"), "000");
        assert_eq!(p.parse("%wy"), "10");
    }

    #[test]
    fn longest_code_wins() {
        let p = fixed();
        // %mo must not eat the start of %mon; %height must not become %h + "eight".
        assert_eq!(p.parse("%mon"), "March");
        assert_eq!(p.parse("%mo"), "03");
        assert_eq!(p.parse("%width x %height"), " x ");
        let p = NameParser { image_size: Some((640, 480)), ..fixed() };
        assert_eq!(p.parse("%width x %height"), "640 x 480");
    }

    #[test]
    fn unknown_or_unavailable_codes_stay_literal() {
        let p = fixed();
        assert_eq!(p.parse("100%"), "100%");
        assert_eq!(p.parse("%q %20 %"), "%q %20 %");
        assert_eq!(p.parse("%t|%pn"), "%t|%pn");
        let p = NameParser { window_title: Some(" My Win ".into()), ..fixed() };
        assert_eq!(p.parse("%t"), "My_Win");
    }

    #[test]
    fn newline_only_in_text_mode() {
        assert_eq!(fixed().parse("a%nb"), "a\nb");
        let p = NameParser { kind: NameParserKind::Default, ..fixed() };
        assert_eq!(p.parse("a%nb"), "a%nb");
    }

    #[test]
    fn auto_increment_family() {
        let p = fixed();
        assert_eq!(p.parse("%i"), "7");
        assert_eq!(p.parse("%i{3}"), "007");
        assert_eq!(p.parse("%ix %iX{4}"), "7 0007");
        assert_eq!(p.parse("%ia"), "7");
        let p = NameParser { auto_increment: 35, ..fixed() };
        assert_eq!(p.parse("%ia %iA"), "z Z");
        let p = NameParser { auto_increment: 62, ..fixed() };
        assert_eq!(p.parse("%iAa"), "10");
        assert_eq!(p.parse("%ib{2,8}"), "00111110");
    }

    #[test]
    fn random_codes_have_expected_shape() {
        let p = fixed();
        let r = p.parse("%rn{6}");
        assert_eq!(r.len(), 6);
        assert!(r.chars().all(|c| c.is_ascii_digit()));
        let r = p.parse("%ra{40}");
        assert_eq!(r.len(), 40);
        assert!(r.chars().all(|c| c.is_ascii_alphanumeric()));
        let r = p.parse("%rx{8}/%rX{8}");
        assert!(r[..8].chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert!(r[9..].chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')));
        let g = p.parse("%guid");
        assert_eq!(g.len(), 36);
        assert_eq!(g.as_bytes()[14], b'4');
        assert_eq!(p.parse("%rna").chars().count(), 1);
        assert!(!p.parse("%radjective").is_empty());
        assert_eq!(p.parse("%remoji{3}").chars().count(), 3);
    }

    #[test]
    fn repeat_counts_are_capped() {
        assert_eq!(fixed().parse("%rn{999999999}").len(), MAX_REPEAT);
    }

    #[test]
    fn counter_increments() {
        let c = AtomicU64::new(0);
        assert_eq!(NameParser::text().with_counter(&c).auto_increment, 1);
        assert_eq!(NameParser::text().with_counter(&c).auto_increment, 2);
    }

    #[test]
    fn braces_without_a_code_are_untouched() {
        assert_eq!(fixed().parse("{json:a}"), "{json:a}");
        let r = fixed().parse("%rn{oops");
        assert!(r.ends_with("{oops") && r.len() == 6, "{r}");
        assert_eq!(fixed().parse("%rn{x}"), "", "unparsable count repeats zero times, as upstream");
    }
}
