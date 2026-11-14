//! A bounded log buffer with a live substring filter. Lines get consecutive ids, so a line is
//! found from its id in O(1) even after old lines fall off the front.

use std::collections::{HashMap, VecDeque};

use crate::structured::{Entry, Severity};

/// Lines kept per stream; older ones are dropped.
pub const CAPACITY: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Plain,
    Debug,
    Warn,
    Error,
    /// kubectl's own stderr (warnings, errors fetching a pod's logs).
    Meta,
}

pub struct Line {
    pub id: u64,
    /// The pod (and container, when there are several) the line came from.
    pub source: String,
    /// The pod's colour slot: pods number from 0 in the order they first log.
    pub slot: usize,
    pub text: String,
    pub level: Level,
    /// The line parsed, when it's a JSON object.
    pub entry: Option<Box<Entry>>,
    /// `source` and `text` lowercased, for case-insensitive filtering.
    haystack: String,
}

impl Line {
    fn new(id: u64, raw: String, stderr: bool) -> Line {
        let (source, text) = if stderr { ("kubectl".to_owned(), raw) } else { split_prefix(raw) };
        let entry = if stderr { None } else { Entry::parse(&text).map(Box::new) };
        let level = match (stderr, &entry) {
            (true, _) => Level::Meta,
            (false, Some(e)) => match e.severity {
                Some(Severity::Error | Severity::Fatal) => Level::Error,
                Some(Severity::Warn) => Level::Warn,
                Some(Severity::Debug | Severity::Trace) => Level::Debug,
                Some(Severity::Info) | None => Level::Plain,
            },
            (false, None) => level_of(&text),
        };
        let haystack = format!("{source} {text}").to_lowercase();
        Line { id, source, slot: 0, text, level, entry, haystack }
    }
}

/// `kubectl logs --prefix` writes `[pod/NAME/CONTAINER] message`.
fn split_prefix(raw: String) -> (String, String) {
    if let Some(rest) = raw.strip_prefix("[pod/")
        && let Some(end) = rest.find("] ")
    {
        let source = rest[..end].to_owned();
        let text = rest[end + 2..].to_owned();
        return (source, text);
    }
    (String::new(), raw)
}

fn level_of(text: &str) -> Level {
    // Only look at the start of the line, where loggers put the level.
    let head: String = text.chars().take(64).collect::<String>().to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| head.contains(w));
    if has(&["error", "fatal", "panic", "\"level\":\"err", "level=err", "exception", " err "]) {
        Level::Error
    } else if has(&["warn", "\"level\":\"warn", "level=warn"]) {
        Level::Warn
    } else {
        Level::Plain
    }
}

#[derive(Default)]
pub struct LogBuffer {
    lines: VecDeque<Line>,
    next_id: u64,
    /// Ids of the lines matching `query`, oldest first.
    shown: VecDeque<u64>,
    query: String,
    /// Colour slots by pod name.
    slots: HashMap<String, usize>,
    /// Bumped when lines fall off the front, so virtual rows know keys moved.
    pub version: u64,
}

impl LogBuffer {
    pub fn clear(&mut self) {
        self.lines.clear();
        self.shown.clear();
        self.slots.clear();
        self.version += 1;
    }

    pub fn total(&self) -> usize {
        self.lines.len()
    }

    pub fn shown_len(&self) -> usize {
        self.shown.len()
    }

    pub fn push(&mut self, raw: String, stderr: bool) {
        let mut line = Line::new(self.next_id, raw, stderr);
        self.next_id += 1;
        let pod = line.source.split('/').next().unwrap_or_default();
        let next = self.slots.len();
        line.slot = *self.slots.entry(pod.to_owned()).or_insert(next);
        if self.matches(&line) {
            self.shown.push_back(line.id);
        }
        self.lines.push_back(line);
        if self.lines.len() > CAPACITY
            && let Some(old) = self.lines.pop_front()
        {
            while self.shown.front().is_some_and(|&id| id <= old.id) {
                self.shown.pop_front();
            }
            self.version += 1;
        }
    }

    fn matches(&self, line: &Line) -> bool {
        self.query.is_empty() || line.haystack.contains(&self.query)
    }

    /// Re-filters every line; returns whether the query changed.
    pub fn set_query(&mut self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query == self.query {
            return false;
        }
        self.query = query;
        self.shown = self.lines.iter().filter(|l| self.matches(l)).map(|l| l.id).collect();
        self.version += 1;
        true
    }

    /// The `i`th shown line.
    pub fn shown(&self, i: usize) -> Option<&Line> {
        let id = *self.shown.get(i)?;
        let first = self.lines.front()?.id;
        self.lines.get(usize::try_from(id.checked_sub(first)?).ok()?)
    }

    pub fn shown_id(&self, i: usize) -> u64 {
        self.shown.get(i).copied().unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_pod_prefix() {
        let l = Line::new(0, "[pod/api-7d9f/app] GET /health 200".into(), false);
        assert_eq!(l.source, "api-7d9f/app");
        assert_eq!(l.text, "GET /health 200");
    }

    #[test]
    fn json_levels_come_from_the_entry() {
        // "error" in a field doesn't make an info line an error.
        let l = Line::new(0, r#"{"level":"info","msg":"x","error":null}"#.into(), false);
        assert!(l.level == Level::Plain && l.entry.is_some());
        assert!(Line::new(0, r#"{"level":40,"msg":"x"}"#.into(), false).level == Level::Warn);
    }

    #[test]
    fn detects_levels() {
        assert!(Line::new(0, "2024 ERROR boom".into(), false).level == Level::Error);
        assert!(Line::new(0, r#"{"level":"warning","msg":"x"}"#.into(), false).level == Level::Warn);
        assert!(Line::new(0, "all good".into(), false).level == Level::Plain);
    }

    #[test]
    fn filters_and_drops_old_lines() {
        let mut b = LogBuffer::default();
        for i in 0..CAPACITY + 10 {
            b.push(format!("line {i} {}", if i % 2 == 0 { "even" } else { "odd" }), false);
        }
        assert_eq!(b.total(), CAPACITY);
        assert_eq!(b.shown(0).map(|l| l.id), Some(10));
        b.set_query("EVEN");
        assert_eq!(b.shown_len(), CAPACITY / 2);
        assert!(b.shown(0).is_some_and(|l| l.text.ends_with("even")));
        // The new line matches and the dropped oldest line ("line 10 even") did too.
        b.push("new even".into(), false);
        assert_eq!(b.shown_len(), CAPACITY / 2);
        assert_eq!(b.shown(b.shown_len() - 1).map(|l| l.text.as_str()), Some("new even"));
    }
}
