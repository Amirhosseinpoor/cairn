//! `bash_background`, `job_output` and `job_kill` (SPEC §6.2.9–§6.2.11).

use std::collections::BTreeMap;
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::event::EventData;
use cairn_sandbox::process::Signal;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::bash::working_dir;
use super::common::{object_schema, parse};
use crate::shell::proc::{child_env, pick_shell, Spec};
use crate::tool::Tool;
use crate::types::{
    Idempotency, PermissionClass, RequestInfo, ShellRequest, SideEffect, ToolContext, ToolError,
    ToolOutput,
};

/// Start `command` as a job and describe it.
pub fn start(
    command: &str,
    cwd: Option<&str>,
    env: Option<BTreeMap<String, String>>,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    start_labelled(command, cwd, env, None, ctx)
}

fn start_labelled(
    command: &str,
    cwd: Option<&str>,
    env: Option<BTreeMap<String, String>>,
    label: Option<String>,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    let cwd = working_dir(ctx, cwd)?;
    let shell = pick_shell()?;
    let label = label.unwrap_or_else(|| command.chars().take(40).collect());
    let spec = Spec {
        command: command.to_string(),
        cwd,
        env: child_env(&env.unwrap_or_default()),
        stdin: None,
    };
    let job = ctx
        .jobs
        .start(&shell, &spec, &label, std::sync::Arc::clone(&ctx.events))?;
    ctx.events.emit(EventData::JobStarted {
        job_id: job.id.clone(),
        label: job.label.clone(),
        pid: job.pid,
    });
    Ok(ToolOutput {
        data: json!({
            "job_id": job.id,
            "pid": job.pid,
            "started_at": job.started_at,
            "label": job.label,
        }),
        truncated: false,
    })
}

// ------------------------------------------------------------ bash_background

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackgroundInput {
    command: String,
    cwd: Option<String>,
    env: Option<BTreeMap<String, String>>,
    label: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BashBackground;

impl Tool for BashBackground {
    fn name(&self) -> &'static str {
        "bash_background"
    }

    fn description(&self) -> &'static str {
        "Start a shell command that keeps running after this call returns (a dev server, a \
         watcher, a long build) and get back a job id. Read its output with `job_output` and \
         stop it with `job_kill`. At most 8 jobs run at once."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["command"],
            "additionalProperties": false,
            "properties": {
                "command": {"type": "string", "minLength": 1, "maxLength": 20000},
                "cwd": {"type": "string"},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "label": {"type": "string", "maxLength": 80}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "job_id": {"type": "string"},
            "pid": {"type": "integer"},
            "started_at": {"type": "string"},
            "label": {"type": "string"}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Execute
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::Execute
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn max_output_bytes(&self) -> u32 {
        64 * 1024
    }

    fn requires_serial(&self) -> bool {
        true
    }

    fn request_info(&self, input: &Value) -> RequestInfo {
        RequestInfo {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: None,
        }
    }

    fn shell_request(&self, input: &Value) -> Option<ShellRequest> {
        Some(ShellRequest {
            command: input.get("command")?.as_str()?.to_string(),
            cwd: input.get("cwd").and_then(Value::as_str).map(str::to_string),
        })
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: BackgroundInput = parse("bash_background", input)?;
            tokio::task::spawn_blocking(move || {
                start_labelled(
                    &input.command,
                    input.cwd.as_deref(),
                    input.env,
                    input.label,
                    &ctx,
                )
            })
            .await
            .map_err(|e| {
                ToolError::new(codes::SHELL_NOEXEC, format!("the start task failed: {e}"))
            })?
        }
        .boxed()
    }
}

// ----------------------------------------------------------------- job_output

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputInput {
    job_id: String,
    since_line: Option<u64>,
    wait_ms: Option<u64>,
    max_lines: Option<usize>,
}

fn not_found(id: &str) -> ToolError {
    ToolError::new(codes::JOB_NOTFOUND, format!("there is no job `{id}`."))
        .recovery("Use the job_id that bash_background returned.")
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JobOutput;

impl Tool for JobOutput {
    fn name(&self) -> &'static str {
        "job_output"
    }

    fn description(&self) -> &'static str {
        "Read the output of a background job from a line number on. Pass the \
         `next_since_line` of the previous call to get only what is new. With `wait_ms` it \
         waits for new output or for the job to end, which is how to follow a job without \
         polling."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["job_id"],
            "additionalProperties": false,
            "properties": {
                "job_id": {"type": "string"},
                "since_line": {"type": "integer", "minimum": 0, "default": 0},
                "wait_ms": {"type": "integer", "minimum": 0, "maximum": 60000, "default": 0},
                "max_lines": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 500}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "job_id": {"type": "string"},
            "lines": {"type": "array"},
            "next_since_line": {"type": "integer"},
            "running": {"type": "boolean"},
            "exit_code": {"type": ["integer", "null"]},
            "bytes_total": {"type": "integer"}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Execute
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Safe
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(70)
    }

    fn max_output_bytes(&self) -> u32 {
        64 * 1024
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: OutputInput = parse("job_output", input)?;
            let job = ctx
                .jobs
                .get(&input.job_id)
                .ok_or_else(|| not_found(&input.job_id))?;
            let wait = Duration::from_millis(input.wait_ms.unwrap_or(0).min(60_000));
            let since = input.since_line.unwrap_or(0);
            let max = input.max_lines.unwrap_or(500).clamp(1, 5000);
            let snap = tokio::task::spawn_blocking(move || job.output(since, max, wait, &cancel))
                .await
                .map_err(|e| {
                    ToolError::new(codes::JOB_NOTFOUND, format!("the read task failed: {e}"))
                })?;
            let lines: Vec<Value> = snap
                .lines
                .iter()
                .map(|l| json!({"n": l.n, "stream": l.stream, "text": l.text}))
                .collect();
            Ok(ToolOutput {
                data: json!({
                    "job_id": input.job_id,
                    "lines": lines,
                    "next_since_line": snap.next_since_line,
                    "running": snap.status == crate::shell::jobs::Status::Running,
                    "status": snap.status.as_str(),
                    "exit_code": snap.exit_code,
                    "bytes_total": snap.bytes_total,
                }),
                truncated: false,
            })
        }
        .boxed()
    }
}

// ------------------------------------------------------------------- job_kill

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KillInput {
    job_id: String,
    signal: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JobKill;

impl Tool for JobKill {
    fn name(&self) -> &'static str {
        "job_kill"
    }

    fn description(&self) -> &'static str {
        "Stop a background job by signalling its whole process group (SIGTERM by default)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["job_id"],
            "additionalProperties": false,
            "properties": {
                "job_id": {"type": "string"},
                "signal": {"enum": ["SIGTERM", "SIGINT", "SIGKILL"], "default": "SIGTERM"}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "job_id": {"type": "string"},
            "killed": {"type": "boolean"},
            "exit_code": {"type": ["integer", "null"]},
            "signal": {"type": "string"}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Execute
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::Execute
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Retryable
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn max_output_bytes(&self) -> u32 {
        4 * 1024
    }

    fn requires_serial(&self) -> bool {
        true
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: KillInput = parse("job_kill", input)?;
            let job = ctx
                .jobs
                .get(&input.job_id)
                .ok_or_else(|| not_found(&input.job_id))?;
            let signal = match input.signal.as_deref() {
                Some("SIGINT") => Signal::Int,
                Some("SIGKILL") => Signal::Kill,
                _ => Signal::Term,
            };
            let killed = job.kill(signal).map_err(|why| {
                ToolError::new(
                    codes::JOB_NOTFOUND,
                    format!("could not signal the job: {why}"),
                )
            })?;
            Ok(ToolOutput {
                data: json!({
                    "job_id": input.job_id,
                    "killed": killed,
                    "exit_code": job.exit_code(),
                    "signal": signal.name(),
                }),
                truncated: false,
            })
        }
        .boxed()
    }
}
