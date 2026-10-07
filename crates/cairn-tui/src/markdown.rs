//! Markdown for the transcript (SPEC §10.5): the subset the model writes,
//! drawn as styled terminal lines. Never HTML.
//!
//! Headings, bold, italic, strikethrough, inline code, fenced code (with an
//! optional highlighter), nested lists, block quotes, tables, rules and links
//! (shown as underlined text) are handled; `path:line` references are
//! underlined so they read as places.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::{ColorSupport, Glyphs, Role, Theme};

/// Colours code by token class. Implemented outside this crate, where the
/// grammars are available.
pub trait Highlight {
    /// One entry per source line, each a list of `(text, token class)`;
    /// classes are `keyword`, `string`, `number`, `comment`, `function`, or
    /// empty for plain text. `None` for a language it does not know.
    fn highlight(&self, language: &str, code: &str) -> Option<Vec<Vec<(String, &'static str)>>>;
}

/// How to draw.
#[derive(Clone, Copy)]
pub struct Style3<'a> {
    pub theme: &'a Theme,
    pub support: ColorSupport,
    pub glyphs: &'a Glyphs,
    pub highlighter: Option<&'a dyn Highlight>,
}

impl std::fmt::Debug for Style3<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Style3").finish_non_exhaustive()
    }
}

fn place_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(r"[A-Za-z0-9_./\\-]+\.[A-Za-z0-9]+:\d+(?::\d+)?").expect("static regex")
    })
}

/// Split `text` so `path:line` references can be styled apart.
fn with_places(text: &str, style: Style, link: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut at = 0;
    for m in place_regex().find_iter(text) {
        if m.start() > at {
            out.push(Span::styled(text[at..m.start()].to_string(), style));
        }
        out.push(Span::styled(m.as_str().to_string(), style.patch(link)));
        at = m.end();
    }
    if at < text.len() {
        out.push(Span::styled(text[at..].to_string(), style));
    }
    out
}

/// Word-wrap styled spans to `width` display columns.
#[must_use]
pub fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let push_piece = |piece: &str,
                      style: Style,
                      current: &mut Vec<Span<'static>>,
                      used: &mut usize,
                      lines: &mut Vec<Line<'static>>| {
        let w = piece.width();
        if w == 0 {
            return;
        }
        if *used + w > width && *used > 0 && !piece.trim().is_empty() {
            trim_trailing_space(current);
            lines.push(Line::from(std::mem::take(current)));
            *used = 0;
        }
        // A piece wider than a whole line is broken by character.
        if w > width {
            let mut chunk = String::new();
            let mut cw = 0;
            for ch in piece.chars() {
                let chw = ch.width().unwrap_or(0);
                if cw + chw > width {
                    current.push(Span::styled(std::mem::take(&mut chunk), style));
                    lines.push(Line::from(std::mem::take(current)));
                    cw = 0;
                }
                chunk.push(ch);
                cw += chw;
            }
            if !chunk.is_empty() {
                current.push(Span::styled(chunk, style));
                *used = cw;
            }
            return;
        }
        // A space that would start a line is dropped.
        if *used == 0 && piece.chars().all(char::is_whitespace) {
            return;
        }
        current.push(Span::styled(piece.to_string(), style));
        *used += w;
    };
    for span in spans {
        let style = span.style;
        let text = span.content.to_string();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            if let Some(nl) = rest.find('\n') {
                let (head, tail) = rest.split_at(nl);
                split_words(head, |piece| {
                    push_piece(piece, style, &mut current, &mut used, &mut lines);
                });
                trim_trailing_space(&mut current);
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
                rest = &tail[1..];
            } else {
                split_words(rest, |piece| {
                    push_piece(piece, style, &mut current, &mut used, &mut lines);
                });
                break;
            }
        }
    }
    trim_trailing_space(&mut current);
    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// Drop whitespace-only spans (and trailing blanks) from the end of a line.
fn trim_trailing_space(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        if trimmed.is_empty() {
            spans.pop();
        } else {
            last.content = trimmed.into();
            break;
        }
    }
}

/// Calls `f` with alternating runs of non-space and space text.
fn split_words(text: &str, mut f: impl FnMut(&str)) {
    let mut start = 0;
    let mut in_space: Option<bool> = None;
    for (i, ch) in text.char_indices() {
        let space = ch.is_whitespace();
        match in_space {
            Some(s) if s != space => {
                f(&text[start..i]);
                start = i;
                in_space = Some(space);
            }
            None => in_space = Some(space),
            _ => {}
        }
    }
    if start < text.len() {
        f(&text[start..]);
    }
}

struct Ctx<'a> {
    st: Style3<'a>,
    width: usize,
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    /// Prefix for continuation lines of the current block (list indent, quote bar).
    prefix: Vec<String>,
    list: Vec<Option<u64>>,
    quote: usize,
    code: Option<(String, String)>,
    table: Option<Table>,
    link: Option<String>,
}

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
    in_head: bool,
    head_rows: usize,
}

impl Ctx<'_> {
    fn base(&self) -> Style {
        self.style_stack
            .iter()
            .fold(Style::default(), |a, s| a.patch(*s))
    }

    fn role(&self, role: Role) -> Style {
        self.st.theme.fg(role, self.st.support)
    }

    fn quote_prefix(&self) -> String {
        if self.quote == 0 {
            String::new()
        } else {
            let bar = if self.st.glyphs.tool == "*" {
                "| "
            } else {
                "▎ "
            };
            bar.repeat(self.quote)
        }
    }

    fn indent(&self) -> String {
        format!("{}{}", self.quote_prefix(), self.prefix.concat())
    }

    /// Flush the pending inline spans as wrapped lines.
    fn flush(&mut self, first_prefix: Option<String>) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let indent = self.indent();
        let inner = self.width.saturating_sub(indent.width()).max(1);
        let first = first_prefix.unwrap_or_else(|| indent.clone());
        let dim = self.role(Role::Dim);
        for (i, mut line) in wrap(spans, inner).into_iter().enumerate() {
            let lead = if i == 0 {
                first.clone()
            } else {
                indent.clone()
            };
            if !lead.is_empty() {
                line.spans.insert(0, Span::styled(lead, dim));
            }
            self.out.push(line);
        }
    }

    fn text(&mut self, text: &str) {
        let link = Style::default().add_modifier(Modifier::UNDERLINED);
        let style = self.base();
        for span in with_places(text, style, link) {
            self.spans.push(span);
        }
    }

    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| !l.spans.is_empty()) {
            self.out.push(Line::default());
        }
    }
}

fn heading_style(level: HeadingLevel, ctx: &Ctx<'_>) -> Style {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => bold.patch(ctx.role(Role::Accent)),
        _ => bold,
    }
}

fn code_lines(ctx: &mut Ctx<'_>, language: &str, code: &str) {
    let dim = ctx.role(Role::Dim);
    let ascii = ctx.st.glyphs.horizontal == "-";
    let (h, v, top, bottom) = if ascii {
        ("-", "| ", "+", "+")
    } else {
        ("─", "│ ", "╭", "╰")
    };
    let indent = ctx.indent();
    let label = if language.is_empty() {
        String::new()
    } else {
        format!(" {language} ")
    };
    let body: Vec<&str> = code.trim_end_matches('\n').split('\n').collect();
    let longest = body.iter().map(|l| l.width()).max().unwrap_or(0);
    // The rules are as wide as the code, within the column, never narrower
    // than the label.
    let room = ctx.width.saturating_sub(indent.width());
    let rule = (longest + 2).max(label.width() + 6).min(room).min(100);
    let fill = rule.saturating_sub(label.width() + 2);
    ctx.out.push(Line::from(vec![Span::styled(
        format!("{indent}{top}{h}{label}{}", h.repeat(fill)),
        dim,
    )]));
    let highlighted = ctx
        .st
        .highlighter
        .and_then(|hl| hl.highlight(language, code));
    for (i, line) in body.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("{indent}{v}"), dim)];
        match highlighted.as_ref().and_then(|hl| hl.get(i)) {
            Some(tokens) => {
                for (text, class) in tokens {
                    let style = match ctx.st.theme.syntax.get(*class) {
                        Some(r) if !class.is_empty() => ctx.role(*r),
                        _ => Style::default(),
                    };
                    spans.push(Span::styled(text.clone(), style));
                }
            }
            None => spans.push(Span::raw((*line).to_string())),
        }
        ctx.out.push(Line::from(spans));
    }
    ctx.out.push(Line::from(vec![Span::styled(
        format!("{indent}{bottom}{}", h.repeat(rule.saturating_sub(1))),
        dim,
    )]));
}

fn render_table(ctx: &mut Ctx<'_>, table: &Table) {
    let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let cell_width =
        |cell: &Vec<Span<'static>>| cell.iter().map(|s| s.content.width()).sum::<usize>();
    let mut widths = vec![0_usize; cols];
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell_width(cell));
        }
    }
    // Shrink the widest columns until the grid fits.
    let indent = ctx.indent();
    let chrome = 3 * cols + 1;
    let budget = ctx.width.saturating_sub(indent.width() + chrome).max(cols);
    while widths.iter().sum::<usize>() > budget {
        if let Some((i, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) {
            if widths[i] <= 3 {
                break;
            }
            widths[i] -= 1;
        }
    }
    let dim = ctx.role(Role::Dim);
    let ascii = ctx.st.glyphs.horizontal == "-";
    let (v, h) = if ascii { ("|", "-") } else { ("│", "─") };
    let line_of = |left: &str, mid: &str, right: &str| {
        let body: Vec<String> = widths.iter().map(|w| h.repeat(w + 2)).collect();
        format!("{indent}{left}{}{right}", body.join(mid))
    };
    ctx.out.push(Line::from(Span::styled(
        line_of(
            if ascii { "+" } else { "┌" },
            if ascii { "+" } else { "┬" },
            if ascii { "+" } else { "┐" },
        ),
        dim,
    )));
    for (r, row) in table.rows.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("{indent}{v}"), dim)];
        for (i, w) in widths.iter().enumerate() {
            let cell = row.get(i).cloned().unwrap_or_default();
            let text: String = cell.iter().map(|s| s.content.to_string()).collect();
            let shown: String = if text.width() > *w {
                let mut cut = String::new();
                let mut cw = 0;
                for ch in text.chars() {
                    let chw = ch.width().unwrap_or(0);
                    if cw + chw + 1 > *w {
                        break;
                    }
                    cut.push(ch);
                    cw += chw;
                }
                format!("{cut}…")
            } else {
                text.clone()
            };
            let pad = w.saturating_sub(shown.width());
            let (left, right) = match table.aligns.get(i) {
                Some(Alignment::Right) => (pad, 0),
                Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            let style = if r < table.head_rows {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::raw(" ".repeat(left + 1)));
            spans.push(Span::styled(shown, style));
            spans.push(Span::raw(" ".repeat(right + 1)));
            spans.push(Span::styled(v.to_string(), dim));
        }
        ctx.out.push(Line::from(spans));
        if r + 1 == table.head_rows {
            ctx.out.push(Line::from(Span::styled(
                line_of(
                    if ascii { "+" } else { "├" },
                    if ascii { "+" } else { "┼" },
                    if ascii { "+" } else { "┤" },
                ),
                dim,
            )));
        }
    }
    ctx.out.push(Line::from(Span::styled(
        line_of(
            if ascii { "+" } else { "└" },
            if ascii { "+" } else { "┴" },
            if ascii { "+" } else { "┘" },
        ),
        dim,
    )));
}

/// Render `markdown` into lines at most `width` columns wide.
#[must_use]
#[allow(clippy::too_many_lines, reason = "one arm per markdown event")]
pub fn render(markdown: &str, width: usize, st: Style3<'_>) -> Vec<Line<'static>> {
    let mut ctx = Ctx {
        st,
        width: width.max(10),
        out: Vec::new(),
        spans: Vec::new(),
        style_stack: Vec::new(),
        prefix: Vec::new(),
        list: Vec::new(),
        quote: 0,
        code: None,
        table: None,
        link: None,
    };
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut item_first: Option<String> = None;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    ctx.blank();
                    ctx.style_stack.push(heading_style(level, &ctx));
                }
                Tag::BlockQuote(_) => {
                    let first = item_first.take();
                    ctx.flush(first);
                    ctx.quote += 1;
                    ctx.style_stack
                        .push(Style::default().add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(kind) => {
                    let first = item_first.take();
                    ctx.flush(first);
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => {
                            l.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    ctx.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    let first = item_first.take();
                    ctx.flush(first);
                    ctx.list.push(start);
                }
                Tag::Item => {
                    ctx.flush(None);
                    let ascii = ctx.st.glyphs.horizontal == "-";
                    let marker = match ctx.list.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}. ");
                            *n += 1;
                            m
                        }
                        _ => (if ascii { "- " } else { "• " }).to_string(),
                    };
                    let depth_indent = " ".repeat(marker.width());
                    item_first = Some(format!(
                        "{}{}{marker}",
                        ctx.quote_prefix(),
                        ctx.prefix.concat()
                    ));
                    ctx.prefix.push(depth_indent);
                }
                Tag::Emphasis => ctx
                    .style_stack
                    .push(Style::default().add_modifier(Modifier::ITALIC)),
                Tag::Strong => ctx
                    .style_stack
                    .push(Style::default().add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => ctx
                    .style_stack
                    .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } => {
                    ctx.link = Some(dest_url.to_string());
                    ctx.style_stack
                        .push(Style::default().add_modifier(Modifier::UNDERLINED));
                }
                Tag::Table(aligns) => {
                    ctx.flush(None);
                    ctx.table = Some(Table {
                        aligns,
                        rows: Vec::new(),
                        row: Vec::new(),
                        in_head: false,
                        head_rows: 0,
                    });
                }
                Tag::TableHead => {
                    if let Some(t) = ctx.table.as_mut() {
                        t.in_head = true;
                    }
                }
                Tag::TableRow | Tag::TableCell => {
                    if matches!(tag, Tag::TableCell) {
                        ctx.spans.clear();
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    let first = item_first.take();
                    ctx.flush(first);
                    ctx.blank();
                }
                TagEnd::Heading(_) => {
                    ctx.flush(None);
                    ctx.style_stack.pop();
                    ctx.blank();
                }
                TagEnd::BlockQuote(_) => {
                    ctx.flush(None);
                    ctx.quote = ctx.quote.saturating_sub(1);
                    ctx.style_stack.pop();
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = ctx.code.take() {
                        code_lines(&mut ctx, &lang, &code);
                        ctx.blank();
                    }
                }
                TagEnd::List(_) => {
                    ctx.flush(None);
                    ctx.list.pop();
                    if ctx.list.is_empty() {
                        ctx.blank();
                    }
                }
                TagEnd::Item => {
                    let first = item_first.take();
                    ctx.flush(first);
                    ctx.prefix.pop();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    ctx.style_stack.pop();
                }
                TagEnd::Link => {
                    ctx.style_stack.pop();
                    if let Some(url) = ctx.link.take() {
                        // The destination is shown when it is not the text.
                        let shown: String =
                            ctx.spans.iter().map(|s| s.content.to_string()).collect();
                        if !url.is_empty() && !shown.ends_with(&url) && shown != url {
                            let dim = ctx.role(Role::Dim);
                            ctx.spans.push(Span::styled(format!(" ({url})"), dim));
                        }
                    }
                }
                TagEnd::TableCell => {
                    let cell = std::mem::take(&mut ctx.spans);
                    if let Some(t) = ctx.table.as_mut() {
                        t.row.push(cell);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = ctx.table.as_mut() {
                        t.rows.push(std::mem::take(&mut t.row));
                        t.in_head = false;
                        t.head_rows = t.rows.len();
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = ctx.table.as_mut() {
                        if !t.in_head {
                            t.rows.push(std::mem::take(&mut t.row));
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = ctx.table.take() {
                        render_table(&mut ctx, &t);
                        ctx.blank();
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some((_, code)) = ctx.code.as_mut() {
                    code.push_str(&text);
                } else {
                    ctx.text(&text);
                }
            }
            Event::Code(code) => {
                let style = ctx.base().patch(ctx.role(Role::Accent));
                // Without colour, backticks keep inline code visible.
                let shown = if ctx.st.support == ColorSupport::None {
                    format!("`{code}`")
                } else {
                    code.to_string()
                };
                ctx.spans.push(Span::styled(shown, style));
            }
            Event::SoftBreak => ctx.spans.push(Span::raw(" ")),
            Event::HardBreak => ctx.spans.push(Span::raw("\n")),
            Event::Rule => {
                ctx.flush(None);
                let rule = ctx.st.glyphs.horizontal.repeat(ctx.width.min(60));
                let dim = ctx.role(Role::Dim);
                ctx.out.push(Line::from(Span::styled(rule, dim)));
                ctx.blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Never HTML: shown as the text it is.
                ctx.text(&html);
            }
            _ => {}
        }
    }
    ctx.flush(None);
    while ctx
        .out
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.is_empty()))
    {
        ctx.out.pop();
    }
    ctx.out
}

/// Plain text of `lines`, for assertions and the plain-text mode.
#[must_use]
pub fn plain(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st<'a>(theme: &'a Theme, glyphs: &'a Glyphs, support: ColorSupport) -> Style3<'a> {
        Style3 {
            theme,
            support,
            glyphs,
            highlighter: None,
        }
    }

    fn text(md: &str, width: usize) -> String {
        let theme = Theme::cairn_dark();
        let glyphs = Glyphs::unicode();
        plain(&render(md, width, st(&theme, &glyphs, ColorSupport::True)))
    }

    #[test]
    fn paragraphs_wrap_on_words_at_the_width() {
        let out = text("one two three four five six seven eight nine ten", 20);
        assert_eq!(out, "one two three four\nfive six seven eight\nnine ten");
        for line in out.lines() {
            assert!(line.width() <= 20);
        }
    }

    #[test]
    fn a_word_longer_than_the_line_is_broken() {
        let out = text("supercalifragilisticexpialidocious", 10);
        assert_eq!(out, "supercalif\nragilistic\nexpialidoc\nious");
    }

    #[test]
    fn wide_characters_are_wrapped_by_display_width() {
        let out = text("日本語のテキストをここに書きます", 10);
        for line in out.lines() {
            assert!(line.width() <= 10, "{line}");
        }
        assert!(out.lines().count() >= 3);
    }

    #[test]
    fn headings_emphasis_and_inline_code_keep_their_styles() {
        let theme = Theme::cairn_dark();
        let glyphs = Glyphs::unicode();
        let lines = render(
            "# Title\n\nsome **bold** and *italic* and `code` and ~~gone~~",
            60,
            st(&theme, &glyphs, ColorSupport::True),
        );
        let title = &lines[0];
        assert!(title.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            title.spans[0].style.fg,
            theme.color(Role::Accent, ColorSupport::True)
        );
        let body = lines.last().unwrap();
        let find = |s: &str| {
            body.spans
                .iter()
                .find(|sp| sp.content == s)
                .unwrap_or_else(|| panic!("{s}"))
        };
        assert!(find("bold").style.add_modifier.contains(Modifier::BOLD));
        assert!(find("italic").style.add_modifier.contains(Modifier::ITALIC));
        assert!(find("gone")
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT));
        assert_eq!(
            find("code").style.fg,
            theme.color(Role::Accent, ColorSupport::True)
        );
    }

    #[test]
    fn without_colour_inline_code_keeps_its_backticks() {
        let theme = Theme::cairn_dark();
        let glyphs = Glyphs::unicode();
        let out = plain(&render(
            "run `cargo test` now",
            40,
            st(&theme, &glyphs, ColorSupport::None),
        ));
        assert_eq!(out, "run `cargo test` now");
    }

    #[test]
    fn lists_nest_and_number() {
        let out = text(
            "- one\n- two\n  - nested a\n  - nested b\n- three\n\n1. first\n2. second",
            40,
        );
        assert_eq!(
            out,
            "• one\n• two\n  • nested a\n  • nested b\n• three\n\n1. first\n2. second"
        );
    }

    #[test]
    fn a_long_list_item_wraps_under_its_marker() {
        let out = text("- alpha beta gamma delta epsilon zeta", 16);
        assert_eq!(out, "• alpha beta\n  gamma delta\n  epsilon zeta");
    }

    #[test]
    fn block_quotes_get_a_bar() {
        let out = text("> quoted line\n> second", 40);
        assert!(out.lines().all(|l| l.starts_with("▎ ")), "{out}");
    }

    #[test]
    fn code_blocks_are_fenced_off_with_their_language() {
        let out = text("```rust\nfn main() {}\nlet x = 1;\n```", 40);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("╭─ rust ─"), "{out}");
        assert_eq!(lines[1], "│ fn main() {}");
        assert_eq!(lines[2], "│ let x = 1;");
        assert!(lines[3].starts_with("╰──"), "{out}");
        // Top and bottom rules are the same width.
        assert_eq!(lines[0].chars().count(), lines[3].chars().count(), "{out}");
    }

    struct Fake;
    impl Highlight for Fake {
        fn highlight(
            &self,
            language: &str,
            code: &str,
        ) -> Option<Vec<Vec<(String, &'static str)>>> {
            (language == "rust").then(|| {
                code.lines()
                    .map(|l| match l.split_once(' ') {
                        Some((kw, rest)) => {
                            vec![(kw.to_string(), "keyword"), (format!(" {rest}"), "")]
                        }
                        None => vec![(l.to_string(), "")],
                    })
                    .collect()
            })
        }
    }

    #[test]
    fn a_highlighter_colours_tokens_by_class() {
        let theme = Theme::cairn_dark();
        let glyphs = Glyphs::unicode();
        let style = Style3 {
            theme: &theme,
            support: ColorSupport::True,
            glyphs: &glyphs,
            highlighter: Some(&Fake),
        };
        let lines = render("```rust\nfn main\n```", 40, style);
        let code = &lines[1];
        let keyword = code.spans.iter().find(|s| s.content == "fn").unwrap();
        assert_eq!(
            keyword.style.fg,
            theme.color(Role::Accent, ColorSupport::True)
        );
        // An unknown language falls back to plain text.
        let lines = render("```zig\nconst x\n```", 40, style);
        assert_eq!(plain(&lines[1..2]), "│ const x");
    }

    #[test]
    fn tables_draw_a_grid_with_a_bold_header() {
        let out = text("| name | n |\n|------|--:|\n| alpha | 1 |\n| b | 22 |", 40);
        let want = "┌───────┬────┐\n│ name  │  n │\n├───────┼────┤\n│ alpha │  1 │\n│ b     │ 22 │\n└───────┴────┘";
        assert_eq!(out, want);
    }

    #[test]
    fn a_table_wider_than_the_screen_is_squeezed() {
        let out = text("| a | b |\n|---|---|\n| aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb |", 30);
        for line in out.lines() {
            assert!(line.width() <= 30, "{line:?}");
        }
        assert!(out.contains('…'));
    }

    #[test]
    fn links_show_their_destination_unless_it_is_the_text() {
        assert_eq!(
            text("see [the docs](https://example.com/d)", 60),
            "see the docs (https://example.com/d)"
        );
        assert_eq!(text("<https://example.com>", 60), "https://example.com");
    }

    #[test]
    fn path_and_line_references_are_underlined() {
        let theme = Theme::cairn_dark();
        let glyphs = Glyphs::unicode();
        let lines = render(
            "fails at src/parser.rs:120 today",
            60,
            st(&theme, &glyphs, ColorSupport::True),
        );
        let place = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "src/parser.rs:120")
            .unwrap();
        assert!(place.style.add_modifier.contains(Modifier::UNDERLINED));
        let plain_part = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("today"))
            .unwrap();
        assert!(!plain_part.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn html_is_shown_as_text_never_interpreted() {
        let out = text("<script>alert(1)</script> hi", 60);
        assert!(out.contains("<script>"), "{out}");
    }

    #[test]
    fn rules_and_ascii_glyphs() {
        let theme = Theme::cairn_dark();
        let glyphs = Glyphs::ascii();
        let out = plain(&render(
            "- a\n\n---\n\n> q",
            20,
            st(&theme, &glyphs, ColorSupport::None),
        ));
        assert!(out.contains("- a"), "{out}");
        assert!(out.contains("--------------------"));
        assert!(out.contains("| q"));
        assert!(out.is_ascii());
    }
}
