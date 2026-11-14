//! The log filter's query language. Bare words match anywhere in the line; `key op value`
//! compares one field of a structured line:
//!
//! ```text
//! timeout                         lines containing "timeout" (any case)
//! "connection reset"              a phrase
//! integration = '5f1e…'           a field equals (any case; `*` is a wildcard)
//! err.message ~ refresh           a nested field contains (`:` works too)
//! status >= 500   level >= warn   numbers, and levels by severity
//! method != GET   msg !~ health   negations
//! a b             a and b         both (the default between terms)
//! a or b   not a   -a   (a or b) c
//! ```
//!
//! Keys are matched without case. `msg`, `level`, `logger`, `time`, `pod` and `raw` name the
//! line's parts; any other key is a field, with dots reaching into nested objects. A missing
//! field matches only `!=` and `!~`.

use std::cmp::Ordering;

use serde_json::Value;

use crate::logs::{Level, Line};
use crate::structured::Severity;

#[derive(Debug, PartialEq)]
pub enum Query {
    /// Case-insensitive substring of the pod name and raw line; stored lowercased.
    Text(String),
    Cmp { key: String, op: Op, value: String },
    Not(Box<Query>),
    And(Vec<Query>),
    Or(Vec<Query>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Contains,
    NotContains,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Query {
    /// `Ok(None)` for an empty query.
    pub fn parse(src: &str) -> Result<Option<Query>, String> {
        let tokens = tokenize(src)?;
        if tokens.is_empty() {
            return Ok(None);
        }
        let mut p = Parser { tokens, pos: 0 };
        let q = p.or()?;
        match p.peek() {
            None => Ok(Some(q)),
            Some(Tok::RParen) => Err("unmatched )".into()),
            Some(t) => Err(format!("unexpected {}", t.describe())),
        }
    }

    pub fn matches(&self, line: &Line) -> bool {
        match self {
            Query::Text(s) => line.haystack.contains(s.as_str()),
            Query::Not(q) => !q.matches(line),
            Query::And(qs) => qs.iter().all(|q| q.matches(line)),
            Query::Or(qs) => qs.iter().any(|q| q.matches(line)),
            Query::Cmp { key, op, value } => compare(line, key, *op, value),
        }
    }
}

// ---- Tokens ----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    /// A quoted string: never a keyword or a key.
    Str(String),
    Op(Op),
    LParen,
    RParen,
    /// `-` before a term, `!` or `not`.
    Not,
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::Word(w) | Tok::Str(w) => format!("\"{w}\""),
            Tok::Op(_) => "operator".into(),
            Tok::LParen => "(".into(),
            Tok::RParen => ")".into(),
            Tok::Not => "not".into(),
        }
    }
}

const OP_CHARS: &[char] = &['=', '!', '<', '>', '~', ':'];

/// Whether `w` can be a key, so an operator right after it splits it off: `status>=500`, but
/// `10:00:01` and `/a/:id` stay whole words.
fn is_key(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '@')
        && w.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | '@' | '/'))
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let (toks, err) = scan(src);
    match err {
        Some(e) => Err(e),
        None => Ok(toks.into_iter().map(|(t, _)| t).collect()),
    }
}

/// The tokens with their byte ranges in `src`, and the first error, scanning past it.
fn scan(src: &str) -> (Vec<(Tok, std::ops::Range<usize>)>, Option<String>) {
    let chars: Vec<char> = src.chars().collect();
    let mut bytes: Vec<usize> = src.char_indices().map(|(b, _)| b).collect();
    bytes.push(src.len());
    let mut out: Vec<(Tok, std::ops::Range<usize>)> = Vec::new();
    let mut err = None;
    let mut push = |t: Tok, from: usize, to: usize| out.push((t, bytes[from]..bytes[to]));
    let mut i = 0;
    // Right after an operator, a bare value runs to the next space or paren, colons and all.
    let mut value_next = false;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            let end = chars[i + 1..].iter().position(|&d| d == c);
            let close = end.map_or(chars.len(), |e| i + 1 + e);
            if end.is_none() {
                err.get_or_insert_with(|| format!("unclosed {c}"));
            }
            push(Tok::Str(chars[i + 1..close].iter().collect()), i, (close + 1).min(chars.len()));
            i = close + 1;
            value_next = false;
            continue;
        }
        if c == '(' || c == ')' {
            push(if c == '(' { Tok::LParen } else { Tok::RParen }, i, i + 1);
            i += 1;
            value_next = false;
            continue;
        }
        if value_next {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '(' && chars[i] != ')' {
                i += 1;
            }
            push(Tok::Str(chars[start..i].iter().collect()), start, i);
            value_next = false;
            continue;
        }
        if let Some((op, len)) = op_at(&chars[i..]) {
            push(Tok::Op(op), i, i + len);
            i += len;
            value_next = true;
            continue;
        }
        if (c == '-' || c == '!') && chars.get(i + 1).is_some_and(|d| !d.is_whitespace()) {
            push(Tok::Not, i, i + 1);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], '(' | ')' | '\'' | '"') {
            if OP_CHARS.contains(&chars[i]) && is_key(&chars[start..i].iter().collect::<String>()) && op_at(&chars[i..]).is_some() {
                break;
            }
            i += 1;
        }
        push(Tok::Word(chars[start..i].iter().collect()), start, i);
    }
    (out, err)
}

/// What a stretch of the query is, for colouring it as it's typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// A field name before an operator.
    Key,
    Op,
    /// A comparison's value, or a quoted phrase.
    Value,
    /// `and`, `or`, `not`, `-`, `!` and parentheses.
    Keyword,
}

/// Byte ranges of `src` to colour.
pub fn highlight(src: &str) -> Vec<(std::ops::Range<usize>, Part)> {
    let (toks, _) = scan(src);
    let mut out = Vec::new();
    for (i, (t, r)) in toks.iter().enumerate() {
        let part = match t {
            Tok::Word(_) if matches!(toks.get(i + 1), Some((Tok::Op(_), _))) => Part::Key,
            Tok::Word(w) if ["and", "or", "not", "&&", "||"].iter().any(|k| w.eq_ignore_ascii_case(k)) => Part::Keyword,
            Tok::Word(_) => continue,
            Tok::Str(_) => Part::Value,
            Tok::Op(_) => Part::Op,
            Tok::LParen | Tok::RParen | Tok::Not => Part::Keyword,
        };
        out.push((r.clone(), part));
    }
    out
}

fn op_at(s: &[char]) -> Option<(Op, usize)> {
    let two: String = s.iter().take(2).collect();
    Some(match two.as_str() {
        "==" => (Op::Eq, 2),
        "!=" => (Op::Ne, 2),
        "!~" => (Op::NotContains, 2),
        ">=" => (Op::Ge, 2),
        "<=" => (Op::Le, 2),
        _ => match s.first()? {
            '=' => (Op::Eq, 1),
            '~' | ':' => (Op::Contains, 1),
            '>' => (Op::Gt, 1),
            '<' => (Op::Lt, 1),
            _ => return None,
        },
    })
}

// ---- Parser ----------------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    fn or(&mut self) -> Result<Query, String> {
        let mut terms = vec![self.and()?];
        while self.keyword("or") || self.keyword("||") {
            self.pos += 1;
            terms.push(self.and()?);
        }
        Ok(if terms.len() == 1 { terms.remove(0) } else { Query::Or(terms) })
    }

    fn and(&mut self) -> Result<Query, String> {
        let mut terms = vec![self.unary()?];
        loop {
            if self.keyword("and") || self.keyword("&&") {
                self.pos += 1;
            } else if self.peek().is_none() || self.keyword("or") || self.keyword("||") || self.peek() == Some(&Tok::RParen) {
                break;
            }
            terms.push(self.unary()?);
        }
        Ok(if terms.len() == 1 { terms.remove(0) } else { Query::And(terms) })
    }

    fn unary(&mut self) -> Result<Query, String> {
        if self.peek() == Some(&Tok::Not) || self.keyword("not") {
            self.pos += 1;
            return Ok(Query::Not(Box::new(self.unary()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Query, String> {
        match self.next() {
            Some(Tok::LParen) => {
                let q = self.or()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(q),
                    _ => Err("missing )".into()),
                }
            }
            Some(Tok::Word(key)) if matches!(self.peek(), Some(Tok::Op(_))) => {
                let Some(Tok::Op(op)) = self.next() else { unreachable!() };
                match self.next() {
                    Some(Tok::Word(v) | Tok::Str(v)) => Ok(Query::Cmp { key, op, value: v }),
                    _ => Err(format!("{key}: missing a value")),
                }
            }
            Some(Tok::Word(w) | Tok::Str(w)) => Ok(Query::Text(w.to_lowercase())),
            Some(Tok::Op(_)) => Err("an operator needs a key before it".into()),
            Some(Tok::RParen) => Err("unmatched )".into()),
            Some(Tok::Not) | None => Err("unfinished query".into()),
        }
    }
}

// ---- Matching --------------------------------------------------------------------------------

/// What a key names on a line.
enum Found<'a> {
    Text(&'a str),
    Json(&'a Value),
    Severity(Severity),
}

fn compare(line: &Line, key: &str, op: Op, want: &str) -> bool {
    let found = lookup(line, key);
    let Some(found) = found else { return matches!(op, Op::Ne | Op::NotContains) };
    if let Found::Severity(have) = found {
        // `level = warning` and `level >= warn` compare severities; other text falls through.
        if let Some(want) = Severity::parse(&Value::String(want.to_owned())) {
            return ordered(op, have.cmp(&want));
        }
    }
    let text = match found {
        Found::Text(s) => s.to_owned(),
        Found::Json(Value::String(s)) => s.clone(),
        Found::Json(v) => v.to_string(),
        Found::Severity(s) => s.label().to_owned(),
    };
    match op {
        Op::Eq => glob(&text, want),
        Op::Ne => !glob(&text, want),
        Op::Contains => text.to_lowercase().contains(&want.to_lowercase()),
        Op::NotContains => !text.to_lowercase().contains(&want.to_lowercase()),
        _ => match (text.trim().parse::<f64>(), want.parse::<f64>()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b).is_some_and(|o| ordered(op, o)),
            _ => ordered(op, text.to_lowercase().as_str().cmp(want.to_lowercase().as_str())),
        },
    }
}

fn ordered(op: Op, o: Ordering) -> bool {
    match op {
        Op::Eq | Op::Contains => o == Ordering::Equal,
        Op::Ne | Op::NotContains => o != Ordering::Equal,
        Op::Gt => o == Ordering::Greater,
        Op::Ge => o != Ordering::Less,
        Op::Lt => o == Ordering::Less,
        Op::Le => o != Ordering::Greater,
    }
}

fn lookup<'a>(line: &'a Line, key: &str) -> Option<Found<'a>> {
    let e = line.entry.as_deref();
    match key.to_ascii_lowercase().as_str() {
        "msg" | "message" => return Some(Found::Text(e.map_or(line.text.as_str(), |e| e.message.as_str()))),
        "level" | "lvl" | "severity" => {
            let sev = match e {
                Some(e) => e.severity,
                None => match line.level {
                    Level::Error => Some(Severity::Error),
                    Level::Warn => Some(Severity::Warn),
                    Level::Debug => Some(Severity::Debug),
                    Level::Plain => Some(Severity::Info),
                    Level::Meta => None,
                },
            };
            return sev.map(Found::Severity);
        }
        "logger" => return e?.logger.as_deref().map(Found::Text),
        "time" | "ts" => return e?.time.as_deref().map(Found::Text),
        "pod" => return Some(Found::Text(&line.source)),
        "raw" | "line" => return Some(Found::Text(&line.text)),
        _ => {}
    }
    let fields = &e?.fields;
    let get = |key: &str| fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v);
    // The whole key first (`log.origin`), then a path into nested objects (`err.message`).
    if let Some(v) = get(key) {
        return Some(Found::Json(v));
    }
    let mut parts = key.split('.');
    let mut v = get(parts.next()?)?;
    for part in parts {
        v = match v {
            Value::Object(o) => o.iter().find(|(k, _)| k.eq_ignore_ascii_case(part)).map(|(_, v)| v)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(Found::Json(v))
}

/// Case-insensitive equality, where `*` in `pattern` matches any run of characters.
fn glob(text: &str, pattern: &str) -> bool {
    let text = text.to_lowercase();
    let pattern = pattern.to_lowercase();
    if !pattern.contains('*') {
        return text == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::LogBuffer;

    fn line(raw: &str) -> Line {
        let mut b = LogBuffer::default();
        b.push(raw.into(), false);
        b.take_last().unwrap()
    }

    fn hits(q: &str, raw: &str) -> bool {
        Query::parse(q).unwrap().unwrap().matches(&line(raw))
    }

    const J: &str = r#"[pod/api-7d9f-xk2lp/api] {"level":"warn","ts":"2026-11-14T10:00:01.120Z","logger":"http","msg":"slow request","integration":"5F1E0a","status":503,"err":{"message":"invalid_request: token"},"tags":["a","b"]}"#;

    #[test]
    fn parses() {
        assert_eq!(Query::parse("  ").unwrap(), None);
        assert_eq!(
            Query::parse("integration = 'some id'").unwrap(),
            Some(Query::Cmp { key: "integration".into(), op: Op::Eq, value: "some id".into() })
        );
        assert_eq!(
            Query::parse("status>=500").unwrap(),
            Some(Query::Cmp { key: "status".into(), op: Op::Ge, value: "500".into() })
        );
        // Times and paths stay words.
        assert_eq!(Query::parse("10:00:01").unwrap(), Some(Query::Text("10:00:01".into())));
        assert_eq!(Query::parse("/a/:id").unwrap(), Some(Query::Text("/a/:id".into())));
        // Values keep their colons.
        assert_eq!(Query::parse("time > 10:00:01").unwrap(), Some(Query::Cmp { key: "time".into(), op: Op::Gt, value: "10:00:01".into() }));
        // A hyphen inside a word isn't a negation.
        assert_eq!(Query::parse("api-7d9f").unwrap(), Some(Query::Text("api-7d9f".into())));
        assert!(Query::parse("'open").is_err());
        assert!(Query::parse("(a or b").is_err());
        assert!(Query::parse("a)").is_err());
        assert!(Query::parse("status =").is_err());
        assert!(Query::parse("= 3").is_err());
    }

    #[test]
    fn matches_fields() {
        assert!(hits("integration = '5f1e0a'", J));
        assert!(hits("integration = 5f1e*", J));
        assert!(!hits("integration = 5f1e", J));
        assert!(hits("err.message ~ token", J));
        assert!(hits("err.message:TOKEN", J));
        assert!(hits("status >= 500", J));
        assert!(!hits("status < 500", J));
        assert!(hits("tags.1 = b", J));
        assert!(hits("missing != x", J));
        assert!(!hits("missing = *", J));
        assert!(hits("integration = *", J));
    }

    #[test]
    fn matches_line_parts() {
        assert!(hits("level >= warn", J));
        assert!(hits("level = warning", J));
        assert!(!hits("level = error", J));
        assert!(hits("logger = http", J));
        assert!(hits("msg ~ slow", J));
        assert!(hits("pod ~ xk2lp", J));
        assert!(hits("time >= 10:00:01", J));
        // Plain lines have a message, a level from keywords, and no fields.
        let plain = "[pod/web-1/web] ERROR upstream timed out";
        assert!(hits("level = error", plain));
        assert!(hits("msg ~ upstream", plain));
        assert!(!hits("status = 1", plain));
    }

    #[test]
    fn combines() {
        assert!(hits("slow integration=5f1e0a", J));
        assert!(hits("slow and status = 503", J));
        assert!(!hits("slow nothing", J));
        assert!(hits("nothing or slow", J));
        assert!(hits("not nothing", J));
        assert!(hits("-nothing slow", J));
        assert!(!hits("!slow", J));
        assert!(hits("(nothing or status = 503) and level = warn", J));
        assert!(hits("\"slow request\"", J));
        assert!(!hits("\"request slow\"", J));
    }

    /// The README's examples parse, and its claims about them hold.
    #[test]
    fn readme_examples() {
        for q in [
            "timeout",
            "\"connection reset\"",
            "integration = '5f1e0a3b9c2d4e6f7a8b9c0d'",
            "integration = 5f1e*",
            "integration = *",
            "err.message ~ token",
            "status >= 500",
            "level >= warn",
            "method != GET and msg !~ health",
            "pod ~ xk2lp",
            "time >= 10:00:00 time < 10:05:00",
            "(level = error or status >= 500) -health",
            "uri = /a/:id",
        ] {
            assert!(matches!(Query::parse(q), Ok(Some(_))), "{q}");
        }
        assert_eq!(
            Query::parse("uri = /a/:id").unwrap(),
            Some(Query::Cmp { key: "uri".into(), op: Op::Eq, value: "/a/:id".into() })
        );
        // `or` binds looser than `and`.
        assert_eq!(
            Query::parse("a b or c").unwrap(),
            Some(Query::Or(vec![Query::And(vec![Query::Text("a".into()), Query::Text("b".into())]), Query::Text("c".into())]))
        );
        // Quoted keywords are words.
        assert_eq!(Query::parse("'or'").unwrap(), Some(Query::Text("or".into())));
        assert!(hits("time >= 10:00:00 time < 10:05:00", J));
        assert!(hits("(level = error or status >= 500) -health", J));
    }

    #[test]
    fn highlights() {
        let src = "é = 'x' and status>=5 (a)";
        let parts: Vec<(&str, Part)> = highlight(src).into_iter().map(|(r, p)| (&src[r], p)).collect();
        assert_eq!(
            parts,
            [
                ("é", Part::Key),
                ("=", Part::Op),
                ("'x'", Part::Value),
                ("and", Part::Keyword),
                ("status", Part::Key),
                (">=", Part::Op),
                ("5", Part::Value),
                ("(", Part::Keyword),
                (")", Part::Keyword),
            ]
        );
        // An open quote colours to the end.
        assert_eq!(highlight("a = 'xy").last().map(|(r, _)| r.clone()), Some(4..7));
    }

    #[test]
    fn globs() {
        assert!(glob("abcdef", "a*f"));
        assert!(glob("abcdef", "*cd*"));
        assert!(glob("abcdef", "*"));
        assert!(!glob("abcdef", "a*x"));
        assert!(!glob("ab", "ab*ab"));
    }
}
