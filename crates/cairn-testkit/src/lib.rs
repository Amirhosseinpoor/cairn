//! `cairn-testkit` — the eval harness (SPEC §13, §14.6, D-17).
//!
//! M1 delivers the harness itself and its three smoke tasks: scripted
//! providers driven through the real turn runner and session store, scored by
//! explicit [`Check`]s. The 30-task suite (T-EVAL-001..030) needs tools and
//! arrives with M2–M5 on the same [`Task`] shape; nothing here is a stand-in
//! for it, only the part that does not need tools.
//!
//! A task never touches the network or the user's data: providers are
//! [`MockProvider`]s playing inline scripts through the production wire
//! decoder, and sessions live under a scratch directory the caller names and
//! that is removed afterwards (REQ-OPS-005).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use cairn_agent::transcript::{self, SessionWriter};
use cairn_agent::turn::{run_turn, TurnEnd};
use cairn_core::cancel::CancellationToken;
use cairn_core::message::{Block, Message, StopReason};
use cairn_core::registry::ProviderKind;
use cairn_provider::{Capabilities, MockProvider, ModelRequest, RetryBudget, Step};
use cairn_session::{Header, Store};

/// One scripted exchange: the prompt, and what the fake provider says back.
#[derive(Debug, Clone)]
pub struct Turn {
    pub prompt: &'static str,
    pub script: Vec<Step>,
}

/// What a finished task must satisfy. `turn` is zero-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The turn completed (not failed or cancelled).
    Completed { turn: usize },
    /// The committed answer contains `needle`.
    AnswerContains { turn: usize, needle: &'static str },
    /// The turn stopped for `stop`.
    Stop { turn: usize, stop: StopReason },
    /// The provider's own usage was kept, not replaced by an estimate.
    UsageReported { turn: usize },
    /// The turn's answer holds exactly `count` tool calls.
    ToolCalls { turn: usize, count: usize },
    /// After a resume from disk, the context holds `count` messages.
    ResumedMessages { count: usize },
}

/// A scripted task (§14.6's shape, minus the fixture repository M1 has no
/// tools to use).
#[derive(Debug, Clone)]
pub struct Task {
    pub id: &'static str,
    pub title: &'static str,
    pub turns: Vec<Turn>,
    pub checks: Vec<Check>,
}

/// How one task went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub id: &'static str,
    pub title: &'static str,
    pub failures: Vec<String>,
    pub millis: u64,
}

impl TaskResult {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// A run of several tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub results: Vec<TaskResult>,
}

impl Report {
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(TaskResult::passed)
    }

    #[must_use]
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed()).count()
    }

    /// The machine form `--json` and baselines use.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "passed": self.passed(),
            "total": self.results.len(),
            "tasks": self.results.iter().map(|r| serde_json::json!({
                "id": r.id,
                "title": r.title,
                "passed": r.passed(),
                "failures": r.failures,
                "millis": r.millis,
            })).collect::<Vec<_>>(),
        })
    }
}

fn openai_chunk(delta: &str, finish: Option<&str>) -> Step {
    let finish = finish.map_or_else(|| "null".to_string(), |f| format!("\"{f}\""));
    Step::Payload(format!(
        r#"{{"id":"c","model":"smoke","choices":[{{"index":0,"delta":{delta},"finish_reason":{finish}}}]}}"#
    ))
}

/// A plain answer: text, a stop, the provider's usage, `[DONE]`.
#[must_use]
pub fn answer(text: &str, input: u32, output: u32) -> Vec<Step> {
    let content = serde_json::json!({ "content": text }).to_string();
    vec![
        openai_chunk(&content, None),
        openai_chunk("{}", Some("stop")),
        Step::Payload(format!(
            r#"{{"usage":{{"prompt_tokens":{input},"completion_tokens":{output}}},"choices":[]}}"#
        )),
        Step::Payload("[DONE]".to_string()),
    ]
}

/// An answer that is a single tool call with `arguments` (already JSON text).
#[must_use]
pub fn tool_call(name: &str, arguments: &str) -> Vec<Step> {
    let delta = serde_json::json!({
        "tool_calls": [{
            "index": 0,
            "id": "call_smoke",
            "type": "function",
            "function": { "name": name, "arguments": arguments }
        }]
    })
    .to_string();
    vec![
        openai_chunk(&delta, None),
        openai_chunk("{}", Some("tool_calls")),
        Step::Payload(
            r#"{"usage":{"prompt_tokens":9,"completion_tokens":4},"choices":[]}"#.to_string(),
        ),
        Step::Payload("[DONE]".to_string()),
    ]
}

/// The three M1 smoke tasks: answer, continuity, and a model asking for a
/// tool a tool-less run does not have.
#[must_use]
pub fn smoke_tasks() -> Vec<Task> {
    vec![
        Task {
            id: "SMOKE-001",
            title: "answer a question",
            turns: vec![Turn {
                prompt: "What is six times seven?",
                script: answer("Six times seven is 42.", 12, 7),
            }],
            checks: vec![
                Check::Completed { turn: 0 },
                Check::AnswerContains {
                    turn: 0,
                    needle: "42",
                },
                Check::Stop {
                    turn: 0,
                    stop: StopReason::EndTurn,
                },
                Check::UsageReported { turn: 0 },
                Check::ToolCalls { turn: 0, count: 0 },
            ],
        },
        Task {
            id: "SMOKE-002",
            title: "carry a conversation across turns and a resume",
            turns: vec![
                Turn {
                    prompt: "My name is Ada.",
                    script: answer("Nice to meet you, Ada.", 8, 6),
                },
                Turn {
                    prompt: "What is my name?",
                    script: answer("Your name is Ada.", 20, 5),
                },
            ],
            checks: vec![
                Check::Completed { turn: 0 },
                Check::Completed { turn: 1 },
                Check::AnswerContains {
                    turn: 1,
                    needle: "Ada",
                },
                Check::ResumedMessages { count: 4 },
            ],
        },
        Task {
            id: "SMOKE-003",
            title: "answer a tool call that has no tool to run",
            turns: vec![Turn {
                prompt: "Read src/main.rs",
                script: tool_call("read_file", r#"{"path":"src/main.rs"}"#),
            }],
            checks: vec![
                Check::Completed { turn: 0 },
                Check::Stop {
                    turn: 0,
                    stop: StopReason::ToolUse,
                },
                Check::ToolCalls { turn: 0, count: 1 },
                // user, assistant (with the call), and the error result.
                Check::ResumedMessages { count: 3 },
            ],
        },
    ]
}

struct TurnRecord {
    completed: bool,
    answer: String,
    stop: Option<StopReason>,
    usage_reported: bool,
    tool_calls: usize,
}

async fn drive(task: &Task, scratch: &Path) -> Result<(Vec<TurnRecord>, usize), String> {
    let store = Store::new(scratch.join("sessions"));
    let header = Header::new(
        format!("eval-{}", task.id),
        scratch.to_string_lossy(),
        "build",
        "smoke/model",
    );
    let mut writer = SessionWriter::create(store.clone(), &header).map_err(|e| e.to_string())?;
    let mut history: Vec<Message> = Vec::new();
    let mut records = Vec::new();

    for (index, turn) in task.turns.iter().enumerate() {
        let turn_id = u64::try_from(index + 1).map_err(|e| e.to_string())?;
        let user = Message::user(turn.prompt, turn_id);
        writer.turn_started(turn_id).map_err(|e| e.to_string())?;
        writer.message(&user).map_err(|e| e.to_string())?;
        history.push(user);

        let provider = Arc::new(MockProvider::new(
            ProviderKind::Openai,
            Capabilities::baseline(),
            turn.script.clone(),
        ));
        let request = ModelRequest::new("smoke/model", history.clone(), 256);
        let outcome = run_turn(
            provider,
            request,
            turn_id,
            CancellationToken::new(),
            RetryBudget::new(),
            None,
            |_| {},
        )
        .await;

        let completed = outcome.end == TurnEnd::Completed;
        let mut answer = String::new();
        let mut tool_calls = 0;
        if let Some(assistant) = &outcome.assistant {
            writer.message(assistant).map_err(|e| e.to_string())?;
            answer = assistant.text();
            tool_calls = assistant
                .blocks
                .iter()
                .filter(|b| matches!(b, Block::ToolCall { .. }))
                .count();
            history.push(assistant.clone());
            for result in transcript::unavailable_tool_results(assistant) {
                writer.message(&result).map_err(|e| e.to_string())?;
                history.push(result);
            }
        }
        writer
            .turn_ended(
                turn_id,
                if completed { "ok" } else { "error" },
                None,
                None,
                0,
            )
            .map_err(|e| e.to_string())?;
        records.push(TurnRecord {
            completed,
            answer,
            stop: outcome.stop,
            usage_reported: outcome.reported_usage.is_some_and(|u| !u.estimated),
            tool_calls,
        });
    }

    // Read it back the way `cairn resume` does, so persistence is scored too.
    let file = store.load(writer.path()).map_err(|e| e.to_string())?;
    let resumed = transcript::resume_state(&file).messages.len();
    Ok((records, resumed))
}

fn evaluate(task: &Task, records: &[TurnRecord], resumed: usize) -> Vec<String> {
    let mut failures = Vec::new();
    let turn_of = |turn: usize| records.get(turn);
    for check in &task.checks {
        let failed = match check {
            Check::Completed { turn } => turn_of(*turn)
                .map_or(Some("no such turn".to_string()), |t| {
                    (!t.completed).then(|| format!("turn {turn} did not complete"))
                }),
            Check::AnswerContains { turn, needle } => turn_of(*turn).and_then(|t| {
                (!t.answer.contains(needle))
                    .then(|| format!("turn {turn} answer lacks `{needle}`: `{}`", t.answer))
            }),
            Check::Stop { turn, stop } => turn_of(*turn).and_then(|t| {
                (t.stop != Some(*stop))
                    .then(|| format!("turn {turn} stopped {:?}, wanted {stop:?}", t.stop))
            }),
            Check::UsageReported { turn } => turn_of(*turn).and_then(|t| {
                (!t.usage_reported).then(|| format!("turn {turn} has no provider usage"))
            }),
            Check::ToolCalls { turn, count } => turn_of(*turn).and_then(|t| {
                (t.tool_calls != *count).then(|| {
                    format!(
                        "turn {turn} made {} tool calls, wanted {count}",
                        t.tool_calls
                    )
                })
            }),
            Check::ResumedMessages { count } => (resumed != *count)
                .then(|| format!("resume restored {resumed} messages, wanted {count}")),
        };
        if let Some(why) = failed {
            failures.push(why);
        }
    }
    failures
}

/// Run one task under `scratch` (created, used, and removed).
#[must_use]
pub fn run_task(task: &Task, scratch: &Path) -> TaskResult {
    let started = Instant::now();
    let dir: PathBuf = scratch.join(format!("{}-{}", task.id, std::process::id()));
    let outcome = std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
        .and_then(|()| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|e| format!("cannot start a runtime: {e}"))?;
            runtime.block_on(drive(task, &dir))
        });
    let _ = std::fs::remove_dir_all(&dir);
    let failures = match outcome {
        Ok((records, resumed)) => evaluate(task, &records, resumed),
        Err(why) => vec![why],
    };
    TaskResult {
        id: task.id,
        title: task.title,
        failures,
        millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// Run `tasks` in order.
#[must_use]
pub fn run_all(tasks: &[Task], scratch: &Path) -> Report {
    Report {
        results: tasks.iter().map(|task| run_task(task, scratch)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_smoke_tasks_pass_against_the_real_turn_loop() {
        let scratch = tempfile::tempdir().expect("tmp");
        let report = run_all(&smoke_tasks(), scratch.path());
        for result in &report.results {
            assert!(result.passed(), "{}: {:?}", result.id, result.failures);
        }
        assert_eq!(report.passed(), 3);
        assert!(report.all_passed());
    }

    /// The harness must be able to *fail*: a task whose script contradicts
    /// its checks reports each broken check, and nothing is left behind.
    #[test]
    fn a_wrong_expectation_is_reported_not_swallowed() {
        let scratch = tempfile::tempdir().expect("tmp");
        let task = Task {
            id: "NEG-001",
            title: "expects the wrong answer",
            turns: vec![Turn {
                prompt: "hi",
                script: answer("hello", 3, 1),
            }],
            checks: vec![
                Check::AnswerContains {
                    turn: 0,
                    needle: "goodbye",
                },
                Check::ToolCalls { turn: 0, count: 2 },
                Check::Stop {
                    turn: 0,
                    stop: StopReason::ToolUse,
                },
                Check::ResumedMessages { count: 99 },
                Check::Completed { turn: 5 },
            ],
        };
        let result = run_task(&task, scratch.path());
        assert_eq!(result.failures.len(), 5, "{:?}", result.failures);
        assert!(!result.passed());
        assert_eq!(
            std::fs::read_dir(scratch.path()).expect("dir").count(),
            0,
            "the scratch directory is cleaned up"
        );
    }

    #[test]
    fn a_failing_provider_fails_the_task_it_does_not_hang() {
        let scratch = tempfile::tempdir().expect("tmp");
        let task = Task {
            id: "NEG-002",
            title: "provider rejects the key",
            turns: vec![Turn {
                prompt: "hi",
                script: vec![Step::SetupError {
                    fault: cairn_provider::ProviderFault::Auth,
                    message: "no".into(),
                }],
            }],
            checks: vec![Check::Completed { turn: 0 }],
        };
        let result = run_task(&task, scratch.path());
        assert_eq!(result.failures, vec!["turn 0 did not complete".to_string()]);
    }

    #[test]
    fn the_report_serialises_for_doctor_and_baselines() {
        let scratch = tempfile::tempdir().expect("tmp");
        let report = run_all(&smoke_tasks()[..1], scratch.path());
        let json = report.to_json();
        assert_eq!(json["passed"], 1);
        assert_eq!(json["total"], 1);
        assert_eq!(json["tasks"][0]["id"], "SMOKE-001");
        assert_eq!(json["tasks"][0]["passed"], true);
    }
}
