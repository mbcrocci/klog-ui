//! The app's state and everything that changes it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rune::{PointerPress, TextEdit, Tx, Waker};
use rune_kit::shell::PaneState;

use crate::kube::{self, Inbox, Kind, LogStream, Msg, Resource};
use crate::logs::LogBuffer;

/// Lines fetched from each pod when a stream starts.
pub const TAIL: u32 = 500;
/// How long a row's copy button shows a check after copying.
const COPIED: Duration = Duration::from_millis(1500);
/// How often the sidebar's list refreshes on its own.
const REFRESH: Duration = Duration::from_secs(15);

pub enum Resources {
    Loading,
    Loaded(Vec<Resource>),
    Failed(String),
}

/// How structured (JSON) lines draw. Plain-text lines draw as they are in every format.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One line: `message key=value …`.
    Pretty,
    /// The message, then every field as an indented tree.
    Tree,
    /// The text the pod wrote.
    Raw,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Pretty, Format::Tree, Format::Raw];

    pub fn label(self) -> &'static str {
        match self {
            Format::Pretty => "Pretty",
            Format::Tree => "Tree",
            Format::Raw => "Raw",
        }
    }
}

pub enum Stream {
    Idle,
    Connecting,
    Live,
    Ended(String),
}

pub struct App {
    pub sidebar: PaneState,
    pub inbox: Arc<Inbox>,
    pub context: String,
    /// What the sidebar lists.
    pub mode: Kind,
    /// Whether the mode picker is open.
    pub mode_open: bool,
    /// The last list of each kind, so switching back shows it while it refreshes.
    pub lists: HashMap<Kind, Resources>,
    /// Namespaces folded in the sidebar.
    pub folded: HashSet<String>,
    pub search: TextEdit,
    /// The resource whose logs show; it stays while the sidebar lists another kind.
    pub selected: Option<Resource>,
    pub stream: Stream,
    /// Each stream gets a generation so lines from a replaced one are dropped.
    generation: u64,
    handle: Option<LogStream>,
    pub logs: LogBuffer,
    pub filter: TextEdit,
    pub wrap: bool,
    pub format: Format,
    /// The log line whose context menu is open, and where it opened.
    pub line_menu: Option<(u64, PointerPress)>,
    /// The line last copied, and when; its copy button shows a check for a moment.
    pub copied: Option<(u64, Instant)>,
    /// When (on `poll`'s clock) the sidebar's list next refreshes.
    refresh_at: Option<f64>,
}

pub fn init() -> App {
    App {
        sidebar: PaneState::new(280.0).range(180.0, 520.0),
        inbox: Arc::new(Inbox::default()),
        context: String::new(),
        mode: Kind::Deployment,
        mode_open: false,
        lists: HashMap::new(),
        folded: HashSet::new(),
        search: TextEdit::new(""),
        selected: None,
        stream: Stream::Idle,
        generation: 0,
        handle: None,
        logs: LogBuffer::default(),
        filter: TextEdit::new(""),
        wrap: true,
        format: Format::Pretty,
        line_menu: None,
        copied: None,
        refresh_at: None,
    }
}

pub fn setup(s: &mut App, waker: Waker) {
    s.inbox.set_waker(waker);
    // Headless snapshots (`--snap`) render scenes from fixed data; don't touch the cluster.
    if rune::tooling::headless_run().is_none() {
        kube::refresh(s.mode, s.inbox.clone());
    }
}

pub fn poll(s: &mut App, clock: f64, tx: &mut Tx) -> bool {
    if s.refresh_at.is_some_and(|due| clock >= due) {
        s.refresh_at = None;
        kube::refresh(s.mode, s.inbox.clone());
    }
    let mut changed = false;
    if s.copied.is_some_and(|(_, at)| at.elapsed() >= COPIED) {
        s.copied = None;
        changed = true;
    }
    let msgs = s.inbox.drain();
    if msgs.is_empty() {
        return changed;
    }
    for msg in msgs {
        match msg {
            Msg::Context(c) => {
                changed |= s.context != c;
                s.context = c;
            }
            Msg::Resources { kind, list } => {
                // The selected resource's badge follows the list.
                if let (Ok(list), Some(sel)) = (&list, &mut s.selected)
                    && let Some(fresh) = list.iter().find(|r| r.id() == sel.id())
                {
                    *sel = fresh.clone();
                }
                let fresh = match list {
                    Ok(list) => Resources::Loaded(list),
                    // A failed refresh keeps the last list rather than blanking the sidebar.
                    Err(e) if !matches!(s.lists.get(&kind), Some(Resources::Loaded(_))) => Resources::Failed(e),
                    Err(_) => continue,
                };
                s.lists.insert(kind, fresh);
                changed = true;
                // Keep the list fresh; ready counts change as pods roll.
                if kind == s.mode {
                    s.refresh_at = Some(clock + REFRESH.as_secs_f64());
                    tx.wake_after(REFRESH);
                }
            }
            Msg::Line { generation, text, stderr } if generation == s.generation => {
                if matches!(s.stream, Stream::Connecting) && !stderr {
                    s.stream = Stream::Live;
                }
                s.logs.push(text, stderr);
                changed = true;
            }
            Msg::Ended { generation, status } if generation == s.generation => {
                s.stream = Stream::Ended(status);
                s.handle = None;
                changed = true;
            }
            Msg::Line { .. } | Msg::Ended { .. } => {}
        }
    }
    // Log lines land at once: a growth animation per line is noise at log rates.
    tx.instant();
    changed
}

impl App {
    /// The sidebar's list for the current mode.
    pub fn resources(&self) -> &Resources {
        self.lists.get(&self.mode).unwrap_or(&Resources::Loading)
    }

    /// Lists another kind in the sidebar; the stream shown carries on.
    pub fn set_mode(&mut self, kind: Kind) {
        self.mode_open = false;
        if self.mode == kind {
            return;
        }
        self.mode = kind;
        self.refresh();
    }

    pub fn select(&mut self, r: Resource) {
        if self.selected.as_ref().is_some_and(|s| s.id() == r.id()) {
            return;
        }
        self.selected = Some(r);
        self.connect();
    }

    /// (Re)starts the stream for the selected resource from a fresh buffer.
    pub fn connect(&mut self) {
        self.handle = None;
        self.generation += 1;
        self.logs.clear();
        let Some(r) = self.selected.clone() else {
            self.stream = Stream::Idle;
            return;
        };
        match kube::stream_logs(&r, TAIL, self.generation, self.inbox.clone()) {
            Ok(h) => {
                self.handle = Some(h);
                self.stream = Stream::Connecting;
            }
            Err(e) => self.stream = Stream::Ended(e),
        }
    }

    pub fn disconnect(&mut self) {
        self.handle = None;
        self.generation += 1;
        if !matches!(self.stream, Stream::Idle) {
            self.stream = Stream::Ended("stopped".into());
        }
    }

    pub fn is_streaming(&self) -> bool {
        self.handle.is_some()
    }

    /// Puts a log line on the clipboard exactly as the pod wrote it.
    pub fn copy_line(&mut self, id: u64, tx: &mut Tx) {
        self.line_menu = None;
        if let Some(l) = self.logs.get(id) {
            tx.copy(l.text.clone());
            self.copied = Some((id, Instant::now()));
            tx.wake_after(COPIED);
        }
    }

    /// Replaces the log filter's text and applies it.
    pub fn set_filter(&mut self, text: String) {
        self.filter.replace_text(text);
        let q = self.filter.text.clone();
        self.logs.set_query(&q);
    }

    pub fn refresh(&mut self) {
        // The timer restarts when this list lands.
        self.refresh_at = None;
        kube::refresh(self.mode, self.inbox.clone());
    }
}
