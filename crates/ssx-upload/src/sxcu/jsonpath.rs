//! A JSONPath evaluator covering what ShareX (Newtonsoft `SelectToken`) accepts.
//!
//! Supported: `$`, `.name`, `['name']`/`["name"]`, `[n]` (negative counts from the end),
//! `[a,b]` unions, `[start:end:step]` slices, `.*`/`[*]`, recursive descent (`..name`,
//! `..[*]`), and filters `[?(@.a == 'x' && @.b > 3)]` with `== != < <= > >= && || !` and
//! bare `@.a` existence tests. Script expressions (`[(@.length-1)]`) and regex match
//! (`=~`) are reported as unsupported instead of silently misbehaving.
//!
//! ShareX prefixes `$.` to paths that do not start with `$.`; [`normalize`] does the same
//! but is also lenient about paths starting with `[` or `$` alone.

use serde_json::Value;

/// A path that could not be parsed or uses an unsupported feature.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JsonPathError {
    /// Malformed path.
    #[error("invalid JSONPath '{path}': {why}")]
    Syntax {
        /// The path as given.
        path: String,
        /// What is wrong.
        why: String,
    },
    /// Valid JSONPath that this implementation does not support.
    #[error("unsupported JSONPath feature in '{path}': {what}")]
    Unsupported {
        /// The path as given.
        path: String,
        /// The unsupported construct.
        what: String,
    },
}

#[derive(Debug, Clone)]
enum Selector {
    Name(String),
    Index(i64),
    Wildcard,
    Slice(Option<i64>, Option<i64>, Option<i64>),
    Union(Vec<Selector>),
    Filter(Expr),
}

#[derive(Debug, Clone)]
struct Step {
    recursive: bool,
    selector: Selector,
}

#[derive(Debug, Clone)]
enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(Operand, CmpOp, Operand),
    Exists(Operand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
enum Operand {
    Path(Vec<PathPart>),
    Literal(Value),
}

#[derive(Debug, Clone)]
enum PathPart {
    Name(String),
    Index(i64),
}

/// Apply ShareX's path normalisation (`a.b` becomes `$.a.b`).
pub fn normalize(path: &str) -> String {
    let p = path.trim();
    if p.starts_with('$') {
        p.to_owned()
    } else if p.starts_with('[') {
        format!("${p}")
    } else {
        format!("$.{p}")
    }
}

/// All values matched by `path`, in document order.
pub fn select_all<'a>(root: &'a Value, path: &str) -> Result<Vec<&'a Value>, JsonPathError> {
    let norm = normalize(path);
    let steps = Parser::new(&norm).parse_path()?;
    let mut current: Vec<&Value> = vec![root];
    for step in &steps {
        let mut next = Vec::new();
        for node in current {
            if step.recursive {
                let mut all = Vec::new();
                descendants(node, &mut all);
                for d in all {
                    apply(&step.selector, d, &mut next);
                }
            } else {
                apply(&step.selector, node, &mut next);
            }
        }
        current = next;
    }
    Ok(current)
}

/// The first value matched by `path` (what `{json:...}` uses).
pub fn select_first<'a>(root: &'a Value, path: &str) -> Result<Option<&'a Value>, JsonPathError> {
    Ok(select_all(root, path)?.into_iter().next())
}

fn descendants<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(v);
    match v {
        Value::Array(a) => a.iter().for_each(|c| descendants(c, out)),
        Value::Object(o) => o.values().for_each(|c| descendants(c, out)),
        _ => {}
    }
}

fn children(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => o.values().collect(),
        _ => Vec::new(),
    }
}

fn resolve_index(len: usize, idx: i64) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let i = if idx < 0 { len + idx } else { idx };
    if (0..len).contains(&i) { usize::try_from(i).ok() } else { None }
}

fn apply<'a>(sel: &Selector, node: &'a Value, out: &mut Vec<&'a Value>) {
    match sel {
        Selector::Name(n) => {
            if let Some(v) = node.as_object().and_then(|o| o.get(n)) {
                out.push(v);
            }
        }
        Selector::Index(i) => {
            if let Some(v) = node.as_array().and_then(|a| resolve_index(a.len(), *i).and_then(|i| a.get(i))) {
                out.push(v);
            }
        }
        Selector::Wildcard => out.extend(children(node)),
        Selector::Slice(start, end, step) => {
            let Some(arr) = node.as_array() else { return };
            let len = i64::try_from(arr.len()).unwrap_or(i64::MAX);
            let step = step.unwrap_or(1);
            if step == 0 {
                return;
            }
            let norm = |i: i64| if i < 0 { (len + i).max(0) } else { i.min(len) };
            if step > 0 {
                let (s, e) = (start.map_or(0, norm), end.map_or(len, norm));
                let mut i = s;
                while i < e {
                    if let Some(v) = usize::try_from(i).ok().and_then(|i| arr.get(i)) {
                        out.push(v);
                    }
                    i += step;
                }
            } else {
                let (s, e) = (start.map_or(len - 1, |s| norm(s).min(len - 1)), end.map(norm));
                let mut i = s;
                while e.is_none_or(|e| i > e) && i >= 0 {
                    if let Some(v) = usize::try_from(i).ok().and_then(|i| arr.get(i)) {
                        out.push(v);
                    }
                    i += step;
                }
            }
        }
        Selector::Union(list) => list.iter().for_each(|s| apply(s, node, out)),
        Selector::Filter(expr) => {
            for child in children(node) {
                if eval_bool(expr, child) {
                    out.push(child);
                }
            }
        }
    }
}

fn eval_bool(e: &Expr, current: &Value) -> bool {
    match e {
        Expr::Or(a, b) => eval_bool(a, current) || eval_bool(b, current),
        Expr::And(a, b) => eval_bool(a, current) && eval_bool(b, current),
        Expr::Not(a) => !eval_bool(a, current),
        Expr::Exists(op) => operand_value(op, current).is_some_and(|v| !v.is_null()),
        Expr::Cmp(l, op, r) => {
            let (Some(l), Some(r)) = (operand_value(l, current), operand_value(r, current)) else {
                return *op == CmpOp::Ne;
            };
            compare(&l, *op, &r)
        }
    }
}

fn operand_value(op: &Operand, current: &Value) -> Option<Value> {
    match op {
        Operand::Literal(v) => Some(v.clone()),
        Operand::Path(parts) => {
            let mut v = current;
            for p in parts {
                v = match p {
                    PathPart::Name(n) => v.as_object()?.get(n)?,
                    PathPart::Index(i) => {
                        let a = v.as_array()?;
                        a.get(resolve_index(a.len(), *i)?)?
                    }
                };
            }
            Some(v.clone())
        }
    }
}

fn compare(l: &Value, op: CmpOp, r: &Value) -> bool {
    use std::cmp::Ordering;
    let ord: Option<Ordering> = match (l, r) {
        (Value::Number(a), Value::Number(b)) => a.as_f64().partial_cmp(&b.as_f64()),
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        _ => None,
    };
    match (op, ord) {
        (CmpOp::Eq, o) => o == Some(Ordering::Equal),
        (CmpOp::Ne, o) => o != Some(Ordering::Equal),
        (CmpOp::Lt, Some(o)) => o == Ordering::Less,
        (CmpOp::Le, Some(o)) => o != Ordering::Greater,
        (CmpOp::Gt, Some(o)) => o == Ordering::Greater,
        (CmpOp::Ge, Some(o)) => o != Ordering::Less,
        _ => false,
    }
}

struct Parser<'a> {
    src: &'a str,
    chars: Vec<char>,
    pos: usize,
}

type PResult<T> = Result<T, JsonPathError>;

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, chars: src.chars().collect(), pos: 0 }
    }

    fn err<T>(&self, why: impl Into<String>) -> PResult<T> {
        Err(JsonPathError::Syntax { path: self.src.to_owned(), why: format!("{} (at offset {})", why.into(), self.pos) })
    }

    fn unsupported<T>(&self, what: impl Into<String>) -> PResult<T> {
        Err(JsonPathError::Unsupported { path: self.src.to_owned(), what: what.into() })
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    fn parse_path(&mut self) -> PResult<Vec<Step>> {
        if !self.eat('$') {
            return self.err("path must start with '$'");
        }
        let mut steps = Vec::new();
        while let Some(c) = self.peek() {
            match c {
                '.' => {
                    self.pos += 1;
                    let recursive = self.eat('.');
                    let selector = match self.peek() {
                        Some('*') => {
                            self.pos += 1;
                            Selector::Wildcard
                        }
                        Some('[') if recursive => {
                            self.pos += 1;
                            self.parse_bracket()?
                        }
                        _ => Selector::Name(self.parse_name()?),
                    };
                    steps.push(Step { recursive, selector });
                }
                '[' => {
                    self.pos += 1;
                    let selector = self.parse_bracket()?;
                    steps.push(Step { recursive: false, selector });
                }
                _ => return self.err(format!("unexpected character '{c}'")),
            }
        }
        Ok(steps)
    }

    fn parse_name(&mut self) -> PResult<String> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c != '.' && c != '[') {
            self.pos += 1;
        }
        if self.pos == start {
            return self.err("empty property name");
        }
        Ok(self.chars[start..self.pos].iter().collect())
    }

    /// After `[`; consumes the closing `]`.
    fn parse_bracket(&mut self) -> PResult<Selector> {
        self.skip_ws();
        let sel = match self.peek() {
            Some('*') => {
                self.pos += 1;
                Selector::Wildcard
            }
            Some('?') => {
                self.pos += 1;
                self.skip_ws();
                if !self.eat('(') {
                    return self.err("filter must be written [?( ... )]");
                }
                let expr = self.parse_or()?;
                self.skip_ws();
                if !self.eat(')') {
                    return self.err("unterminated filter, expected ')'");
                }
                Selector::Filter(expr)
            }
            Some('(') => return self.unsupported("script expressions '[(...)]'"),
            Some('\'' | '"') => {
                let mut names = vec![Selector::Name(self.parse_quoted()?)];
                loop {
                    self.skip_ws();
                    if !self.eat(',') {
                        break;
                    }
                    self.skip_ws();
                    names.push(Selector::Name(self.parse_quoted()?));
                }
                if names.len() == 1 { names.remove(0) } else { Selector::Union(names) }
            }
            _ => self.parse_index_or_slice()?,
        };
        self.skip_ws();
        if !self.eat(']') {
            return self.err("expected ']'");
        }
        Ok(sel)
    }

    fn parse_quoted(&mut self) -> PResult<String> {
        let Some(quote) = self.peek().filter(|c| *c == '\'' || *c == '"') else {
            return self.err("expected a quoted string");
        };
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated string"),
                Some(c) if c == quote => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some(c) => out.push(c),
                        None => return self.err("unterminated escape"),
                    }
                    self.pos += 1;
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn parse_int(&mut self) -> PResult<Option<i64>> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return Ok(None);
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        match s.parse::<i64>() {
            Ok(n) => Ok(Some(n)),
            Err(_) => self.err(format!("bad integer '{s}'")),
        }
    }

    fn parse_index_or_slice(&mut self) -> PResult<Selector> {
        let first = self.parse_int()?;
        self.skip_ws();
        if self.peek() == Some(':') {
            self.pos += 1;
            let end = self.parse_int()?;
            self.skip_ws();
            let step = if self.eat(':') { self.parse_int()? } else { None };
            return Ok(Selector::Slice(first, end, step));
        }
        let Some(first) = first else { return self.err("expected an index, slice, quoted name or '*'") };
        let mut items = vec![Selector::Index(first)];
        loop {
            self.skip_ws();
            if !self.eat(',') {
                break;
            }
            match self.parse_int()? {
                Some(n) => items.push(Selector::Index(n)),
                None => return self.err("expected an index after ','"),
            }
        }
        Ok(if items.len() == 1 { items.remove(0) } else { Selector::Union(items) })
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') && self.peek_at(1) == Some('|') {
                self.pos += 2;
                let right = self.parse_and()?;
                left = Expr::Or(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('&') && self.peek_at(1) == Some('&') {
                self.pos += 2;
                let right = self.parse_unary()?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        self.skip_ws();
        if self.peek() == Some('!') && self.peek_at(1) != Some('=') {
            self.pos += 1;
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.peek() == Some('(') {
            self.pos += 1;
            let e = self.parse_or()?;
            self.skip_ws();
            if !self.eat(')') {
                return self.err("expected ')'");
            }
            return Ok(e);
        }
        let left = self.parse_operand()?;
        self.skip_ws();
        let op = match (self.peek(), self.peek_at(1)) {
            (Some('='), Some('=')) => Some((CmpOp::Eq, 2)),
            (Some('!'), Some('=')) => Some((CmpOp::Ne, 2)),
            (Some('<'), Some('=')) => Some((CmpOp::Le, 2)),
            (Some('>'), Some('=')) => Some((CmpOp::Ge, 2)),
            (Some('<'), _) => Some((CmpOp::Lt, 1)),
            (Some('>'), _) => Some((CmpOp::Gt, 1)),
            (Some('='), Some('~')) => return self.unsupported("regex match '=~'"),
            _ => None,
        };
        match op {
            Some((op, len)) => {
                self.pos += len;
                let right = self.parse_operand()?;
                Ok(Expr::Cmp(left, op, right))
            }
            None => Ok(Expr::Exists(left)),
        }
    }

    fn parse_operand(&mut self) -> PResult<Operand> {
        self.skip_ws();
        match self.peek() {
            Some('@') => {
                self.pos += 1;
                let mut parts = Vec::new();
                loop {
                    match self.peek() {
                        Some('.') => {
                            self.pos += 1;
                            let start = self.pos;
                            while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                                self.pos += 1;
                            }
                            if start == self.pos {
                                return self.err("empty property name in filter");
                            }
                            parts.push(PathPart::Name(self.chars[start..self.pos].iter().collect()));
                        }
                        Some('[') => {
                            self.pos += 1;
                            self.skip_ws();
                            if matches!(self.peek(), Some('\'' | '"')) {
                                parts.push(PathPart::Name(self.parse_quoted()?));
                            } else if let Some(n) = self.parse_int()? {
                                parts.push(PathPart::Index(n));
                            } else {
                                return self.err("expected index or quoted name in filter");
                            }
                            self.skip_ws();
                            if !self.eat(']') {
                                return self.err("expected ']' in filter");
                            }
                        }
                        _ => break,
                    }
                }
                Ok(Operand::Path(parts))
            }
            Some('\'' | '"') => Ok(Operand::Literal(Value::String(self.parse_quoted()?))),
            Some(c) if c.is_ascii_digit() || c == '-' => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
                    self.pos += 1;
                }
                let s: String = self.chars[start..self.pos].iter().collect();
                match serde_json::from_str::<Value>(&s) {
                    Ok(v @ Value::Number(_)) => Ok(Operand::Literal(v)),
                    _ => self.err(format!("bad number '{s}'")),
                }
            }
            Some(c) if c.is_alphabetic() => {
                let start = self.pos;
                while self.peek().is_some_and(char::is_alphanumeric) {
                    self.pos += 1;
                }
                let word: String = self.chars[start..self.pos].iter().collect();
                match word.as_str() {
                    "true" => Ok(Operand::Literal(Value::Bool(true))),
                    "false" => Ok(Operand::Literal(Value::Bool(false))),
                    "null" => Ok(Operand::Literal(Value::Null)),
                    _ => self.err(format!("unknown word '{word}' in filter")),
                }
            }
            _ => self.err("expected '@', a literal or '(' in filter"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn first(v: &Value, p: &str) -> Option<Value> {
        select_first(v, p).unwrap().cloned()
    }

    #[test]
    fn plain_paths_and_normalisation() {
        let v = json!({"data": {"link": "http://x/1.png", "id": 5}, "files": [{"url": "a"}, {"url": "b"}]});
        assert_eq!(first(&v, "data.link"), Some(json!("http://x/1.png")));
        assert_eq!(first(&v, "$.data.id"), Some(json!(5)));
        assert_eq!(first(&v, "files[1].url"), Some(json!("b")));
        assert_eq!(first(&v, "files[-1].url"), Some(json!("b")));
        assert_eq!(first(&v, "$['data']['link']"), Some(json!("http://x/1.png")));
        assert_eq!(first(&v, "['data'].id"), Some(json!(5)));
        assert_eq!(first(&v, "$"), Some(v.clone()));
        assert_eq!(first(&v, "missing.key"), None);
        assert_eq!(first(&v, "files[9].url"), None);
    }

    #[test]
    fn wildcards_recursion_slices_unions() {
        let v = json!({"a": [1, 2, 3, 4, 5], "b": {"c": {"id": 1}, "d": {"id": 2}}});
        assert_eq!(select_all(&v, "a[*]").unwrap().len(), 5);
        assert_eq!(select_all(&v, "a[1:3]").unwrap(), vec![&json!(2), &json!(3)]);
        assert_eq!(select_all(&v, "a[-2:]").unwrap(), vec![&json!(4), &json!(5)]);
        assert_eq!(select_all(&v, "a[::2]").unwrap(), vec![&json!(1), &json!(3), &json!(5)]);
        assert_eq!(select_all(&v, "a[::-2]").unwrap(), vec![&json!(5), &json!(3), &json!(1)]);
        assert_eq!(select_all(&v, "a[0,2]").unwrap(), vec![&json!(1), &json!(3)]);
        assert_eq!(select_all(&v, "$..id").unwrap(), vec![&json!(1), &json!(2)]);
        assert_eq!(select_all(&v, "b.*.id").unwrap(), vec![&json!(1), &json!(2)]);
        assert_eq!(select_all(&v, "b['c','d'].id").unwrap().len(), 2);
    }

    #[test]
    fn filters() {
        let v = json!({"items": [
            {"n": "a", "size": 10, "ok": true},
            {"n": "b", "size": 30, "ok": false},
            {"n": "c", "size": 50}
        ]});
        assert_eq!(select_all(&v, "items[?(@.size > 20)].n").unwrap(), vec![&json!("b"), &json!("c")]);
        assert_eq!(select_all(&v, "items[?(@.n == 'a' || @.n == \"c\")].n").unwrap().len(), 2);
        assert_eq!(select_all(&v, "items[?(@.size >= 10 && @.ok == true)].n").unwrap(), vec![&json!("a")]);
        assert_eq!(select_all(&v, "items[?(@.ok)].n").unwrap().len(), 2, "existence test");
        assert_eq!(select_all(&v, "items[?(!@.ok)].n").unwrap(), vec![&json!("c")]);
        assert_eq!(select_all(&v, "items[?(@.n != 'a')].n").unwrap().len(), 2);
    }

    #[test]
    fn errors_are_reported_not_panics() {
        let v = json!({});
        for bad in ["a[", "a[1", "a['x", "a[?(@.x", "a[?(@.x ==)]", "a..", "a[]", "a[1:2:x]", "a.[0]"] {
            assert!(select_all(&v, bad).is_err(), "{bad} should be an error");
        }
        assert!(matches!(select_all(&v, "a[(@.length-1)]"), Err(JsonPathError::Unsupported { .. })));
        assert!(matches!(select_all(&v, "a[?(@.x =~ /a/)]"), Err(JsonPathError::Unsupported { .. })));
    }

    #[test]
    fn unicode_keys() {
        let v = json!({"ключ": {"値": "ok"}});
        assert_eq!(first(&v, "ключ.値"), Some(json!("ok")));
    }
}
