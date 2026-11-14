//! The app's state and everything that changes it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rune::{TextEdit, Tx, Waker};
use rune_kit::shell::PaneState;

use crate::kube::{self, Deployment, Inbox, LogStream, Msg};
use crate::logs::LogBuffer;

/// Lines fetched from each pod when a stream starts.
pub const TAIL: u32 = 500;
/// How often the deployment list refreshes on its own.
const REFRESH: Duration = Duration::from_secs(15);

pub enum Deployments {
    Loading,
    Loaded(Vec<Deployment>),
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
    pub deployments: Deployments,
    /// Namespaces folded in the sidebar.
    pub folded: HashSet<String>,
    pub search: TextEdit,
    /// `namespace/name` of the deployment shown.
    pub selected: Option<String>,
    pub stream: Stream,
    /// Each stream gets a generation so lines from a replaced one are dropped.
    generation: u64,
    handle: Option<LogStream>,
    pub logs: LogBuffer,
    pub filter: TextEdit,
    pub wrap: bool,
    pub format: Format,
    /// When (on `poll`'s clock) the deployment list next refreshes.
    refresh_at: Option<f64>,
}

pub fn init() -> App {
    App {
        sidebar: PaneState::new(280.0).range(180.0, 520.0),
        inbox: Arc::new(Inbox::default()),
        context: String::new(),
        deployments: Deployments::Loading,
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
        refresh_at: None,
    }
}

pub fn setup(s: &mut App, waker: Waker) {
    s.inbox.set_waker(waker);
    // Headless snapshots (`--snap`) render scenes from fixed data; don't touch the cluster.
    if rune::tooling::headless_run().is_none() {
        kube::refresh(s.inbox.clone());
    }
}

pub fn poll(s: &mut App, clock: f64, tx: &mut Tx) -> bool {
    if s.refresh_at.is_some_and(|due| clock >= due) {
        s.refresh_at = None;
        kube::refresh(s.inbox.clone());
    }
    let msgs = s.inbox.drain();
    if msgs.is_empty() {
        return false;
    }
    let mut changed = false;
    for msg in msgs {
        match msg {
            Msg::Context(c) => {
                changed |= s.context != c;
                s.context = c;
            }
            Msg::Deployments(r) => {
                s.deployments = match r {
                    Ok(list) => Deployments::Loaded(list),
                    Err(e) => Deployments::Failed(e),
                };
                changed = true;
                // Keep the list fresh; ready counts change as pods roll.
                s.refresh_at = Some(clock + REFRESH.as_secs_f64());
                tx.wake_after(REFRESH);
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
    pub fn deployment(&self, id: &str) -> Option<&Deployment> {
        match &self.deployments {
            Deployments::Loaded(list) => list.iter().find(|d| d.id() == id),
            _ => None,
        }
    }

    pub fn select(&mut self, id: String) {
        if self.selected.as_deref() == Some(id.as_str()) {
            return;
        }
        self.selected = Some(id);
        self.connect();
    }

    /// (Re)starts the stream for the selected deployment from a fresh buffer.
    pub fn connect(&mut self) {
        self.handle = None;
        self.generation += 1;
        self.logs.clear();
        let Some(d) = self.selected.as_deref().and_then(|id| self.deployment(id)).cloned() else {
            self.stream = Stream::Idle;
            return;
        };
        match kube::stream_logs(&d, TAIL, self.generation, self.inbox.clone()) {
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

    pub fn refresh(&mut self) {
        kube::refresh(self.inbox.clone());
    }
}
