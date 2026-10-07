//! Drawing the overlays and popups (SPEC §10.1, §10.4–§10.6).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DiffLayout};
use crate::interact::{Popup, PopupKind};
use crate::markdown;
use crate::overlay::{
    Approval, ApprovalAnswer, Confirm, DiffView, HistorySearch, HunkStatus, LineKind, Overlay,
    PlanView,
};
use crate::slash;
use crate::theme::Role;
use crate::view::{bold, is_ascii_look, put_line, style, Regions};

/// Keys the help overlay lists.
const HELP: &[(&str, &str)] = &[
    ("Enter", "send"),
    ("Shift+Enter / Alt+Enter", "newline"),
    ("Tab", "complete the popup choice"),
    ("Shift+Tab", "cycle mode (plan, build, auto)"),
    ("Esc", "cancel the turn (twice: force)"),
    ("Ctrl+C", "cancel, or quit when idle"),
    ("Ctrl+D", "quit when the prompt is empty"),
    ("Ctrl+R", "search history"),
    ("Ctrl+L", "clear the view"),
    ("Ctrl+O", "expand / collapse the last tool card"),
    ("Ctrl+T", "show / hide the todo list"),
    ("Ctrl+P", "quick open a file"),
    ("Ctrl+V", "paste an image"),
    ("PgUp / PgDn", "scroll the transcript"),
    ("F1 / Ctrl+G", "this help"),
    ("Ctrl+Q", "quit (asks first)"),
];

fn boxed(app: &App, buf: &mut Buffer, rect: Rect, title: &str, lines: &[Line<'static>]) {
    let ascii = is_ascii_look(app);
    let (h, v, tl, tr, bl, br) = if ascii {
        ("-", "|", "+", "+", "+", "+")
    } else {
        ("─", "│", "╭", "╮", "╰", "╯")
    };
    let edge = style(app, Role::Accent);
    let right = rect.x + rect.width - 1;
    let bottom = rect.y + rect.height - 1;
    for y in rect.y..=bottom {
        for x in rect.x..=right {
            buf.set_string(x, y, " ", Style::default());
        }
    }
    buf.set_string(rect.x, rect.y, tl, edge);
    buf.set_string(right, rect.y, tr, edge);
    buf.set_string(rect.x, bottom, bl, edge);
    buf.set_string(right, bottom, br, edge);
    for x in rect.x + 1..right {
        buf.set_string(x, rect.y, h, edge);
        buf.set_string(x, bottom, h, edge);
    }
    for y in rect.y + 1..bottom {
        buf.set_string(rect.x, y, v, edge);
        buf.set_string(right, y, v, edge);
    }
    let title = format!(" {title} ");
    buf.set_stringn(
        rect.x + 2,
        rect.y,
        &title,
        usize::from(rect.width).saturating_sub(4),
        edge.add_modifier(Modifier::BOLD),
    );
    let inner = Rect::new(rect.x + 2, rect.y + 1, rect.width - 4, rect.height - 2);
    for (i, line) in lines.iter().enumerate().take(usize::from(inner.height)) {
        put_line(
            buf,
            inner.x,
            inner.y + u16::try_from(i).unwrap_or(0),
            inner.width,
            line,
        );
    }
}

/// A panel of `want` rows across the whole transcript, resting on the
/// prompt, so nothing of what is under it shows beside it.
fn centred(area: Rect, _width: u16, want: usize) -> Rect {
    let h = u16::try_from(want + 2).unwrap_or(u16::MAX).min(area.height);
    Rect::new(area.x, area.y + area.height - h, area.width, h)
}

fn wrapped(text: &str, width: usize, st: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for raw in text.lines() {
        out.extend(markdown::wrap(
            vec![Span::styled(raw.to_string(), st)],
            width,
        ));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

fn line(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(spans)
}

/// Draw whatever overlay or popup applies, on top of the finished screen.
pub fn draw_overlay(app: &App, buf: &mut Buffer, regions: &Regions) {
    let transcript = Rect::new(
        regions.transcript.x,
        regions.transcript.y,
        regions.transcript.width,
        regions.transcript.height.max(4),
    );
    match &app.overlay {
        Overlay::None => {
            if let Some(popup) = app.popup() {
                draw_popup(app, buf, regions, &popup);
            }
        }
        Overlay::Approval(a) => approval(app, buf, transcript, a),
        Overlay::Diff(d) => diff(app, buf, transcript, d),
        Overlay::Plan(p) => plan(app, buf, transcript, p),
        Overlay::Help => help(app, buf, transcript),
        Overlay::History(h) => history(app, buf, transcript, h),
        Overlay::Confirm(c) => confirm(app, buf, regions, c),
    }
}

fn approval(app: &App, buf: &mut Buffer, area: Rect, a: &Approval) {
    let width = area.width.min(78);
    let inner = usize::from(width).saturating_sub(4);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !a.files.is_empty() {
        lines.push(line(vec![
            Span::styled("files: ", style(app, Role::Dim)),
            Span::raw(a.files.join(", ")),
        ]));
    }
    let prefix = if matches!(a.tool.as_str(), "bash" | "bash_background") {
        "$ "
    } else {
        ""
    };
    for b in &a.body {
        lines.extend(wrapped(&format!("{prefix}{b}"), inner, bold()));
    }
    if let Some((_, diff)) = &a.diff {
        for d in diff {
            let role = if d.starts_with('-') {
                Role::Red
            } else {
                Role::Green
            };
            lines.extend(wrapped(d, inner, style(app, role)));
        }
    }
    if let Some(rule) = &a.rule {
        lines.push(line(vec![Span::styled(
            format!("rule: {rule}"),
            style(app, Role::Dim),
        )]));
    }
    lines.push(Line::default());
    let mut buttons = Vec::new();
    for (i, b) in ApprovalAnswer::BUTTONS.iter().enumerate() {
        let st = if i == a.focus % ApprovalAnswer::BUTTONS.len() {
            style(app, Role::Accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        buttons.push(Span::styled(b.label().to_string(), st));
        buttons.push(Span::raw("  "));
    }
    lines.extend(markdown::wrap(buttons, inner));
    let rect = centred(area, width, lines.len());
    boxed(app, buf, rect, &format!("Approval · {}", a.tool), &lines);
}

fn side_by_side(app: &App, width: usize) -> bool {
    match app.look.diff_layout {
        DiffLayout::Inline => false,
        DiffLayout::Side => true,
        DiffLayout::Auto => width >= usize::from(app.look.diff_side_by_side_min_width),
    }
}

fn cut(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let cut = cut(text, width);
    let fill = width.saturating_sub(cut.width());
    format!("{cut}{}", " ".repeat(fill))
}

fn diff(app: &App, buf: &mut Buffer, area: Rect, d: &DiffView) {
    let total = usize::from(area.width);
    // The threshold is about the terminal, not the box inside it.
    let side = side_by_side(app, total + 2);
    let Some(file) = d.files.get(d.file) else {
        boxed(
            app,
            buf,
            centred(area, 40, 1),
            "Diff",
            &[line(vec![Span::raw("no changes")])],
        );
        return;
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let inner = total.saturating_sub(4);
    let pending: usize = d
        .files
        .iter()
        .flat_map(|f| &f.hunks)
        .filter(|h| h.status == HunkStatus::Pending)
        .count();
    lines.push(line(vec![
        Span::styled(file.path.clone(), bold()),
        Span::styled(
            format!(
                "  file {}/{} · hunk {}/{} · {pending} pending",
                d.file + 1,
                d.files.len(),
                d.hunk + 1,
                file.hunks.len()
            ),
            style(app, Role::Dim),
        ),
    ]));
    let half = inner.saturating_sub(3) / 2;
    let mut current_header = 1;
    for (hi, hunk) in file.hunks.iter().enumerate() {
        let mark = match hunk.status {
            HunkStatus::Pending => " ",
            HunkStatus::Accepted => "✓",
            HunkStatus::Rejected => "✗",
        };
        if hi == d.hunk {
            current_header = lines.len();
        }
        let cur = if hi == d.hunk { ">" } else { " " };
        lines.push(line(vec![Span::styled(
            format!("{cur}{mark} @@ -{} +{} @@", hunk.old_start, hunk.new_start),
            style(app, Role::Blue),
        )]));
        if side {
            let mut i = 0;
            let ls = &hunk.lines;
            while i < ls.len() {
                if ls[i].kind == LineKind::Context {
                    let l = &ls[i];
                    lines.push(line(vec![
                        Span::raw(pad(
                            &format!("{:>4}   {}", l.old_no.unwrap_or(0), l.text),
                            half,
                        )),
                        Span::raw(" │ "),
                        Span::raw(pad(
                            &format!("{:>4}   {}", l.new_no.unwrap_or(0), l.text),
                            half,
                        )),
                    ]));
                    i += 1;
                } else {
                    let mut rem = Vec::new();
                    let mut add = Vec::new();
                    while i < ls.len() && ls[i].kind != LineKind::Context {
                        if ls[i].kind == LineKind::Removed {
                            rem.push(&ls[i]);
                        } else {
                            add.push(&ls[i]);
                        }
                        i += 1;
                    }
                    for k in 0..rem.len().max(add.len()) {
                        let l = rem.get(k).map_or_else(
                            || " ".repeat(half),
                            |l| pad(&format!("{:>4} - {}", l.old_no.unwrap_or(0), l.text), half),
                        );
                        let r = add.get(k).map_or_else(
                            || " ".repeat(half),
                            |l| pad(&format!("{:>4} + {}", l.new_no.unwrap_or(0), l.text), half),
                        );
                        lines.push(line(vec![
                            Span::styled(l, style(app, Role::Red)),
                            Span::raw(" │ "),
                            Span::styled(r, style(app, Role::Green)),
                        ]));
                    }
                }
            }
        } else {
            for l in &hunk.lines {
                let (sign, role) = match l.kind {
                    LineKind::Context => (' ', None),
                    LineKind::Added => ('+', Some(Role::Green)),
                    LineKind::Removed => ('-', Some(Role::Red)),
                };
                let n = l.new_no.or(l.old_no).unwrap_or(0);
                let text = cut(&format!("{n:>4} {sign} {}", l.text), inner);
                lines.push(line(vec![Span::styled(
                    text,
                    role.map_or_else(Style::default, |r| style(app, r)),
                )]));
            }
        }
    }
    lines.push(Line::default());
    lines.push(line(vec![Span::styled(
        "Space accept · r reject · a accept all · h/l hunk · n/p file · Esc close",
        style(app, Role::Dim),
    )]));
    // Keep the title and footer; scroll the body so the current hunk shows.
    let room = usize::from(area.height).saturating_sub(2);
    if lines.len() > room && room > 3 {
        let footer = lines.split_off(lines.len() - 2);
        let title = lines.remove(0);
        let cap = room - 3;
        let start = (current_header - 1).min(lines.len().saturating_sub(cap));
        lines = std::iter::once(title)
            .chain(lines.into_iter().skip(start).take(cap))
            .chain(footer)
            .collect();
    }
    let rect = centred(area, area.width, lines.len());
    boxed(app, buf, rect, "Diff", &lines);
}

fn plan(app: &App, buf: &mut Buffer, area: Rect, p: &PlanView) {
    let width = area.width.min(86);
    let inner = usize::from(width).saturating_sub(4);
    let mut lines = vec![line(vec![Span::styled(
        format!("Goal: {}", p.goal),
        bold(),
    )])];
    if !p.assumptions.is_empty() {
        lines.push(line(vec![Span::styled(
            "Assumptions",
            style(app, Role::Dim),
        )]));
        for a in &p.assumptions {
            let flag = if a.verified { "" } else { "  ⚠ unverified" };
            lines.extend(wrapped(
                &format!("  {} {}{flag}", a.id, a.text),
                inner,
                if a.verified {
                    Style::default()
                } else {
                    style(app, Role::Amber)
                },
            ));
        }
    }
    lines.push(line(vec![Span::styled("Steps", style(app, Role::Dim))]));
    for (i, s) in p.steps.iter().enumerate() {
        let cur = if i == p.selected { ">" } else { " " };
        let st = if i == p.selected {
            bold()
        } else {
            Style::default()
        };
        lines.push(line(vec![Span::styled(
            format!("{cur} {}. {} ({} files)", i + 1, s.title, s.files),
            st,
        )]));
        if p.show_detail && i == p.selected {
            lines.extend(wrapped(
                &format!("      {}", s.detail),
                inner,
                style(app, Role::Dim),
            ));
        }
    }
    if !p.risks.is_empty() {
        lines.push(line(vec![Span::styled("Risks", style(app, Role::Dim))]));
        for r in &p.risks {
            lines.extend(wrapped(
                &format!("  [{}] {} → {}", r.severity, r.text, r.mitigation),
                inner,
                Style::default(),
            ));
        }
    }
    if !p.test.is_empty() {
        lines.extend(wrapped(
            &format!("Test: {}", p.test),
            inner,
            Style::default(),
        ));
    }
    if !p.rollback.is_empty() {
        lines.extend(wrapped(
            &format!("Rollback: {}", p.rollback),
            inner,
            Style::default(),
        ));
    }
    lines.push(Line::default());
    lines.push(line(vec![Span::styled(
        "Enter approve · e edit · s save only · d detail · Esc dismiss",
        style(app, Role::Accent),
    )]));
    let rect = centred(area, width, lines.len());
    boxed(app, buf, rect, &format!("Plan {}", p.id), &lines);
}

fn help(app: &App, buf: &mut Buffer, area: Rect) {
    let width = area.width.min(66);
    let mut lines = Vec::new();
    for (k, d) in HELP {
        lines.push(line(vec![
            Span::styled(format!("{k:<26}"), style(app, Role::Accent)),
            Span::raw((*d).to_string()),
        ]));
    }
    lines.push(Line::default());
    lines.push(line(vec![Span::styled(
        format!(
            "{} slash commands: type / to list them",
            slash::SLASH_COMMAND_COUNT
        ),
        style(app, Role::Dim),
    )]));
    let rect = centred(area, width, lines.len());
    boxed(app, buf, rect, "Help", &lines);
}

fn history(app: &App, buf: &mut Buffer, area: Rect, h: &HistorySearch) {
    let width = area.width.min(78);
    let inner = usize::from(width).saturating_sub(4);
    let mut lines = vec![line(vec![
        Span::styled("search: ", style(app, Role::Dim)),
        Span::raw(h.query.clone()),
    ])];
    let hits = app.history.search(&h.query, 10);
    if hits.is_empty() {
        lines.push(line(vec![Span::styled("no match", style(app, Role::Dim))]));
    }
    for (i, hit) in hits.iter().enumerate() {
        let first = hit.lines().next().unwrap_or("");
        let st = if i == h.selected {
            style(app, Role::Accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(line(vec![Span::styled(pad(first, inner), st)]));
    }
    let rect = centred(area, width, lines.len());
    boxed(app, buf, rect, "History", &lines);
}

fn confirm(app: &App, buf: &mut Buffer, regions: &Regions, c: &Confirm) {
    let text = match c {
        Confirm::Paste { chars } => format!("Paste {} chars? [Y/n]", group_thousands(*chars)),
        Confirm::Quit => "Quit? [y/N]".to_string(),
        Confirm::QuitWithJobs { names } => format!(
            "{} background jobs still running ({}). [k]ill all  [c]ontinue in background  [q]uit anyway",
            names.len(),
            names.join(", ")
        ),
    };
    let y = regions.status.y;
    let line = Line::from(Span::styled(
        pad(&text, usize::from(regions.status.width)),
        style(app, Role::Amber).add_modifier(Modifier::BOLD),
    ));
    put_line(buf, regions.status.x, y, regions.status.width, &line);
}

fn group_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn draw_popup(app: &App, buf: &mut Buffer, regions: &Regions, popup: &Popup) {
    let n = u16::try_from(popup.items.len())
        .unwrap_or(0)
        .min(regions.transcript.height);
    if n == 0 {
        return;
    }
    let width = regions.transcript.width.min(60);
    let y0 = regions.input.y.saturating_sub(1 + n);
    for (i, item) in popup.items.iter().take(usize::from(n)).enumerate() {
        let label = match popup.kind {
            PopupKind::Slash => {
                let c = slash::lookup(item);
                let args = c.map_or("", |c| c.args);
                let what = c.map_or("", |c| c.behavior);
                format!(" /{item} {args}  {what}")
            }
            PopupKind::Mention => format!(" @{item}"),
        };
        let st = if i == popup.selected {
            style(app, Role::Accent).add_modifier(Modifier::REVERSED)
        } else {
            style(app, Role::Fg)
        };
        let y = y0 + u16::try_from(i).unwrap_or(0);
        buf.set_string(regions.transcript.x, y, " ".repeat(usize::from(width)), st);
        buf.set_stringn(regions.transcript.x, y, &label, usize::from(width), st);
    }
}
