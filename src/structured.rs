//! Structured (JSON) log lines: one normalised [`Entry`] from the dialects our services write
//! (zap, pino, slog, Effect), and the styled text the viewer draws from it, both inline
//! (`message key=value …`) and expanded (indented, YAML-like, multi-line strings as blocks).
//!
//! Nothing here knows about the UI: text comes out as a string plus byte ranges tagged with a
//! [`Tok`], and the view picks the colours.

use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Trace => "TRACE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
            Severity::Fatal => "FATAL",
        }
    }

    fn parse(v: &Value) -> Option<Severity> {
        if let Some(n) = v.as_u64() {
            // pino's numeric levels.
            return Some(match n {
                0..=10 => Severity::Trace,
                11..=20 => Severity::Debug,
                21..=30 => Severity::Info,
                31..=40 => Severity::Warn,
                41..=50 => Severity::Error,
                _ => Severity::Fatal,
            });
        }
        let s = v.as_str()?.to_ascii_lowercase();
        Some(match s.as_str() {
            "trace" | "verbose" => Severity::Trace,
            "debug" | "dbg" => Severity::Debug,
            "info" | "information" | "notice" => Severity::Info,
            "warn" | "warning" => Severity::Warn,
            "error" | "err" => Severity::Error,
            "fatal" | "panic" | "dpanic" | "critical" | "crit" | "emergency" | "alert" => Severity::Fatal,
            _ => return None,
        })
    }
}

/// A JSON log line, normalised.
#[derive(Debug)]
pub struct Entry {
    /// Time of day, `HH:MM:SS.mmm` (UTC when the source is an epoch).
    pub time: Option<String>,
    pub severity: Option<Severity>,
    pub logger: Option<String>,
    pub message: String,
    /// Everything else, in the order it was logged, noise removed.
    pub fields: Vec<(String, Value)>,
}

const TIME_KEYS: &[&str] = &["ts", "time", "timestamp", "@timestamp", "t"];
const LEVEL_KEYS: &[&str] = &["level", "severity", "lvl", "log.level"];
const MESSAGE_KEYS: &[&str] = &["msg", "message", "@message"];
const LOGGER_KEYS: &[&str] = &["logger", "logger_name", "name"];
/// Fields every line of a pod repeats: pino's `pid` and `hostname` (the pod's name).
const NOISE_KEYS: &[&str] = &["pid", "hostname", "v"];
/// Objects whose fields read as the line's own (Effect's annotations).
const HOISTED_KEYS: &[&str] = &["annotations"];

impl Entry {
    /// `None` unless the line is a JSON object.
    pub fn parse(line: &str) -> Option<Entry> {
        let line = line.trim();
        if !line.starts_with('{') {
            return None;
        }
        let Value::Object(mut obj) = serde_json::from_str::<Value>(line).ok()? else { return None };

        let time = take_first(&mut obj, TIME_KEYS).map(|v| time_of_day(&v));
        let severity = take_first_where(&mut obj, LEVEL_KEYS, |v| Severity::parse(v).is_some()).and_then(|v| Severity::parse(&v));
        let logger = take_first_where(&mut obj, LOGGER_KEYS, Value::is_string).and_then(|v| v.as_str().map(str::to_owned));

        let mut fields = Vec::new();
        let mut message = String::new();
        match take_first(&mut obj, MESSAGE_KEYS) {
            Some(Value::String(s)) => message = s,
            // Effect logs its arguments as an array: strings make the message, objects add fields.
            Some(Value::Array(items)) => {
                let mut extra = Vec::new();
                for item in items {
                    match item {
                        Value::String(s) => push_word(&mut message, &s),
                        Value::Object(o) => fields.extend(o),
                        other => extra.push(other),
                    }
                }
                if !extra.is_empty() {
                    fields.push(("args".to_owned(), Value::Array(extra)));
                }
            }
            Some(other) => message = compact(&other),
            None => {}
        }

        for (k, v) in obj {
            if NOISE_KEYS.contains(&k.as_str()) || is_empty(&v) {
                continue;
            }
            match v {
                Value::Object(inner) if HOISTED_KEYS.contains(&k.as_str()) => fields.extend(inner),
                v => fields.push((k, v)),
            }
        }
        // A line with no message of its own (Effect logging only a cause) leads with the cause.
        if message.is_empty()
            && let Some(i) = fields.iter().position(|(k, v)| k == "cause" && v.is_string())
        {
            let (_, v) = fields.remove(i);
            message = v.as_str().unwrap_or_default().to_owned();
        }
        Some(Entry { time, severity, logger, message, fields })
    }
}

fn push_word(s: &mut String, w: &str) {
    if !s.is_empty() {
        s.push(' ');
    }
    s.push_str(w);
}

fn take_first(obj: &mut Map<String, Value>, keys: &[&str]) -> Option<Value> {
    take_first_where(obj, keys, |_| true)
}

fn take_first_where(obj: &mut Map<String, Value>, keys: &[&str], ok: impl Fn(&Value) -> bool) -> Option<Value> {
    let key = keys.iter().find(|k| obj.get(**k).is_some_and(&ok))?;
    obj.shift_remove(*key)
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(o) => o.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// `2026-11-14T15:00:01.377Z` → `15:00:01.377`; epoch milliseconds or seconds → UTC.
fn time_of_day(v: &Value) -> String {
    match v {
        Value::String(s) => match s.find('T') {
            Some(i) => {
                let t = &s[i + 1..];
                let end = t.find(|c: char| !(c.is_ascii_digit() || c == ':' || c == '.')).unwrap_or(t.len());
                t[..end.min(12)].to_owned()
            }
            None => s.clone(),
        },
        Value::Number(n) => {
            let Some(x) = n.as_f64() else { return n.to_string() };
            let ms = if x > 1e11 { x } else { x * 1000.0 } as u64;
            let day = ms % 86_400_000;
            format!("{:02}:{:02}:{:02}.{:03}", day / 3_600_000, day / 60_000 % 60, day / 1000 % 60, day % 1000)
        }
        other => compact(other),
    }
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

// ---- Styled text -----------------------------------------------------------------------------

/// What a run of styled text is, for the view to colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Key,
    /// The logger's name.
    Logger,
    Str,
    Num,
    Bool,
    Null,
    /// Punctuation and elisions.
    Punct,
}

/// Text plus the byte ranges styled by [`Tok`]; unstyled text is the line's own colour.
#[derive(Default, Debug)]
pub struct Styled {
    pub text: String,
    pub runs: Vec<(usize, usize, Tok)>,
}

impl Styled {
    fn push(&mut self, s: &str, tok: Option<Tok>) {
        let start = self.text.len();
        self.text.push_str(s);
        if let Some(t) = tok
            && !s.is_empty()
        {
            self.runs.push((start, self.text.len(), t));
        }
    }
}

/// Longest value shown inline before it's elided; the expanded view shows all of it.
const INLINE_MAX: usize = 160;

/// The message alone, for the head of a tree.
pub fn headline(e: &Entry) -> Styled {
    let mut out = Styled::default();
    out.push(&first_line(&e.message, usize::MAX), None);
    out
}

/// `message  key=value key=value`, on one logical line.
pub fn inline(e: &Entry) -> Styled {
    let mut out = Styled::default();
    out.push(&first_line(&e.message, usize::MAX), None);
    for (k, v) in &e.fields {
        out.push(if out.text.is_empty() { "" } else { "  " }, None);
        out.push(k, Some(Tok::Key));
        out.push("=", Some(Tok::Punct));
        inline_value(&mut out, v);
    }
    out
}

fn inline_value(out: &mut Styled, v: &Value) {
    match v {
        Value::String(s) if s.is_empty() => out.push("\"\"", Some(Tok::Punct)),
        Value::String(s) => {
            let shown = first_line(s, INLINE_MAX);
            let quote = shown.contains(' ');
            if quote {
                out.push("\"", Some(Tok::Punct));
            }
            out.push(&shown, Some(Tok::Str));
            if quote {
                out.push("\"", Some(Tok::Punct));
            }
        }
        Value::Number(n) => out.push(&n.to_string(), Some(Tok::Num)),
        Value::Bool(b) => out.push(&b.to_string(), Some(Tok::Bool)),
        Value::Null => out.push("null", Some(Tok::Null)),
        // An error object reads as its message; the rest (a stack) is in the expanded view.
        Value::Object(o) if o.get("message").is_some_and(Value::is_string) => {
            inline_value(out, &o["message"]);
        }
        Value::Object(o) => {
            let start = out.text.len();
            out.push("{", Some(Tok::Punct));
            for (i, (k, v)) in o.iter().enumerate() {
                if out.text.len() - start > INLINE_MAX {
                    out.push(" …", Some(Tok::Punct));
                    break;
                }
                if i > 0 {
                    out.push(" ", None);
                }
                out.push(k, Some(Tok::Key));
                out.push("=", Some(Tok::Punct));
                inline_value(out, v);
            }
            out.push("}", Some(Tok::Punct));
        }
        Value::Array(a) => {
            let start = out.text.len();
            out.push("[", Some(Tok::Punct));
            for (i, v) in a.iter().enumerate() {
                if out.text.len() - start > INLINE_MAX {
                    out.push(" …", Some(Tok::Punct));
                    break;
                }
                if i > 0 {
                    out.push(", ", Some(Tok::Punct));
                }
                inline_value(out, v);
            }
            out.push("]", Some(Tok::Punct));
        }
    }
}

/// The first line of `s`, at most `max` characters, marking what's cut with `…`.
fn first_line(s: &str, max: usize) -> String {
    let mut lines = s.lines();
    let first = lines.next().unwrap_or("");
    let cut = elide(first, max);
    if lines.next().is_some() && cut.len() == first.len() { format!("{cut} …") } else { cut }
}

fn elide(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// Every field, one per line, nested values indented; multi-line strings (stack traces) as
/// indented blocks. Empty when there are no fields.
pub fn expanded(e: &Entry) -> Styled {
    let mut out = Styled::default();
    // A multi-line message shows whole here, since the inline view keeps only its first line.
    if e.message.contains('\n') {
        block(&mut out, "message", &Value::String(e.message.clone()), 0);
    }
    for (k, v) in &e.fields {
        block(&mut out, k, v, 0);
    }
    if out.text.ends_with('\n') {
        out.text.pop();
    }
    out
}

fn indent(out: &mut Styled, depth: usize) {
    out.push(&"  ".repeat(depth), None);
}

fn block(out: &mut Styled, key: &str, v: &Value, depth: usize) {
    indent(out, depth);
    out.push(key, Some(Tok::Key));
    out.push(":", Some(Tok::Punct));
    value_block(out, v, depth);
}

/// Writes what follows `key:` (or `- `): a scalar on the same line, or nested lines below.
fn value_block(out: &mut Styled, v: &Value, depth: usize) {
    match v {
        Value::Object(o) if !o.is_empty() => {
            out.push("\n", None);
            for (k, v) in o {
                block(out, k, v, depth + 1);
            }
        }
        Value::Array(a) if !a.is_empty() => {
            out.push("\n", None);
            for item in a {
                indent(out, depth + 1);
                out.push("-", Some(Tok::Punct));
                value_block(out, item, depth + 1);
            }
        }
        Value::String(s) if s.contains('\n') => {
            out.push(" |", Some(Tok::Punct));
            out.push("\n", None);
            for line in s.trim_end().lines() {
                indent(out, depth + 1);
                out.push(line, Some(Tok::Str));
                out.push("\n", None);
            }
        }
        scalar => {
            out.push(" ", None);
            match scalar {
                Value::String(s) if s.is_empty() => out.push("\"\"", Some(Tok::Punct)),
                Value::String(s) => out.push(s, Some(Tok::Str)),
                Value::Number(n) => out.push(&n.to_string(), Some(Tok::Num)),
                Value::Bool(b) => out.push(&b.to_string(), Some(Tok::Bool)),
                Value::Object(_) => out.push("{}", Some(Tok::Punct)),
                Value::Array(_) => out.push("[]", Some(Tok::Punct)),
                Value::Null => out.push("null", Some(Tok::Null)),
            }
            out.push("\n", None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zap() {
        let e = Entry::parse(r#"{"level":"error","ts":"2026-11-14T15:00:01.377Z","logger":"billing","msg":"unable to update integration","error":"","integration":"ab12"}"#).unwrap();
        assert_eq!(e.time.as_deref(), Some("15:00:01.377"));
        assert_eq!(e.severity, Some(Severity::Error));
        assert_eq!(e.logger.as_deref(), Some("billing"));
        assert_eq!(e.message, "unable to update integration");
        assert_eq!(inline(&e).text, r#"unable to update integration  error=""  integration=ab12"#);
    }

    #[test]
    fn pino_with_error_and_stack() {
        let e = Entry::parse(r#"{"level":50,"time":1794664800582,"pid":1,"hostname":"api-x","integration":"cd34","err":{"type":"Error","message":"Invalid Refresh Token","stack":"Error: Invalid\n    at a (x.js:1:2)\n    at b (y.js:3:4)"}}"#).unwrap();
        assert_eq!(e.severity, Some(Severity::Error));
        assert_eq!(e.time.as_deref(), Some("14:00:00.582"));
        assert!(e.fields.iter().all(|(k, _)| k != "pid" && k != "hostname"));
        assert_eq!(inline(&e).text, r#"integration=cd34  err="Invalid Refresh Token""#);
        let x = expanded(&e).text;
        assert_eq!(x, "integration: cd34\nerr:\n  type: Error\n  message: Invalid Refresh Token\n  stack: |\n    Error: Invalid\n        at a (x.js:1:2)\n        at b (y.js:3:4)");
    }

    #[test]
    fn effect_message_array_and_annotations() {
        let e = Entry::parse(r##"{"message":["Extension sync published","ef56",{"extensions":5}],"level":"INFO","timestamp":"2026-11-14T15:09:42.694Z","annotations":{"workPlanID":"aa78"},"spans":{},"fiberId":"#20"}"##).unwrap();
        assert_eq!(e.severity, Some(Severity::Info));
        assert_eq!(e.message, "Extension sync published ef56");
        assert_eq!(inline(&e).text, "Extension sync published ef56  extensions=5  workPlanID=aa78  fiberId=#20");
    }

    #[test]
    fn effect_cause_becomes_the_message() {
        let e = Entry::parse(r#"{"message":[],"level":"INFO","timestamp":"2026-11-06T00:29:39.307Z","cause":"HttpServerError: RouteNotFound (HEAD /)","annotations":{"http.status":404}}"#).unwrap();
        assert_eq!(e.message, "HttpServerError: RouteNotFound (HEAD /)");
        assert_eq!(inline(&e).text, "HttpServerError: RouteNotFound (HEAD /)  http.status=404");
    }

    #[test]
    fn slog_nanoseconds_and_nested() {
        let e = Entry::parse(r#"{"time":"2026-11-14T15:00:09.490358595Z","level":"INFO","msg":"completed work plan","plan":{"ID":"bb90","RetryCount":0}}"#).unwrap();
        assert_eq!(e.time.as_deref(), Some("15:00:09.490"));
        assert_eq!(inline(&e).text, "completed work plan  plan={ID=bb90 RetryCount=0}");
        assert_eq!(expanded(&e).text, "plan:\n  ID: bb90\n  RetryCount: 0");
    }

    #[test]
    fn plain_text_is_not_structured() {
        assert!(Entry::parse("ERROR: FetchError: Unleash").is_none());
        assert!(Entry::parse("{not json").is_none());
        assert!(Entry::parse("[1,2]").is_none());
    }

    #[test]
    fn runs_cover_their_text() {
        let e = Entry::parse(r#"{"msg":"hi","n":1,"ok":true,"s":"a b"}"#).unwrap();
        let s = inline(&e);
        let pick = |t: Tok| s.runs.iter().filter(|r| r.2 == t).map(|r| &s.text[r.0..r.1]).collect::<Vec<_>>();
        assert_eq!(pick(Tok::Key), ["n", "ok", "s"]);
        assert_eq!(pick(Tok::Num), ["1"]);
        assert_eq!(pick(Tok::Bool), ["true"]);
        assert_eq!(pick(Tok::Str), ["a b"]);
    }
}
