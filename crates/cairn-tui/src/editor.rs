//! The prompt editor (SPEC §10.2): multi-line text with grapheme-aware
//! movement, emacs or vi keys, a kill ring, undo, and a guarded paste.
//!
//! It knows nothing about terminals or rendering: keys go in, text and a
//! cursor come out, so every behaviour here is a plain unit test.

use unicode_segmentation::UnicodeSegmentation;

use crate::keys::{Code, Key};

/// Pastes longer than this ask before they land (§10.2).
pub const PASTE_CONFIRM_CHARS: usize = 5_000;

/// How keys are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Emacs,
    Vi,
}

/// What the editor wants the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// `Enter` (or the configured submit key): the whole text.
    Submit(String),
    /// `:q!` in vi mode.
    Quit,
}

/// What a paste did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paste {
    /// It went in.
    Inserted,
    /// It is large; ask first with `Paste {chars} chars? [Y/n]`.
    Confirm { chars: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vi {
    Normal,
    Insert,
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

/// The text being typed.
#[derive(Debug, Clone)]
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    /// Grapheme index within the line.
    col: usize,
    wanted_col: usize,
    mode: EditMode,
    vi: Vi,
    /// A pending first key of a two-key vi command (`d`, `g`).
    pending: Option<char>,
    /// Typing a `/search` or `:command` in vi normal mode.
    line_command: Option<(char, String)>,
    last_search: Option<String>,
    kill: String,
    kill_linewise: bool,
    undo: Vec<Snapshot>,
    held_paste: Option<String>,
    /// `true` when `Enter` inserts a newline and `Ctrl+Enter` submits.
    pub enter_is_newline: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new(EditMode::Emacs)
    }
}

fn graphemes(line: &str) -> Vec<&str> {
    line.graphemes(true).collect()
}

fn glen(line: &str) -> usize {
    line.graphemes(true).count()
}

/// Byte offset of grapheme `col` in `line`.
fn byte_at(line: &str, col: usize) -> usize {
    line.grapheme_indices(true)
        .nth(col)
        .map_or(line.len(), |(i, _)| i)
}

fn is_word(g: &str) -> bool {
    g.chars().any(|c| c.is_alphanumeric() || c == '_')
}

/// Remove terminal escape sequences from pasted text.
#[must_use]
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // Parameters and intermediates, then a final byte.
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // OSC: until BEL or ST.
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if c == '\r' {
            // CRLF and lone CR both become a newline.
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else if c.is_control() && c != '\n' && c != '\t' {
            // Dropped.
        } else {
            out.push(c);
        }
    }
    out
}

impl Editor {
    #[must_use]
    pub fn new(mode: EditMode) -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            wanted_col: 0,
            mode,
            vi: Vi::Insert,
            pending: None,
            line_command: None,
            last_search: None,
            kill: String::new(),
            kill_linewise: false,
            undo: Vec::new(),
            held_paste: None,
            enter_is_newline: false,
        }
    }

    // ------------------------------------------------------------ queries

    /// The whole text, lines joined with `\n`.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// `(row, grapheme column)`.
    #[must_use]
    pub const fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    #[must_use]
    pub const fn mode(&self) -> EditMode {
        self.mode
    }

    /// `[E]`, `[V]` for normal mode `[N]`, as the status bar shows it.
    #[must_use]
    pub const fn mode_label(&self) -> &'static str {
        match (self.mode, self.vi) {
            (EditMode::Emacs, _) => "[E]",
            (EditMode::Vi, Vi::Normal) => "[N]",
            (EditMode::Vi, Vi::Insert) => "[V]",
        }
    }

    /// In vi normal mode.
    #[must_use]
    pub fn in_normal_mode(&self) -> bool {
        self.mode == EditMode::Vi && self.vi == Vi::Normal
    }

    /// The `/` or `:` being typed in vi normal mode, with what has been typed.
    #[must_use]
    pub fn line_command(&self) -> Option<(char, &str)> {
        self.line_command.as_ref().map(|(c, s)| (*c, s.as_str()))
    }

    /// Line breaks in the text, for the `⏎×3` counter.
    #[must_use]
    pub fn newline_count(&self) -> usize {
        self.lines.len() - 1
    }

    #[must_use]
    pub fn kill_buffer(&self) -> &str {
        &self.kill
    }

    // ----------------------------------------------------------- mutation

    fn remember(&mut self) {
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
    }

    /// Replace everything and put the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_string).collect();
        self.row = self.lines.len() - 1;
        self.col = glen(&self.lines[self.row]);
        self.wanted_col = self.col;
        self.pending = None;
        self.line_command = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
        self.undo.clear();
    }

    fn clamp(&mut self) {
        self.row = self.row.min(self.lines.len() - 1);
        let max = glen(&self.lines[self.row]);
        // Vi normal mode keeps the cursor on a character.
        let limit = if self.in_normal_mode() {
            max.saturating_sub(1)
        } else {
            max
        };
        self.col = self.col.min(limit);
    }

    /// Put `text` at the cursor (newlines split lines).
    pub fn insert_str(&mut self, text: &str) {
        self.remember();
        let mut pieces = text.split('\n');
        let first = pieces.next().unwrap_or("");
        let at = byte_at(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(at);
        self.lines[self.row].push_str(first);
        for piece in pieces {
            self.row += 1;
            self.lines.insert(self.row, piece.to_string());
        }
        // The cursor goes after the inserted text; counting graphemes of the
        // whole prefix keeps a combining mark with the letter before it.
        let end = self.lines[self.row].len();
        self.col = glen(&self.lines[self.row][..end]);
        self.lines[self.row].push_str(&tail);
        self.wanted_col = self.col;
    }

    fn insert_char(&mut self, c: char) {
        self.insert_str(&c.to_string());
    }

    fn newline(&mut self) {
        self.insert_str("\n");
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.remember();
            let line = &mut self.lines[self.row];
            let (from, to) = (byte_at(line, self.col - 1), byte_at(line, self.col));
            line.replace_range(from..to, "");
            self.col -= 1;
        } else if self.row > 0 {
            self.remember();
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = glen(&self.lines[self.row]);
            self.lines[self.row].push_str(&line);
        }
        self.wanted_col = self.col;
    }

    fn delete(&mut self) {
        let len = glen(&self.lines[self.row]);
        if self.col < len {
            self.remember();
            let line = &mut self.lines[self.row];
            let (from, to) = (byte_at(line, self.col), byte_at(line, self.col + 1));
            line.replace_range(from..to, "");
        } else if self.row + 1 < self.lines.len() {
            self.remember();
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn kill_range(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        self.remember();
        let line = &mut self.lines[self.row];
        let (a, b) = (byte_at(line, from), byte_at(line, to));
        self.kill = line[a..b].to_string();
        self.kill_linewise = false;
        line.replace_range(a..b, "");
        self.col = from;
        self.wanted_col = from;
    }

    fn kill_to_end(&mut self) {
        let len = glen(&self.lines[self.row]);
        if self.col < len {
            self.kill_range(self.col, len);
        } else if self.row + 1 < self.lines.len() {
            // At the end of a line, `ctrl+k` joins the next one.
            self.remember();
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
            self.kill = "\n".to_string();
            self.kill_linewise = false;
        }
    }

    fn kill_to_start(&mut self) {
        self.kill_range(0, self.col);
    }

    fn word_start_before(&self) -> usize {
        let g = graphemes(&self.lines[self.row]);
        let mut i = self.col;
        while i > 0 && !is_word(g[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(g[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_end_after(&self) -> usize {
        let g = graphemes(&self.lines[self.row]);
        let mut i = self.col;
        while i < g.len() && !is_word(g[i]) {
            i += 1;
        }
        while i < g.len() && is_word(g[i]) {
            i += 1;
        }
        i
    }

    fn kill_word_back(&mut self) {
        let start = self.word_start_before();
        self.kill_range(start, self.col);
    }

    fn yank(&mut self) {
        if self.kill.is_empty() {
            return;
        }
        let text = self.kill.clone();
        self.insert_str(&text);
    }

    // ----------------------------------------------------------- movement

    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 && !self.in_normal_mode() {
            self.row -= 1;
            self.col = glen(&self.lines[self.row]);
        }
        self.wanted_col = self.col;
    }

    fn right(&mut self) {
        let len = glen(&self.lines[self.row]);
        let limit = if self.in_normal_mode() {
            len.saturating_sub(1)
        } else {
            len
        };
        if self.col < limit {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() && !self.in_normal_mode() {
            self.row += 1;
            self.col = 0;
        }
        self.wanted_col = self.col;
    }

    fn up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.wanted_col.min(glen(&self.lines[self.row]));
        self.clamp();
        true
    }

    fn down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.wanted_col.min(glen(&self.lines[self.row]));
        self.clamp();
        true
    }

    fn home(&mut self) {
        self.col = 0;
        self.wanted_col = 0;
    }

    fn end(&mut self) {
        self.col = glen(&self.lines[self.row]);
        self.clamp();
        self.wanted_col = self.col;
    }

    fn word_left(&mut self) {
        self.col = self.word_start_before();
        self.wanted_col = self.col;
    }

    fn word_right(&mut self) {
        self.col = self.word_end_after();
        self.clamp();
        self.wanted_col = self.col;
    }

    /// Whether `Up` at the first line and `Down` at the last should fall
    /// through to history.
    #[must_use]
    pub fn at_first_line(&self) -> bool {
        self.row == 0
    }

    #[must_use]
    pub fn at_last_line(&self) -> bool {
        self.row + 1 == self.lines.len()
    }

    // -------------------------------------------------------------- paste

    /// A bracketed paste. ANSI is stripped; a large paste is held until
    /// [`Editor::confirm_paste`].
    pub fn paste(&mut self, text: &str) -> Paste {
        let clean = strip_ansi(text);
        let chars = clean.chars().count();
        if chars > PASTE_CONFIRM_CHARS {
            self.held_paste = Some(clean);
            return Paste::Confirm { chars };
        }
        self.insert_str(&clean);
        Paste::Inserted
    }

    /// Answer the `Paste N chars? [Y/n]` question.
    pub fn confirm_paste(&mut self, accept: bool) {
        if let Some(text) = self.held_paste.take() {
            if accept {
                self.insert_str(&text);
            }
        }
    }

    #[must_use]
    pub const fn paste_pending(&self) -> bool {
        self.held_paste.is_some()
    }

    // ------------------------------------------------------- popups' text

    /// The `@fragment` the cursor is in, with the column it starts at.
    #[must_use]
    pub fn mention_query(&self) -> Option<(usize, String)> {
        let g = graphemes(&self.lines[self.row]);
        let mut i = self.col;
        while i > 0 && !g[i - 1].chars().all(char::is_whitespace) {
            i -= 1;
        }
        let word: String = g[i..self.col].concat();
        let rest = word.strip_prefix('@')?;
        Some((i, rest.to_string()))
    }

    /// Replace the `@fragment` with `@replacement ` (§10.2: a chip).
    pub fn complete_mention(&mut self, replacement: &str) {
        let Some((start, _)) = self.mention_query() else {
            return;
        };
        self.remember();
        let line = &mut self.lines[self.row];
        let (a, b) = (byte_at(line, start), byte_at(line, self.col));
        let text = format!("@{replacement} ");
        line.replace_range(a..b, &text);
        self.col = start + glen(&text);
        self.wanted_col = self.col;
    }

    /// The `/command` being typed at the very start of the prompt.
    #[must_use]
    pub fn slash_query(&self) -> Option<String> {
        if self.row != 0 {
            return None;
        }
        let g = graphemes(&self.lines[0]);
        let typed: String = g[..self.col.min(g.len())].concat();
        let rest = typed.strip_prefix('/')?;
        (!rest.contains(char::is_whitespace)).then(|| rest.to_string())
    }

    /// Replace the command being typed with `/name `.
    pub fn complete_slash(&mut self, name: &str) {
        if self.slash_query().is_none() {
            return;
        }
        self.remember();
        let line = &mut self.lines[0];
        let end = byte_at(line, self.col);
        let text = format!("/{} ", name.trim_start_matches('/'));
        line.replace_range(..end, &text);
        self.col = glen(&text);
        self.wanted_col = self.col;
    }

    // --------------------------------------------------------------- keys

    /// Handle one key.
    pub fn handle(&mut self, key: Key) -> Outcome {
        // Submit and newline work the same in every mode.
        match (key.code, key.ctrl, key.alt, key.shift) {
            (Code::Enter, false, false, false) if !self.enter_is_newline => {
                if self.line_command.is_none() {
                    return self.submit();
                }
            }
            (Code::Enter, true, _, _) if self.enter_is_newline => return self.submit(),
            (Code::Enter, _, true, _) | (Code::Enter, _, _, true) => {
                self.newline();
                return Outcome::None;
            }
            (Code::Enter, false, false, false) if self.enter_is_newline => {
                self.newline();
                return Outcome::None;
            }
            _ => {}
        }
        match self.mode {
            EditMode::Emacs => {
                self.emacs(key);
                Outcome::None
            }
            EditMode::Vi => self.vi_key(key),
        }
    }

    /// Submit whatever is typed, whatever key was pressed (a remapped
    /// `submit` binding).
    pub fn submit_now(&mut self) -> Outcome {
        self.submit()
    }

    /// Insert a line break, whatever key was pressed.
    pub fn newline_now(&mut self) {
        self.newline();
    }

    fn submit(&mut self) -> Outcome {
        let text = self.text();
        self.clear();
        Outcome::Submit(text)
    }

    /// Keys shared by emacs mode and vi's insert mode.
    fn emacs(&mut self, key: Key) {
        match (key.code, key.ctrl, key.alt) {
            (Code::Char('a'), true, _) | (Code::Home, false, _) => self.home(),
            (Code::Char('e'), true, _) | (Code::End, false, _) => self.end(),
            (Code::Char('k'), true, _) => self.kill_to_end(),
            (Code::Char('u'), true, _) => self.kill_to_start(),
            (Code::Char('w'), true, _) => self.kill_word_back(),
            (Code::Char('y'), true, _) => self.yank(),
            (Code::Char('b'), false, true) | (Code::Left, true, _) | (Code::Left, _, true) => {
                self.word_left();
            }
            (Code::Char('f'), false, true) | (Code::Right, true, _) | (Code::Right, _, true) => {
                self.word_right();
            }
            (Code::Char('b'), true, _) | (Code::Left, false, false) => self.left(),
            (Code::Char('f'), true, _) | (Code::Right, false, false) => self.right(),
            (Code::Char('d'), true, _) | (Code::Delete, ..) => self.delete(),
            (Code::Char('h'), true, _) | (Code::Backspace, ..) => self.backspace(),
            (Code::Up, ..) => {
                self.up();
            }
            (Code::Down, ..) => {
                self.down();
            }
            (Code::Char('_' | 'z'), true, _) => self.undo(),
            (Code::Tab, false, false) => self.insert_str("\t"),
            _ => {
                if let Some(c) = key.printable() {
                    self.insert_char(c);
                }
            }
        }
    }

    fn undo(&mut self) {
        if let Some(snap) = self.undo.pop() {
            self.lines = snap.lines;
            self.row = snap.row;
            self.col = snap.col;
            self.wanted_col = self.col;
            self.clamp();
        }
    }

    // ---------------------------------------------------------------- vi

    fn vi_key(&mut self, key: Key) -> Outcome {
        if self.vi == Vi::Insert {
            if key.code == Code::Esc {
                self.vi = Vi::Normal;
                self.col = self.col.saturating_sub(1);
                self.clamp();
                self.wanted_col = self.col;
            } else {
                self.emacs(key);
            }
            return Outcome::None;
        }
        // A `/search` or `:command` being typed.
        if let Some((kind, mut typed)) = self.line_command.take() {
            match key.code {
                Code::Esc => {}
                Code::Enter => return self.run_line_command(kind, &typed),
                Code::Backspace => {
                    typed.pop();
                    self.line_command = Some((kind, typed));
                }
                Code::Char(c) if !key.ctrl && !key.alt => {
                    typed.push(c);
                    self.line_command = Some((kind, typed));
                }
                _ => self.line_command = Some((kind, typed)),
            }
            return Outcome::None;
        }
        if key.ctrl {
            if key.code == Code::Char('w') {
                self.kill_word_back();
            } else if key.code == Code::Char('r') {
                // Redo is not kept; the key must not insert anything.
            }
            return Outcome::None;
        }
        let Code::Char(c) = key.code else {
            self.vi_special(key);
            return Outcome::None;
        };
        if let Some(first) = self.pending.take() {
            self.vi_second(first, c);
            return Outcome::None;
        }
        match c {
            'h' => self.left(),
            'l' => self.right(),
            'j' => {
                self.down();
            }
            'k' => {
                self.up();
            }
            '0' => self.home(),
            '$' => self.end(),
            '^' => {
                let g = graphemes(&self.lines[self.row]);
                self.col = g
                    .iter()
                    .position(|x| !x.chars().all(char::is_whitespace))
                    .unwrap_or(0);
                self.wanted_col = self.col;
            }
            'w' => self.word_right_start(),
            'b' => self.word_left(),
            'e' => self.word_end(),
            'i' => self.vi = Vi::Insert,
            'a' => {
                self.vi = Vi::Insert;
                self.col = (self.col + 1).min(glen(&self.lines[self.row]));
            }
            'I' => {
                self.vi = Vi::Insert;
                self.home();
            }
            'A' => {
                self.vi = Vi::Insert;
                self.col = glen(&self.lines[self.row]);
            }
            'o' => {
                self.vi = Vi::Insert;
                self.remember();
                self.row += 1;
                self.lines.insert(self.row, String::new());
                self.col = 0;
            }
            'O' => {
                self.vi = Vi::Insert;
                self.remember();
                self.lines.insert(self.row, String::new());
                self.col = 0;
            }
            'x' => {
                let len = glen(&self.lines[self.row]);
                if self.col < len {
                    self.kill_range(self.col, self.col + 1);
                    self.clamp();
                }
            }
            'D' => {
                let len = glen(&self.lines[self.row]);
                self.kill_range(self.col, len);
                self.clamp();
            }
            'p' => self.vi_put(),
            'u' => self.undo(),
            'd' | 'g' => self.pending = Some(c),
            'G' => {
                self.row = self.lines.len() - 1;
                self.col = self.wanted_col.min(glen(&self.lines[self.row]));
                self.clamp();
            }
            '/' | ':' => self.line_command = Some((c, String::new())),
            'n' => self.search_next(false),
            'N' => self.search_next(true),
            _ => {}
        }
        self.clamp();
        Outcome::None
    }

    fn vi_special(&mut self, key: Key) {
        match key.code {
            Code::Left => self.left(),
            Code::Right => self.right(),
            Code::Up => {
                self.up();
            }
            Code::Down => {
                self.down();
            }
            Code::Home => self.home(),
            Code::End => self.end(),
            Code::Delete => {
                let len = glen(&self.lines[self.row]);
                if self.col < len {
                    self.kill_range(self.col, self.col + 1);
                }
            }
            _ => {}
        }
        self.clamp();
    }

    fn vi_second(&mut self, first: char, second: char) {
        match (first, second) {
            ('d', 'd') => {
                self.remember();
                self.kill = self.lines[self.row].clone();
                self.kill_linewise = true;
                if self.lines.len() == 1 {
                    self.lines[0].clear();
                } else {
                    self.lines.remove(self.row);
                    self.row = self.row.min(self.lines.len() - 1);
                }
                self.col = 0;
                self.wanted_col = 0;
            }
            ('d', 'w') => {
                let end = self.word_end_after();
                // `dw` also eats the space after the word.
                let g = graphemes(&self.lines[self.row]);
                let mut stop = end;
                while stop < g.len() && g[stop].chars().all(char::is_whitespace) {
                    stop += 1;
                }
                self.kill_range(self.col, stop);
            }
            ('d', 'e') => {
                let end = self.word_end_after();
                self.kill_range(self.col, end);
            }
            ('d', '$') => {
                let len = glen(&self.lines[self.row]);
                self.kill_range(self.col, len);
            }
            ('g', 'g') => {
                self.row = 0;
                self.col = self.wanted_col.min(glen(&self.lines[0]));
            }
            _ => {}
        }
        self.clamp();
    }

    fn word_right_start(&mut self) {
        let g = graphemes(&self.lines[self.row]);
        let mut i = self.col;
        while i < g.len() && is_word(g[i]) {
            i += 1;
        }
        while i < g.len() && !is_word(g[i]) {
            i += 1;
        }
        self.col = i;
        self.clamp();
        self.wanted_col = self.col;
    }

    fn word_end(&mut self) {
        let g = graphemes(&self.lines[self.row]);
        let mut i = (self.col + 1).min(g.len());
        while i < g.len() && !is_word(g[i]) {
            i += 1;
        }
        while i < g.len() && is_word(g[i]) {
            i += 1;
        }
        self.col = i.saturating_sub(1);
        self.clamp();
        self.wanted_col = self.col;
    }

    fn vi_put(&mut self) {
        if self.kill.is_empty() {
            return;
        }
        self.remember();
        if self.kill_linewise {
            self.row += 1;
            self.lines.insert(self.row, self.kill.clone());
            self.col = 0;
        } else {
            let line = &mut self.lines[self.row];
            let at = byte_at(line, (self.col + 1).min(glen(line)));
            line.insert_str(at, &self.kill);
            self.col =
                (self.col + glen(&self.kill)).min(glen(&self.lines[self.row]).saturating_sub(1));
        }
        self.wanted_col = self.col;
    }

    fn run_line_command(&mut self, kind: char, typed: &str) -> Outcome {
        if kind == ':' {
            return if matches!(typed.trim(), "q" | "q!" | "quit" | "qa" | "qa!") {
                Outcome::Quit
            } else {
                Outcome::None
            };
        }
        if !typed.is_empty() {
            self.last_search = Some(typed.to_string());
            self.search_next(false);
        }
        Outcome::None
    }

    /// Next (or previous) occurrence of the last `/search`, wrapping.
    fn search_next(&mut self, backwards: bool) {
        let Some(pattern) = self.last_search.clone() else {
            return;
        };
        // Every match as (row, col).
        let mut hits: Vec<(usize, usize)> = Vec::new();
        for (r, line) in self.lines.iter().enumerate() {
            let mut from = 0;
            while let Some(found) = line[from..].find(&pattern) {
                let byte = from + found;
                hits.push((r, line[..byte].graphemes(true).count()));
                from = byte + pattern.len().max(1);
                if from >= line.len() {
                    break;
                }
            }
        }
        if hits.is_empty() {
            return;
        }
        let here = (self.row, self.col);
        let target = if backwards {
            hits.iter().rev().find(|h| **h < here).or(hits.last())
        } else {
            hits.iter().find(|h| **h > here).or(hits.first())
        };
        if let Some(&(r, c)) = target {
            self.row = r;
            self.col = c;
            self.wanted_col = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            if c == '\n' {
                editor.handle(Key::new(Code::Enter).with_shift());
            } else {
                editor.handle(Key::char(c));
            }
        }
    }

    fn emacs(text: &str) -> Editor {
        let mut e = Editor::new(EditMode::Emacs);
        typed(&mut e, text);
        e
    }

    fn vi(text: &str) -> Editor {
        let mut e = Editor::new(EditMode::Vi);
        typed(&mut e, text);
        e
    }

    fn keys(e: &mut Editor, seq: &str) {
        for c in seq.chars() {
            e.handle(Key::char(c));
        }
    }

    #[test]
    fn typing_and_enter_submit_with_shift_enter_for_a_newline() {
        let mut e = emacs("hello");
        e.handle(Key::new(Code::Enter).with_shift());
        typed(&mut e, "world");
        e.handle(Key::new(Code::Enter).with_alt());
        assert_eq!(e.text(), "hello\nworld\n");
        assert_eq!(e.newline_count(), 2);
        assert_eq!(
            e.handle(Key::new(Code::Enter)),
            Outcome::Submit("hello\nworld\n".into())
        );
        assert!(e.is_empty(), "submitting clears the prompt");
    }

    #[test]
    fn ctrl_enter_mode_makes_enter_a_newline() {
        let mut e = Editor::new(EditMode::Emacs);
        e.enter_is_newline = true;
        typed(&mut e, "a");
        assert_eq!(e.handle(Key::new(Code::Enter)), Outcome::None);
        typed(&mut e, "b");
        assert_eq!(e.text(), "a\nb");
        assert_eq!(
            e.handle(Key::new(Code::Enter).with_ctrl()),
            Outcome::Submit("a\nb".into())
        );
    }

    /// T-TUI-025.
    #[test]
    fn emacs_kill_and_yank_round_trip() {
        let mut e = emacs("hello brave new world");
        e.handle(Key::ctrl('a'));
        for _ in 0..6 {
            e.handle(Key::ctrl('f'));
        }
        e.handle(Key::ctrl('k'));
        assert_eq!(e.text(), "hello ");
        assert_eq!(e.kill_buffer(), "brave new world");
        e.handle(Key::ctrl('a'));
        e.handle(Key::ctrl('y'));
        assert_eq!(e.text(), "brave new worldhello ");
        // Kill to start, then yank elsewhere.
        e.handle(Key::ctrl('e'));
        e.handle(Key::ctrl('u'));
        assert_eq!(e.text(), "");
        assert_eq!(e.kill_buffer(), "brave new worldhello ");
        e.handle(Key::ctrl('y'));
        assert_eq!(e.text(), "brave new worldhello ");
    }

    #[test]
    fn ctrl_w_deletes_a_word_and_alt_b_f_move_by_word() {
        let mut e = emacs("one two  three");
        e.handle(Key::ctrl('w'));
        assert_eq!(e.text(), "one two  ");
        e.handle(Key::ctrl('w'));
        assert_eq!(e.text(), "one ");
        let mut e = emacs("alpha beta gamma");
        e.handle(Key::alt('b'));
        assert_eq!(e.cursor(), (0, 11));
        e.handle(Key::alt('b'));
        assert_eq!(e.cursor(), (0, 6));
        e.handle(Key::alt('f'));
        assert_eq!(e.cursor(), (0, 10));
        e.handle(Key::new(Code::Left).with_ctrl());
        assert_eq!(e.cursor(), (0, 6));
    }

    #[test]
    fn ctrl_k_at_the_end_of_a_line_joins_the_next() {
        let mut e = emacs("one\ntwo");
        e.handle(Key::new(Code::Up));
        e.handle(Key::ctrl('e'));
        e.handle(Key::ctrl('k'));
        assert_eq!(e.text(), "onetwo");
    }

    #[test]
    fn movement_is_by_grapheme_not_by_byte_or_char() {
        // A family emoji, an accented letter written as two chars, a flag.
        let mut e = emacs("a👨‍👩‍👧e\u{301}🇸🇪b");
        assert_eq!(e.cursor().1, 5);
        e.handle(Key::new(Code::Left));
        e.handle(Key::new(Code::Left));
        assert_eq!(e.cursor().1, 3);
        e.handle(Key::new(Code::Backspace));
        assert_eq!(e.text(), "a👨‍👩‍👧🇸🇪b", "the whole accented letter went");
        e.handle(Key::new(Code::Backspace));
        assert_eq!(e.text(), "a🇸🇪b", "and the whole family");
    }

    #[test]
    fn backspace_at_the_start_joins_lines_and_delete_at_the_end_pulls_one_up() {
        let mut e = emacs("ab\ncd");
        e.handle(Key::ctrl('a'));
        e.handle(Key::new(Code::Backspace));
        assert_eq!(e.text(), "abcd");
        assert_eq!(e.cursor(), (0, 2));
        let mut e = emacs("ab\ncd");
        e.handle(Key::new(Code::Up));
        e.handle(Key::ctrl('e'));
        e.handle(Key::new(Code::Delete));
        assert_eq!(e.text(), "abcd");
    }

    #[test]
    fn up_and_down_remember_the_column() {
        let mut e = emacs("long line here\nx\nanother long one");
        // On the last line, column 12.
        e.handle(Key::ctrl('a'));
        for _ in 0..12 {
            e.handle(Key::ctrl('f'));
        }
        e.handle(Key::new(Code::Up));
        assert_eq!(e.cursor(), (1, 1));
        e.handle(Key::new(Code::Up));
        assert_eq!(e.cursor(), (0, 12));
        assert!(e.at_first_line());
    }

    #[test]
    fn undo_steps_back_through_edits() {
        let mut e = emacs("abc");
        e.handle(Key::ctrl('w'));
        assert_eq!(e.text(), "");
        e.handle(Key::ctrl('_'));
        assert_eq!(e.text(), "abc");
    }

    /// T-TUI-022.
    #[test]
    fn a_big_paste_asks_first_and_a_small_one_does_not() {
        let mut e = Editor::default();
        assert_eq!(e.paste("short\nmulti"), Paste::Inserted);
        assert_eq!(e.text(), "short\nmulti");
        assert_eq!(e.newline_count(), 1);
        e.clear();
        let big = "x".repeat(12_412);
        assert_eq!(e.paste(&big), Paste::Confirm { chars: 12_412 });
        assert!(e.paste_pending());
        assert!(e.is_empty(), "nothing lands before the answer");
        e.confirm_paste(false);
        assert!(e.is_empty() && !e.paste_pending());
        e.paste(&big);
        e.confirm_paste(true);
        assert_eq!(e.text().len(), 12_412);
        // Exactly the limit goes straight in.
        e.clear();
        assert_eq!(e.paste(&"y".repeat(5_000)), Paste::Inserted);
    }

    #[test]
    fn pasted_escape_sequences_and_carriage_returns_are_cleaned() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m text"), "red text");
        assert_eq!(strip_ansi("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}ok"), "ok");
        assert_eq!(strip_ansi("bell\u{7}\u{0}tab\tkept"), "belltab\tkept");
        let mut e = Editor::default();
        e.paste("\u{1b}[1mbold\u{1b}[0m");
        assert_eq!(e.text(), "bold");
    }

    #[test]
    fn a_mention_fragment_is_found_and_completed() {
        let mut e = emacs("look at @src/par");
        assert_eq!(e.mention_query(), Some((8, "src/par".into())));
        e.complete_mention("src/parser.rs");
        assert_eq!(e.text(), "look at @src/parser.rs ");
        assert_eq!(e.mention_query(), None, "the space ended it");
        let e = emacs("an email a@b.c");
        assert_eq!(
            e.mention_query(),
            None,
            "an @ inside a word is not a mention"
        );
    }

    #[test]
    fn a_slash_command_is_recognised_only_at_the_start() {
        let mut e = emacs("/co");
        assert_eq!(e.slash_query(), Some("co".into()));
        e.complete_slash("compact");
        assert_eq!(e.text(), "/compact ");
        assert_eq!(e.slash_query(), None);
        assert_eq!(emacs("not /co").slash_query(), None);
        assert_eq!(
            emacs("/model gpt").slash_query(),
            None,
            "arguments end the popup"
        );
    }

    // ----------------------------------------------------------------- vi

    /// T-TUI-024: `Esc`, `dd`, `0`, `C-w`, `i` on a three-line buffer.
    #[test]
    fn vi_esc_dd_zero_ctrl_w_i_on_three_lines() {
        let mut e = vi("one\ntwo words\nthree");
        assert_eq!(e.mode_label(), "[V]");
        e.handle(Key::new(Code::Esc));
        assert_eq!(e.mode_label(), "[N]");
        assert_eq!(e.cursor().0, 2);
        e.handle(Key::new(Code::Up));
        keys(&mut e, "dd");
        assert_eq!(e.text(), "one\nthree");
        assert_eq!(e.kill_buffer(), "two words");
        keys(&mut e, "0");
        assert_eq!(e.cursor(), (1, 0));
        e.handle(Key::ctrl('w'));
        keys(&mut e, "i");
        assert_eq!(e.mode_label(), "[V]");
        keys(&mut e, "X");
        assert_eq!(e.text(), "one\nXthree");
    }

    #[test]
    fn vi_motions_and_edits() {
        let mut e = vi("alpha beta gamma");
        e.handle(Key::new(Code::Esc));
        keys(&mut e, "0w");
        assert_eq!(e.cursor(), (0, 6));
        keys(&mut e, "e");
        assert_eq!(e.cursor(), (0, 9));
        keys(&mut e, "b");
        assert_eq!(e.cursor(), (0, 6));
        keys(&mut e, "dw");
        assert_eq!(e.text(), "alpha gamma");
        keys(&mut e, "$");
        assert_eq!(e.cursor(), (0, 10));
        keys(&mut e, "x");
        assert_eq!(e.text(), "alpha gamm");
        keys(&mut e, "p");
        assert_eq!(e.text(), "alpha gamma");
    }

    #[test]
    fn vi_dd_then_p_puts_the_line_back_below() {
        let mut e = vi("a\nb\nc");
        e.handle(Key::new(Code::Esc));
        keys(&mut e, "ggdd");
        assert_eq!(e.text(), "b\nc");
        keys(&mut e, "p");
        assert_eq!(e.text(), "b\na\nc");
        keys(&mut e, "u");
        assert_eq!(e.text(), "b\nc");
        keys(&mut e, "G");
        assert_eq!(e.cursor().0, 1);
        keys(&mut e, "gg");
        assert_eq!(e.cursor().0, 0);
    }

    #[test]
    fn vi_open_line_commands_and_append() {
        let mut e = vi("a\nc");
        e.handle(Key::new(Code::Esc));
        keys(&mut e, "ggob");
        e.handle(Key::new(Code::Esc));
        assert_eq!(e.text(), "a\nb\nc");
        keys(&mut e, "ggOz");
        e.handle(Key::new(Code::Esc));
        assert_eq!(e.text(), "z\na\nb\nc");
        keys(&mut e, "A!");
        assert_eq!(e.text(), "z!\na\nb\nc");
        e.handle(Key::new(Code::Esc));
        keys(&mut e, "Ix");
        assert_eq!(e.text(), "xz!\na\nb\nc");
    }

    #[test]
    fn vi_search_next_and_previous_wrap() {
        let mut e = vi("foo bar\nbar baz\nbar");
        e.handle(Key::new(Code::Esc));
        keys(&mut e, "gg0/bar");
        e.handle(Key::new(Code::Enter));
        assert_eq!(e.cursor(), (0, 4));
        keys(&mut e, "n");
        assert_eq!(e.cursor(), (1, 0));
        keys(&mut e, "n");
        assert_eq!(e.cursor(), (2, 0));
        keys(&mut e, "n");
        assert_eq!(e.cursor(), (0, 4), "wraps");
        keys(&mut e, "N");
        assert_eq!(e.cursor(), (2, 0));
    }

    #[test]
    fn vi_colon_q_bang_quits_and_enter_in_normal_mode_still_submits() {
        let mut e = vi("text");
        e.handle(Key::new(Code::Esc));
        keys(&mut e, ":q!");
        assert_eq!(e.handle(Key::new(Code::Enter)), Outcome::Quit);
        let mut e = vi("send me");
        e.handle(Key::new(Code::Esc));
        assert_eq!(
            e.handle(Key::new(Code::Enter)),
            Outcome::Submit("send me".into())
        );
    }

    #[test]
    fn vi_normal_mode_keeps_the_cursor_on_a_character() {
        let mut e = vi("abc");
        e.handle(Key::new(Code::Esc));
        assert_eq!(e.cursor().1, 2);
        keys(&mut e, "l");
        assert_eq!(e.cursor().1, 2);
        keys(&mut e, "$");
        assert_eq!(e.cursor().1, 2);
        keys(&mut e, "a");
        assert_eq!(e.cursor().1, 3, "append goes past the end");
    }
}
