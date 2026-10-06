//! The validation-and-execution pipeline (SPEC §6.5) and the parallel policy
//! (§6.6).
//!
//! ```text
//! schema → limits → normalize → boundary → deny-path → permission
//!        → (hooks) → execute → truncate → redact → (post hooks) → record
//! ```
//!
//! Every failure before `execute` is a *result*, not an error of the turn: the
//! model gets `ok:false` with a code and what to do next (REQ-TOOL-019). The
//! one thing that ends a turn is cancellation, which the caller sees on its
//! own token.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::event::{EventData, ToolStatus};
use cairn_core::redact::Redactor;
use cairn_core::{Mode, SessionId};
use cairn_perm::{Decision, Effect, PermissionPolicy, PermissionRequest, Scope};
use cairn_sandbox::Sandbox;
use cairn_search::IgnoreEngine;
use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore};

use crate::output::{fit, scrub};
use crate::paths::Boundary;
use crate::registry::Registry;
use crate::tool::Tool;
use crate::types::{
    Access, EventSink, FileState, LineEndings, NoSyntax, PermissionClass, SideEffect, SyntaxCheck,
    ToolContext, ToolError,
};

/// §6.6: parallel-safe tools run up to this many at once.
pub const MAX_PARALLEL_READS: usize = 8;
/// REQ-TOOL-022: more calls than this in one turn are queued.
pub const MAX_CALLS_PER_BATCH: usize = 16;
/// How often a waiting call looks at its cancellation token. The token is
/// std-only (§3.2), so there is no future to await on it.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// A call the model made.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub input: Value,
}

/// How an approval was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Run this once; remember nothing.
    Once,
    /// Run it, and for the rest of this session.
    Session,
    /// Run it, and write an allow rule to the project file.
    Always,
    /// Do not run it.
    Deny,
    /// Do not run it, and write a deny rule to the project file.
    DenyAlways,
}

/// What the user is asked (§7.2, §9.7 mitigation 7: the *exact* command or
/// path, never only the model's description of it).
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub detail: Value,
    pub rule_id: String,
    pub reason: String,
}

/// Whoever answers approvals: the TUI, `--allow-ask`'s stdin, or nobody.
pub trait Approver: Send + Sync {
    fn ask(&self, request: ApprovalRequest) -> BoxFuture<'_, Answer>;
}

/// The headless default: nobody to ask, so every `ask` is a refusal.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn ask(&self, _request: ApprovalRequest) -> BoxFuture<'_, Answer> {
        Box::pin(async { Answer::Deny })
    }
}

/// What a call came to.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub ok: bool,
    /// The §6.1 envelope the model reads.
    pub envelope: Value,
    pub status: ToolStatus,
    pub duration_ms: u64,
    pub truncated: bool,
    /// The failure's code, when `ok` is false.
    pub error_code: Option<&'static str>,
    /// A write was refused by a permission rule (exit 6 headless, T-CLI-013).
    pub denied: bool,
    /// The workspace-relative paths a successful write tool changed.
    pub paths_written: Vec<PathBuf>,
}

impl ToolResult {
    /// A failed result for a call that never reached the pipeline (arguments
    /// that did not parse, a call the loop refused to dispatch).
    #[must_use]
    pub fn rejected(call_id: &str, name: &str, error: &ToolError) -> Self {
        let call = ToolCall {
            call_id: call_id.to_string(),
            name: name.to_string(),
            input: Value::Null,
        };
        failure(&call, Instant::now(), error, ToolStatus::Error, false)
    }

    /// The text sent to the model.
    #[must_use]
    pub fn text(&self) -> String {
        self.envelope.to_string()
    }
}

/// The environment one batch of calls runs in.
#[derive(Debug, Clone)]
pub struct CallEnv {
    pub session_id: SessionId,
    pub turn_id: u64,
    pub cwd: PathBuf,
    pub mode: Mode,
}

/// Runs tool calls through the pipeline.
pub struct Executor {
    registry: Arc<Registry>,
    policy: Arc<dyn PermissionPolicy>,
    approver: Arc<dyn Approver>,
    sandbox: Arc<dyn Sandbox>,
    events: Arc<dyn EventSink>,
    boundary: Arc<Boundary>,
    ignore: Arc<IgnoreEngine>,
    redactor: Arc<Redactor>,
    syntax: Arc<dyn SyntaxCheck>,
    line_endings: LineEndings,
    observer: Arc<dyn cairn_git::WriteObserver>,
    jobs: Arc<crate::shell::jobs::JobTable>,
    questioner: Option<Arc<dyn crate::types::Questioner>>,
    taint: Arc<crate::types::Taint>,
    file_state: Arc<FileState>,
    approval_timeout: Duration,
    reads: Semaphore,
    /// Writers hold a read guard, serial tools the write guard: a serial tool
    /// therefore waits for every write in flight (REQ-TOOL-021).
    barrier: RwLock<()>,
    serial: AsyncMutex<()>,
    path_locks: Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("tools", &self.registry)
            .finish_non_exhaustive()
    }
}

/// Everything an [`Executor`] is built from.
#[allow(
    missing_debug_implementations,
    reason = "holds trait objects with no Debug bound"
)]
pub struct ExecutorParts {
    pub registry: Arc<Registry>,
    pub policy: Arc<dyn PermissionPolicy>,
    pub approver: Arc<dyn Approver>,
    pub sandbox: Arc<dyn Sandbox>,
    pub events: Arc<dyn EventSink>,
    pub boundary: Arc<Boundary>,
    pub ignore: Arc<IgnoreEngine>,
    pub redactor: Arc<Redactor>,
    /// Post-edit validation; `None` means [`NoSyntax`].
    pub syntax: Option<Arc<dyn SyntaxCheck>>,
    /// `line_endings` for new files.
    pub line_endings: LineEndings,
    /// Checkpoint hook; `None` records nothing.
    pub observer: Option<Arc<dyn cairn_git::WriteObserver>>,
    /// Who answers `ask_user`; `None` means nobody can.
    pub questioner: Option<Arc<dyn crate::types::Questioner>>,
    /// `permissions.ask_timeout_ms` (§8.1; default 10 minutes).
    pub approval_timeout: Duration,
}

fn failure(
    call: &ToolCall,
    started: Instant,
    error: &ToolError,
    status: ToolStatus,
    denied: bool,
) -> ToolResult {
    let mut error_obj = json!({ "code": error.code, "message": error.message });
    if let Some(recovery) = &error.recovery {
        error_obj["recovery"] = json!(recovery);
    }
    let data = error
        .data
        .as_deref()
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let envelope = json!({
        "ok": false,
        "data": data,
        "error": error_obj,
        "truncated": false,
        "bytes": 0,
    });
    ToolResult {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        ok: false,
        envelope,
        status,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        truncated: false,
        error_code: Some(error.code),
        denied,
        paths_written: Vec::new(),
    }
}

/// How a tool takes part in §6.6's concurrency policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// Parallel-safe: a permit from the shared semaphore.
    Read,
    /// A path-serialized writer.
    Writer,
    /// One at a time, after every write in flight.
    Serial,
}

fn lane_of(tool: &dyn Tool) -> Lane {
    if tool.requires_serial() {
        Lane::Serial
    } else if matches!(tool.side_effect(), SideEffect::Write) {
        Lane::Writer
    } else {
        Lane::Read
    }
}

impl Executor {
    #[must_use]
    pub fn new(parts: ExecutorParts) -> Self {
        Self {
            registry: parts.registry,
            policy: parts.policy,
            approver: parts.approver,
            sandbox: parts.sandbox,
            events: parts.events,
            boundary: parts.boundary,
            ignore: parts.ignore,
            redactor: parts.redactor,
            observer: parts
                .observer
                .unwrap_or_else(|| Arc::new(cairn_git::NoObserver)),
            syntax: parts.syntax.unwrap_or_else(|| Arc::new(NoSyntax)),
            line_endings: parts.line_endings,
            jobs: Arc::new(crate::shell::jobs::JobTable::new()),
            questioner: parts.questioner,
            taint: Arc::new(crate::types::Taint::default()),
            file_state: Arc::new(FileState::default()),
            approval_timeout: parts.approval_timeout,
            reads: Semaphore::new(MAX_PARALLEL_READS),
            barrier: RwLock::new(()),
            serial: AsyncMutex::new(()),
            path_locks: Mutex::new(HashMap::new()),
        }
    }

    /// The registry, for `doctor --tools` and for building the request.
    #[must_use]
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Background jobs started through this executor.
    #[must_use]
    pub fn jobs(&self) -> Arc<crate::shell::jobs::JobTable> {
        Arc::clone(&self.jobs)
    }

    /// The hashes the model last saw.
    #[must_use]
    pub fn file_state(&self) -> Arc<FileState> {
        Arc::clone(&self.file_state)
    }

    fn path_lock(&self, path: PathBuf) -> Arc<AsyncMutex<()>> {
        let mut locks = self.path_locks.lock().expect("path locks");
        Arc::clone(locks.entry(path).or_default())
    }

    /// Run a batch of calls as §6.6 describes: reads in parallel, writers
    /// serialized per path, serial tools one at a time; one call's failure
    /// never cancels its siblings (REQ-TOOL-023). Calls beyond
    /// [`MAX_CALLS_PER_BATCH`] wait for the first group (REQ-TOOL-022); the
    /// flag says whether that happened, so the loop can warn the model
    /// (`W-TOOL-BURST`). Results come back in call order.
    pub async fn run_batch(
        &self,
        calls: Vec<ToolCall>,
        env: &CallEnv,
        cancel: &CancellationToken,
    ) -> (Vec<ToolResult>, bool) {
        let burst = calls.len() > MAX_CALLS_PER_BATCH;
        let mut results = Vec::with_capacity(calls.len());
        let mut index = 0;
        for group in calls.chunks(MAX_CALLS_PER_BATCH) {
            let futures = group.iter().enumerate().map(|(i, call)| {
                let parallel_index = u32::try_from(index + i).unwrap_or(u32::MAX);
                self.run(call, env, cancel, parallel_index)
            });
            results.extend(futures::future::join_all(futures).await);
            index += group.len();
        }
        (results, burst)
    }

    /// Run one call through the whole pipeline.
    pub async fn run(
        &self,
        call: &ToolCall,
        env: &CallEnv,
        cancel: &CancellationToken,
        parallel_index: u32,
    ) -> ToolResult {
        let started = Instant::now();
        let Some(tool) = self.registry.get(&call.name).cloned() else {
            let names = self.registry.names().join(", ");
            let error = ToolError::new(
                codes::TOOL_BADSCHEMA,
                format!("no tool named `{}` is available", call.name),
            )
            .recovery(if names.is_empty() {
                "No tools are available in this run; answer in text.".to_string()
            } else {
                format!("Use one of: {names}.")
            });
            return failure(call, started, &error, ToolStatus::Error, false);
        };

        // 1–2: schema and limits.
        if let Some(validator) = self.registry.validator(&call.name) {
            if let Err(error) = validator.check(&call.name, &call.input) {
                return self.finish_early(call, started, &error, ToolStatus::Error, false);
            }
        }
        // Pure checks first: nothing to ask a person about a request that
        // can only fail.
        if let Err(error) = tool.precheck(&call.input) {
            return self.finish_early(call, started, &error, ToolStatus::Denied, true);
        }
        // 3–5: normalize, boundary, protected paths.
        let mut resolved = Vec::new();
        for arg in tool.path_args(&call.input) {
            match self.boundary.resolve(&arg.value, &env.cwd, arg.access) {
                Ok(r) if r.protected => {
                    let error = ToolError::new(
                        codes::FS_PROTECTED,
                        format!("`{}` is a protected path and cannot be {}", r.display(), match arg.access {
                            Access::Read => "read",
                            Access::Write => "changed",
                        }),
                    )
                    .recovery("Choose another path; the user can lift this with security.allow_protected_paths.");
                    return self.finish_early(call, started, &error, ToolStatus::Denied, true);
                }
                Ok(r) => {
                    if r.secret && arg.access == Access::Read {
                        self.taint.secrets_read(env.turn_id);
                    }
                    resolved.push((arg.access, r));
                }
                Err(error) => {
                    return self.finish_early(call, started, &error, ToolStatus::Error, false)
                }
            }
        }
        // 6: permission.
        let info = tool.request_info(&call.input);
        let mut request = PermissionRequest::new(&call.name, env.mode);
        request.path = resolved.first().map(|(_, r)| r.display());
        request.command = info.command.clone();
        request.url = info.url.clone();
        request.protected_path = resolved.iter().any(|(_, r)| r.protected);
        // §9.3: a shell command is judged leaf by leaf, not as one string.
        let shell = match self.judge_shell(call, &tool, env) {
            Ok(outcome) => outcome,
            Err(error) => {
                return self.finish_early(call, started, &error, ToolStatus::Denied, true)
            }
        };
        match self
            .authorise(call, &tool, &request, shell, env, &resolved)
            .await
        {
            Ok(()) => {}
            Err((error, status)) => {
                return self.finish_early(call, started, &error, status, true);
            }
        }
        if cancel.is_cancelled() {
            let error = ToolError::new(
                codes::TOOL_CANCELLED,
                "the turn was cancelled before the tool ran",
            );
            return self.finish_early(call, started, &error, ToolStatus::Cancelled, false);
        }

        // 8: execute under the §6.6 lane, with timeout and cancellation.
        self.events.emit(EventData::ToolStarted {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
            parallel_index,
        });
        let ctx = ToolContext {
            session_id: env.session_id.clone(),
            turn_id: env.turn_id,
            workspace_root: self.boundary.root().to_path_buf(),
            cwd: env.cwd.clone(),
            mode: env.mode,
            permissions: Arc::clone(&self.policy),
            sandbox: Arc::clone(&self.sandbox),
            events: Arc::clone(&self.events),
            call_id: call.call_id.clone(),
            boundary: Arc::clone(&self.boundary),
            ignore: Arc::clone(&self.ignore),
            file_state: Arc::clone(&self.file_state),
            syntax: Arc::clone(&self.syntax),
            line_endings: self.line_endings,
            observer: Arc::clone(&self.observer),
            jobs: Arc::clone(&self.jobs),
            questioner: self.questioner.clone(),
            taint: Arc::clone(&self.taint),
        };
        let write_target = resolved
            .iter()
            .find(|(access, _)| *access == Access::Write)
            .map(|(_, r)| r.abs.clone());
        let outcome = self
            .in_lane(&tool, write_target, call.input.clone(), ctx, cancel)
            .await;

        // 9–10: truncate and scrub; 12: record.
        match outcome {
            Ok(output) => {
                let mut data = output.data;
                let mut truncated = output.truncated;
                let obscured = scrub(&mut data, &self.redactor);
                truncated |= fit(&mut data, tool.max_output_bytes() as usize);
                let bytes = data.to_string().len();
                let mut envelope = json!({
                    "ok": true,
                    "data": data,
                    "truncated": truncated,
                    "bytes": bytes,
                });
                if obscured {
                    envelope["warning"] =
                        json!("W-INJ-OBSCURE: hidden characters were removed from this output");
                }
                let paths_written: Vec<PathBuf> = resolved
                    .iter()
                    .filter(|(access, _)| *access == Access::Write)
                    .filter_map(|(_, r)| r.rel.as_ref().map(PathBuf::from))
                    .collect();
                let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.events.emit(EventData::ToolFinished {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    status: ToolStatus::Ok,
                    duration_ms,
                    output_bytes: u32::try_from(bytes).unwrap_or(u32::MAX),
                    truncated,
                    error: None,
                });
                ToolResult {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    ok: true,
                    envelope,
                    status: ToolStatus::Ok,
                    duration_ms,
                    truncated,
                    error_code: None,
                    denied: false,
                    paths_written,
                }
            }
            Err(error) => {
                let status = match error.code {
                    c if c == codes::TOOL_TIMEOUT => ToolStatus::Timeout,
                    c if c == codes::TOOL_CANCELLED => ToolStatus::Cancelled,
                    _ => ToolStatus::Error,
                };
                self.finish_early(call, started, &error, status, false)
            }
        }
    }

    /// Build the failure result for `error` and publish `tool.finished`.
    fn finish_early(
        &self,
        call: &ToolCall,
        started: Instant,
        error: &ToolError,
        status: ToolStatus,
        denied: bool,
    ) -> ToolResult {
        let mut error = error.clone();
        // Messages carry paths and command text; they go through the same
        // scrub as any output (§6.5 step 10 applies to errors too).
        error.message = self.redactor.redact(&error.message);
        let result = failure(call, started, &error, status, denied);
        if denied {
            self.events.emit(EventData::PermissionDenied {
                call_id: call.call_id.clone(),
                rule_id: error.code.to_string(),
                reason: error.message.clone(),
            });
        }
        self.events.emit(EventData::ToolFinished {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            status,
            duration_ms: result.duration_ms,
            output_bytes: 0,
            truncated: false,
            error: Some(error.code.to_string()),
        });
        result
    }

    /// Step 6, for tools that run shell commands: analyse the command and
    /// ask the policy about each command in it.
    fn judge_shell(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        env: &CallEnv,
    ) -> Result<Option<crate::shell::Outcome>, ToolError> {
        let Some(shell) = tool.shell_request(&call.input) else {
            return Ok(None);
        };
        let cwd = match &shell.cwd {
            Some(dir) => {
                self.boundary
                    .resolve(dir, &env.cwd, Access::Read)
                    .map_err(|e| {
                        ToolError::new(codes::PERM_DENIED, e.message)
                            .recovery("Run the command from a directory inside the workspace.")
                    })?
                    .abs
            }
            None => env.cwd.clone(),
        };
        let home = self
            .boundary
            .home()
            .map(|h| h.to_string_lossy().into_owned());
        let ctx = crate::shell::Ctx {
            boundary: &self.boundary,
            cwd: &cwd,
            home: home.as_deref(),
        };
        let analysis = crate::shell::analyze(&shell.command, &ctx);
        Ok(Some(crate::shell::evaluate(
            &analysis,
            &call.name,
            env.mode,
            &*self.policy,
            &shell.command,
        )))
    }

    /// Step 6: decide, and if the answer is "ask", ask.
    async fn authorise(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        request: &PermissionRequest,
        shell: Option<crate::shell::Outcome>,
        env: &CallEnv,
        resolved: &[(Access, crate::paths::Resolved)],
    ) -> Result<(), (ToolError, ToolStatus)> {
        let denied = |error: ToolError| (error, ToolStatus::Denied);
        // What an approval is about, and what "always" remembers, is the
        // command that decided — the one leaf — not the whole line.
        let remembered = shell.as_ref().map_or(request, |o| &o.request);
        let decision = shell
            .as_ref()
            .map_or_else(|| self.policy.decide(request), |o| o.decision.clone());
        match decision {
            Decision::Allow { .. } => Ok(()),
            Decision::Deny { rule_id, reason } => {
                if let Some(outcome) = &shell {
                    if let Some(code) = outcome.code {
                        return Err(denied(
                            ToolError::new(code, outcome.message.clone().unwrap_or(reason))
                                .recovery(
                                    "Do not retry this; choose another approach or ask the user.",
                                ),
                        ));
                    }
                    if let Some(message) = &outcome.message {
                        if env.mode != Mode::Plan {
                            return Err(denied(
                                ToolError::new(
                                    codes::PERM_DENIED,
                                    format!("Permission denied by rule {rule_id}: {message}"),
                                )
                                .recovery("Do not retry this action; choose another approach or ask the user."),
                            ));
                        }
                    }
                }
                let plan_write = env.mode == Mode::Plan
                    && (matches!(tool.side_effect(), SideEffect::Write | SideEffect::Execute)
                        || tool.permission_class() == PermissionClass::Write);
                if plan_write {
                    return Err(denied(
                        ToolError::new(
                            codes::PERM_MODE,
                            "Plan mode is read-only. This action was not executed.",
                        )
                        .recovery("Describe the change in your plan instead of making it."),
                    ));
                }
                Err(denied(
                    ToolError::new(
                        codes::PERM_DENIED,
                        format!("Permission denied by rule {rule_id}: {reason}"),
                    )
                    .recovery("Do not retry this action; choose another approach or ask the user."),
                ))
            }
            Decision::Ask { rule_id, reason } => {
                let summary = request
                    .command
                    .clone()
                    .or_else(|| request.url.clone())
                    .or_else(|| request.path.clone())
                    .unwrap_or_else(|| call.name.clone());
                let ask = ApprovalRequest {
                    request_id: format!("ap_{}", call.call_id),
                    call_id: call.call_id.clone(),
                    tool: call.name.clone(),
                    summary: self.redactor.redact(&summary),
                    // §9.7 mitigation 7: what will really be done, in the
                    // model's own words and bytes — never only a description.
                    detail: {
                        let mut input = call.input.clone();
                        crate::output::scrub(&mut input, &self.redactor);
                        crate::output::fit(&mut input, 8 * 1024);
                        json!({
                            "tool": call.name,
                            "paths": resolved.iter().map(|(_, r)| r.display()).collect::<Vec<_>>(),
                            "mode": env.mode.as_str(),
                            "rule": rule_id,
                            "reason": reason,
                            "input": input,
                        })
                    },
                    rule_id: rule_id.clone(),
                    reason,
                };
                self.events.emit(EventData::ApprovalRequested {
                    request_id: ask.request_id.clone(),
                    call_id: call.call_id.clone(),
                    kind: tool.permission_class().as_str().to_lowercase(),
                    summary: ask.summary.clone(),
                    detail: ask.detail.clone(),
                    expires_in_ms: u32::try_from(self.approval_timeout.as_millis())
                        .unwrap_or(u32::MAX),
                });
                let answer =
                    tokio::time::timeout(self.approval_timeout, self.approver.ask(ask.clone()))
                        .await;
                let Ok(answer) = answer else {
                    return Err(denied(
                        ToolError::new(
                            codes::PERM_TIMEOUT,
                            "No answer to the approval request in time; the action was not run.",
                        )
                        .recovery("Do not retry; ask the user in text."),
                    ));
                };
                self.events.emit(EventData::ApprovalAnswered {
                    request_id: ask.request_id,
                    answer: format!("{answer:?}").to_lowercase(),
                    rule: Some(rule_id),
                });
                match answer {
                    Answer::Once => Ok(()),
                    Answer::Session => {
                        let _ = self
                            .policy
                            .remember(remembered, Effect::Allow, Scope::Session);
                        Ok(())
                    }
                    Answer::Always => {
                        // A failure to write the rule must not become a
                        // failure to run what the user just approved.
                        if self
                            .policy
                            .remember(remembered, Effect::Allow, Scope::Project)
                            .is_err()
                        {
                            let _ = self
                                .policy
                                .remember(remembered, Effect::Allow, Scope::Session);
                        }
                        Ok(())
                    }
                    Answer::Deny | Answer::DenyAlways => {
                        if answer == Answer::DenyAlways {
                            let _ = self
                                .policy
                                .remember(remembered, Effect::Deny, Scope::Project);
                        }
                        Err(denied(
                            ToolError::new(codes::PERM_DENIED, "The user declined this action.")
                                .recovery("Do not retry it; propose something else."),
                        ))
                    }
                }
            }
        }
    }

    /// Run `tool` in its §6.6 lane, under its timeout, abandoning it if the
    /// token is raised.
    async fn in_lane(
        &self,
        tool: &Arc<dyn Tool>,
        write_target: Option<PathBuf>,
        input: Value,
        ctx: ToolContext,
        cancel: &CancellationToken,
    ) -> Result<crate::types::ToolOutput, ToolError> {
        // Guards live to the end of this function.
        let _read_permit;
        let _writer_barrier;
        let _path_guard;
        let _serial_barrier;
        let _serial_guard;
        match lane_of(tool.as_ref()) {
            Lane::Read => {
                _read_permit = self.reads.acquire().await.ok();
            }
            Lane::Writer => {
                _writer_barrier = Some(self.barrier.read().await);
                if let Some(path) = write_target {
                    let lock = self.path_lock(path);
                    _path_guard = Some(lock.lock_owned().await);
                }
            }
            Lane::Serial => {
                _serial_guard = Some(self.serial.lock().await);
                _serial_barrier = Some(self.barrier.write().await);
            }
        }
        let timeout = tool.max_timeout();
        let work = tool.execute(input, ctx, cancel.clone());
        let timed = tokio::time::timeout(timeout, work);
        let cancelled = async {
            loop {
                if cancel.is_cancelled() {
                    return;
                }
                tokio::time::sleep(CANCEL_POLL).await;
            }
        };
        tokio::select! {
            result = timed => match result {
                Ok(done) => done,
                Err(_) => Err(ToolError::new(
                    codes::TOOL_TIMEOUT,
                    format!("`{}` did not finish within {} ms", tool.name(), timeout.as_millis()),
                )
                .recovery("Try a smaller request.")),
            },
            () = cancelled => Err(ToolError::new(codes::TOOL_CANCELLED, "the call was cancelled")),
        }
    }
}
