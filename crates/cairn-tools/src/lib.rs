//! `cairn-tools` — the tool layer (SPEC §6).
//!
//! * [`tool`] and [`types`] — the [`Tool`] trait and its vocabulary.
//! * [`registry`] — registration and the §6.1 table; what each mode is shown.
//! * [`paths`] — the §9.4 filesystem boundary and protected paths.
//! * [`schema`] — input validation (§6.5 steps 1–2).
//! * [`output`] — deterministic truncation, redaction and sanitising.
//! * [`pipeline`] — the twelve-step pipeline and §6.6's parallel policy.
//! * [`builtin`] — the tools themselves.

pub mod builtin;
pub mod edit;
pub mod output;
pub mod paths;
pub mod pipeline;
pub mod registry;
pub mod schema;
pub mod syntax;
pub mod tool;
pub mod types;

pub use paths::{Boundary, Resolved};
pub use pipeline::{
    Answer, ApprovalRequest, Approver, CallEnv, DenyAll, Executor, ExecutorParts, ToolCall,
    ToolResult,
};
pub use registry::{offered_in, Registry, RegistryError, TableRow, ToolDef};
pub use syntax::ParseCheck;
pub use tool::Tool;
pub use types::{
    Access, EventSink, FileState, Idempotency, LineEndings, NoSyntax, NullSink, PathArg,
    PermissionClass, RequestInfo, SideEffect, SyntaxCheck, SyntaxProblem, SyntaxVerdict,
    ToolContext, ToolError, ToolOutput,
};
