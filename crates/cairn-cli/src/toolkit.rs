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
    ExecutorParts, Question, Questioner, Registry, Reply,
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
    /// Checkpoints (§9.8): told about every write.
    pub observer: Option<Arc<dyn cairn_git::WriteObserver>>,
    /// The interactive session answers approvals and questions itself.
    pub approver: Option<Arc<dyn Approver>>,
    pub questioner: Option<Arc<dyn Questioner>>,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// A volume that folds case: protected-path and rule globs must too.
const CASE_INSENSITIVE_FS: bool = cfg!(any(windows, target_os = "macos"));

/// The ignore rules of §5.1 for `root`, with the user's global ignore file and
/// the `discovery.include`/`exclude` globs.
#[must_use]
pub fn ignore_engine(config: &Config, root: &Path) -> IgnoreEngine {
    let home = expand_tilde("~", &env);
    let xdg = env("XDG_CONFIG_HOME").map(PathBuf::from);
    IgnoreEngine::new(
        root,
        &IgnoreOptions {
            global_file: Some(default_global_ignore(&home, xdg.as_deref())),
            include: config.discovery.include.clone(),
            exclude: config.discovery.exclude.clone(),
        },
    )
}

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

    let ignore = Arc::new(ignore_engine(config, boundary.root()));

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
    let web = builtin::web::WebFetch::new(builtin::web::Policy {
        allow_hosts: config.network.allow_hosts.clone(),
        offline: config.network.offline,
        allow_loopback: false,
    });
    builtin::register_with(&mut registry, web).map_err(|e| {
        Fail::new(
            codes::LOOP_INVARIANT,
            ExitStatus::Generic,
            format!("tool registration failed: {e}"),
            None,
        )
    })?;

    let approver: Arc<dyn Approver> = if let Some(approver) = &wiring.approver {
        Arc::clone(approver)
    } else if wiring.allow_ask {
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
        syntax: Some(Arc::new(cairn_tools::ParseCheck)),
        observer: wiring.observer.clone(),
        questioner: if wiring.questioner.is_some() {
            wiring.questioner.clone()
        } else if wiring.allow_ask {
            Some(Arc::new(StdinQuestioner) as Arc<dyn Questioner>)
        } else {
            None
        },
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

/// `--allow-ask`: `ask_user` questions go to stderr as one JSON line and the
/// answer comes back on stdin as `{"answer": "...", "selected_option": 2}`.
/// End of input is "nobody there", never an invented answer.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdinQuestioner;

/// What a stdin answer line means for a question with `options` options.
#[must_use]
pub fn parse_reply(line: &str, options: usize) -> Option<Reply> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let selected = value
        .get("selected_option")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (1..=options).contains(n));
    let answer = value
        .get("answer")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    match (answer, selected) {
        (Some(answer), selected) => Some(Reply {
            answer,
            selected_option: selected,
        }),
        (None, Some(n)) => Some(Reply {
            answer: String::new(),
            selected_option: Some(n),
        }),
        (None, None) => None,
    }
}

impl Questioner for StdinQuestioner {
    fn ask(&self, question: Question) -> BoxFuture<'_, Option<Reply>> {
        async move {
            eprintln!(
                "{}",
                serde_json::json!({
                    "type": "question.request",
                    "question": question.text,
                    "options": question.options,
                    "allow_free_text": question.allow_free_text,
                })
            );
            let count = question.options.len();
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                match std::io::stdin().lock().read_line(&mut line) {
                    Ok(n) if n > 0 => parse_reply(line.trim(), count),
                    _ => None,
                }
            })
            .await
            .ok()
            .flatten()
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_parse_and_a_bad_line_is_no_answer() {
        let r = parse_reply(r#"{"answer":"blue"}"#, 0).expect("reply");
        assert_eq!((r.answer.as_str(), r.selected_option), ("blue", None));
        let r = parse_reply(r#"{"selected_option":2}"#, 3).expect("reply");
        assert_eq!(r.selected_option, Some(2));
        // An option that does not exist is ignored; with no text either, no answer.
        assert!(parse_reply(r#"{"selected_option":9}"#, 3).is_none());
        assert!(parse_reply("not json", 3).is_none());
        assert!(parse_reply("{}", 3).is_none());
    }

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
