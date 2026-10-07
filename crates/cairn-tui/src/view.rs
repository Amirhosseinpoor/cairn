//! Drawing the screen (SPEC §10.1): header, transcript, status, prompt.
//!
//! Everything draws into a ratatui [`Buffer`], so a test renders at any size
//! and reads the cells back. [`draw`] returns where the cursor belongs.

use std::fmt::Write as _;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{group, short, window, App, ErrorCard, Item, NoticeKind, ToolCard, ToolState};
use crate::markdown::{self, Style3};
use crate::theme::Role;

/// §10.1: below this the interface says so and nothing else.
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 12;
/// The prompt grows to this many rows.
pub const MAX_INPUT_ROWS: usize = 8;

/// Where the pieces of the screen are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regions {
    pub header: Rect,
    pub transcript: Rect,
    pub status: Rect,
    pub input: Rect,
}

/// Box-drawing, or plain ASCII when asked.
struct Frame {
    h: &'static str,
    v: &'static str,
    tl: &'static str,
    tr: &'static str,
    bl: &'static str,
    br: &'static str,
}

impl Frame {
    fn new(ascii: bool) -> Self {
        if ascii {
            Self {
                h: "-",
                v: "|",
                tl: "+",
                tr: "+",
                bl: "+",
                br: "+",
            }
        } else {
            Self {
                h: "─",
                v: "│",
                tl: "╭",
                tr: "╮",
                bl: "╰",
                br: "╯",
            }
        }
    }
}

pub(crate) fn is_ascii_look(app: &App) -> bool {
    app.look.screen_reader || app.look.glyphs.tool == "*"
}

pub(crate) fn style(app: &App, role: Role) -> Style {
    app.look.theme.fg(role, app.look.support)
}

pub(crate) fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Cut `spans` to `width` columns, with `…` when something was dropped.
fn truncate(spans: Vec<Span<'static>>, width: usize, ellipsis: &str) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    if total <= width {
        return spans;
    }
    let keep = width.saturating_sub(ellipsis.width());
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > keep {
                break;
            }
            text.push(ch);
            used += w;
        }
        let done = text.width() < span.content.width();
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        if done {
            break;
        }
    }
    out.push(Span::raw(ellipsis.to_string()));
    out
}

/// `left` at the start and `right` at the end of a `width`-wide line.
fn spread(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
    ellipsis: &str,
) -> Line<'static> {
    let rw: usize = right.iter().map(|s| s.content.width()).sum();
    let lw: usize = left.iter().map(|s| s.content.width()).sum();
    if lw + rw + 1 > width {
        let mut spans = truncate(left, width.saturating_sub(rw + 1), ellipsis);
        spans.push(Span::raw(" "));
        spans.extend(right);
        return Line::from(spans);
    }
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(width - lw - rw)));
    spans.extend(right);
    Line::from(spans)
}

/// `1.9 KiB`, `14 B`.
#[must_use]
pub fn size(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", f64::from(bytes) / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", f64::from(bytes) / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// `14 ms`, `4.1 s`, `2m 05s`.
#[must_use]
pub fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        #[allow(clippy::cast_precision_loss)]
        let s = ms as f64 / 1000.0;
        format!("{s:.1} s")
    } else {
        format!("{}m {:02}s", ms / 60_000, (ms / 1000) % 60)
    }
}

// ----------------------------------------------------------------- layout

/// Where everything goes in `area`, or `None` if it is too small.
///
/// The transcript fills the screen from the top; the prompt sits in a box
/// at the bottom with one status line under it.
#[must_use]
pub fn layout(app: &App, area: Rect) -> Option<Regions> {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let (rows, _) = input_layout(app, usize::from(area.width).saturating_sub(8));
    let input_rows = u16::try_from(rows.len().clamp(1, MAX_INPUT_ROWS)).unwrap_or(1);
    let transcript_h = area.height.saturating_sub(input_rows + 3);
    Some(Regions {
        header: Rect::new(area.x + 1, area.y, area.width - 2, 0),
        transcript: Rect::new(area.x + 1, area.y, area.width - 2, transcript_h),
        input: Rect::new(
            area.x + 3,
            area.y + transcript_h + 1,
            area.width - 6,
            input_rows,
        ),
        status: Rect::new(area.x + 3, area.y + area.height - 1, area.width - 6, 1),
    })
}

/// The prompt as visual rows, and where the cursor is in them.
///
/// Each editor line is wrapped by display width after the two-column prompt.
fn input_layout(app: &App, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(4);
    let mut rows = Vec::new();
    let mut cursor = (0, 0);
    let (crow, ccol) = app.editor.cursor();
    for (r, line) in app.editor.lines().iter().enumerate() {
        let mut current = String::new();
        let mut used = 0;
        let mut graphemes = 0;
        let mut placed = r != crow;
        if !placed && ccol == 0 {
            cursor = (rows.len(), 0);
            placed = true;
        }
        for g in unicode_segmentation::UnicodeSegmentation::graphemes(line.as_str(), true) {
            let w = g.width();
            if used + w > width && used > 0 {
                rows.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push_str(g);
            used += w;
            graphemes += 1;
            if !placed && graphemes == ccol {
                cursor = (rows.len(), used);
                placed = true;
            }
        }
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    (rows, cursor)
}

// ---------------------------------------------------------------- drawing

pub(crate) fn put_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &Line<'_>) {
    let mut col = x;
    let end = x + width;
    for span in &line.spans {
        if col >= end {
            break;
        }
        let (c, _) = buf.set_stringn(
            col,
            y,
            span.content.as_ref(),
            usize::from(end - col),
            span.style,
        );
        col = c;
    }
}

/// The rounded box around the prompt.
fn draw_input_box(app: &App, buf: &mut Buffer, regions: &Regions) {
    let f = Frame::new(is_ascii_look(app));
    let edge = if app.running.is_some() {
        style(app, Role::Dim)
    } else {
        style(app, Role::Accent)
    };
    let left = regions.transcript.x;
    let right = left + regions.transcript.width - 1;
    let top = regions.input.y - 1;
    let bottom = regions.input.y + regions.input.height;
    buf.set_string(left, top, f.tl, edge);
    buf.set_string(right, top, f.tr, edge);
    buf.set_string(left, bottom, f.bl, edge);
    buf.set_string(right, bottom, f.br, edge);
    for x in left + 1..right {
        buf.set_string(x, top, f.h, edge);
        buf.set_string(x, bottom, f.h, edge);
    }
    for y in top + 1..bottom {
        buf.set_string(left, y, f.v, edge);
        buf.set_string(right, y, f.v, edge);
    }
}

/// `⏵⏵ build mode (shift+tab to cycle)`, in the mode's colour.
fn mode_hint(app: &App) -> Vec<Span<'static>> {
    let ascii = is_ascii_look(app);
    let (mark, text) = match app.mode {
        cairn_core::Mode::Plan => (if ascii { "" } else { "⏸ " }, "plan mode"),
        cairn_core::Mode::Build => (if ascii { "" } else { "⏵ " }, "build mode"),
        cairn_core::Mode::Auto => (if ascii { "" } else { "⏵⏵ " }, "auto mode"),
        cairn_core::Mode::AutoUnsafe => (if ascii { "" } else { "⏵⏵ " }, "UNSAFE: nothing asks"),
    };
    let role = app
        .look
        .theme
        .mode_role(&app.mode.as_str().replace('-', "_"));
    vec![
        Span::styled(format!("{mark}{text}"), style(app, role).patch(bold())),
        Span::styled(" (shift+tab to cycle)".to_string(), style(app, Role::Dim)),
    ]
}

/// The line under the prompt: the mode on the left, the facts on the right.
fn status_line(app: &App, width: usize) -> Line<'static> {
    // (drop order, text): lower numbers go first when the line is too long;
    // 0 is never dropped.
    let mut parts: Vec<(u8, String)> = Vec::new();
    if app.todos.is_some() {
        if let Some((done, total)) = app.todos {
            parts.push((3, format!("todos {done}/{total}")));
        }
    }
    if let Some((used, win)) = app.context {
        let pct = if win == 0 {
            0
        } else {
            (u64::from(used) * 100 + u64::from(win) / 2) / u64::from(win)
        };
        parts.push((
            0,
            format!("ctx {}/{} ({pct}%)", group(u64::from(used)), window(win)),
        ));
    }
    if app.jobs > 0 {
        parts.push((5, format!("⚙{}", app.jobs)));
    }
    if let Some(s) = &app.sandbox {
        parts.push((6, format!("⛨ {s}")));
    }
    if let Some(b) = &app.branch {
        let mut text = format!("git:{}", b.name);
        if b.added > 0 {
            let _ = write!(text, " +{}", b.added);
        }
        if b.removed > 0 {
            let _ = write!(text, " −{}", b.removed);
        }
        parts.push((4, text));
    }
    let tokens = app.tokens_in + app.tokens_out;
    if tokens > 0 {
        parts.push((2, format!("{} tok", short(tokens))));
    }
    let cost = match (app.cost_usd, tokens) {
        (Some(c), _) => Some(format!("${c:.3}")),
        (None, 0) => None,
        (None, _) => Some("$—".to_string()),
    };
    if let Some(cost) = cost {
        parts.push((7, cost));
    }
    parts.push((1, app.model.clone()));
    let label = app.editor.mode_label();
    if label != "[E]" {
        parts.push((0, label.to_string()));
    }
    let link = if app.online {
        app.look.glyphs.online
    } else {
        app.look.glyphs.offline
    };
    // The context figure warns as the window fills.
    let ctx_role = app.context.and_then(|(used, win)| {
        let pct = u64::from(used) * 100 / u64::from(win.max(1));
        match pct {
            90.. => Some(Role::Red),
            75..=89 => Some(Role::Amber),
            _ => None,
        }
    });
    let dot = format!(" {} ", app.look.glyphs.bullet);
    let dim = style(app, Role::Dim);
    let right = |parts: &[(u8, String)]| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, (_, text)) in parts.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(dot.clone(), dim));
            }
            match ctx_role.filter(|_| text.starts_with("ctx ")) {
                Some(role) => {
                    spans.push(Span::styled(text.clone(), style(app, role).patch(bold())));
                }
                None => spans.push(Span::styled(text.clone(), dim)),
            }
        }
        spans.push(Span::styled(format!(" {link}"), dim));
        spans
    };
    let left = mode_hint(app);
    let measure = |spans: &[Span<'static>]| spans.iter().map(|s| s.content.width()).sum::<usize>();
    let mut kept = parts;
    while measure(&left) + measure(&right(&kept)) + 2 > width {
        let Some(victim) = kept
            .iter()
            .filter(|(p, _)| *p > 0)
            .min_by_key(|(p, _)| *p)
            .map(|(p, _)| *p)
        else {
            break;
        };
        kept.retain(|(p, _)| *p != victim);
    }
    spread(left, right(&kept), width, app.look.glyphs.ellipsis)
}

// -------------------------------------------------------------- transcript

/// `  ⎿  ` leads the first line of a tool's result, blanks the rest.
fn result_lead(app: &App, first: bool) -> Span<'static> {
    let dim = style(app, Role::Dim);
    if first {
        Span::styled(format!("  {}  ", app.look.glyphs.result), dim)
    } else {
        Span::styled("     ".to_string(), dim)
    }
}

fn tool_lines(app: &App, card: &ToolCard, width: usize) -> Vec<Line<'static>> {
    let g = &app.look.glyphs;
    let dim = style(app, Role::Dim);
    let mut out = Vec::new();
    let marker = if card.expanded { g.collapse } else { g.expand };
    let toggle = vec![Span::styled(format!("[{marker}]"), dim)];
    let call = if card.summary.is_empty() {
        card.name.clone()
    } else {
        format!("{}({})", card.name, card.summary)
    };
    let name = Span::styled(call, bold());
    let spinner = if app.look.animation {
        g.spinner[usize::try_from(app.tick).unwrap_or(0) % g.spinner.len()]
    } else {
        g.tool
    };
    match &card.state {
        ToolState::Running => {
            out.push(Line::from(truncate(
                vec![
                    Span::styled(format!("{spinner} "), style(app, Role::Accent)),
                    name,
                ],
                width,
                g.ellipsis,
            )));
            let seconds = app.tick.saturating_sub(card.started_tick) * 80 / 1000;
            for (i, l) in card.tail.iter().enumerate() {
                out.push(Line::from(truncate(
                    vec![result_lead(app, i == 0), Span::styled(l.clone(), dim)],
                    width,
                    g.ellipsis,
                )));
            }
            out.push(Line::from(vec![
                result_lead(app, card.tail.is_empty()),
                Span::styled(
                    format!(
                        "Running{} ({seconds}s {} esc to cancel {} ctrl+o details)",
                        g.ellipsis, g.bullet, g.bullet
                    ),
                    dim,
                ),
            ]));
        }
        ToolState::Done { duration_ms } => {
            out.push(Line::from(truncate(
                vec![
                    Span::styled(format!("{} ", g.tool), style(app, Role::Green)),
                    name,
                ],
                width,
                g.ellipsis,
            )));
            let mut facts = Vec::new();
            if card.bytes > 0 {
                facts.push(size(card.bytes));
            }
            facts.push(duration(*duration_ms));
            let left = vec![
                result_lead(app, true),
                Span::styled(format!("{} ", g.ok), style(app, Role::Green)),
                Span::styled(facts.join(&format!(" {} ", g.bullet)), dim),
            ];
            out.push(spread(left, toggle, width, g.ellipsis));
        }
        ToolState::Failed {
            duration_ms,
            code,
            recovery,
        } => {
            let red = style(app, Role::Red);
            out.push(Line::from(truncate(
                vec![Span::styled(format!("{} ", g.tool), red), name],
                width,
                g.ellipsis,
            )));
            out.push(spread(
                vec![
                    result_lead(app, true),
                    Span::styled(format!("{} {code}", g.fail), red.patch(bold())),
                    Span::styled(format!(" {} {}", g.bullet, duration(*duration_ms)), dim),
                ],
                toggle,
                width,
                g.ellipsis,
            ));
            if let Some(r) = recovery {
                out.push(Line::from(vec![
                    result_lead(app, false),
                    Span::styled(format!("recovery: {r}"), dim),
                ]));
            }
        }
        ToolState::Denied { code, reason } => {
            let amber = style(app, Role::Amber);
            out.push(Line::from(truncate(
                vec![Span::styled(format!("{} ", g.tool), amber), name],
                width,
                g.ellipsis,
            )));
            out.push(Line::from(vec![
                result_lead(app, true),
                Span::styled(format!("{} denied {code}", g.warn), amber),
            ]));
            if !reason.is_empty() {
                out.push(Line::from(vec![
                    result_lead(app, false),
                    Span::styled(reason.clone(), dim),
                ]));
            }
        }
    }
    if let Some((first, lines)) = &card.diff {
        out.extend(diff_lines(app, *first, lines, width));
    }
    if card.expanded && !matches!(card.state, ToolState::Running) {
        let pretty = serde_json::to_string_pretty(&card.input).unwrap_or_default();
        out.push(Line::from(vec![
            result_lead(app, false),
            Span::styled("input", dim),
        ]));
        for l in pretty.lines().take(40) {
            out.push(Line::from(vec![
                result_lead(app, false),
                Span::raw(format!("  {l}")),
            ]));
        }
        if let Some(text) = &card.output {
            out.push(Line::from(vec![
                result_lead(app, false),
                Span::styled("output", dim),
            ]));
            for l in text.lines().take(40) {
                out.push(Line::from(truncate(
                    vec![result_lead(app, false), Span::raw(format!("  {l}"))],
                    width,
                    g.ellipsis,
                )));
            }
        }
        if let Some(rule) = &card.rule {
            out.push(Line::from(vec![
                result_lead(app, false),
                Span::styled(format!("rule: {rule}"), dim),
            ]));
        }
    }
    out
}

/// The changed lines under an edit, numbered, in red and green.
fn diff_lines(app: &App, first: u32, lines: &[String], width: usize) -> Vec<Line<'static>> {
    const SHOWN: usize = 30;
    let dim = style(app, Role::Dim);
    let adds = lines.iter().filter(|l| l.starts_with('+')).count();
    let removes = lines.iter().filter(|l| l.starts_with('-')).count();
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let mut out = vec![Line::from(vec![
        result_lead(app, false),
        Span::styled(
            format!("{} added, {} removed", plural(adds, "line"), removes),
            dim,
        ),
    ])];
    let mut number = first;
    for l in lines.iter().take(SHOWN) {
        let (role, shown) = match l.chars().next() {
            Some('-') => (Some(Role::Red), true),
            Some('+') => (Some(Role::Green), true),
            _ => (None, true),
        };
        let _ = shown;
        let text = format!("{number:>5} {l}");
        if !l.starts_with('-') {
            number += 1;
        }
        let st = role.map_or_else(Style::default, |r| style(app, r));
        out.push(Line::from(truncate(
            vec![
                Span::styled("    ".to_string(), dim),
                Span::styled(text, st),
            ],
            width,
            app.look.glyphs.ellipsis,
        )));
    }
    if lines.len() > SHOWN {
        out.push(Line::from(Span::styled(
            format!(
                "    {} {} more lines",
                app.look.glyphs.ellipsis,
                lines.len() - SHOWN
            ),
            dim,
        )));
    }
    out
}

fn error_lines(app: &App, card: &ErrorCard, width: usize) -> Vec<Line<'static>> {
    let g = &app.look.glyphs;
    let red = style(app, Role::Red);
    let dim = style(app, Role::Dim);
    let mut head = vec![Span::styled(format!("{} ", g.fail), red)];
    if !card.title.is_empty() {
        head.push(Span::styled(card.title.clone(), red.patch(bold())));
        head.push(Span::raw("  "));
    }
    head.push(Span::styled(card.code.clone(), red));
    if let Some(extra) = &card.extra {
        head.push(Span::styled(format!(" ({extra})"), red));
    }
    let mut out = vec![Line::from(truncate(head, width, g.ellipsis))];
    for l in markdown::wrap(
        vec![Span::raw(card.message.clone())],
        width.saturating_sub(2),
    ) {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(l.spans);
        out.push(Line::from(spans));
    }
    if let Some(detail) = &card.detail {
        let (h, v, tl, tr, bl, br) = if is_ascii_look(app) {
            ("-", "|", "+", "+", "+", "+")
        } else {
            ("─", "│", "╭", "╮", "╰", "╯")
        };
        let inner = width.saturating_sub(6).max(12);
        let title = " detail ";
        out.push(Line::from(Span::styled(
            format!(
                "  {tl}{title}{}{tr}",
                h.repeat(inner.saturating_sub(title.len()))
            ),
            dim,
        )));
        for l in detail.lines().take(6) {
            let text = truncate(vec![Span::raw(l.to_string())], inner - 1, g.ellipsis);
            let used: usize = text.iter().map(|s| s.content.width()).sum();
            let mut spans = vec![Span::styled(format!("  {v} "), dim)];
            spans.extend(text);
            spans.push(Span::styled(
                format!("{}{v}", " ".repeat(inner.saturating_sub(used + 1))),
                dim,
            ));
            out.push(Line::from(spans));
        }
        out.push(Line::from(Span::styled(
            format!("  {bl}{}{br}", h.repeat(inner)),
            dim,
        )));
    }
    if let Some(actions) = &card.actions {
        out.push(Line::from(Span::styled(actions.clone(), dim)));
    }
    out
}

/// Lines of `body` after a lead on the first line and blanks on the rest.
fn led(lead: &Span<'static>, body: Vec<Line<'static>>, indent: usize) -> Vec<Line<'static>> {
    body.into_iter()
        .enumerate()
        .map(|(i, line)| {
            let mut spans = vec![if i == 0 {
                lead.clone()
            } else {
                Span::raw(" ".repeat(indent))
            }];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn item_lines(app: &App, item: &Item, width: usize) -> Vec<Line<'static>> {
    let g = &app.look.glyphs;
    let dim = style(app, Role::Dim);
    match item {
        Item::User(text) => {
            // What you said stands out from what came back.
            let body = markdown::wrap(vec![Span::raw(text.clone())], width.saturating_sub(2))
                .into_iter()
                .map(|l| {
                    Line::from(
                        l.spans
                            .into_iter()
                            .map(|sp| Span::styled(sp.content, sp.style.patch(bold())))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            led(
                &Span::styled(format!("{} ", g.prompt), dim.patch(bold())),
                body,
                2,
            )
        }
        Item::Assistant { text, streaming } => {
            let mut lines = markdown::render(
                text,
                width.saturating_sub(2),
                Style3 {
                    theme: &app.look.theme,
                    support: app.look.support,
                    glyphs: g,
                    highlighter: None,
                },
            );
            if *streaming {
                let cursor = if app.look.animation { "▌" } else { "_" };
                match lines.last_mut() {
                    Some(last) => last.spans.push(Span::raw(cursor)),
                    None => lines.push(Line::from(Span::raw(cursor))),
                }
            }
            led(&Span::raw(format!("{} ", g.tool)), lines, 2)
        }
        Item::Reasoning {
            text,
            tokens,
            expanded,
        } => {
            use crate::app::ShowReasoning;
            if app.look.show_reasoning == ShowReasoning::Never {
                return Vec::new();
            }
            let marker = if *expanded { g.collapse } else { g.expand };
            let head = spread(
                vec![Span::styled(
                    format!("{} reasoning {} {tokens} tokens", g.star, g.bullet),
                    dim,
                )],
                vec![Span::styled(format!("[{marker}]"), dim)],
                width,
                g.ellipsis,
            );
            let mut out = vec![head];
            if *expanded {
                for l in markdown::wrap(
                    vec![Span::styled(text.clone(), dim)],
                    width.saturating_sub(2),
                ) {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(l.spans);
                    out.push(Line::from(spans));
                }
            }
            out
        }
        Item::Tool(card) => tool_lines(app, card, width),
        Item::Notice(kind, text) => {
            let (lead, role) = match kind {
                NoticeKind::Info => (format!("  {}  ", g.result), Role::Dim),
                NoticeKind::Success => (format!("{} ", g.ok), Role::Green),
                NoticeKind::Warning => (format!("{} ", g.warn), Role::Amber),
            };
            let pad = lead.width();
            let body = markdown::wrap(vec![Span::raw(text.clone())], width.saturating_sub(pad))
                .into_iter()
                .map(|l| {
                    Line::from(
                        l.spans
                            .into_iter()
                            .map(|s| Span::styled(s.content, style(app, role).patch(s.style)))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            led(&Span::styled(lead, style(app, role)), body, pad)
        }
        Item::Error(card) => error_lines(app, card, width),
    }
}

/// The welcome box and the startup facts (§10.9).
fn idle_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let g = &app.look.glyphs;
    let dim = style(app, Role::Dim);
    let f = Frame::new(is_ascii_look(app));
    let edge = style(app, Role::Accent);
    let inner = width.saturating_sub(2).clamp(20, 62);
    let row = |spans: Vec<Span<'static>>| {
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        let mut line = vec![Span::styled(f.v.to_string(), edge), Span::raw(" ")];
        line.extend(spans);
        line.push(Span::raw(" ".repeat((inner - 1).saturating_sub(used))));
        line.push(Span::styled(f.v.to_string(), edge));
        Line::from(truncate(line, inner + 2, g.ellipsis))
    };
    let fact = |label: &str, value: String| {
        row(vec![
            Span::styled(format!("  {label:<6}"), dim),
            Span::raw(value),
        ])
    };
    let mut out = vec![Line::from(Span::styled(
        format!("{}{}{}", f.tl, f.h.repeat(inner), f.tr),
        edge,
    ))];
    out.push(row(vec![
        Span::styled(format!("{} ", g.star), edge.patch(bold())),
        Span::styled("Welcome to ".to_string(), bold()),
        Span::styled("Cairn".to_string(), edge.patch(bold())),
    ]));
    out.push(row(Vec::new()));
    out.push(fact("model", app.model.clone()));
    out.push(fact(
        "mode",
        format!("{} {} shift+tab to change", app.mode.as_str(), g.bullet),
    ));
    if let Some(dir) = &app.workspace {
        out.push(fact("cwd", dir.clone()));
    }
    out.push(Line::from(Span::styled(
        format!("{}{}{}", f.bl, f.h.repeat(inner), f.br),
        edge,
    )));
    out.push(Line::default());
    out.push(Line::from(Span::raw(
        "Ready. Type a prompt, @ to mention a file, / for commands.",
    )));
    let s = &app.startup;
    let agents = if s.instructions == 0 {
        "AGENTS.md: none found · run /init to create one.".to_string()
    } else {
        format!(
            "AGENTS.md loaded ({} instruction{})",
            s.instructions,
            if s.instructions == 1 { "" } else { "s" }
        )
    };
    let map = match (s.indexing, s.repo_files, s.repo_ms) {
        (true, files, _) => Some(format!(
            "Loading repository map… {} files",
            group(files.unwrap_or(0) as u64)
        )),
        (false, Some(files), Some(ms)) => Some(format!(
            "repo map: {} files (warm {ms} ms)",
            group(files as u64)
        )),
        (false, Some(files), None) => Some(format!("repo map: {} files", group(files as u64))),
        _ => None,
    };
    let second = match map {
        Some(m) if s.instructions == 0 => format!("{agents}\n{m}"),
        Some(m) => format!("{agents} · {m}"),
        None => agents,
    };
    for l in second.lines() {
        out.push(Line::from(truncate(
            vec![Span::styled(l.to_string(), dim)],
            width,
            "…",
        )));
    }
    if let Some((new, old)) = &app.update_available {
        out.push(Line::from(Span::styled(
            format!("↻ Cairn {new} available (you have {old}). Run: cairn update"),
            dim,
        )));
    }
    out
}

/// Every transcript line at `width`, with a blank line between items.
#[must_use]
pub fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    if app.transcript.is_empty() && app.running.is_none() {
        return idle_lines(app, width);
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, item) in app.transcript.iter().enumerate() {
        let lines = item_lines(app, item, width);
        if lines.is_empty() {
            continue;
        }
        // Tool cards sit tight against each other.
        let tight = matches!(item, Item::Tool(_))
            && matches!(app.transcript.get(i.wrapping_sub(1)), Some(Item::Tool(_)));
        if !out.is_empty() && !tight {
            out.push(Line::default());
        }
        out.extend(lines);
    }
    // A turn is running: say what it is doing, and how to stop it.
    if let Some(r) = app.running {
        let tool_running =
            matches!(app.transcript.last(), Some(Item::Tool(c)) if c.state == ToolState::Running);
        if !tool_running {
            let g = &app.look.glyphs;
            let frame = if app.look.animation {
                g.spinner[usize::try_from(app.tick).unwrap_or(0) % g.spinner.len()]
            } else {
                g.star
            };
            let word = if r.streaming {
                "Responding"
            } else {
                "Thinking"
            };
            let seconds = app.tick.saturating_sub(r.started_tick) * 80 / 1000;
            if !out.is_empty() {
                out.push(Line::default());
            }
            out.push(Line::from(vec![
                Span::styled(format!("{frame} "), style(app, Role::Accent)),
                Span::styled(format!("{word}{}", g.ellipsis), style(app, Role::Accent)),
                Span::styled(
                    format!(" ({seconds}s {} esc to interrupt)", g.bullet),
                    style(app, Role::Dim),
                ),
            ]));
        }
    }
    out
}

impl App {
    /// What the status bar calls the running turn.
    #[must_use]
    pub fn running_phase(&self) -> String {
        let Some(r) = self.running else {
            return String::new();
        };
        if let Some(Item::Tool(card)) = self.transcript.last() {
            if card.state == ToolState::Running {
                return format!("running {}", card.name);
            }
        }
        if r.streaming {
            "streaming".to_string()
        } else {
            "thinking".to_string()
        }
    }
}

// ------------------------------------------------------------------- draw

/// Draw the whole screen into `buf`; returns the cursor cell, if one shows.
pub fn draw(app: &App, buf: &mut Buffer, area: Rect) -> Option<(u16, u16)> {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            buf.set_string(x, y, " ", Style::default());
        }
    }
    let Some(regions) = layout(app, area) else {
        let msg = format!(
            "Terminal too small (need {MIN_WIDTH}x{MIN_HEIGHT}, have {}x{}). Resize to continue.",
            area.width, area.height
        );
        for (i, line) in markdown::wrap(vec![Span::raw(msg)], usize::from(area.width))
            .into_iter()
            .enumerate()
        {
            if let Ok(i) = u16::try_from(i) {
                if i < area.height {
                    put_line(buf, area.x, area.y + i, area.width, &line);
                }
            }
        }
        return None;
    };
    put_line(
        buf,
        regions.status.x,
        regions.status.y,
        regions.status.width,
        &status_line(app, usize::from(regions.status.width)),
    );
    draw_input_box(app, buf, &regions);

    // Transcript, scrolled in lines from the bottom.
    let width = usize::from(regions.transcript.width);
    let all = transcript_lines(app, width);
    let height = usize::from(regions.transcript.height);
    let max_scroll = all.len().saturating_sub(height);
    let scroll = app.scroll.min(max_scroll);
    let end = all.len() - scroll;
    let start = end.saturating_sub(height);
    for (row, line) in all[start..end].iter().enumerate() {
        let y = regions.transcript.y + u16::try_from(row).unwrap_or(0);
        put_line(buf, regions.transcript.x, y, regions.transcript.width, line);
    }
    if scroll > 0 {
        let hint = format!(" {} {scroll} lines below ", app.look.glyphs.collapse);
        let w = u16::try_from(hint.width())
            .unwrap_or(0)
            .min(regions.transcript.width);
        let x = regions.transcript.x + regions.transcript.width - w;
        let y = regions.transcript.y + regions.transcript.height.saturating_sub(1);
        buf.set_string(
            x,
            y,
            &hint,
            style(app, Role::Dim).add_modifier(Modifier::REVERSED),
        );
    }

    // The prompt.
    let (rows, (crow, ccol)) =
        input_layout(app, usize::from(regions.input.width).saturating_sub(2));
    let visible = usize::from(regions.input.height);
    let first = (crow + 1)
        .saturating_sub(visible)
        .min(rows.len().saturating_sub(visible));
    let prompt = app.look.glyphs.prompt;
    for (i, row) in rows.iter().enumerate().skip(first).take(visible) {
        let y = regions.input.y + u16::try_from(i - first).unwrap_or(0);
        let lead = if i == 0 {
            format!("{prompt} ")
        } else {
            "  ".to_string()
        };
        buf.set_string(
            regions.input.x,
            y,
            &lead,
            style(app, Role::Accent).patch(bold()),
        );
        buf.set_stringn(
            regions.input.x + 2,
            y,
            row,
            usize::from(regions.input.width).saturating_sub(2),
            Style::default(),
        );
    }
    if app.editor.is_empty() && app.running.is_none() {
        let hint = "Ask anything · / for commands · @ for files";
        buf.set_stringn(
            regions.input.x + 2,
            regions.input.y,
            hint,
            usize::from(regions.input.width).saturating_sub(2),
            style(app, Role::Dim),
        );
    }
    if app.editor.newline_count() > 0 {
        let counter = format!("⏎×{}", app.editor.newline_count());
        let w = u16::try_from(counter.width()).unwrap_or(0);
        buf.set_string(
            regions.input.x + regions.input.width - w,
            regions.input.y,
            &counter,
            style(app, Role::Dim),
        );
    }
    let cx = regions.input.x + 2 + u16::try_from(ccol).unwrap_or(0);
    let cy = regions.input.y + u16::try_from(crow - first).unwrap_or(0);
    crate::overlay_view::draw_overlay(app, buf, &regions);
    // The cursor hides while the model streams (REQ-TUI-002).
    let streaming = app.running.is_some_and(|r| r.streaming);
    (!streaming && matches!(app.overlay, crate::overlay::Overlay::None)).then_some((cx, cy))
}

/// The buffer's rows as plain text, for tests and the plain-text mode.
#[must_use]
pub fn rows(buf: &Buffer) -> Vec<String> {
    let area = buf.area;
    (area.y..area.y + area.height)
        .map(|y| {
            let mut row = String::new();
            let mut skip = 0;
            for x in area.x..area.x + area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let cell = &buf[(x, y)];
                let symbol = cell.symbol();
                skip = symbol.width().saturating_sub(1);
                row.push_str(symbol);
            }
            row.trim_end().to_string()
        })
        .collect()
}
