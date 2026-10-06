//! `bash` (SPEC §6.2.8, §6.4): run a command line in the user's shell.
//!
//! Nothing here decides whether the command may run: the pipeline has already
//! analysed it (`shell::analyze`), asked the policy about every command in it
//! and, where the answer was "ask", asked the person. What is left is doing it
//! well: a clean environment, a process group that can be killed whole, output
//! that is capped rather than fatal, and a result the model can act on.

use std::collections::BTreeMap;
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{object_schema, parse};
use crate::shell::interactive;
use crate::shell::proc::{child_env, pick_shell, run, Captured, Ending, Spec};
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PermissionClass, RequestInfo, ShellRequest, SideEffect, ToolContext,
    ToolError, ToolOutput,
};

/// §6.4.4: combined stdout and stderr kept per call. Below the tool's
/// 64 KiB result limit, leaving room for the JSON around it.
const OUTPUT_CAP: usize = 56 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    env: Option<BTreeMap<String, String>>,
    background: Option<bool>,
    #[serde(rename = "input")]
    stdin: Option<String>,
    tty: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Bash;

impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the workspace and return its stdout, stderr and exit code. \
         Commands run without a terminal and with a minimal environment. Output beyond about \
         56 KiB is dropped, not fatal. A command that runs past `timeout_ms` (default 2 minutes, \
         at most 10) is stopped with its whole process group and what it printed so far is \
         kept. Prefer the file tools to read, search and edit; use this for builds, tests and \
         other programs. For anything long-running use `background: true` and read it with \
         `job_output`."
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
                "timeout_ms": {"type": "integer", "minimum": 1000, "maximum": 600_000, "default": 120_000},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "default": {}},
                "background": {"type": "boolean", "default": false},
                "input": {"type": ["string", "null"], "default": null},
                "tty": {"type": ["boolean", "null"], "default": null}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "job_id": {"type": ["string", "null"]},
            "exit_code": {"type": ["integer", "null"]},
            "signal": {"type": ["integer", "null"]},
            "stdout": {"type": "string"},
            "stderr": {"type": "string"},
            "duration_ms": {"type": "integer"},
            "truncated": {"type": "boolean"},
            "command": {"type": "string"},
            "cwd": {"type": "string"},
            "shell": {"type": "string"}
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
        // The call's own `timeout_ms` (at most 600 s) governs; this is the
        // outer bound with room for the kill sequence.
        Duration::from_secs(610)
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
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: Input = parse("bash", input)?;
            if input.background == Some(true) {
                return super::jobs::start(&input.command, input.cwd.as_deref(), input.env, &ctx);
            }
            tokio::task::spawn_blocking(move || foreground(&input, &ctx, &cancel))
                .await
                .map_err(|e| {
                    ToolError::new(codes::SHELL_NOEXEC, format!("the command task failed: {e}"))
                })?
        }
        .boxed()
    }
}

/// The directory a command runs in, checked against the boundary again at
/// execution time (REQ-SAFE-007).
pub fn working_dir(
    ctx: &ToolContext,
    requested: Option<&str>,
) -> Result<std::path::PathBuf, ToolError> {
    let Some(requested) = requested else {
        return Ok(ctx.cwd.clone());
    };
    let resolved = ctx
        .boundary
        .resolve(requested, &ctx.cwd, Access::Read)
        .map_err(|e| {
            ToolError::new(codes::PERM_DENIED, e.message)
                .recovery("Run the command from a directory inside the workspace.")
        })?;
    if !resolved.abs.is_dir() {
        return Err(
            ToolError::new(codes::FS_DIR, format!("`{requested}` is not a directory."))
                .recovery("Pass an existing directory as cwd."),
        );
    }
    Ok(resolved.abs)
}

fn foreground(
    input: &Input,
    ctx: &ToolContext,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    if input.tty == Some(true) {
        return Err(ToolError::new(
            codes::SHELL_PTY,
            "A terminal (PTY) is not available in this build; commands run with pipes.",
        )
        .recovery(
            "Run the command without `tty`, or use `background: true` and read it with job_output.",
        ));
    }
    let cwd = working_dir(ctx, input.cwd.as_deref())?;
    let shell = pick_shell()?;
    let timeout = Duration::from_millis(input.timeout_ms.unwrap_or(120_000).clamp(1000, 600_000));
    let extra = input.env.clone().unwrap_or_default();
    let spec = Spec {
        command: input.command.clone(),
        cwd: cwd.clone(),
        env: child_env(&extra),
        stdin: input.stdin.clone(),
    };
    let started = std::time::Instant::now();
    let done = run(&shell, &spec, timeout, OUTPUT_CAP, cancel)?;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut data = report(input, &cwd, shell.display(), &done, duration_ms);
    match done.ending {
        Ending::TimedOut => Err(failure(
            codes::SHELL_TIMEOUT,
            format!(
                "Command timed out after {} ms; the process group was killed (SIGTERM, then SIGKILL after 2s). Output kept.",
                timeout.as_millis()
            ),
            "Run it with `background: true`, or raise timeout_ms (at most 600000).",
            data,
        )),
        Ending::Cancelled => Err(failure(
            codes::TOOL_CANCELLED,
            "The command was cancelled and its process group was killed.".to_string(),
            "The user stopped it; do not retry unless asked.",
            data,
        )),
        Ending::Exited => match done.exit_code {
            Some(0) => {
                if done.truncated {
                    data["warnings"] = json!(["W-SHELL-DISCARDED"]);
                }
                Ok(ToolOutput {
                    data,
                    truncated: done.truncated,
                })
            }
            code => Err(failure(
                codes::SHELL_EXITNONZERO,
                match (code, done.signal) {
                    (Some(c), _) => format!("The command exited with status {c}."),
                    (None, Some(s)) => format!("The command was killed by signal {s}."),
                    (None, None) => "The command did not report an exit status.".to_string(),
                },
                "Read stdout and stderr in `data` to see why.",
                data,
            )),
        },
    }
}

fn failure(code: &'static str, message: String, recovery: &str, data: Value) -> ToolError {
    let mut error = ToolError::new(code, message).recovery(recovery);
    error.data = Some(Box::new(data));
    error
}

fn report(
    input: &Input,
    cwd: &std::path::Path,
    shell: &str,
    done: &Captured,
    duration_ms: u64,
) -> Value {
    let mut stdout = done.stdout.clone();
    if done.truncated {
        stdout.push_str("\n[output dropped: W-SHELL-DISCARDED]");
    }
    let mut data = json!({
        "job_id": Value::Null,
        "exit_code": done.exit_code,
        "signal": done.signal,
        "stdout": stdout,
        "stderr": done.stderr,
        "duration_ms": duration_ms,
        "truncated": done.truncated,
        "command": input.command,
        "cwd": cwd.to_string_lossy().replace('\\', "/"),
        "shell": shell,
    });
    if interactive::is_interactive(&input.command) {
        data["hint"] = json!(
            "This looks like an interactive program. It ran without a terminal and stdin was closed."
        );
    }
    data
}
