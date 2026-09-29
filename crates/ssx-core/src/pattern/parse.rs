//! Tokeniser for ShareX-style patterns.
//!
//! Parsing never fails: text that is not a token is kept as a literal, and `%word`s that
//! look like a token but are not one become [`Node::Unknown`] so that the *renderer* (and
//! settings validation) can decide what to do with them.

/// Digits `0-9`.
pub(super) const NUMBERS: &str = "0123456789";
/// ShareX `Helpers.Alphanumeric`: `0-9A-Za-z`.
pub(super) const ALNUM: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// ShareX `Helpers.AlphanumericInverse`: `0-9a-zA-Z`.
pub(super) const ALNUM_INVERSE: &str =
    "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const HEX_LOWER: &str = "0123456789abcdef";
const HEX_UPPER: &str = "0123456789ABCDEF";
const LOWER36: &str = "0123456789abcdefghijklmnopqrstuvwxyz";
const UPPER36: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// ShareX `Helpers.Base56`: alphanumerics without look-alikes (`0 O 1 I l`).
pub(super) const BASE56: &str = "23456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz";

/// Largest `%i{n}` zero padding.
const MAX_COUNTER_WIDTH: usize = 64;
/// Largest `%ra{n}`-style repeat count.
const MAX_RANDOM_COUNT: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    Percent,
    Year,
    Year2,
    Month,
    MonthName,
    Day,
    Hour,
    Minute,
    Second,
    Millis,
    AmPm,
    DayName,
    WeekOfYear,
    Unix,
    WindowTitle,
    ProcessName,
    UserName,
    UserDomain,
    MachineName,
    Newline,
    Width,
    Height,
    Counter { chars: &'static str, base: usize, width: usize },
    Random { chars: &'static str, count: usize },
    Emoji(usize),
    Adjective,
    Animal,
    RandomFile(String),
    Guid { upper: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Node {
    Literal(String),
    Token(Token),
    /// `%something` that is not a token (or has an invalid parameter); holds the raw text.
    Unknown(String),
}

/// How a name in [`TABLE`] takes parameters.
#[derive(Clone, Copy)]
enum Spec {
    Simple(fn() -> Token),
    Counter(&'static str, usize),
    /// `%ib{base,len}` / `%iB{base,len}`: custom base over the given alphabet.
    CounterCustom(&'static str),
    Random(&'static str),
    Emoji,
    RandomFile,
}

const TABLE: &[(&str, Spec)] = &[
    ("y", Spec::Simple(|| Token::Year)),
    ("yy", Spec::Simple(|| Token::Year2)),
    ("mo", Spec::Simple(|| Token::Month)),
    ("mon", Spec::Simple(|| Token::MonthName)),
    ("mon2", Spec::Simple(|| Token::MonthName)),
    ("d", Spec::Simple(|| Token::Day)),
    ("h", Spec::Simple(|| Token::Hour)),
    ("mi", Spec::Simple(|| Token::Minute)),
    ("s", Spec::Simple(|| Token::Second)),
    ("ms", Spec::Simple(|| Token::Millis)),
    ("pm", Spec::Simple(|| Token::AmPm)),
    ("w", Spec::Simple(|| Token::DayName)),
    ("w2", Spec::Simple(|| Token::DayName)),
    ("wy", Spec::Simple(|| Token::WeekOfYear)),
    ("unix", Spec::Simple(|| Token::Unix)),
    ("t", Spec::Simple(|| Token::WindowTitle)),
    ("pn", Spec::Simple(|| Token::ProcessName)),
    ("un", Spec::Simple(|| Token::UserName)),
    ("uln", Spec::Simple(|| Token::UserDomain)),
    ("cn", Spec::Simple(|| Token::MachineName)),
    ("n", Spec::Simple(|| Token::Newline)),
    ("width", Spec::Simple(|| Token::Width)),
    ("height", Spec::Simple(|| Token::Height)),
    ("i", Spec::Counter(NUMBERS, 10)),
    ("ix", Spec::Counter(HEX_LOWER, 16)),
    ("iX", Spec::Counter(HEX_UPPER, 16)),
    ("ia", Spec::Counter(LOWER36, 36)),
    ("iA", Spec::Counter(UPPER36, 36)),
    ("iAa", Spec::Counter(ALNUM, 62)),
    ("iaA", Spec::Counter(ALNUM_INVERSE, 62)),
    ("ib", Spec::CounterCustom(ALNUM_INVERSE)),
    ("iB", Spec::CounterCustom(ALNUM)),
    ("ra", Spec::Random(ALNUM)),
    ("rn", Spec::Random(NUMBERS)),
    ("rna", Spec::Random(BASE56)),
    ("rx", Spec::Random(HEX_LOWER)),
    ("rX", Spec::Random(HEX_UPPER)),
    ("remoji", Spec::Emoji),
    ("radjective", Spec::Simple(|| Token::Adjective)),
    ("ranimal", Spec::Simple(|| Token::Animal)),
    ("rf", Spec::RandomFile),
    ("guid", Spec::Simple(|| Token::Guid { upper: false })),
    ("GUID", Spec::Simple(|| Token::Guid { upper: true })),
];

/// All token names (without the leading `%`) this engine understands.
pub fn known_token_names() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|(n, _)| *n)
}

/// A parsed pattern. Parsing is total; inspect [`Pattern::unknown_tokens`] to find typos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    source: String,
    pub(super) nodes: Vec<Node>,
    pub(super) has_ampm: bool,
}

impl Pattern {
    /// Parses `source`. `%%` is an escaped literal percent sign.
    pub fn parse(source: &str) -> Self {
        let mut nodes = Vec::new();
        let mut literal = String::new();
        let mut rest = source;
        while let Some(pos) = rest.find('%') {
            literal.push_str(&rest[..pos]);
            let after = &rest[pos + 1..];
            match parse_token(after) {
                Parsed::Token(tok, used) => {
                    flush(&mut literal, &mut nodes);
                    nodes.push(Node::Token(tok));
                    rest = &after[used..];
                }
                Parsed::Invalid(used) => {
                    flush(&mut literal, &mut nodes);
                    nodes.push(Node::Unknown(format!("%{}", &after[..used])));
                    rest = &after[used..];
                }
                Parsed::NotAToken => {
                    literal.push('%');
                    rest = after;
                }
            }
        }
        literal.push_str(rest);
        flush(&mut literal, &mut nodes);
        let has_ampm = nodes.iter().any(|n| matches!(n, Node::Token(Token::AmPm)));
        Self { source: source.to_owned(), nodes, has_ampm }
    }

    /// The pattern text as given.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Raw text of every `%token` that is unknown or has an invalid parameter.
    pub fn unknown_tokens(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter_map(|n| if let Node::Unknown(s) = n { Some(s.as_str()) } else { None })
            .collect()
    }

    /// `true` if rendering consumes the auto-increment counter (any `%i…` token).
    pub fn uses_counter(&self) -> bool {
        self.nodes.iter().any(|n| matches!(n, Node::Token(Token::Counter { .. })))
    }

    /// `true` if the pattern needs the window title (`%t`) or process name (`%pn`).
    pub fn uses_window_info(&self) -> bool {
        self.nodes.iter().any(|n| matches!(n, Node::Token(Token::WindowTitle | Token::ProcessName)))
    }

    /// `true` if the pattern needs the image size (`%width` / `%height`).
    pub fn uses_dimensions(&self) -> bool {
        self.nodes.iter().any(|n| matches!(n, Node::Token(Token::Width | Token::Height)))
    }
}

fn flush(literal: &mut String, nodes: &mut Vec<Node>) {
    if !literal.is_empty() {
        nodes.push(Node::Literal(std::mem::take(literal)));
    }
}

enum Parsed {
    Token(Token, usize),
    /// Looks like a token; `usize` bytes (after the `%`) are consumed into an Unknown node.
    Invalid(usize),
    NotAToken,
}

/// Parses the text following a `%`.
fn parse_token(after: &str) -> Parsed {
    if after.starts_with('%') {
        return Parsed::Token(Token::Percent, 1);
    }
    let best =
        TABLE.iter().filter(|(name, _)| after.starts_with(name)).max_by_key(|(name, _)| name.len());
    let Some((name, spec)) = best else {
        // `%word` with no known prefix is a probable typo; a bare `%` or `% ` is prose.
        let ident: usize =
            after.chars().take_while(char::is_ascii_alphanumeric).map(char::len_utf8).sum();
        return if ident > 0 { Parsed::Invalid(ident) } else { Parsed::NotAToken };
    };
    let name_len = name.len();
    let tail = &after[name_len..];
    // Optional `{param}`.
    let param = tail.strip_prefix('{').and_then(|t| t.find('}').map(|end| &t[..end]));
    let param_len = param.map_or(0, |p| p.len() + 2);
    let used = name_len + param_len;
    let invalid = Parsed::Invalid(used);
    match *spec {
        Spec::Simple(make) => {
            // These take no parameter; a following `{` is ordinary text.
            Parsed::Token(make(), name_len)
        }
        Spec::Counter(chars, base) => match param {
            None => Parsed::Token(Token::Counter { chars, base, width: 0 }, name_len),
            Some(p) => match p.trim().parse::<usize>() {
                Ok(w) if w <= MAX_COUNTER_WIDTH => {
                    Parsed::Token(Token::Counter { chars, base, width: w }, used)
                }
                _ => invalid,
            },
        },
        Spec::CounterCustom(chars) => {
            let Some(p) = param else { return invalid };
            let mut it = p.split(',').map(str::trim);
            let (Some(b), Some(w), None) = (it.next(), it.next(), it.next()) else {
                return invalid;
            };
            match (b.parse::<usize>(), w.parse::<usize>()) {
                (Ok(base), Ok(width))
                    if (2..=chars.len()).contains(&base) && width <= MAX_COUNTER_WIDTH =>
                {
                    Parsed::Token(Token::Counter { chars, base, width }, used)
                }
                _ => invalid,
            }
        }
        Spec::Random(chars) => match param {
            None => Parsed::Token(Token::Random { chars, count: 1 }, name_len),
            Some(p) => match p.trim().parse::<usize>() {
                Ok(n) if (1..=MAX_RANDOM_COUNT).contains(&n) => {
                    Parsed::Token(Token::Random { chars, count: n }, used)
                }
                _ => invalid,
            },
        },
        Spec::Emoji => match param {
            None => Parsed::Token(Token::Emoji(1), name_len),
            Some(p) => match p.trim().parse::<usize>() {
                Ok(n) if (1..=MAX_RANDOM_COUNT).contains(&n) => {
                    Parsed::Token(Token::Emoji(n), used)
                }
                _ => invalid,
            },
        },
        Spec::RandomFile => match param {
            Some(p) if !p.trim().is_empty() => Parsed::Token(Token::RandomFile(p.to_owned()), used),
            _ => invalid,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(s: &str) -> Vec<Node> {
        Pattern::parse(s).nodes
    }

    #[test]
    fn literal_only() {
        assert_eq!(nodes("hello"), vec![Node::Literal("hello".into())]);
        assert_eq!(nodes(""), vec![]);
    }

    #[test]
    fn longest_match_wins() {
        assert_eq!(nodes("%mon2"), vec![Node::Token(Token::MonthName)]);
        assert_eq!(
            nodes("%mon"),
            vec![Node::Token(Token::MonthName)],
            "%mon must not parse as %mo + n"
        );
        assert_eq!(nodes("%width"), vec![Node::Token(Token::Width)]);
        assert_eq!(nodes("%ms"), vec![Node::Token(Token::Millis)]);
        assert_eq!(nodes("%uln"), vec![Node::Token(Token::UserDomain)]);
    }

    #[test]
    fn escaped_percent() {
        assert_eq!(nodes("100%%"), vec![Node::Literal("100".into()), Node::Token(Token::Percent)]);
    }

    #[test]
    fn bare_percent_is_literal() {
        assert_eq!(nodes("50% off"), vec![Node::Literal("50% off".into())]);
        assert_eq!(nodes("trailing%"), vec![Node::Literal("trailing%".into())]);
    }

    #[test]
    fn unknown_tokens_are_reported() {
        let p = Pattern::parse("a%foo_%q-%y");
        assert_eq!(p.unknown_tokens(), vec!["%foo", "%q"]);
    }

    #[test]
    fn parameters() {
        assert_eq!(
            nodes("%i{5}"),
            vec![Node::Token(Token::Counter { chars: NUMBERS, base: 10, width: 5 })]
        );
        assert_eq!(nodes("%ra{8}"), vec![Node::Token(Token::Random { chars: ALNUM, count: 8 })]);
        assert_eq!(
            nodes("%ib{16,4}"),
            vec![Node::Token(Token::Counter { chars: ALNUM_INVERSE, base: 16, width: 4 })]
        );
        assert_eq!(
            nodes("%rf{words.txt}"),
            vec![Node::Token(Token::RandomFile("words.txt".into()))]
        );
    }

    #[test]
    fn invalid_parameters_are_unknown() {
        for bad in [
            "%i{abc}",
            "%i{99999}",
            "%ra{0}",
            "%ra{9999}",
            "%ib",
            "%ib{1,4}",
            "%ib{99,4}",
            "%rf{}",
            "%rf",
        ] {
            let p = Pattern::parse(bad);
            assert_eq!(p.unknown_tokens().len(), 1, "{bad}: {:?}", p.nodes);
        }
    }

    #[test]
    fn unclosed_brace_is_literal_after_token() {
        assert_eq!(nodes("%y{"), vec![Node::Token(Token::Year), Node::Literal("{".into())]);
        assert_eq!(nodes("%y{abc}"), vec![Node::Token(Token::Year), Node::Literal("{abc}".into())]);
    }

    #[test]
    fn introspection() {
        let p = Pattern::parse("%t_%pn_%width_%i_%h%pm");
        assert!(p.uses_window_info());
        assert!(p.uses_dimensions());
        assert!(p.uses_counter());
        assert!(p.has_ampm);
        let p = Pattern::parse("%y");
        assert!(!p.uses_window_info() && !p.uses_dimensions() && !p.uses_counter());
    }

    #[test]
    fn non_ascii_after_percent() {
        assert_eq!(nodes("%é"), vec![Node::Literal("%é".into())]);
        assert_eq!(nodes("%日本"), vec![Node::Literal("%日本".into())]);
    }

    #[test]
    fn known_names_listed() {
        let names: Vec<_> = known_token_names().collect();
        for n in [
            "y", "yy", "mo", "mon", "mon2", "d", "h", "mi", "s", "ms", "pm", "w", "w2", "wy",
            "unix", "t", "pn", "un", "uln", "cn", "width", "height", "i", "ra", "rn", "guid",
            "GUID",
        ] {
            assert!(names.contains(&n), "{n}");
        }
    }
}
