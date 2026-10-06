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

/// The file hashes the model last saw (§6.3.3's `file_state`), shared by the
/// calls of one session.
#[derive(Debug, Default)]
pub struct FileState {
    seen: Mutex<std::collections::HashMap<PathBuf, String>>,
}

impl FileState {
    pub fn record(&self, path: PathBuf, sha256: String) {
        self.seen.lock().expect("file state").insert(path, sha256);
    }

    #[must_use]
    pub fn last_seen(&self, path: &std::path::Path) -> Option<String> {
        self.seen.lock().expect("file state").get(path).cloned()
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
