//! What the interface knows (SPEC §10.1): the transcript, the status bar's
//! numbers, the prompt, and whichever overlay is open.
//!
//! The agent talks to it in [`EventData`]; [`App::apply`] turns those into
//! transcript items. Nothing here draws.

use std::collections::VecDeque;

use cairn_core::event::{EventData, ToolStatus, TurnStatus};
use cairn_core::Mode;

use crate::editor::{EditMode, Editor};
use crate::history::History;
use crate::overlay::{Approval, Overlay};
use crate::theme::{ColorSupport, Glyphs, Theme};

/// `ui.show_reasoning` (§10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowReasoning {
    Always,
    Collapsed,
    Never,
}

/// `ui.diff_layout` (§10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLayout {
    Auto,
    Inline,
    Side,
}

/// How the interface looks and moves.
#[derive(Debug, Clone)]
pub struct Look {
    pub theme: Theme,
    pub support: ColorSupport,
    pub glyphs: Glyphs,
    /// Spinners and progress bars run.
    pub animation: bool,
    /// Announce what happens in words, drop box drawing (§10.8).
    pub screen_reader: bool,
    pub show_reasoning: ShowReasoning,
    pub diff_layout: DiffLayout,
    pub diff_side_by_side_min_width: u16,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            theme: Theme::cairn_dark(),
            support: ColorSupport::True,
            glyphs: Glyphs::unicode(),
            animation: true,
            screen_reader: false,
            show_reasoning: ShowReasoning::Collapsed,
            diff_layout: DiffLayout::Auto,
            diff_side_by_side_min_width: 100,
        }
    }
}

/// A tool call as the transcript shows it (§10.5, "Tool-call cards").
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    /// `src/parser.rs`, `cargo test --quiet` — what it is about.
    pub summary: String,
    pub input: serde_json::Value,
    pub state: ToolState,
    /// The newest lines of output while it runs (five are shown).
    pub tail: VecDeque<String>,
    pub bytes: u32,
    pub output: Option<String>,
    pub rule: Option<String>,
    pub expanded: bool,
    /// For edits: the changed lines, `-`/`+`/` ` prefixed, with the first
    /// line number.
    pub diff: Option<(u32, Vec<String>)>,
    pub started_tick: u64,
}

/// Where a tool call is.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolState {
    Running,
    Done {
        duration_ms: u64,
    },
    Failed {
        duration_ms: u64,
        code: String,
        recovery: Option<String>,
    },
    Denied {
        code: String,
        reason: String,
    },
}

/// What kind of one-line notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Warning,
}

/// A failure the transcript shows with its code and a way forward (§10.9).
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorCard {
    /// `Provider error`, `Permission denied`, …
    pub title: String,
    pub code: String,
    /// `HTTP 429`
    pub extra: Option<String>,
    pub message: String,
    pub detail: Option<String>,
    /// `[r]etry now  [m]odel  [v]iew log  [Esc] dismiss`
    pub actions: Option<String>,
}

/// One thing in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User(String),
    Assistant {
        text: String,
        streaming: bool,
    },
    Reasoning {
        text: String,
        tokens: u32,
        expanded: bool,
    },
    Tool(ToolCard),
    Notice(NoticeKind, String),
    Error(ErrorCard),
}

/// A turn in progress.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Running {
    pub started_tick: u64,
    pub streaming: bool,
    pub tokens_per_second: Option<f32>,
}

/// The branch the status shows: `git:main +1 −2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub added: u32,
    pub removed: u32,
}

/// What the idle screen says about startup (§10.9).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Startup {
    /// How many instruction files loaded.
    pub instructions: usize,
    pub repo_files: Option<usize>,
    /// Milliseconds the map took, when it is ready.
    pub repo_ms: Option<u64>,
    /// Still indexing (the "Loading repository map…" line).
    pub indexing: bool,
}

/// Everything.
#[derive(Debug, Clone)]
pub struct App {
    pub look: Look,
    pub mode: Mode,
    pub model: String,
    pub model_is_default: bool,
    pub session: Option<String>,
    pub cost_usd: Option<f64>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub branch: Option<Branch>,
    pub todos: Option<(u32, u32)>,
    /// Tokens in context and the window.
    pub context: Option<(u32, u32)>,
    pub jobs: u32,
    pub sandbox: Option<String>,
    pub online: bool,
    pub running: Option<Running>,
    pub transcript: Vec<Item>,
    /// Lines scrolled up from the bottom; 0 follows new output.
    pub scroll: usize,
    pub editor: Editor,
    pub history: History,
    pub overlay: Overlay,
    pub startup: Startup,
    pub update_available: Option<(String, String)>,
    pub tick: u64,
    /// Something to say below the transcript for a moment.
    pub note: Option<String>,
    /// `Esc` pressed once (a second within 800 ms forces).
    pub last_esc_tick: Option<u64>,
    /// Files the `@` popup offers (the controller fills it from the index).
    pub mention_candidates: Vec<String>,
    pub popup_sel: usize,
    /// `Esc` hid the popup for the text as it stands.
    pub popup_hidden: bool,
    /// The last `@` fragment the controller was asked about.
    pub last_mention: Option<String>,
    /// Names of background jobs, for the quit prompt.
    pub job_names: Vec<String>,
    /// The todo panel (§10.4 `Ctrl+T`).
    pub show_todos: bool,
    /// Rows of one transcript page, set by the draw loop.
    pub page_lines: usize,
    /// A plan file the controller should load and show.
    pub plan_to_load: Option<String>,
    /// The directory shown on the welcome screen.
    pub workspace: Option<String>,
}

impl App {
    #[must_use]
    pub fn new(look: Look, mode: Mode, model: &str) -> Self {
        Self {
            look,
            mode,
            model: model.to_string(),
            model_is_default: true,
            session: None,
            cost_usd: None,
            tokens_in: 0,
            tokens_out: 0,
            branch: None,
            todos: None,
            context: None,
            jobs: 0,
            sandbox: None,
            online: true,
            running: None,
            transcript: Vec::new(),
            scroll: 0,
            editor: Editor::new(EditMode::Emacs),
            history: History::in_memory(),
            overlay: Overlay::None,
            startup: Startup::default(),
            update_available: None,
            tick: 0,
            note: None,
            last_esc_tick: None,
            mention_candidates: Vec::new(),
            popup_sel: 0,
            popup_hidden: false,
            last_mention: None,
            job_names: Vec::new(),
            show_todos: false,
            page_lines: 10,
            plan_to_load: None,
            workspace: None,
        }
    }

    /// A user turn begins: the prompt joins the transcript, the screen
    /// follows the output.
    pub fn submit(&mut self, text: &str) {
        self.transcript.push(Item::User(text.to_string()));
        self.running = Some(Running {
            started_tick: self.tick,
            streaming: false,
            tokens_per_second: None,
        });
        self.scroll = 0;
    }

    /// One animation step.
    pub fn tick(&mut self) {
        self.tick += 1;
    }

    fn last_assistant(&mut self) -> Option<&mut String> {
        match self.transcript.last_mut() {
            Some(Item::Assistant {
                text,
                streaming: true,
            }) => Some(text),
            _ => None,
        }
    }

    fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
        self.transcript
            .iter_mut()
            .rev()
            .find_map(|item| match item {
                Item::Tool(card) if card.call_id == call_id => Some(card),
                _ => None,
            })
    }

    fn close_stream(&mut self) {
        if let Some(Item::Assistant { streaming, .. }) = self.transcript.last_mut() {
            *streaming = false;
        }
    }

    /// Fold an agent event into the screen.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per event the screen reacts to"
    )]
    pub fn apply(&mut self, event: &EventData) {
        match event {
            EventData::SessionCreated {
                session_id,
                mode,
                model,
                ..
            } => {
                self.session = Some(session_id.clone());
                self.model.clone_from(model);
                if let Some(m) = Mode::ALL.into_iter().find(|m| m.as_str() == mode) {
                    self.mode = m;
                }
            }
            EventData::SessionResumed { session_id, .. } => self.session = Some(session_id.clone()),
            EventData::ModeChanged { to, .. } => {
                if let Some(m) = Mode::ALL.into_iter().find(|m| m.as_str() == to) {
                    self.mode = m;
                }
            }
            EventData::ModelRequest { .. } => {
                if let Some(r) = self.running.as_mut() {
                    r.streaming = false;
                }
            }
            EventData::ModelDelta { text, .. } => {
                if let Some(r) = self.running.as_mut() {
                    r.streaming = true;
                }
                if let Some(current) = self.last_assistant() {
                    current.push_str(text);
                } else {
                    self.transcript.push(Item::Assistant {
                        text: text.clone(),
                        streaming: true,
                    });
                }
            }
            EventData::ModelReasoning { text, .. } => {
                if let Some(Item::Reasoning {
                    text: t, tokens, ..
                }) = self.transcript.last_mut()
                {
                    t.push_str(text);
                    *tokens += rough_tokens(text);
                } else {
                    self.transcript.push(Item::Reasoning {
                        text: text.clone(),
                        tokens: rough_tokens(text),
                        expanded: self.look.show_reasoning == ShowReasoning::Always,
                    });
                }
            }
            EventData::ModelUsage {
                input,
                output,
                cost_usd,
                ..
            } => {
                self.tokens_in += u64::from(*input);
                self.tokens_out += u64::from(*output);
                if *cost_usd > 0.0 {
                    self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + cost_usd);
                }
                if let Some(r) = self.running.as_mut() {
                    let elapsed = self.tick.saturating_sub(r.started_tick).max(1);
                    // Ticks are 80 ms apart.
                    #[allow(clippy::cast_precision_loss)]
                    let seconds = elapsed as f32 * 0.08;
                    #[allow(clippy::cast_precision_loss)]
                    let rate = *output as f32 / seconds.max(0.08);
                    r.tokens_per_second = Some(rate);
                }
            }
            EventData::MessageAppended { message, .. } => {
                // The committed text replaces what streamed in.
                self.close_stream();
                if message.role == cairn_core::message::Role::Assistant {
                    let text = message.text();
                    if let Some(Item::Assistant { text: t, .. }) = self.transcript.last_mut() {
                        if !text.is_empty() {
                            *t = text;
                        }
                    } else if !text.is_empty() {
                        self.transcript.push(Item::Assistant {
                            text,
                            streaming: false,
                        });
                    }
                }
            }
            EventData::ModelError {
                code,
                http_status,
                retryable,
                attempt,
                ..
            } => {
                let extra = http_status.map(|s| format!("HTTP {s}"));
                let (title, message) = if *retryable {
                    (
                        "Provider error".to_string(),
                        format!("The model call failed; retrying (retry {attempt})."),
                    )
                } else {
                    (
                        "Provider error".to_string(),
                        "The model call failed.".to_string(),
                    )
                };
                self.transcript.push(Item::Error(ErrorCard {
                    title,
                    code: code.clone(),
                    extra,
                    message,
                    detail: None,
                    actions: None,
                }));
            }
            EventData::ToolStarted {
                call_id,
                name,
                input,
                ..
            } => {
                self.close_stream();
                self.transcript.push(Item::Tool(ToolCard {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    summary: summarise(name, input),
                    input: input.clone(),
                    state: ToolState::Running,
                    tail: VecDeque::new(),
                    bytes: 0,
                    output: None,
                    rule: None,
                    expanded: false,
                    diff: None,
                    started_tick: self.tick,
                }));
            }
            EventData::ToolProgress {
                call_id,
                bytes_read,
                preview,
                ..
            } => {
                if let Some(card) = self.tool_mut(call_id) {
                    card.bytes = *bytes_read;
                    for line in preview.lines().filter(|l| !l.trim().is_empty()) {
                        card.tail.push_back(line.to_string());
                        while card.tail.len() > 5 {
                            card.tail.pop_front();
                        }
                    }
                }
            }
            EventData::ToolFinished {
                call_id,
                status,
                duration_ms,
                output_bytes,
                error,
                ..
            } => {
                let (status, duration_ms, bytes, error) =
                    (*status, *duration_ms, *output_bytes, error.clone());
                if let Some(card) = self.tool_mut(call_id) {
                    card.bytes = bytes;
                    card.state = match status {
                        ToolStatus::Ok => ToolState::Done { duration_ms },
                        ToolStatus::Denied => ToolState::Denied {
                            code: error.clone().unwrap_or_else(|| "E-PERM-DENIED".into()),
                            reason: String::new(),
                        },
                        other => ToolState::Failed {
                            duration_ms,
                            code: error.clone().unwrap_or_else(|| match other {
                                ToolStatus::Timeout => "E-TOOL-TIMEOUT".into(),
                                ToolStatus::Cancelled => "E-TOOL-CANCELLED".into(),
                                _ => "E-TOOL-ERROR".into(),
                            }),
                            recovery: None,
                        },
                    };
                }
            }
            EventData::PermissionDenied {
                call_id,
                rule_id,
                reason,
            } => {
                if let Some(card) = self.tool_mut(call_id) {
                    card.state = ToolState::Denied {
                        code: "E-PERM-DENIED".into(),
                        reason: format!("{reason} (rule {rule_id})"),
                    };
                }
            }
            EventData::GuardrailTrip {
                rule,
                limit,
                actual,
            } => {
                self.transcript.push(Item::Notice(
                    NoticeKind::Warning,
                    format!(
                        "Guardrail tripped: {rule} ({limit}). Turn stopped at {actual}. Partial work saved; checkpoint intact."
                    ),
                ));
            }
            EventData::CheckpointRestored {
                checkpoint_id,
                files,
                ..
            } => {
                self.transcript.push(Item::Notice(
                    NoticeKind::Info,
                    format!("Restored {files} file(s) from {checkpoint_id}."),
                ));
            }
            EventData::CompactionPerformed {
                before_tokens,
                after_tokens,
                ..
            } => {
                self.transcript.push(Item::Notice(
                    NoticeKind::Info,
                    format!(
                        "Compacted history: {} → {} tokens. /undo compaction to restore.",
                        group(u64::from(*before_tokens)),
                        group(u64::from(*after_tokens))
                    ),
                ));
            }
            EventData::ApprovalRequested {
                request_id,
                call_id,
                kind,
                summary,
                detail,
                ..
            } => {
                self.overlay = Overlay::Approval(Approval::from_event(
                    request_id, call_id, kind, summary, detail,
                ));
            }
            EventData::ApprovalAnswered { request_id, .. } => {
                if matches!(&self.overlay, Overlay::Approval(a) if a.request_id == *request_id) {
                    self.overlay = Overlay::None;
                }
            }
            EventData::PlanCreated { path, .. } => self.plan_to_load = Some(path.clone()),
            EventData::JobStarted { .. } => self.jobs += 1,
            EventData::JobFinished { .. } => self.jobs = self.jobs.saturating_sub(1),
            EventData::Error {
                code,
                message,
                hint,
                ..
            } => {
                self.transcript.push(Item::Error(ErrorCard {
                    title: "Error".into(),
                    code: code.clone(),
                    extra: None,
                    message: if hint.is_empty() {
                        message.clone()
                    } else {
                        format!("{message} {hint}")
                    },
                    detail: None,
                    actions: None,
                }));
            }
            EventData::UsageTotals {
                session_cost_usd,
                session_tokens,
                ..
            } => {
                if *session_cost_usd > 0.0 {
                    self.cost_usd = Some(*session_cost_usd);
                }
                let _ = session_tokens;
            }
            EventData::TurnEnded { status, .. } => {
                self.close_stream();
                self.running = None;
                if *status == TurnStatus::Cancelled {
                    self.transcript
                        .push(Item::Notice(NoticeKind::Info, "Cancelled.".into()));
                }
            }
            _ => {}
        }
        // New output follows the screen unless the reader has scrolled up.
    }

    /// Whether anything is running and the animation should move.
    #[must_use]
    pub fn animating(&self) -> bool {
        self.look.animation && (self.look.support != ColorSupport::None || self.running.is_some())
    }

    /// The elapsed time of the running turn as `MM:SS`.
    #[must_use]
    pub fn elapsed(&self) -> Option<String> {
        let r = self.running?;
        let seconds = self.tick.saturating_sub(r.started_tick) * 80 / 1000;
        Some(format!("{:02}:{:02}", seconds / 60, seconds % 60))
    }
}

/// Words in a reasoning delta, as a token guess.
fn rough_tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(4)).unwrap_or(u32::MAX)
}

/// `12,880`.
#[must_use]
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `12.8k`, `340`, `1.2M`.
#[must_use]
pub fn short(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", f / 1_000.0)
    } else {
        n.to_string()
    }
}

/// `200k`, `1M`.
#[must_use]
pub fn window(n: u32) -> String {
    if n >= 1_000_000 && n % 1_000_000 == 0 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// What a tool call is about, for its one-line card.
#[must_use]
pub fn summarise(name: &str, input: &serde_json::Value) -> String {
    let text = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let path_line = || {
        let path = text("path")?;
        Some(
            match input.get("start_line").and_then(serde_json::Value::as_u64) {
                Some(line) => format!("{path}:{line}"),
                None => path,
            },
        )
    };
    match name {
        "read_file" | "write_file" | "edit_file" | "multi_edit" | "list_dir" | "git_diff"
        | "git_status" => path_line().unwrap_or_default(),
        "glob" | "grep" => text("pattern").unwrap_or_default(),
        "bash" | "bash_background" => text("command").unwrap_or_default(),
        "web_fetch" => text("url").unwrap_or_default(),
        "git_commit" => text("message")
            .map(|m| m.lines().next().unwrap_or("").to_string())
            .unwrap_or_default(),
        "ask_user" => text("question").unwrap_or_default(),
        "job_output" | "job_kill" => text("job_id").unwrap_or_default(),
        "todo_write" => input
            .get("todos")
            .and_then(serde_json::Value::as_array)
            .map(|t| format!("{} item(s)", t.len()))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app() -> App {
        App::new(Look::default(), Mode::Build, "claude-sonnet-4-5")
    }

    #[test]
    fn numbers_are_formatted_for_the_status_bar() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(12_880), "12,880");
        assert_eq!(group(1_234_567), "1,234,567");
        assert_eq!(short(340), "340");
        assert_eq!(short(12_800), "12.8k");
        assert_eq!(short(1_250_000), "1.2M");
        assert_eq!(window(200_000), "200k");
        assert_eq!(window(1_000_000), "1M");
        assert_eq!(window(32_768), "32k");
    }

    #[test]
    fn summaries_say_what_a_call_is_about() {
        assert_eq!(
            summarise("read_file", &json!({"path": "src/a.rs"})),
            "src/a.rs"
        );
        assert_eq!(
            summarise("read_file", &json!({"path": "src/a.rs", "start_line": 40})),
            "src/a.rs:40"
        );
        assert_eq!(
            summarise("bash", &json!({"command": "cargo test"})),
            "cargo test"
        );
        assert_eq!(summarise("grep", &json!({"pattern": "TODO"})), "TODO");
        assert_eq!(
            summarise("git_commit", &json!({"message": "fix: x\n\nbody"})),
            "fix: x"
        );
        assert_eq!(
            summarise("todo_write", &json!({"todos": [{}, {}, {}]})),
            "3 item(s)"
        );
        assert_eq!(summarise("mystery", &json!({})), "");
    }

    #[test]
    fn deltas_build_one_streaming_message_that_the_committed_text_replaces() {
        let mut a = app();
        a.submit("hello");
        assert!(a.running.is_some());
        for chunk in ["The par", "ser fails"] {
            a.apply(&EventData::ModelDelta {
                turn_id: 1,
                text: chunk.into(),
            });
        }
        assert_eq!(a.transcript.len(), 2);
        assert_eq!(
            a.transcript[1],
            Item::Assistant {
                text: "The parser fails".into(),
                streaming: true
            }
        );
        let message = cairn_core::message::Message::new(
            cairn_core::message::Role::Assistant,
            vec![cairn_core::message::Block::Text {
                text: "The parser fails on EOF.".into(),
            }],
            1,
        );
        a.apply(&EventData::MessageAppended {
            turn_id: 1,
            message,
        });
        assert_eq!(
            a.transcript[1],
            Item::Assistant {
                text: "The parser fails on EOF.".into(),
                streaming: false
            }
        );
        a.apply(&EventData::TurnEnded {
            turn_id: 1,
            status: TurnStatus::Ok,
            duration_ms: 10,
            cost_usd: 0.0,
        });
        assert!(a.running.is_none());
    }

    #[test]
    fn a_tool_call_runs_shows_progress_and_finishes() {
        let mut a = app();
        a.apply(&EventData::ToolStarted {
            call_id: "c1".into(),
            name: "bash".into(),
            input: json!({"command": "cargo test --quiet"}),
            parallel_index: 0,
        });
        let Item::Tool(card) = &a.transcript[0] else {
            panic!()
        };
        assert_eq!(card.summary, "cargo test --quiet");
        assert_eq!(card.state, ToolState::Running);
        for i in 0..8 {
            a.apply(&EventData::ToolProgress {
                call_id: "c1".into(),
                bytes_read: 100 * (i + 1),
                lines: i,
                truncated: false,
                preview: format!("line {i}\n"),
            });
        }
        let Item::Tool(card) = &a.transcript[0] else {
            panic!()
        };
        assert_eq!(card.tail.len(), 5, "five rolling lines");
        assert_eq!(card.tail.front().map(String::as_str), Some("line 3"));
        a.apply(&EventData::ToolFinished {
            call_id: "c1".into(),
            name: "bash".into(),
            status: ToolStatus::Ok,
            duration_ms: 4100,
            output_bytes: 900,
            truncated: false,
            error: None,
        });
        let Item::Tool(card) = &a.transcript[0] else {
            panic!()
        };
        assert_eq!(card.state, ToolState::Done { duration_ms: 4100 });
    }

    #[test]
    fn denials_and_failures_keep_their_codes() {
        let mut a = app();
        for id in ["a", "b"] {
            a.apply(&EventData::ToolStarted {
                call_id: id.into(),
                name: "edit_file".into(),
                input: json!({}),
                parallel_index: 0,
            });
        }
        a.apply(&EventData::PermissionDenied {
            call_id: "a".into(),
            rule_id: "r12".into(),
            reason: "denies bash:rm".into(),
        });
        a.apply(&EventData::ToolFinished {
            call_id: "b".into(),
            name: "edit_file".into(),
            status: ToolStatus::Error,
            duration_ms: 3,
            output_bytes: 0,
            truncated: false,
            error: Some("E-EDIT-NOMATCH".into()),
        });
        let Item::Tool(denied) = &a.transcript[0] else {
            panic!()
        };
        assert!(
            matches!(&denied.state, ToolState::Denied { reason, .. } if reason.contains("r12"))
        );
        let Item::Tool(failed) = &a.transcript[1] else {
            panic!()
        };
        assert!(
            matches!(&failed.state, ToolState::Failed { code, .. } if code == "E-EDIT-NOMATCH")
        );
    }

    #[test]
    fn usage_accumulates_and_cost_stays_unknown_until_priced() {
        let mut a = app();
        a.apply(&EventData::ModelUsage {
            turn_id: 1,
            input: 1000,
            output: 200,
            cache_read: 0,
            cache_write: 0,
            cost_usd: 0.0,
        });
        assert_eq!((a.tokens_in, a.tokens_out, a.cost_usd), (1000, 200, None));
        a.apply(&EventData::ModelUsage {
            turn_id: 1,
            input: 500,
            output: 100,
            cache_read: 0,
            cache_write: 0,
            cost_usd: 0.012,
        });
        assert_eq!((a.tokens_in, a.tokens_out), (1500, 300));
        assert_eq!(a.cost_usd, Some(0.012));
    }

    #[test]
    fn mode_session_jobs_and_notices_follow_events() {
        let mut a = app();
        a.apply(&EventData::ModeChanged {
            from: "build".into(),
            to: "plan".into(),
            trigger: "user".into(),
            in_flight: String::new(),
        });
        assert_eq!(a.mode, Mode::Plan);
        a.apply(&EventData::JobStarted {
            job_id: "j".into(),
            label: "x".into(),
            pid: 1,
        });
        a.apply(&EventData::JobStarted {
            job_id: "k".into(),
            label: "y".into(),
            pid: 2,
        });
        a.apply(&EventData::JobFinished {
            job_id: "j".into(),
            exit_code: Some(0),
            duration_ms: 1,
        });
        assert_eq!(a.jobs, 1);
        a.apply(&EventData::GuardrailTrip {
            rule: "max_tool_calls".into(),
            limit: json!(120),
            actual: json!(120),
        });
        let Some(Item::Notice(NoticeKind::Warning, text)) = a.transcript.last() else {
            panic!()
        };
        assert_eq!(text, "Guardrail tripped: max_tool_calls (120). Turn stopped at 120. Partial work saved; checkpoint intact.");
        a.apply(&EventData::CompactionPerformed {
            before_tokens: 41_203,
            after_tokens: 12_880,
            messages_dropped: 30,
            messages_summarized: 30,
            summary_tokens: 700,
        });
        let Some(Item::Notice(_, text)) = a.transcript.last() else {
            panic!()
        };
        assert_eq!(
            text,
            "Compacted history: 41,203 → 12,880 tokens. /undo compaction to restore."
        );
    }

    #[test]
    fn elapsed_time_counts_ticks_of_80_ms() {
        let mut a = app();
        a.submit("go");
        for _ in 0..513 {
            a.tick();
        }
        let e = a.elapsed().unwrap();
        assert_eq!(e, "00:41");
    }
}
