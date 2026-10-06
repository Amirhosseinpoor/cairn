//! Wiring the tool layer from configuration: the registry, the permission
//! policy and its files, the filesystem boundary, the ignore engine, and
//! whoever answers approvals.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cairn_config::{expand_tilde, Config, Paths};
use cairn_core::error::{codes, ExitStatus};
use cairn_core::Mode;
use cairn_perm::{PermissionPolicy, PolicyFiles, RulePolicy};
use cairn_sandbox::PathChecksOnly;
use cairn_search::{default_global_ignore, IgnoreEngine, IgnoreOptions};
use cairn_tools::{
    builtin, Answer, ApprovalRequest, Approver, Boundary, DenyAll, EventSink, Executor,
    ExecutorParts, Registry,
};
use futures::future::BoxFuture;
use futures::FutureExt;

use crate::output::Fail;

/// Everything `build` needs.
pub struct Wiring<'a> {
    pub config: &'a Config,
    pub paths: &'a Paths,
    pub workspace: &'a Path,
    pub mode: Mode,
    pub events: Arc<dyn EventSink>,
    /// `--allow-ask`: approvals are answered from stdin.
    pub allow_ask: bool,
    pub quiet: bool,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// A volume that folds case: protected-path and rule globs must too.
const CASE_INSENSITIVE_FS: bool = cfg!(any(windows, target_os = "macos"));

/// Build the executor for one invocation.
///
/// # Errors
/// A [`Fail`] when the workspace is unusable or a permission file exists but
/// cannot be understood (exit 2: silently running without someone's deny
/// rules is the one outcome this must not have).
pub fn build(wiring: &Wiring<'_>) -> Result<Arc<Executor>, Fail> {
    let config = wiring.config;
    let home = expand_tilde("~", &env);
    let additional: Vec<PathBuf> = config
        .security
        .additional_dirs
        .iter()
        .map(|dir| expand_tilde(dir, &env))
        .collect();
    let boundary = Boundary::new(
        wiring.workspace,
        &additional,
        config.security.additional_dirs_writable,
        Some(home.clone()),
        &config.security.allow_protected_paths,
        CASE_INSENSITIVE_FS,
    )
    .map_err(|e| Fail::new(e.code, ExitStatus::Generic, e.message, e.recovery))?;
    let boundary = Arc::new(boundary);

    let xdg = env("XDG_CONFIG_HOME").map(PathBuf::from);
    let ignore = Arc::new(IgnoreEngine::new(
        boundary.root(),
        &IgnoreOptions {
            global_file: Some(default_global_ignore(&home, xdg.as_deref())),
            include: config.discovery.include.clone(),
            exclude: config.discovery.exclude.clone(),
        },
    ));

    let files = PolicyFiles {
        project: Some(wiring.workspace.join(".cairn").join("permissions.json")),
        user: Some(wiring.paths.config_home.join("permissions.json")),
    };
    let policy = RulePolicy::new(
        wiring.mode,
        files,
        Some(home.to_string_lossy().into_owned()),
        CASE_INSENSITIVE_FS,
    )
    .map_err(|e| {
        Fail::new(
            codes::PERM_BADPARSE,
            ExitStatus::Usage,
            e.to_string(),
            Some("fix or remove the permissions file".to_string()),
        )
    })?;
    if !wiring.quiet {
        for skipped in policy.skipped() {
            crate::output::warn_line(
                skipped.code,
                &format!(
                    "ignored permission rule {}: {}",
                    skipped.id.as_deref().unwrap_or("?"),
                    skipped.reason
                ),
            );
        }
    }

    let mut registry = Registry::new();
    builtin::register_all(&mut registry).map_err(|e| {
        Fail::new(
            codes::LOOP_INVARIANT,
            ExitStatus::Generic,
            format!("tool registration failed: {e}"),
            None,
        )
    })?;

    let approver: Arc<dyn Approver> = if wiring.allow_ask {
        Arc::new(StdinApprover)
    } else {
        Arc::new(DenyAll)
    };
    Ok(Arc::new(Executor::new(ExecutorParts {
        registry: Arc::new(registry),
        policy: Arc::new(policy) as Arc<dyn PermissionPolicy>,
        approver,
        sandbox: Arc::new(PathChecksOnly),
        events: Arc::clone(&wiring.events),
        boundary,
        ignore,
        redactor: Arc::new(crate::log::redactor_always(config)),
        syntax: None,
        line_endings: match config.line_endings {
            cairn_config::LineEndingsSetting::Auto => cairn_tools::LineEndings::Auto,
            cairn_config::LineEndingsSetting::Lf => cairn_tools::LineEndings::Lf,
            cairn_config::LineEndingsSetting::Crlf => cairn_tools::LineEndings::Crlf,
        },
        approval_timeout: Duration::from_millis(config.permissions.ask_timeout_ms),
    })))
}

/// `--allow-ask` (§11.1): approvals are written to stderr as one JSON line
/// each and answered on stdin as `{"request_id": "...", "answer": "..."}`.
/// End of input, a malformed line, or an unknown answer is a refusal — an
/// approval is never assumed.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdinApprover;

/// What a stdin answer line means.
#[must_use]
pub fn parse_answer(line: &str, request_id: &str) -> Answer {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Answer::Deny;
    };
    if value
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|id| id != request_id)
    {
        return Answer::Deny;
    }
    match value.get("answer").and_then(serde_json::Value::as_str) {
        Some("once") => Answer::Once,
        Some("session") => Answer::Session,
        Some("always") => Answer::Always,
        Some("deny_always") => Answer::DenyAlways,
        _ => Answer::Deny,
    }
}

impl Approver for StdinApprover {
    fn ask(&self, request: ApprovalRequest) -> BoxFuture<'_, Answer> {
        async move {
            eprintln!(
                "{}",
                serde_json::json!({
                    "type": "approval.request",
                    "request_id": request.request_id,
                    "tool": request.tool,
                    "summary": request.summary,
                    "rule": request.rule_id,
                    "reason": request.reason,
                    "answers": ["once", "session", "always", "deny", "deny_always"],
                })
            );
            let id = request.request_id.clone();
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                match std::io::stdin().lock().read_line(&mut line) {
                    Ok(n) if n > 0 => parse_answer(line.trim(), &id),
                    _ => Answer::Deny,
                }
            })
            .await
            .unwrap_or(Answer::Deny)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse_and_everything_unclear_is_a_refusal() {
        for (line, want) in [
            (r#"{"request_id":"r1","answer":"once"}"#, Answer::Once),
            (r#"{"answer":"session"}"#, Answer::Session),
            (r#"{"request_id":"r1","answer":"always"}"#, Answer::Always),
            (
                r#"{"request_id":"r1","answer":"deny_always"}"#,
                Answer::DenyAlways,
            ),
            (r#"{"request_id":"r1","answer":"deny"}"#, Answer::Deny),
            (r#"{"request_id":"r1","answer":"yes please"}"#, Answer::Deny),
            (r#"{"request_id":"other","answer":"once"}"#, Answer::Deny),
            ("not json", Answer::Deny),
            ("", Answer::Deny),
            (r#"{"request_id":"r1"}"#, Answer::Deny),
        ] {
            assert_eq!(parse_answer(line, "r1"), want, "{line}");
        }
    }
}
