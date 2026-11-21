//! The window: workloads in a sidebar, the selected one's live logs in the main pane.

use std::borrow::Cow;
use std::collections::BTreeMap;

use rune::{Color, Cx, El, GrowthSpec, Icon, ScrollSpec, Span, Tx, col, el, icon, oklch, rich, row, text};
use rune_kit::{MenuItem, context_menu};
use rune_kit::shell::{PaneMotion, RowStatus, Sidebar, SidebarRow, SidebarSection, SidebarToggle, SplitView, TOOLBAR_HEIGHT, Toolbar};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl, Select, Spinner, TextInput,
    glyphs,
};

use crate::app::{App, Format, Resources, Stream};
use crate::kube::{Kind, Resource};
use crate::logs::{Level, Line};
use crate::query::{self, Part};
use crate::structured::{self, Entry, Severity, Tok};

macro_rules! icon {
    ($id:ident, $name:literal, $body:literal) => {
        static $id: Icon = Icon { name: $name, view: 16.0, body: $body };
    };
}
icon!(REFRESH, "logui-refresh", r#"<path d="M12.5 8A4.5 4.5 0 1 1 11.2 4.8M12.5 2.75V5.25H10" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(STOP, "logui-stop", r#"<rect x="4.25" y="4.25" width="7.5" height="7.5" rx="1.25" stroke="currentColor" stroke-width="1.2" fill="none"/>"#);
icon!(PLAY, "logui-play", r#"<path d="M5 3.75L12 8L5 12.25Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/>"#);
icon!(TRASH, "logui-trash", r#"<path d="M3.5 4.5H12.5M6.5 4.5V3H9.5V4.5M4.75 4.5L5.25 13H10.75L11.25 4.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(WRAP, "logui-wrap", r#"<path d="M3 4H13M3 8H11A2 2 0 0 1 11 12H8.5M10 10.5L8.5 12L10 13.5M3 12H6" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(BOTTOM, "logui-bottom", r#"<path d="M8 3V11M4.5 7.5L8 11L11.5 7.5M4 13H12" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(POD, "logui-pod", r#"<circle cx="8" cy="8" r="5" stroke="currentColor" stroke-width="1.1" fill="none"/><circle cx="8" cy="8" r="1.75" fill="currentColor"/>"#);
icon!(SERVICE, "logui-service", r#"<path d="M8 3V6.5M8 6.5L4 10M8 6.5L12 10" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" fill="none"/><circle cx="8" cy="3" r="1.25" stroke="currentColor" stroke-width="1.1" fill="none"/><circle cx="4" cy="11.5" r="1.5" stroke="currentColor" stroke-width="1.1" fill="none"/><circle cx="12" cy="11.5" r="1.5" stroke="currentColor" stroke-width="1.1" fill="none"/>"#);
icon!(STACK, "logui-stack", r#"<path d="M8 2.5L13.5 5.25L8 8L2.5 5.25Z M2.5 8L8 10.75L13.5 8M2.5 10.75L8 13.5L13.5 10.75" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/>"#);
icon!(NODES, "logui-nodes", r#"<rect x="2.5" y="2.5" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.1" fill="none"/><rect x="9" y="2.5" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.1" fill="none"/><rect x="2.5" y="9" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.1" fill="none"/><rect x="9" y="9" width="4.5" height="4.5" rx="1" stroke="currentColor" stroke-width="1.1" fill="none"/>"#);
icon!(JOB, "logui-job", r#"<circle cx="8" cy="8" r="5.5" stroke="currentColor" stroke-width="1.1" fill="none"/><path d="M5.75 8.25L7.25 9.75L10.25 6.5" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(LOGO, "logui-logo", r#"<path d="M8 1.75L13.5 4.9V11.1L8 14.25L2.5 11.1V4.9Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/><path d="M5.5 6.5H10.5M5.5 8.5H10.5M5.5 10.5H8.5" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" fill="none"/>"#);
icon!(BOX, "logui-box", r#"<path d="M8 2.5L13 5V11L8 13.5L3 11V5Z M3 5L8 7.5L13 5M8 7.5V13.5" stroke="currentColor" stroke-width="1.1" stroke-linejoin="round" fill="none"/>"#);

/// Estimated height of a log row, for rows not yet measured.
const ROW: f32 = 20.0;
const LOG_SCROLL: &str = "log-scroll";

pub fn view(s: &App, cx: &mut Cx<App>) -> El {
    let k = s.theme.kit(cx.system_dark());
    let mut overlays = Vec::new();
    let inset = cx.titlebar_inset();
    let shell = SplitView::new(main(s, &k, &mut overlays))
        .sidebar(&s.sidebar, |s: &mut App| &mut s.sidebar, sidebar(s, &k, &mut overlays))
        .toolbar(toolbar(s, &k))
        .titlebar_inset(inset)
        .sidebar_toggle(SidebarToggle::new().in_header())
        .pane_motion(PaneMotion::new())
        .reduced_motion(cx.is_reduced_motion())
        .build(&k);
    rune_kit::root(shell.child(brand(s, &k, inset)), overlays)
}

/// The app's name in the sidebar's title bar row, between the traffic lights and the sidebar
/// toggle. It fades with the sidebar, whose toolbar takes the row while it's closed.
fn brand(s: &App, k: &Kit, inset: f32) -> El {
    // Where `SidebarToggle` starts: 8 past the traffic lights, or 12 from the edge without them.
    let x = if inset > 0.0 { inset + 8.0 } else { 16.0 };
    let side = 24.0;
    let open = s.sidebar.is_open();
    row()
        .key("brand")
        .gap(8.0)
        .center_y()
        .h(side)
        .w((s.sidebar.width() - x - side - 16.0).max(0.0))
        .absolute(x, (TOOLBAR_HEIGHT - side) / 2.0)
        .opacity(if open { 1.0 } else { 0.0 })
        .window_drag()
        .child(icon(&LOGO, 16.0).color(k.c.text_base))
        .child(text("Kube Logs").font(k.ty.ui_medium).color(k.c.text_base).truncate().grow(1.0).shrink().min_w(0.0))
}

// ---- Sidebar ---------------------------------------------------------------------------------

fn sidebar(s: &App, k: &Kit, overlays: &mut Vec<El>) -> El {
    let plurals = Kind::ALL.map(Kind::plural);
    let mode = Select::new("mode", &plurals)
        .label("Show")
        .current(Some(s.mode.plural()))
        .open(s.mode_open)
        .on_toggle(|s: &mut App| s.mode_open = !s.mode_open)
        .on_close(|s: &mut App| s.mode_open = false)
        .on_choose(|s: &mut App, v: &str, _: &mut Tx| {
            if let Some(kind) = Kind::from_plural(v) {
                s.set_mode(kind);
            }
        })
        .build(k, overlays);
    let search = TextInput::new(&s.search)
        .placeholder(format!("Filter {}", s.mode.plural().to_lowercase()))
        .leading_icon(&glyphs::SEARCH)
        .clear_button()
        .width(None)
        .on_edit(|s: &mut App| &mut s.search)
        .on_action(|s: &mut App, _| s.search.clear())
        .build(k)
        .key("resource-search");
    let footer = row()
        .w_full()
        .px(8.0)
        .py(6.0)
        .gap(6.0)
        .center_y()
        .child(
            text(if s.context.is_empty() { "no context".to_owned() } else { s.context.clone() })
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate()
                .shrink()
                .grow(1.0),
        )
        .child(
            IconButton::new(&REFRESH)
                .variant(IconButtonVariant::GhostMuted)
                .help(format!("Refresh {}", s.mode.plural().to_lowercase()))
                .on_press(|s: &mut App| s.refresh())
                .build(k)
                .key("refresh"),
        );
    let mut sb = Sidebar::new()
        .label(s.mode.plural())
        .header(row().key("mode-row").ml(-8.0).child(mode))
        .header(search)
        .footer(footer.key("context"));

    match s.resources() {
        Resources::Loading => {
            sb = sb.child(row().w_full().py(16.0).center().child(Spinner::new().build(k)).key("loading"));
        }
        Resources::Failed(err) => {
            sb = sb.child(
                col()
                    .w_full()
                    .px(8.0)
                    .py(12.0)
                    .gap(8.0)
                    .child(text(format!("Couldn't list {}", s.mode.plural().to_lowercase())).font(k.ty.ui_medium).color(k.c.danger.fg))
                    .child(text(err.clone()).font(k.ty.small).color(k.c.text_muted).wrap_anywhere())
                    .child(Button::new("Retry").variant(ButtonVariant::Outline).on_press(|s: &mut App| s.refresh()).build(k))
                    .key("error"),
            );
        }
        Resources::Loaded(list) => {
            let query = s.search.text.trim().to_lowercase();
            let mut by_ns: BTreeMap<&str, Vec<&Resource>> = BTreeMap::new();
            for r in list.iter().filter(|r| query.is_empty() || format!("{}/{}", r.namespace, r.name).to_lowercase().contains(&query)) {
                by_ns.entry(&r.namespace).or_default().push(r);
            }
            if by_ns.is_empty() {
                let msg = if list.is_empty() { format!("No {}", s.mode.plural().to_lowercase()) } else { "No matches".to_owned() };
                sb = sb.child(text(msg).font(k.ty.small).color(k.c.text_faint).key("empty"));
            }
            for (ns, items) in by_ns {
                // While searching every match shows, folded or not.
                let folded = query.is_empty() && s.folded.contains(ns);
                let ns_owned = ns.to_owned();
                let section = SidebarSection::new(ns)
                    .key(format!("ns:{ns}"))
                    .count(items.len())
                    .collapsed(folded)
                    .on_toggle(move |s: &mut App, _| {
                        if !s.folded.remove(&ns_owned) {
                            s.folded.insert(ns_owned.clone());
                        }
                    })
                    .rows(items.into_iter().map(|r| resource_row(s, r, k)));
                sb = sb.section(section);
            }
        }
    }
    sb.build(k)
}

fn kind_icon(kind: Kind) -> &'static Icon {
    match kind {
        Kind::Deployment => &BOX,
        Kind::Pod => &POD,
        Kind::Service => &SERVICE,
        Kind::StatefulSet => &STACK,
        Kind::DaemonSet => &NODES,
        Kind::Job => &JOB,
    }
}

fn resource_row(s: &App, r: &Resource, k: &Kit) -> El {
    let id = r.id();
    let mut row = SidebarRow::new(r.name.clone())
        .icon(kind_icon(r.kind))
        .selected(s.selected.as_ref().is_some_and(|sel| sel.id() == id))
        .on_select({
            let r = r.clone();
            move |s: &mut App, _| s.select(r.clone())
        });
    if !r.badge.is_empty() {
        row = row.badge(r.badge.clone());
    }
    if !r.healthy {
        row = row.status(RowStatus::Attention);
    }
    row.build(k).key(id)
}

// ---- Toolbar ---------------------------------------------------------------------------------

fn toolbar(s: &App, k: &Kit) -> Toolbar {
    let Some(d) = &s.selected else {
        return Toolbar::new().title("Logs");
    };
    let status = match &s.stream {
        Stream::Idle => el(),
        Stream::Connecting => row().gap(6.0).center_y().child(Spinner::new().size(12.0).build(k)).child(
            text("Connecting").font(k.ty.small).color(k.c.text_muted),
        ),
        Stream::Live => Badge::new("● Live").variant(BadgeVariant::Accent).build(k),
        Stream::Ended(why) => Badge::new(why.clone()).variant(BadgeVariant::Subtle).build(k),
    };
    // Gives way to the trailing controls in a narrow window: the name and namespace truncate.
    let leading = row()
        .gap(10.0)
        .center_y()
        .shrink()
        .min_w(0.0)
        .clip_rounded(0.0)
        .help(format!("{}/{}", d.namespace, d.name))
        .child(icon(kind_icon(d.kind), 16.0).color(k.c.icon_muted))
        .child(text(d.name.clone()).font(k.ty.ui_medium).color(k.c.text_base).truncate().max_w(320.0))
        .child(text(d.namespace.clone()).font(k.ty.small).color(k.c.text_faint).truncate().shrink().min_w(0.0))
        .child(status.key("stream-status"));

    let count = if s.filter.text.trim().is_empty() {
        format!("{} lines", s.logs.total())
    } else {
        format!("{} of {}", s.logs.shown_len(), s.logs.total())
    };
    let spans: Vec<Span> = query::highlight(&s.filter.text)
        .into_iter()
        .map(|(r, part)| {
            let c = match part {
                Part::Key => tok_color(Tok::Logger, k.dark, k, &s.theme),
                Part::Op | Part::Keyword => k.c.text_faint,
                Part::Value => tok_color(Tok::Str, k.dark, k, &s.theme),
            };
            Span::new(r.start, r.end).color(c)
        })
        .collect();
    let filter = TextInput::new(&s.filter)
        .placeholder("Filter logs")
        .label("Filter logs")
        .leading_icon(&glyphs::SEARCH)
        .clear_button()
        .width(Some(300.0))
        .spans(spans)
        .invalid(s.logs.query_error.is_some())
        .on_edit(|s: &mut App| &mut s.filter)
        .on_change(|s: &mut App, _| {
            let q = s.filter.text.clone();
            s.logs.set_query(&q);
        })
        .on_action(|s: &mut App, _| {
            s.filter.clear();
            s.logs.set_query("");
        })
        .build(k)
        .help(match &s.logs.query_error {
            Some(e) => format!("Incomplete query: {e}. The last complete one still applies."),
            None => FILTER_HELP.to_owned(),
        })
        .key("log-filter");
    let btn = |icon: &'static Icon, help: &str| {
        IconButton::new(icon).variant(IconButtonVariant::GhostMuted).control_size(ControlSize::Regular).help(help.to_owned()).label(help.to_owned())
    };
    let stream_btn = if s.is_streaming() {
        btn(&STOP, "Stop streaming").on_press(|s: &mut App| s.disconnect())
    } else {
        btn(&PLAY, "Reconnect").on_press(|s: &mut App| s.connect())
    };
    let labels = Format::ALL.map(Format::label);
    let format = SegmentedControl::new(&labels, Format::ALL.iter().position(|f| *f == s.format))
        .label("Log format")
        .fit()
        .on_pick(|s: &mut App, i, _| s.format = Format::ALL[i])
        .build(k)
        .key("format");
    let trailing = row()
        .gap(4.0)
        .center_y()
        .child(text(count).font(k.ty.small).color(k.c.text_faint).key("line-count"))
        .child(format)
        .child(filter)
        .child(
            btn(&WRAP, if s.wrap { "Don't wrap lines" } else { "Wrap lines" })
                .expanded(s.wrap)
                .on_press(|s: &mut App| s.wrap = !s.wrap)
                .build(k)
                .key("wrap"),
        )
        .child(
            btn(&BOTTOM, "Jump to latest")
                .on_press_tx(|_: &mut App, tx| tx.scroll_to_bottom(LOG_SCROLL))
                .build(k)
                .key("bottom"),
        )
        .child(btn(&TRASH, "Clear").on_press(|s: &mut App| s.logs.clear()).build(k).key("clear"))
        .child(stream_btn.build(k).key("stream-toggle"));
    Toolbar::new().leading(leading).trailing(trailing)
}

// ---- Logs ------------------------------------------------------------------------------------

/// The menu a right-click on a log line opens: copy it, or narrow the filter to its pod.
fn line_menu(s: &App, k: &Kit, overlays: &mut Vec<El>) -> El {
    let line = s.line_menu.and_then(|(id, _)| s.logs.get(id));
    let mut menu = context_menu("line-menu", el(), line.and(s.line_menu.map(|(_, p)| p))).on_close(|s: &mut App| s.line_menu = None);
    if let Some(l) = line {
        let id = l.id;
        menu = menu.item(MenuItem::new("Copy line").on_select_tx(move |s: &mut App, tx| s.copy_line(id, tx)));
        if let Some(e) = &l.entry {
            let msg = e.message.clone();
            menu = menu.item(MenuItem::new("Copy message").on_select_tx(move |s: &mut App, tx| {
                s.line_menu = None;
                tx.copy(msg.clone());
            }));
        }
        let pod = l.source.split('/').next().unwrap_or_default().to_owned();
        if !pod.is_empty() {
            menu = menu.separator().item(MenuItem::new("Show only this pod").on_select(move |s: &mut App| {
                s.line_menu = None;
                s.set_filter(format!("pod ~ '{pod}'"));
            }));
        }
    }
    menu.build(k, overlays).key("line-menu-anchor")
}

const FILTER_HELP: &str = "Words match anywhere; quote a phrase.\n\
key = value   equals (any case, * wildcard)\n\
key ~ value   contains (or key:value)\n\
!= and !~   negate;  > >= < <=   compare numbers\n\
Keys: any field (err.message for nested), msg, level, logger, time, pod, raw\n\
Combine with and (or a space), or, not / -, and ( )";

fn main(s: &App, k: &Kit, overlays: &mut Vec<El>) -> El {
    let body = col().key("logs").w_full().h_full().min_h(0.0);
    if s.selected.is_none() {
        return body.center().gap(6.0).child(text("Select a workload").font(k.ty.ui_medium).color(k.c.text_muted)).child(
            text("Its pods' logs stream here live.").font(k.ty.small).color(k.c.text_faint),
        );
    }
    if s.logs.shown_len() == 0 {
        let msg = match (&s.stream, s.logs.total()) {
            (_, n) if n > 0 => "No lines match the filter",
            (Stream::Connecting, _) => "Waiting for logs…",
            _ => "No logs yet",
        };
        return body.center().child(text(msg).font(k.ty.small).color(k.c.text_faint));
    }
    let kit = k.clone();
    let dark = k.dark;
    let rows = col()
        .key("log-rows")
        .w_full()
        .py(6.0)
        .growth(GrowthSpec::default())
        .virtual_rows(s.logs.shown_len(), ROW, |s: &App, i| Cow::Owned(s.logs.shown_id(i).to_string()), move |s: &App, i| {
            s.logs.shown(i).map_or_else(el, |l| log_row(l, s, &kit, dark))
        })
        .virtual_rows_version((s.logs.version, s.theme.revision));
    let viewport = col().key(LOG_SCROLL).w_full().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: true });
    let viewport = if s.wrap { viewport.child(rows) } else { viewport.scroll_xy().child(rows) };
    body.fill(k.c.bg_base).child(viewport).child(line_menu(s, k, overlays))
}

fn log_row(l: &Line, s: &App, k: &Kit, dark: bool) -> El {
    let mono = k.ty.mono;
    let ink = match l.level {
        Level::Plain => k.c.text_base,
        Level::Warn => k.c.warning.fg,
        Level::Error => k.c.danger.fg,
        Level::Debug | Level::Meta => k.c.text_faint,
    };
    // The pod's unique suffix only, small; the whole pod/container name is in its tooltip.
    let mut small = mono;
    small.size = 10.5; // keeps the mono line height, so it sits on the line's baseline
    let source = text(short_source(&l.source)).font(small).color(source_color(l.slot, &s.theme)).truncate().w(SOURCE_W).help(l.source.clone());
    let line = row().w_full().px(12.0).py(1.0).gap(10.0).child(source);
    let line = match (&l.entry, s.format) {
        (Some(e), Format::Pretty | Format::Tree) => structured_row(line, e, s, k, dark, ink),
        _ => {
            let msg = text(if l.text.is_empty() { " ".to_owned() } else { l.text.clone() }).font(mono).color(ink).selectable(true);
            line.child(if s.wrap { msg.wrap_anywhere().grow(1.0).shrink() } else { msg })
        }
    };
    let id = l.id;
    let copied = s.copied.is_some_and(|(c, _)| c == id);
    let copy = IconButton::new(if copied { k.glyph.check_small } else { k.glyph.copy })
        .variant(IconButtonVariant::GhostMuted)
        .control_size(ControlSize::Small)
        .help("Copy line")
        .on_press_tx(move |s: &mut App, tx| s.copy_line(id, tx))
        .build(k);
    // Shown while the row is hovered (or just copied), over the line's end.
    let copy = el()
        .key("copy")
        .pin(None, Some(0.0), Some(6.0), None)
        .radius(k.radius.sm)
        .fill(k.c.bg_base)
        .group_opacity(if copied { 1.0 } else { 0.0 }, 1.0)
        .child(copy);
    let line = line.hover_group().child(copy).on_secondary_press(move |s: &mut App, p, _| s.line_menu = Some((id, p)));
    match l.level {
        Level::Error => line.fill(k.c.danger.bg),
        _ => line.hover(k.c.hover),
    }
}

const SOURCE_W: f32 = 44.0;
const TIME_W: f32 = 92.0;
const LEVEL_W: f32 = 40.0;

/// `time  LEVEL  logger  message`, then the fields: inline (`key=value …`) in Pretty, as an
/// indented tree under the message in Tree.
fn structured_row(line: El, e: &Entry, s: &App, k: &Kit, dark: bool, ink: Color) -> El {
    let mono = k.ty.mono;
    let tree = s.format == Format::Tree;
    let time = text(e.time.clone().unwrap_or_default()).font(mono).color(k.c.text_faint).w(TIME_W);
    let (label, level_ink) = match e.severity {
        Some(sev @ (Severity::Error | Severity::Fatal)) => (sev.label(), k.c.danger.fg),
        Some(sev @ Severity::Warn) => (sev.label(), k.c.warning.fg),
        Some(sev @ Severity::Info) => (sev.label(), k.c.info.fg),
        Some(sev) => (sev.label(), k.c.text_faint),
        None => ("", k.c.text_faint),
    };
    let level = text(label).font(mono).color(level_ink).w(LEVEL_W);

    // The head line, led by the logger's name; in Tree it's the message alone.
    let body = if tree { structured::headline(e) } else { structured::inline(e) };
    let mut content = String::new();
    let mut spans = Vec::new();
    if let Some(logger) = &e.logger {
        content.push_str(logger);
        spans.push(Span::new(0, logger.len()).color(tok_color(Tok::Logger, dark, k, &s.theme)));
        content.push_str("  ");
    }
    let shift = content.len();
    content.push_str(&body.text);
    spans.extend(body.runs.iter().map(|&(a, b, t)| Span::new(a + shift, b + shift).color(tok_color(t, dark, k, &s.theme))));
    let head = rich(content, spans).font(mono).color(ink).selectable(true);
    let head = if s.wrap { head.wrap_anywhere() } else { head };

    let mut message = col().grow(1.0).shrink().min_w(0.0).child(head);
    if tree {
        let fields = structured::expanded(e);
        if !fields.text.is_empty() {
            let spans = fields.runs.iter().map(|&(a, b, t)| Span::new(a, b).color(tok_color(t, dark, k, &s.theme))).collect();
            message = message.child(
                el().key("fields")
                    .mt(2.0)
                    .mb(6.0)
                    .pl(12.0)
                    .border_sides([0.0, 0.0, 0.0, 2.0], k.c.border_weak)
                    .child(rich(fields.text, spans).font(mono).color(k.c.text_base).selectable(true)),
            );
        }
    }
    line.child(time).child(level).child(message)
}

/// Syntax colours for structured text, tuned for each appearance.
fn tok_color(t: Tok, dark: bool, k: &Kit, theme: &crate::theme::Theme) -> Color {
    let name = match t {
        Tok::Logger => "magenta",
        Tok::Str => "green",
        Tok::Num => "blue",
        Tok::Bool | Tok::Null => "cyan",
        _ => "",
    };
    if let Some(color) = theme.color(name) {
        return color;
    }
    let l = if dark { 0.78 } else { 0.50 };
    match t {
        Tok::Key => k.c.text_muted,
        Tok::Logger => oklch(l, 0.07, 285.0),
        Tok::Str => oklch(l, 0.11, 150.0),
        Tok::Num => oklch(l, 0.11, 235.0),
        Tok::Bool | Tok::Null => oklch(l, 0.12, 305.0),
        Tok::Punct => k.c.text_faint,
    }
}

/// `pod-name-7d9f8b-xk2lp/container` → `xk2lp`: the pod's unique tail is what tells replicas
/// apart.
fn short_source(source: &str) -> String {
    let pod = source.split('/').next().unwrap_or(source);
    pod.rsplit('-').next().unwrap_or(pod).to_owned()
}

/// A distinct colour per pod, so lines from one replica read as a group. Golden-angle steps
/// keep any few pods far apart on the hue circle.
fn source_color(slot: usize, theme: &crate::theme::Theme) -> Color {
    const COLORS: [&str; 6] = ["yellow", "cyan", "magenta", "green", "blue", "red"];
    if let Some(color) = theme.color(COLORS[slot % COLORS.len()]) {
        return color;
    }
    oklch(0.74, 0.11, (40.0 + slot as f32 * 137.5) % 360.0)
}
