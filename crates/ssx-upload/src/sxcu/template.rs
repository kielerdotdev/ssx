//! The `.sxcu` template language: `{function:arg|arg}` with `\` escapes and nesting.
//!
//! The grammar follows ShareX's `ShareXSyntaxParser`:
//!
//! ```text
//! text     := (literal | '\' any | call)*
//! call     := '{' name [ ':' arg ('|' arg)* ] '}'
//! ```
//!
//! Inside a call name, `:` starts the arguments and `}` ends the call. Inside an argument
//! `|` starts the next argument and `}` ends the call, while `:` is an ordinary character
//! (so `{regex:href="(.+)"|1}` and `{json:{response}|a.b}` work). Arguments may contain
//! nested calls. Function names are case-insensitive.
//!
//! Differences from upstream, all towards leniency: a stray `}` or `|` outside any call is
//! literal (upstream silently truncates the text there), and an unterminated call is closed
//! at the end of input (as upstream does). Nesting deeper than [`MAX_DEPTH`] is rejected so
//! hostile templates cannot overflow the stack.
//!
//! Parsing produces an AST first so [`crate::sxcu::CustomUploader::validate`] can inspect
//! templates statically (unknown functions, missing arguments, response-only functions used
//! in request fields) before any network traffic happens.

use base64::Engine as _;

use super::jsonpath::{self, JsonPathError};
use crate::nameparser::NameParser;
pub use crate::util::url_encode;

/// Maximum call nesting depth.
pub const MAX_DEPTH: usize = 48;

/// Escape `s` so that it renders back to exactly `s` (no call is started, `\` is kept).
pub fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '{' | '}' | '|' | ':') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Errors from parsing or evaluating a template.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// Calls nested deeper than [`MAX_DEPTH`].
    #[error("template nesting is deeper than {MAX_DEPTH} levels")]
    TooDeep,
    /// `{}` or `{:x}`.
    #[error("function name cannot be empty")]
    EmptyName,
    /// A function that does not exist.
    #[error("invalid function name: {0}")]
    UnknownFunction(String),
    /// Too few arguments.
    #[error("function '{function}' needs at least {min} parameter(s)")]
    MissingParameters {
        /// Function name.
        function: String,
        /// Minimum count.
        min: usize,
    },
    /// A response-only function used where no response exists (request fields).
    #[error("function '{0}' can only be used in response templates (URL, ThumbnailURL, DeletionURL, ErrorMessage)")]
    NoResponse(String),
    /// Input to `{json:}` is not JSON.
    #[error("expected JSON but the input is not valid JSON: {0}")]
    InvalidJson(String),
    /// Bad JSONPath.
    #[error(transparent)]
    JsonPath(#[from] JsonPathError),
    /// Input to `{xml:}` is not XML.
    #[error("expected XML but the input is not valid XML: {0}")]
    InvalidXml(String),
    /// Bad XPath.
    #[error("invalid XPath: {0}")]
    XPath(String),
    /// Bad regular expression (or one that backtracks too much).
    #[error("regex error: {0}")]
    Regex(String),
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Template {
    /// Top level nodes.
    pub nodes: Vec<Node>,
}

/// AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Literal text (escapes already resolved).
    Literal(String),
    /// A `{...}` call.
    Call(Call),
}

/// A `{name:args}` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The function name (usually a single literal; nested calls are allowed).
    pub name: Vec<Node>,
    /// Arguments, `None` when there is no `:`.
    pub args: Option<Vec<Vec<Node>>>,
}

impl Call {
    /// The name when it is a plain literal.
    pub fn literal_name(&self) -> Option<String> {
        match self.name.as_slice() {
            [] => Some(String::new()),
            [Node::Literal(s)] => Some(s.clone()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Top,
    Name,
    Arg,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn seq(&mut self, ctx: Ctx, depth: usize) -> Result<Vec<Node>, TemplateError> {
        let mut nodes = Vec::new();
        let mut lit = String::new();
        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    self.pos += 1;
                    if let Some(n) = self.peek() {
                        lit.push(n);
                        self.pos += 1;
                    }
                }
                '{' => {
                    if depth >= MAX_DEPTH {
                        return Err(TemplateError::TooDeep);
                    }
                    self.pos += 1;
                    if !lit.is_empty() {
                        nodes.push(Node::Literal(std::mem::take(&mut lit)));
                    }
                    nodes.push(Node::Call(self.call(depth + 1)?));
                }
                '}' if ctx != Ctx::Top => break,
                '|' if ctx == Ctx::Arg => break,
                ':' if ctx == Ctx::Name => break,
                c => {
                    lit.push(c);
                    self.pos += 1;
                }
            }
        }
        if !lit.is_empty() {
            nodes.push(Node::Literal(lit));
        }
        Ok(nodes)
    }

    /// After `{`.
    fn call(&mut self, depth: usize) -> Result<Call, TemplateError> {
        let name = self.seq(Ctx::Name, depth)?;
        match self.peek() {
            Some(':') => {
                self.pos += 1;
                let mut args = Vec::new();
                loop {
                    args.push(self.seq(Ctx::Arg, depth)?);
                    match self.peek() {
                        Some('|') => self.pos += 1,
                        Some('}') => {
                            self.pos += 1;
                            break;
                        }
                        _ => break,
                    }
                }
                Ok(Call { name, args: Some(args) })
            }
            Some('}') => {
                self.pos += 1;
                Ok(Call { name, args: None })
            }
            _ => Ok(Call { name, args: None }),
        }
    }
}

impl Template {
    /// Parse `text`.
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let mut p = Parser { chars: text.chars().collect(), pos: 0 };
        let nodes = p.seq(Ctx::Top, 0)?;
        Ok(Self { nodes })
    }

    /// Every call in the template, including nested ones, outermost first.
    pub fn calls(&self) -> Vec<&Call> {
        fn walk<'a>(nodes: &'a [Node], out: &mut Vec<&'a Call>) {
            for n in nodes {
                if let Node::Call(c) = n {
                    out.push(c);
                    walk(&c.name, out);
                    for a in c.args.iter().flatten() {
                        walk(a, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.nodes, &mut out);
        out
    }

    /// Evaluate against `env`.
    pub fn render(&self, env: &Env<'_>) -> Result<String, TemplateError> {
        render_nodes(&self.nodes, env)
    }
}

/// Parse and evaluate `text` in one go.
pub fn render(text: &str, env: &Env<'_>) -> Result<String, TemplateError> {
    Template::parse(text)?.render(env)
}

/// Like [`render`], but first expands `%` codes (outside `\`-escaped characters), which is
/// what ShareX does for `Parameters`, `Headers` and `Arguments`.
pub fn render_with_names(
    text: &str,
    names: &NameParser,
    env: &Env<'_>,
) -> Result<String, TemplateError> {
    render(&expand_names_keeping_escapes(text, names), env)
}

/// Run the name parser over `text` but leave `\x` pairs untouched (escape kept), so the
/// template parser can still see them.
pub fn expand_names_keeping_escapes(text: &str, names: &NameParser) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push_str(&names.parse(&run));
            run.clear();
            out.push('\\');
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            run.push(c);
        }
    }
    out.push_str(&names.parse(&run));
    out
}

fn render_nodes(nodes: &[Node], env: &Env<'_>) -> Result<String, TemplateError> {
    let mut out = String::new();
    for n in nodes {
        match n {
            Node::Literal(s) => out.push_str(s),
            Node::Call(c) => out.push_str(&eval_call(c, env)?),
        }
    }
    Ok(out)
}

/// Response data visible to response templates.
#[derive(Debug, Clone, Default)]
pub struct TemplateResponse {
    /// Body text.
    pub text: String,
    /// Final URL after redirects.
    pub url: String,
    /// Response headers (name, value); names compared case-insensitively.
    pub headers: Vec<(String, String)>,
}

impl TemplateResponse {
    fn header(&self, name: &str) -> Option<String> {
        let vals: Vec<&str> = self
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect();
        if vals.is_empty() { None } else { Some(vals.join(", ")) }
    }
}

/// Hooks for the interactive functions (`select`, `inputbox`, `outputbox`).
pub trait Interaction: Send + Sync {
    /// `{select:a|b|c}`: choose one of the non-empty options; `None` cancels (empty result).
    fn select(&self, options: &[String]) -> Option<String>;
    /// `{inputbox:title|default}`: ask for text; `None` cancels (empty result).
    fn input_box(&self, title: &str, default: &str) -> Option<String>;
    /// `{outputbox:title|text}`: show text to the user. The function itself yields "".
    fn output_box(&self, title: &str, text: &str);
}

/// Headless behaviour: `select` picks the first option, `inputbox` returns its default
/// text, `outputbox` logs at info level.
#[derive(Debug, Clone, Copy, Default)]
pub struct NonInteractive;

impl Interaction for NonInteractive {
    fn select(&self, options: &[String]) -> Option<String> {
        options.first().cloned()
    }

    fn input_box(&self, _title: &str, default: &str) -> Option<String> {
        Some(default.to_owned())
    }

    fn output_box(&self, title: &str, text: &str) {
        tracing::info!(title, text, "custom uploader output box");
    }
}

/// Evaluation environment.
pub struct Env<'a> {
    /// `{filename}`.
    pub file_name: &'a str,
    /// `{input}` (text/URL uploads; empty for files).
    pub input: &'a str,
    /// Response for the response-only functions; `None` in request templates.
    pub response: Option<&'a TemplateResponse>,
    /// Percent-encode `{filename}` and `{input}` (ShareX does this for the request URL and
    /// for response templates only).
    pub url_encode: bool,
    /// Interactive function handler.
    pub interaction: &'a dyn Interaction,
}

impl std::fmt::Debug for Env<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("file_name", &self.file_name)
            .field("has_response", &self.response.is_some())
            .field("url_encode", &self.url_encode)
            .finish_non_exhaustive()
    }
}

/// Static description of a template function (used by validation and the docs).
#[derive(Debug, Clone, Copy)]
pub struct FunctionSpec {
    /// Canonical lowercase name.
    pub name: &'static str,
    /// Alternative names.
    pub aliases: &'static [&'static str],
    /// Minimum argument count.
    pub min_params: usize,
    /// Needs a response (given the number of arguments actually supplied).
    pub needs_response: fn(usize) -> bool,
    /// Involves user interaction.
    pub interactive: bool,
}

fn never(_: usize) -> bool {
    false
}
fn always(_: usize) -> bool {
    true
}

/// All functions ShareX defines.
pub const FUNCTIONS: &[FunctionSpec] = &[
    FunctionSpec { name: "base64", aliases: &[], min_params: 1, needs_response: never, interactive: false },
    FunctionSpec { name: "filename", aliases: &[], min_params: 0, needs_response: never, interactive: false },
    FunctionSpec { name: "header", aliases: &[], min_params: 1, needs_response: always, interactive: false },
    FunctionSpec { name: "input", aliases: &[], min_params: 0, needs_response: never, interactive: false },
    FunctionSpec { name: "inputbox", aliases: &["prompt"], min_params: 0, needs_response: never, interactive: true },
    FunctionSpec { name: "json", aliases: &[], min_params: 1, needs_response: |n| n < 2, interactive: false },
    FunctionSpec { name: "outputbox", aliases: &[], min_params: 1, needs_response: never, interactive: true },
    FunctionSpec { name: "random", aliases: &[], min_params: 2, needs_response: never, interactive: false },
    FunctionSpec { name: "regex", aliases: &[], min_params: 1, needs_response: |n| n < 3, interactive: false },
    FunctionSpec { name: "response", aliases: &[], min_params: 0, needs_response: always, interactive: false },
    FunctionSpec { name: "responseurl", aliases: &[], min_params: 0, needs_response: always, interactive: false },
    FunctionSpec { name: "select", aliases: &[], min_params: 1, needs_response: never, interactive: true },
    FunctionSpec { name: "xml", aliases: &[], min_params: 1, needs_response: |n| n < 2, interactive: false },
];

/// Look a function up by name or alias (case-insensitive).
pub fn lookup(name: &str) -> Option<&'static FunctionSpec> {
    FUNCTIONS.iter().find(|f| {
        f.name.eq_ignore_ascii_case(name) || f.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
    })
}

fn eval_call(call: &Call, env: &Env<'_>) -> Result<String, TemplateError> {
    let name = render_nodes(&call.name, env)?;
    if name.is_empty() {
        return Err(TemplateError::EmptyName);
    }
    // Upstream evaluates every argument (including nested calls) before dispatching.
    let args: Vec<String> = match &call.args {
        Some(list) => list.iter().map(|a| render_nodes(a, env)).collect::<Result<_, _>>()?,
        None => Vec::new(),
    };
    let spec = lookup(&name).ok_or_else(|| TemplateError::UnknownFunction(name.clone()))?;
    if args.len() < spec.min_params {
        return Err(TemplateError::MissingParameters { function: spec.name.to_owned(), min: spec.min_params });
    }
    if (spec.needs_response)(args.len()) && env.response.is_none() {
        return Err(TemplateError::NoResponse(spec.name.to_owned()));
    }
    let response = env.response;
    let resp_text = || response.map(|r| r.text.as_str()).unwrap_or_default();
    let out: Option<String> = match spec.name {
        "base64" => Some(base64::engine::general_purpose::STANDARD.encode(args[0].as_bytes())).filter(|_| !args[0].is_empty()),
        "filename" => Some(if env.url_encode { url_encode(env.file_name) } else { env.file_name.to_owned() }),
        "input" => Some(if env.url_encode { url_encode(env.input) } else { env.input.to_owned() }),
        "header" => response.and_then(|r| r.header(&args[0])),
        "response" => Some(resp_text().to_owned()),
        "responseurl" => response.map(|r| r.url.clone()),
        "random" => {
            let i = rand::Rng::random_range(&mut rand::rng(), 0..args.len());
            args.get(i).cloned()
        }
        "select" => {
            let options: Vec<String> = args.iter().filter(|a| !a.is_empty()).cloned().collect();
            if options.is_empty() { None } else { env.interaction.select(&options) }
        }
        "inputbox" => {
            let title = args.first().map_or("Input", String::as_str);
            let default = args.get(1).map_or("", String::as_str);
            env.interaction.input_box(title, default)
        }
        "outputbox" => {
            let (title, text) = if args.len() > 1 { (args[0].as_str(), args[1].as_str()) } else { ("", args[0].as_str()) };
            if !text.is_empty() {
                env.interaction.output_box(if title.is_empty() { "Output" } else { title }, text);
            }
            None
        }
        "json" => {
            let (input, path) = if args.len() > 1 { (args[0].as_str(), args[1].as_str()) } else { (resp_text(), args[0].as_str()) };
            json_select(input, path)?
        }
        "xml" => {
            let (input, path) = if args.len() > 1 { (args[0].as_str(), args[1].as_str()) } else { (resp_text(), args[0].as_str()) };
            xml_select(input, path)?
        }
        "regex" => {
            let (input, pattern, group) = if args.len() > 2 {
                (args[0].as_str(), args[1].as_str(), args[2].as_str())
            } else {
                (resp_text(), args[0].as_str(), args.get(1).map_or("", String::as_str))
            };
            regex_select(input, pattern, group)?
        }
        _ => None,
    };
    Ok(out.unwrap_or_default())
}

fn json_select(input: &str, path: &str) -> Result<Option<String>, TemplateError> {
    if input.is_empty() || path.is_empty() {
        return Ok(None);
    }
    let doc: serde_json::Value = serde_json::from_str(input).map_err(|e| TemplateError::InvalidJson(e.to_string()))?;
    Ok(jsonpath::select_first(&doc, path)?.and_then(json_to_string))
}

/// Upstream casts the token to `string`: strings as-is, numbers/bools by value, null as
/// nothing. It throws for objects/arrays; we return their compact JSON instead.
fn json_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn xml_select(input: &str, xpath: &str) -> Result<Option<String>, TemplateError> {
    if input.is_empty() || xpath.is_empty() {
        return Ok(None);
    }
    // sxd-document/sxd-xpath are old and unaudited against hostile input; a panic inside
    // them must not take the upload worker down, so it becomes an ordinary error.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| xml_select_inner(input, xpath)));
    match result {
        Ok(r) => r,
        Err(_) => Err(TemplateError::InvalidXml("the XML engine failed on this document".into())),
    }
}

fn xml_select_inner(input: &str, xpath: &str) -> Result<Option<String>, TemplateError> {
    let package = sxd_document::parser::parse(input).map_err(|e| TemplateError::InvalidXml(e.to_string()))?;
    let doc = package.as_document();
    let value = sxd_xpath::evaluate_xpath(&doc, xpath).map_err(|e| TemplateError::XPath(e.to_string()))?;
    Ok(match value {
        sxd_xpath::Value::Nodeset(ns) => ns.document_order_first().map(|n| n.string_value()),
        other => Some(other.string()),
    })
}

fn regex_select(input: &str, pattern: &str, group: &str) -> Result<Option<String>, TemplateError> {
    if input.is_empty() || pattern.is_empty() {
        return Ok(None);
    }
    let re = fancy_regex::RegexBuilder::new(pattern)
        .backtrack_limit(1_000_000)
        .build()
        .map_err(|e| TemplateError::Regex(e.to_string()))?;
    let caps = re.captures(input).map_err(|e| TemplateError::Regex(e.to_string()))?;
    let Some(caps) = caps else { return Ok(None) };
    let whole = caps.get(0).map(|m| m.as_str().to_owned());
    if group.is_empty() {
        return Ok(whole);
    }
    let m = match group.parse::<usize>() {
        Ok(n) => caps.get(n),
        Err(_) => caps.name(group),
    };
    // A group that did not participate (or does not exist) is "" like .NET's Group.Empty.
    Ok(Some(m.map(|m| m.as_str().to_owned()).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(resp: Option<&'a TemplateResponse>) -> Env<'a> {
        Env { file_name: "my file.png", input: "hi there", response: resp, url_encode: false, interaction: &NonInteractive }
    }

    fn resp(text: &str) -> TemplateResponse {
        TemplateResponse {
            text: text.into(),
            url: "https://final.example/x".into(),
            headers: vec![("Location".into(), "https://loc/1".into()), ("X-A".into(), "1".into()), ("x-a".into(), "2".into())],
        }
    }

    fn r(t: &str) -> String {
        render(t, &env(None)).unwrap()
    }

    fn rr(t: &str, body: &str) -> Result<String, TemplateError> {
        let rs = resp(body);
        render(t, &env(Some(&rs)))
    }

    #[test]
    fn plain_text_and_escapes() {
        assert_eq!(r("plain"), "plain");
        assert_eq!(r(r"\{filename\}"), "{filename}");
        assert_eq!(r(r"a\\b"), r"a\b");
        assert_eq!(r("trailing\\"), "trailing");
        assert_eq!(r("stray } and | outside"), "stray } and | outside");
        assert_eq!(r(""), "");
    }

    #[test]
    fn filename_and_input_with_url_encoding() {
        assert_eq!(r("{filename}|{input}"), "my file.png|hi there");
        let mut e = env(None);
        e.url_encode = true;
        assert_eq!(render("{filename}/{input}", &e).unwrap(), "my%20file.png/hi%20there");
        assert_eq!(render("{FILENAME}", &e).unwrap(), "my%20file.png", "names are case-insensitive");
    }

    #[test]
    fn url_encode_matches_sharex_unreserved_set() {
        assert_eq!(url_encode("aZ09-._~"), "aZ09-._~");
        assert_eq!(url_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }

    #[test]
    fn base64_random_and_min_params() {
        assert_eq!(r("{base64:hello}"), "aGVsbG8=");
        assert_eq!(r("{base64:}"), "");
        assert_eq!(r("{base64:héllo ✓}"), "aMOpbGxvIOKckw==");
        let picked = r("{random:a|b|c}");
        assert!(["a", "b", "c"].contains(&picked.as_str()));
        assert!(matches!(render("{random:only}", &env(None)), Err(TemplateError::MissingParameters { min: 2, .. })));
        assert!(matches!(render("{base64}", &env(None)), Err(TemplateError::MissingParameters { .. })));
    }

    #[test]
    fn random_covers_all_options() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(r("{random:x|y}"));
        }
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn nesting_and_arguments_with_colons() {
        assert_eq!(r("{base64:{filename}}"), "bXkgZmlsZS5wbmc=");
        assert_eq!(rr("{json:{response}|a.b}", r#"{"a":{"b":"deep"}}"#).unwrap(), "deep");
        assert_eq!(r("{random:http://a|http://b}").len(), 8);
        assert_eq!(r(r"{random:x\|y|x\|y}"), "x|y");
    }

    #[test]
    fn unknown_and_empty_functions() {
        assert!(matches!(render("{nope}", &env(None)), Err(TemplateError::UnknownFunction(n)) if n == "nope"));
        assert!(matches!(render("{}", &env(None)), Err(TemplateError::EmptyName)));
        assert!(matches!(render("{:x}", &env(None)), Err(TemplateError::EmptyName)));
    }

    #[test]
    fn unterminated_call_is_closed_at_end() {
        assert_eq!(r("{filename"), "my file.png");
        assert_eq!(r("{base64:abc"), "YWJj");
    }

    #[test]
    fn response_functions() {
        assert_eq!(rr("{response}", "body").unwrap(), "body");
        assert_eq!(rr("{responseurl}", "").unwrap(), "https://final.example/x");
        assert_eq!(rr("{header:location}", "").unwrap(), "https://loc/1");
        assert_eq!(rr("{header:X-A}", "").unwrap(), "1, 2", "multiple values are joined");
        assert_eq!(rr("[{header:Missing}]", "").unwrap(), "[]");
        assert!(matches!(render("{response}", &env(None)), Err(TemplateError::NoResponse(_))));
        assert!(matches!(render("{json:a}", &env(None)), Err(TemplateError::NoResponse(_))));
        assert_eq!(render("{json:x|a}", &env(None)).unwrap_err(), TemplateError::InvalidJson("expected value at line 1 column 1".into()));
    }

    #[test]
    fn json_function_edge_cases() {
        let body = r#"{"data":{"link":"http://i/1.png","id":42,"ok":true,"none":null,"list":[{"u":"a"},{"u":"b"}],"obj":{"k":1}},"ключ":"значение"}"#;
        assert_eq!(rr("{json:data.link}", body).unwrap(), "http://i/1.png");
        assert_eq!(rr("{json:$.data.id}", body).unwrap(), "42");
        assert_eq!(rr("{json:data.ok}", body).unwrap(), "true");
        assert_eq!(rr("{json:data.none}", body).unwrap(), "");
        assert_eq!(rr("{json:data.list[1].u}", body).unwrap(), "b");
        assert_eq!(rr("{json:data.list[7].u}", body).unwrap(), "", "missing index is empty");
        assert_eq!(rr("{json:data.missing.deeper}", body).unwrap(), "");
        assert_eq!(rr("{json:data.obj}", body).unwrap(), r#"{"k":1}"#);
        assert_eq!(rr("{json:ключ}", body).unwrap(), "значение");
        assert_eq!(rr("{json:$..u}", body).unwrap(), "a", "first match wins");
        assert!(matches!(rr("{json:data.link}", "not json"), Err(TemplateError::InvalidJson(_))));
        assert_eq!(rr("{json:data.link}", "").unwrap(), "", "empty response yields empty");
        assert!(matches!(rr("{json:data[}", body), Err(TemplateError::JsonPath(_))));
    }

    #[test]
    fn xml_function() {
        let body = "<?xml version=\"1.0\"?><r><files><file><url>http://a/1</url></file><file><url>http://a/2</url></file></files><n>5</n></r>";
        assert_eq!(rr("{xml:/r/files/file[2]/url}", body).unwrap(), "http://a/2");
        assert_eq!(rr("{xml:/r/files/file/url}", body).unwrap(), "http://a/1", "first node in document order");
        assert_eq!(rr("{xml:/r/missing}", body).unwrap(), "");
        assert_eq!(rr("{xml:count(/r/files/file)}", body).unwrap(), "2");
        assert_eq!(rr("{xml:{response}|/r/n}", body).unwrap(), "5");
        assert!(matches!(rr("{xml:/r}", "<r><unclosed>"), Err(TemplateError::InvalidXml(_))));
        assert!(matches!(rr("{xml:/r[}", body), Err(TemplateError::XPath(_))));
    }

    #[test]
    fn regex_function() {
        let html = r#"<a href="https://x.example/f/abc123">dl</a>"#;
        assert_eq!(rr(r#"{regex:(?<=href=").+?(?=")}"#, html).unwrap(), "https://x.example/f/abc123");
        assert_eq!(rr(r#"{regex:href="(.+?)"|1}"#, html).unwrap(), "https://x.example/f/abc123");
        assert_eq!(rr(r#"{regex:href="(?<url>.+?)"|url}"#, html).unwrap(), "https://x.example/f/abc123");
        assert_eq!(rr(r#"{regex:{response}|href="(.+?)"|1}"#, html).unwrap(), "https://x.example/f/abc123");
        assert_eq!(rr("{regex:nomatch}", html).unwrap(), "");
        assert_eq!(rr("{regex:href|9}", html).unwrap(), "", "nonexistent group is empty like .NET");
        assert_eq!(rr("{regex:(a)|(b)?|2}", "a").unwrap(), "");
        assert!(matches!(rr("{regex:(}", html), Err(TemplateError::Regex(_))));
        assert_eq!(rr("{regex:(.)\\\\1}", "xaabx").unwrap(), "aa", "back references work");
    }

    #[test]
    fn catastrophic_regex_is_bounded() {
        let body = "a".repeat(64) + "!";
        let res = rr("{regex:(a+)+$}", &body);
        // Either it finishes or reports the backtracking limit; it must not hang.
        assert!(res.is_ok() || matches!(res, Err(TemplateError::Regex(_))));
    }

    #[test]
    fn interactive_functions_are_headless_by_default() {
        assert_eq!(r("{select:first|second}"), "first");
        assert_eq!(r("{select:|second}"), "second");
        assert_eq!(r("{select:}"), "");
        assert_eq!(r("{inputbox}"), "");
        assert_eq!(r("{inputbox:Title|dflt}"), "dflt");
        assert_eq!(r("{prompt:T|d2}"), "d2");
        assert_eq!(r("[{outputbox:Title|shown}]"), "[]");
    }

    struct Scripted;
    impl Interaction for Scripted {
        fn select(&self, options: &[String]) -> Option<String> {
            options.last().cloned()
        }
        fn input_box(&self, title: &str, _default: &str) -> Option<String> {
            Some(format!("<{title}>"))
        }
        fn output_box(&self, _title: &str, _text: &str) {}
    }

    #[test]
    fn interaction_can_be_injected() {
        let e = Env { file_name: "", input: "", response: None, url_encode: false, interaction: &Scripted };
        assert_eq!(render("{select:a|b|c}{inputbox:T}", &e).unwrap(), "c<T>");
    }

    #[test]
    fn depth_limit() {
        let deep = "{a:".repeat(MAX_DEPTH + 5);
        assert_eq!(Template::parse(&deep).unwrap_err(), TemplateError::TooDeep);
        let ok = format!("{}x{}", "{base64:".repeat(10), "}".repeat(10));
        assert!(Template::parse(&ok).is_ok());
    }

    #[test]
    fn calls_are_listed_for_validation() {
        let t = Template::parse("a{json:x|{header:y}}b{filename}").unwrap();
        let names: Vec<_> = t.calls().iter().filter_map(|c| c.literal_name()).collect();
        assert_eq!(names, vec!["json", "header", "filename"]);
    }

    #[test]
    fn name_parser_runs_before_templates_and_respects_escapes() {
        let names = NameParser { auto_increment: 3, ..NameParser::text() };
        assert_eq!(render_with_names("n=%i {filename}", &names, &env(None)).unwrap(), "n=3 my file.png");
        assert_eq!(render_with_names(r"\%i %i", &names, &env(None)).unwrap(), "%i 3");
        // A code that expands to template syntax is not re-evaluated as a call.
        assert_eq!(expand_names_keeping_escapes(r"a\{%i\}", &names), r"a\{3\}");
    }

    #[test]
    fn escape_literal_round_trips_examples() {
        for s in ["", "plain", "a{b}c", "x|y:z", r"back\slash", "{json:a}", "日本語{}", "\\{"] {
            assert_eq!(r(&escape_literal(s)), s, "{s:?}");
        }
    }
}
