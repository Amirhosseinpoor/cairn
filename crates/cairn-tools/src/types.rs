//! The vocabulary of the tool layer (SPEC §3.4, §6.1).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cairn_core::event::EventData;
use cairn_core::{Mode, SessionId};
use cairn_perm::PermissionPolicy;
use cairn_sandbox::Sandbox;
use cairn_search::IgnoreEngine;
use serde_json::Value;

use crate::paths::Boundary;

/// §6.1's permission classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionClass {
    /// Auto-allowed in every mode.
    Read,
    /// Gated per mode (§7.2).
    Write,
    /// Always gated in `build`, rule-gated in `auto`, never in `plan`.
    Execute,
    /// Gated in `plan`/`build`, rule-gated in `auto`.
    Network,
    /// `.cairn/` writes, allowed in every mode.
    WriteState,
    /// Only talks to the user.
    Ask,
    /// Gated by `subagent.enabled`.
    Spawn,
}

impl PermissionClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Write => "Write",
            Self::Execute => "Execute",
            Self::Network => "Network",
            Self::WriteState => "WriteState",
            Self::Ask => "Ask",
            Self::Spawn => "Spawn",
        }
    }
}

/// What a tool does to the world (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideEffect {
    None,
    Read,
    Write,
    Execute,
    Network,
}

impl SideEffect {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Read => "Read",
            Self::Write => "Write",
            Self::Execute => "Execute",
            Self::Network => "Network",
        }
    }
}

/// Whether repeating a call is safe (§6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Idempotency {
    Safe,
    Retryable,
    NonIdempotent,
}

impl Idempotency {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "Safe",
            Self::Retryable => "Retryable",
            Self::NonIdempotent => "NonIdempotent",
        }
    }
}

/// How a path argument is used, which decides the boundary rule (§9.4:
/// additional directories are read-only unless granted writable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// A path-valued field of a tool's input.
#[derive(Debug, Clone)]
pub struct PathArg {
    /// The field's name, for messages.
    pub field: &'static str,
    /// The value as the model wrote it.
    pub value: String,
    pub access: Access,
}

/// What the permission engine needs to know about a call beyond its paths.
#[derive(Debug, Clone, Default)]
pub struct RequestInfo {
    pub command: Option<String>,
    pub url: Option<String>,
}

/// A successful result: the tool's `data` object (§6.1's envelope).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub data: Value,
    /// The tool itself cut its output short.
    pub truncated: bool,
}

impl ToolOutput {
    #[must_use]
    pub fn new(data: Value) -> Self {
        Self {
            data,
            truncated: false,
        }
    }

    #[must_use]
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

/// A failed result: a stable code, a message, and what to do about it
/// (REQ-TOOL-019 — model-visible, never a turn abort).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub code: &'static str,
    pub message: String,
    pub recovery: Option<String>,
    /// Structured detail that rides along with `ok:false` (an exit code, the
    /// failing edit index). Boxed so `Result<_, ToolError>` stays small.
    pub data: Option<Box<Value>>,
}

impl ToolError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: None,
            data: None,
        }
    }

    #[must_use]
    pub fn recovery(mut self, recovery: impl Into<String>) -> Self {
        self.recovery = Some(recovery.into());
        self
    }

    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(Box::new(data));
        self
    }
}

/// Where tools publish events (`tool.progress`, `job.finished`, ...). The
/// agent connects it to the event bus; tests collect into a vector.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: EventData);
}

/// An [`EventSink`] that drops everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: EventData) {}
}

/// What the model last saw of one file.
#[derive(Debug, Clone)]
struct Observed {
    sha256: String,
    /// LF-normalised text, kept only for files small enough to diff against.
    content: Option<std::sync::Arc<str>>,
}

/// Files above this are remembered by hash alone.
const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
/// How many files are remembered, and how many bytes of snapshots in total.
const SNAPSHOT_MAX_FILES: usize = 64;
const SNAPSHOT_MAX_TOTAL: usize = 8 * 1024 * 1024;

/// The files the model last saw (§6.3.3's `file_state`), shared by the calls
/// of one session. A hash says *that* a file changed; the snapshot lets the
/// error say *how* (REQ-TOOL-013).
#[derive(Debug, Default)]
pub struct FileState {
    seen: Mutex<FileStateInner>,
}

#[derive(Debug, Default)]
struct FileStateInner {
    files: std::collections::HashMap<PathBuf, Observed>,
    order: std::collections::VecDeque<PathBuf>,
}

impl FileState {
    /// Remember `path` as seen with `sha256` (and, if small, its text).
    pub fn record(&self, path: PathBuf, sha256: String, content: Option<&str>) {
        let content = content
            .filter(|c| c.len() <= SNAPSHOT_MAX_BYTES)
            .map(std::sync::Arc::<str>::from);
        let mut inner = self.seen.lock().expect("file state");
        if inner
            .files
            .insert(path.clone(), Observed { sha256, content })
            .is_none()
        {
            inner.order.push_back(path);
        }
        // Oldest first, until both bounds hold.
        loop {
            let total: usize = inner
                .files
                .values()
                .map(|o| o.content.as_ref().map_or(0, |c| c.len()))
                .sum();
            if inner.files.len() <= SNAPSHOT_MAX_FILES && total <= SNAPSHOT_MAX_TOTAL {
                break;
            }
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.files.remove(&oldest);
        }
    }

    #[must_use]
    pub fn last_seen(&self, path: &std::path::Path) -> Option<String> {
        self.seen
            .lock()
            .expect("file state")
            .files
            .get(path)
            .map(|o| o.sha256.clone())
    }

    /// The text the model last saw, when it was small enough to keep.
    #[must_use]
    pub fn last_content(&self, path: &std::path::Path) -> Option<std::sync::Arc<str>> {
        self.seen
            .lock()
            .expect("file state")
            .files
            .get(path)
            .and_then(|o| o.content.clone())
    }
}

/// `line_endings` (REQ-TOOL-004): what a *new* file gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEndings {
    /// The platform's own: CRLF on Windows, LF elsewhere.
    #[default]
    Auto,
    Lf,
    Crlf,
}

impl LineEndings {
    /// The style a new file is written in.
    #[must_use]
    pub const fn for_new_files(self) -> crate::edit::Eol {
        match self {
            Self::Lf => crate::edit::Eol::Lf,
            Self::Crlf => crate::edit::Eol::Crlf,
            Self::Auto => {
                if cfg!(windows) {
                    crate::edit::Eol::Crlf
                } else {
                    crate::edit::Eol::Lf
                }
            }
        }
    }
}

/// Where a syntax check found a problem (§6.3.6 step 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxProblem {
    pub line: usize,
    pub column: usize,
    pub expected: Option<String>,
    pub found: Option<String>,
    /// At most eight lines around it.
    pub snippet: String,
}

/// The result of checking an edited buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxVerdict {
    /// Parsed, and nothing wrong near the edit.
    Valid,
    /// No grammar or validator for this file: `syntax_ok: null`.
    Unchecked,
    /// The check ran past its budget (REQ-TOOL-015).
    TimedOut,
    Invalid(SyntaxProblem),
}

/// Post-edit syntax validation (§6.3.6). The tree-sitter implementation lives
/// in `cairn-parse`; the tools only need this.
pub trait SyntaxCheck: Send + Sync {
    /// Check `after` (the buffer about to be written). `before` is what the
    /// file holds now, so errors the edit did not cause can be told apart.
    fn check(&self, path: &str, before: &str, after: &str) -> SyntaxVerdict;
}

/// No validation: every verdict is [`SyntaxVerdict::Unchecked`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSyntax;

impl SyntaxCheck for NoSyntax {
    fn check(&self, _path: &str, _before: &str, _after: &str) -> SyntaxVerdict {
        SyntaxVerdict::Unchecked
    }
}

/// Everything a tool call may use (§3.4's `ToolContext`).
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub turn_id: u64,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub mode: Mode,
    pub permissions: Arc<dyn PermissionPolicy>,
    pub sandbox: Arc<dyn Sandbox>,
    pub events: Arc<dyn EventSink>,
    pub call_id: String,
    pub boundary: Arc<Boundary>,
    pub ignore: Arc<IgnoreEngine>,
    pub file_state: Arc<FileState>,
    pub syntax: Arc<dyn SyntaxCheck>,
    pub line_endings: LineEndings,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("workspace_root", &self.workspace_root)
            .field("mode", &self.mode)
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}
