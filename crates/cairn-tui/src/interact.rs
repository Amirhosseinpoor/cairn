//! Keys and pastes in, state changes and [`Command`]s out (SPEC §10.2, §10.4).
//!
//! Everything the interface can decide for itself — moving the cursor,
//! scrolling, opening an overlay, filtering a popup — it does here. What
//! needs the outside world (sending a prompt, answering an approval, running
//! a slash command, cancelling a turn) comes back as a [`Command`].

use crate::app::{App, Item, NoticeKind};
use crate::editor::{Outcome, Paste};
use crate::fuzzy;
use crate::keymap::{Action, Chords, Context, Keymap, Resolved};
use crate::keys::{Code, Key};
use crate::overlay::{ApprovalAnswer, Confirm, HistorySearch, HunkStatus, Overlay, PlanAction};
use crate::slash;

/// What the controller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A prompt for the model (already in the transcript).
    Submit(String),
    /// A slash command that is known and available now.
    Slash {
        name: &'static str,
        args: String,
    },
    /// `Esc` or `Ctrl+C` during a turn.
    Cancel,
    /// A second `Esc` within 800 ms.
    ForceCancel,
    CycleMode,
    /// Leave. `kill_jobs` is the answer to the running-jobs question.
    Quit {
        kill_jobs: bool,
    },
    Approval {
        request_id: String,
        answer: ApprovalAnswer,
    },
    /// A hunk was accepted or rejected in the diff viewer.
    Hunk {
        file: usize,
        hunk: usize,
        status: HunkStatus,
    },
    /// Accept every pending hunk of every file.
    AcceptAllHunks,
    Plan(PlanAction),
    PasteImage,
    QuickOpen,
    /// The transcript view was cleared (the session stays on disk).
    ClearView,
    /// The `@fragment` changed: ask the index for candidates.
    MentionQuery(String),
}

/// The popup above the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind {
    Slash,
    Mention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub kind: PopupKind,
    pub items: Vec<String>,
    pub selected: usize,
}

/// §10.4: two `Esc`s this close together force the cancel.
const ESC_FORCE_MS: u64 = 800;
/// A tick is 80 ms.
const TICK_MS: u64 = 80;

impl App {
    /// The popup the prompt's text calls for, if any.
    #[must_use]
    pub fn popup(&self) -> Option<Popup> {
        if self.popup_hidden || self.overlay.is_open() {
            return None;
        }
        if let Some(query) = self.editor.slash_query() {
            let mode_bit = match self.mode {
                cairn_core::Mode::Plan => slash::MODE_PLAN,
                cairn_core::Mode::Build => slash::MODE_BUILD,
                cairn_core::Mode::Auto => slash::MODE_AUTO,
                cairn_core::Mode::AutoUnsafe => slash::MODE_UNSAFE,
            };
            let names: Vec<&str> = slash::SLASH_COMMANDS
                .iter()
                .filter(|c| c.available_in(mode_bit))
                .map(|c| c.name)
                .collect();
            let items: Vec<String> = fuzzy::rank_short_first(&query, names, 10)
                .into_iter()
                .map(str::to_string)
                .collect();
            return (!items.is_empty()).then(|| Popup {
                kind: PopupKind::Slash,
                selected: self.popup_sel.min(items.len() - 1),
                items,
            });
        }
        let (_, query) = self.editor.mention_query()?;
        let items: Vec<String> = fuzzy::rank_short_first(
            &query,
            self.mention_candidates.iter().map(String::as_str),
            10,
        )
        .into_iter()
        .map(str::to_string)
        .collect();
        (!items.is_empty()).then(|| Popup {
            kind: PopupKind::Mention,
            selected: self.popup_sel.min(items.len() - 1),
            items,
        })
    }

    fn context(&self) -> Context {
        match &self.overlay {
            Overlay::None => Context::Input,
            Overlay::Approval(_) => Context::Approval,
            Overlay::Diff(_) => Context::Diff,
            Overlay::Confirm(_) => Context::Prompt,
            Overlay::Plan(_) | Overlay::Help | Overlay::History(_) => Context::Overlay,
        }
    }

    fn now_ms(&self) -> u64 {
        self.tick * TICK_MS
    }

    /// A bracketed paste.
    pub fn on_paste(&mut self, text: &str) {
        match self.editor.paste(text) {
            Paste::Inserted => self.after_edit(),
            Paste::Confirm { chars } => self.overlay = Overlay::Confirm(Confirm::Paste { chars }),
        }
    }

    /// The prompt's text changed: forget the history walk and the popup.
    fn after_edit(&mut self) {
        self.history.reset();
        self.popup_hidden = false;
    }

    /// One key.
    pub fn on_key(&mut self, key: Key, keymap: &Keymap, chords: &mut Chords) -> Vec<Command> {
        let ctx = self.context();
        if self.overlay.is_open() {
            return self.overlay_key(key, keymap, chords, ctx);
        }
        let before = self.editor.text();
        let mut out = Vec::new();
        let popup = self.popup();
        let mut resolved = keymap.resolve(ctx, key, chords, self.now_ms());
        if resolved == Resolved::None {
            // Scrolling keys work from the prompt too.
            resolved = keymap.resolve(Context::Transcript, key, chords, self.now_ms());
        }
        match resolved {
            Resolved::Pending => return out,
            Resolved::Action(action) => {
                if self.input_action(action, key, popup.as_ref(), &mut out) {
                    // Walking history changes the text without ending the walk.
                    let walking = matches!(action, Action::HistoryPrev | Action::HistoryNext);
                    self.finish_input(&before, !walking, &mut out);
                    return out;
                }
            }
            Resolved::None => {}
        }
        // Popup navigation takes the arrows before anything else sees them.
        if let Some(p) = &popup {
            match key.code {
                Code::Up if !key.ctrl && !key.alt => {
                    self.popup_sel = p.selected.saturating_sub(1);
                    return out;
                }
                Code::Down if !key.ctrl && !key.alt => {
                    self.popup_sel = (p.selected + 1).min(p.items.len() - 1);
                    return out;
                }
                _ => {}
            }
        }
        // Everything else is typing.
        let result = self.editor.handle(key);
        self.finish_input(&before, true, &mut out);
        match result {
            Outcome::Submit(text) => out.extend(self.process_submit(&text)),
            Outcome::Quit => out.push(Command::Quit { kill_jobs: false }),
            Outcome::None => {}
        }
        out
    }

    fn finish_input(&mut self, before: &str, reset: bool, out: &mut Vec<Command>) {
        if self.editor.text() != before {
            if reset {
                self.after_edit();
            }
            self.popup_sel = 0;
            let query = self.editor.mention_query().map(|(_, q)| q);
            if query != self.last_mention {
                self.last_mention.clone_from(&query);
                if let Some(q) = query {
                    out.push(Command::MentionQuery(q));
                }
            }
        }
    }

    /// Returns `true` when the action consumed the key.
    #[allow(clippy::too_many_lines, reason = "one arm per action")]
    fn input_action(
        &mut self,
        action: Action,
        key: Key,
        popup: Option<&Popup>,
        out: &mut Vec<Command>,
    ) -> bool {
        match action {
            Action::Cancel => {
                // `Esc` belongs to the editor (vi mode) unless a turn runs or
                // a popup is open.
                if self.running.is_some() {
                    let now = self.tick;
                    let double = self
                        .last_esc_tick
                        .is_some_and(|t| now.saturating_sub(t) * TICK_MS <= ESC_FORCE_MS);
                    self.last_esc_tick = Some(now);
                    out.push(if double {
                        Command::ForceCancel
                    } else {
                        Command::Cancel
                    });
                    true
                } else if popup.is_some() {
                    self.popup_hidden = true;
                    true
                } else {
                    false
                }
            }
            Action::CopyOrQuit => {
                if self.running.is_some() {
                    out.push(Command::Cancel);
                } else {
                    self.ask_quit();
                }
                true
            }
            Action::QuitIfEmpty => {
                if self.editor.is_empty() {
                    out.push(Command::Quit { kill_jobs: false });
                    true
                } else {
                    false
                }
            }
            Action::QuitConfirm => {
                self.ask_quit();
                true
            }
            Action::CycleMode => {
                out.push(Command::CycleMode);
                true
            }
            Action::ClearView => {
                self.transcript.clear();
                self.scroll = 0;
                out.push(Command::ClearView);
                true
            }
            Action::HistorySearch => {
                self.overlay = Overlay::History(HistorySearch::default());
                true
            }
            Action::ExpandTool => {
                for item in self.transcript.iter_mut().rev() {
                    match item {
                        Item::Tool(card) => {
                            card.expanded = !card.expanded;
                            break;
                        }
                        Item::Reasoning { expanded, .. } => {
                            *expanded = !*expanded;
                            break;
                        }
                        _ => {}
                    }
                }
                true
            }
            Action::ToggleTodos => {
                self.show_todos = !self.show_todos;
                true
            }
            Action::QuickOpen => {
                out.push(Command::QuickOpen);
                true
            }
            Action::ToggleHelp => {
                self.overlay = Overlay::Help;
                true
            }
            Action::PasteImage => {
                out.push(Command::PasteImage);
                true
            }
            Action::PageUp => {
                self.scroll = self.scroll.saturating_add(self.page_lines.max(1));
                true
            }
            Action::PageDown => {
                self.scroll = self.scroll.saturating_sub(self.page_lines.max(1));
                true
            }
            Action::JumpTop => {
                self.scroll = usize::MAX / 2;
                true
            }
            Action::JumpBottom => {
                self.scroll = 0;
                true
            }
            Action::HistoryPrev => {
                if popup.is_some() || !self.editor.at_first_line() {
                    return false;
                }
                let current = self.editor.text();
                if let Some(entry) = self.history.previous(&current).map(str::to_string) {
                    self.editor.set_text(&entry);
                }
                true
            }
            Action::HistoryNext => {
                if popup.is_some() || !self.editor.at_last_line() {
                    return false;
                }
                if let Some(entry) = self.history.following() {
                    self.editor.set_text(&entry);
                }
                true
            }
            Action::Complete => {
                if let Some(p) = popup {
                    if let Some(choice) = p.items.get(p.selected).cloned() {
                        match p.kind {
                            PopupKind::Slash => self.editor.complete_slash(&choice),
                            PopupKind::Mention => self.editor.complete_mention(&choice),
                        }
                        self.popup_sel = 0;
                        return true;
                    }
                }
                false
            }
            Action::Submit => {
                let outcome = self.editor.submit_now();
                if let Outcome::Submit(text) = outcome {
                    out.extend(self.process_submit(&text));
                }
                true
            }
            Action::Newline => {
                self.editor.newline_now();
                true
            }
            Action::KillLine => {
                self.editor.handle(Key::ctrl('u'));
                true
            }
            Action::KillToEnd => {
                self.editor.handle(Key::ctrl('k'));
                true
            }
            Action::KillWord => {
                self.editor.handle(Key::ctrl('w'));
                true
            }
            Action::Yank => {
                self.editor.handle(Key::ctrl('y'));
                true
            }
            _ => {
                let _ = key;
                false
            }
        }
    }

    fn ask_quit(&mut self) {
        self.overlay = if self.jobs > 0 {
            Overlay::Confirm(Confirm::QuitWithJobs {
                names: self.job_names.clone(),
            })
        } else {
            Overlay::Confirm(Confirm::Quit)
        };
    }

    /// A line was submitted: record it, then route it.
    fn process_submit(&mut self, text: &str) -> Vec<Command> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        self.history.push(trimmed);
        self.history.reset();
        if slash::is_slash(trimmed) {
            let word = trimmed.split_whitespace().next().unwrap_or(trimmed);
            let args = trimmed[word.len()..].trim().to_string();
            let Some(command) = slash::lookup(word) else {
                // REQ-TUI-004: said inline, never sent to the model.
                self.transcript.push(Item::Notice(
                    NoticeKind::Warning,
                    slash::unknown_message(word),
                ));
                return Vec::new();
            };
            let bit = match self.mode {
                cairn_core::Mode::Plan => slash::MODE_PLAN,
                cairn_core::Mode::Build => slash::MODE_BUILD,
                cairn_core::Mode::Auto => slash::MODE_AUTO,
                cairn_core::Mode::AutoUnsafe => slash::MODE_UNSAFE,
            };
            if command.available_in(bit) {
                return vec![Command::Slash {
                    name: command.name,
                    args,
                }];
            }
            self.transcript.push(Item::Notice(
                NoticeKind::Warning,
                format!(
                    "/{} is not available in {} mode.",
                    command.name,
                    self.mode.as_str()
                ),
            ));
            return Vec::new();
        }
        self.submit(trimmed);
        vec![Command::Submit(trimmed.to_string())]
    }

    // ---------------------------------------------------------- overlays

    fn overlay_key(
        &mut self,
        key: Key,
        keymap: &Keymap,
        chords: &mut Chords,
        ctx: Context,
    ) -> Vec<Command> {
        let resolved = keymap.resolve(ctx, key, chords, self.now_ms());
        let action = if let Resolved::Action(a) = resolved {
            Some(a)
        } else {
            None
        };
        match self.overlay.clone() {
            Overlay::Approval(mut a) => {
                let answer = match (action, key.code) {
                    (Some(Action::Allow), _) => Some(ApprovalAnswer::Once),
                    (Some(Action::AllowAlways), _) | (_, Code::Char('A')) => {
                        Some(ApprovalAnswer::Always)
                    }
                    (Some(Action::Deny), _) | (_, Code::Esc) => Some(ApprovalAnswer::Deny),
                    (Some(Action::EditRequest), _) => Some(ApprovalAnswer::Edit),
                    (_, Code::Enter) => Some(a.focused()),
                    (_, Code::Tab | Code::Right) => {
                        a.cycle(1);
                        None
                    }
                    (_, Code::BackTab | Code::Left) => {
                        a.cycle(-1);
                        None
                    }
                    _ => None,
                };
                if let Some(answer) = answer {
                    self.overlay = Overlay::None;
                    vec![Command::Approval {
                        request_id: a.request_id,
                        answer,
                    }]
                } else {
                    self.overlay = Overlay::Approval(a);
                    Vec::new()
                }
            }
            Overlay::Diff(mut v) => {
                let mut out = Vec::new();
                match (action, key.code) {
                    (Some(Action::AcceptHunk | Action::RejectHunk), _) => {
                        let status = if action == Some(Action::AcceptHunk) {
                            HunkStatus::Accepted
                        } else {
                            HunkStatus::Rejected
                        };
                        v.set_status(status);
                        out.push(Command::Hunk {
                            file: v.file,
                            hunk: v.hunk,
                            status,
                        });
                        v.next_hunk();
                    }
                    (Some(Action::PrevHunk), _) => v.prev_hunk(),
                    (Some(Action::NextHunk), _) => v.next_hunk(),
                    (Some(Action::NextFile), _) => v.next_file(),
                    (Some(Action::PrevFile), _) => v.prev_file(),
                    (_, Code::Char('a')) => {
                        for file in &mut v.files {
                            for h in &mut file.hunks {
                                if h.status == HunkStatus::Pending {
                                    h.status = HunkStatus::Accepted;
                                }
                            }
                        }
                        out.push(Command::AcceptAllHunks);
                    }
                    (_, Code::Esc) | (Some(Action::CloseOverlay), _) => {
                        self.overlay = Overlay::None;
                        return out;
                    }
                    _ => {}
                }
                self.overlay = Overlay::Diff(v);
                out
            }
            Overlay::Plan(mut p) => {
                let mut out = Vec::new();
                match key.code {
                    Code::Enter => {
                        self.overlay = Overlay::None;
                        return vec![Command::Plan(PlanAction::Approve)];
                    }
                    Code::Char('e') => {
                        self.overlay = Overlay::None;
                        return vec![Command::Plan(PlanAction::Edit)];
                    }
                    Code::Char('s') => {
                        self.overlay = Overlay::None;
                        return vec![Command::Plan(PlanAction::SaveOnly)];
                    }
                    Code::Esc => {
                        self.overlay = Overlay::None;
                        return vec![Command::Plan(PlanAction::Dismiss)];
                    }
                    Code::Char('j') | Code::Down => {
                        p.selected = (p.selected + 1).min(p.steps.len().saturating_sub(1));
                    }
                    Code::Char('k') | Code::Up => p.selected = p.selected.saturating_sub(1),
                    Code::Char('d') => p.show_detail = !p.show_detail,
                    _ => {}
                }
                self.overlay = Overlay::Plan(p);
                out.shrink_to_fit();
                out
            }
            Overlay::Help => {
                if matches!(action, Some(Action::CloseOverlay | Action::ToggleHelp))
                    || key.code == Code::Esc
                {
                    self.overlay = Overlay::None;
                }
                Vec::new()
            }
            Overlay::History(mut h) => {
                match key.code {
                    Code::Esc => {
                        // Once clears what was typed, twice leaves.
                        if h.query.is_empty() {
                            self.overlay = Overlay::None;
                            return Vec::new();
                        }
                        h.query.clear();
                        h.selected = 0;
                    }
                    Code::Enter => {
                        let hits = self.history.search(&h.query, 10);
                        if let Some(choice) = hits.get(h.selected).map(|s| (*s).to_string()) {
                            self.editor.set_text(&choice);
                        }
                        self.overlay = Overlay::None;
                        return Vec::new();
                    }
                    Code::Backspace => {
                        h.query.pop();
                        h.selected = 0;
                    }
                    Code::Up => h.selected = h.selected.saturating_sub(1),
                    Code::Down => h.selected += 1,
                    Code::Char('r') if key.ctrl => h.selected += 1,
                    Code::Char(c) if !key.ctrl && !key.alt => {
                        h.query.push(c);
                        h.selected = 0;
                    }
                    _ => {}
                }
                let max = self.history.search(&h.query, 10).len().saturating_sub(1);
                h.selected = h.selected.min(max);
                self.overlay = Overlay::History(h);
                Vec::new()
            }
            Overlay::Confirm(c) => self.confirm_key(&c, key),
            Overlay::None => Vec::new(),
        }
    }

    fn confirm_key(&mut self, confirm: &Confirm, key: Key) -> Vec<Command> {
        let yes = matches!(key.code, Code::Char('y' | 'Y'));
        let no = matches!(key.code, Code::Char('n' | 'N') | Code::Esc);
        match confirm {
            Confirm::Paste { .. } => {
                // `[Y/n]`: Enter means yes.
                if yes || key.code == Code::Enter {
                    self.editor.confirm_paste(true);
                    self.overlay = Overlay::None;
                    self.after_edit();
                } else if no {
                    self.editor.confirm_paste(false);
                    self.overlay = Overlay::None;
                }
                Vec::new()
            }
            Confirm::Quit => {
                // `[y/N]`: Enter means no.
                if yes {
                    self.overlay = Overlay::None;
                    return vec![Command::Quit { kill_jobs: false }];
                }
                if no || key.code == Code::Enter {
                    self.overlay = Overlay::None;
                }
                Vec::new()
            }
            Confirm::QuitWithJobs { .. } => match key.code {
                Code::Char('k') => {
                    self.overlay = Overlay::None;
                    vec![Command::Quit { kill_jobs: true }]
                }
                // "Continue in background" and "quit anyway" both leave.
                Code::Char('c' | 'q') => {
                    self.overlay = Overlay::None;
                    vec![Command::Quit { kill_jobs: false }]
                }
                Code::Esc => {
                    self.overlay = Overlay::None;
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }
}
