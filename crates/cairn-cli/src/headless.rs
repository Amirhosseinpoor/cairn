//! `cairn run` — one headless turn over a persisted session (SPEC §7.7).
//!
//! The order is the one §8.1 draws: open (or continue) the session, make the
//! user's prompt durable, run the model turn, make the answer durable, then
//! render. Rendering never decides anything: exit codes and the JSON
//! document are derived from [`TurnOutcome`] and the write results.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use cairn_agent::transcript::{self, status, Recovery, ResumeState, SessionWriter};
use cairn_agent::turn::{run_turn, TurnEnd, TurnOutcome};
use cairn_agent::usage::{usage_report, UsageReport};
use cairn_core::cancel::CancellationToken;
use cairn_core::error::ExitStatus;
use cairn_core::event::{Event, EventData, TurnStatus};
use cairn_core::message::{Block, Message, Role, StopReason};
use cairn_core::SessionId;
use cairn_provider::{estimate_request, ModelRequest, ProviderError, StreamEvent};
use cairn_session::{Header, Store};

use crate::output::Fail;
use crate::provide::LiveProvider;

/// `--output` for `run`: `text` streams the answer, `json` prints one
/// document at the end, `stream-json` prints one §3.5 envelope per line
/// (T-CLI-017).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

/// Everything one headless turn needs, resolved by `run` before any I/O.
pub struct Plan {
    /// `None` re-issues the model call over the stored history (§8.7 `r`).
    pub prompt: Option<String>,
    pub format: OutputFormat,
    pub live: LiveProvider,
    pub store: Store,
    pub workspace: PathBuf,
    pub mode: String,
    /// `--session ID`: continue this session instead of creating one.
    pub session: Option<String>,
    /// `--input` messages, already parsed.
    pub input: Vec<Message>,
    /// `session.auto_recover` (§8.7): close an interrupted turn as `r`
    /// (rebuild) rather than `k` (keep).
    pub auto_recover: bool,
    pub quiet: bool,
    /// Where the in-flight-turn marker goes (`cairn update` reads it).
    pub cache_home: PathBuf,
}

/// `--input-fmt`: `text` is one user context message; `json`/`jsonl` is one
/// `{"role": "user"|"assistant", "content": "..."}` object per line.
///
/// # Errors
/// `E-CLI-USAGE` (exit 2) naming the offending line.
pub fn parse_input(text: &str, format: &str, turn_id: u64) -> Result<Vec<Message>, Fail> {
    match format {
        "text" => {
            if text.trim().is_empty() {
                return Ok(Vec::new());
            }
            Ok(vec![Message::user(text.trim_end(), turn_id)])
        }
        "json" | "jsonl" => text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                let bad = |why: &str| {
                    Fail::usage(
                        format!("--input line {}: {why}", index + 1),
                        r#"each line must be {"role":"user"|"assistant","content":"..."}"#
                            .to_string(),
                    )
                };
                let value: serde_json::Value =
                    serde_json::from_str(line).map_err(|e| bad(&e.to_string()))?;
                let role = match value.get("role").and_then(serde_json::Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => return Err(bad("`role` must be \"user\" or \"assistant\"")),
                };
                let content = value
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| bad("`content` must be a string"))?;
                Ok(Message::new(
                    role,
                    vec![Block::Text {
                        text: content.to_string(),
                    }],
                    turn_id,
                ))
            })
            .collect(),
        other => Err(Fail::usage(
            format!("--input-fmt {other} is not a format"),
            "use text or json".to_string(),
        )),
    }
}

fn flush_fail(error: cairn_core::CairnError) -> Fail {
    Fail::from_cairn(error)
}

/// Open the session this run writes to: the one named by `--session`
/// (recovering a dangling turn first), or a new one.
fn open_session(plan: &Plan) -> Result<(SessionWriter, ResumeState, bool), Fail> {
    if let Some(id) = &plan.session {
        let path = plan.store.find(id).ok_or_else(|| {
            Fail::not_found(
                format!("session '{id}' not found"),
                "run `cairn sessions` to list stored sessions".to_string(),
            )
        })?;
        let file = plan.store.load(&path).map_err(Fail::from_cairn)?;
        let mut writer = SessionWriter::resume(plan.store.clone(), &file);
        let state = transcript::resume_state(&file);
        if let Some(dangling) = &state.dangling {
            if !plan.quiet {
                eprintln!("{}", dangling.banner());
            }
            // Headless cannot ask r/d/k. The new prompt is sent over the
            // committed messages either way, which *is* the re-issue `r`
            // describes, so the only difference is how the turn is closed.
            let choice = if plan.auto_recover {
                Recovery::Rebuild
            } else {
                Recovery::Keep
            };
            transcript::recover(&mut writer, &state, choice).map_err(flush_fail)?;
            let file = plan.store.load(&path).map_err(Fail::from_cairn)?;
            let state = transcript::resume_state(&file);
            return Ok((writer, state, false));
        }
        return Ok((writer, state, false));
    }
    let header = Header::new(
        SessionId::new().to_string(),
        plan.workspace.to_string_lossy(),
        plan.mode.clone(),
        plan.live.model_id.clone(),
    );
    let writer = SessionWriter::create(plan.store.clone(), &header).map_err(flush_fail)?;
    let file = plan.store.load(writer.path()).map_err(Fail::from_cairn)?;
    Ok((writer, transcript::resume_state(&file), true))
}

/// Writes §3.5 envelopes to stdout — and only envelopes (REQ-MODE-012).
struct Emitter {
    session: String,
    seq: u64,
}

impl Emitter {
    fn emit(&mut self, data: EventData) {
        self.seq += 1;
        let event = Event::new(data, self.seq, Some(self.session.clone()));
        println!(
            "{}",
            serde_json::to_string(&event).expect("events serialise")
        );
    }
}

fn turn_status(end: &TurnEnd) -> TurnStatus {
    match end {
        TurnEnd::Completed => TurnStatus::Ok,
        TurnEnd::Failed => TurnStatus::Error,
        TurnEnd::Cancelled => TurnStatus::Cancelled,
    }
}

fn status_name(end: &TurnEnd) -> &'static str {
    match end {
        TurnEnd::Completed => status::OK,
        TurnEnd::Failed => status::ERROR,
        TurnEnd::Cancelled => status::CANCELLED,
    }
}

fn stop_name(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ContentFilter => "content_filter",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

/// Run one headless turn; returns the process exit code (0 / 7) or the
/// failure to report (3 provider, 9 session, 13 flush, ...).
///
/// # Errors
/// Any [`Fail`], already carrying its stable code and exit status.
pub async fn execute(plan: Plan, cancel: CancellationToken) -> Result<i32, Fail> {
    let watcher = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        watcher.cancel();
    });

    let _turn = crate::activity::begin(&plan.cache_home);
    let (mut writer, state, created) = open_session(&plan)?;
    let session_id = state.header.session_id.clone();
    let turn_id = state.next_turn_id;
    let started = Instant::now();

    let mut context: Vec<Message> = plan
        .input
        .iter()
        .map(|message| {
            let mut message = message.clone();
            message.turn_id = turn_id;
            message
        })
        .collect();
    if let Some(prompt) = &plan.prompt {
        context.push(Message::user(prompt.clone(), turn_id));
    }

    let ends_with_user = context
        .last()
        .or_else(|| state.messages.last())
        .is_some_and(|message| message.role == Role::User);
    if !ends_with_user {
        return Err(Fail::usage(
            "nothing to send: the conversation does not end with a user message",
            "pass a prompt with -p, or resume a session whose last turn was interrupted"
                .to_string(),
        ));
    }
    writer.turn_started(turn_id).map_err(flush_fail)?;
    for message in &context {
        writer.message(message).map_err(flush_fail)?;
    }

    let mut history = state.messages.clone();
    history.extend(context);
    let mut request = ModelRequest::new(plan.live.model_id.clone(), history, plan.live.max_tokens);
    request.temperature = plan.live.temperature;
    // The prompt alone, without the output reserve `estimate_request` adds.
    let prompt_tokens = {
        let mut probe = request.clone();
        probe.max_tokens = 0;
        estimate_request(&probe).input
    };

    let mut emitter = Emitter {
        session: session_id.clone(),
        seq: 0,
    };
    let streaming = plan.format == OutputFormat::StreamJson;
    // Text streams live only for a person watching; a pipe gets the committed
    // answer once, so a replayed attempt can never double up in a script.
    let live_text = plan.format == OutputFormat::Text && std::io::stdout().is_terminal();
    if streaming {
        emitter.emit(if created {
            EventData::SessionCreated {
                session_id: session_id.clone(),
                mode: state.mode.clone(),
                model: plan.live.model_id.clone(),
                workspace: plan.workspace.to_string_lossy().into_owned(),
            }
        } else {
            EventData::SessionResumed {
                session_id: session_id.clone(),
                from_record: 0,
            }
        });
        emitter.emit(EventData::TurnStarted {
            turn_id,
            prompt: plan.prompt.clone().unwrap_or_default(),
        });
        emitter.emit(EventData::ModelRequest {
            turn_id,
            provider: plan.live.provider.id().to_string(),
            model: plan.live.model_id.clone(),
            estimated_input_tokens: prompt_tokens,
            cache_hit_tokens: 0,
        });
    }

    let pricing = plan.live.pricing;
    let outcome = run_turn(
        plan.live.provider.clone(),
        request,
        turn_id,
        cancel.clone(),
        plan.live.budget.clone(),
        None,
        |event| match event {
            StreamEvent::TextDelta { text } if live_text => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            StreamEvent::TextDelta { text } if streaming => {
                emitter.emit(EventData::ModelDelta {
                    turn_id,
                    text: text.clone(),
                });
            }
            StreamEvent::ReasoningDelta { text } if streaming => {
                emitter.emit(EventData::ModelReasoning {
                    turn_id,
                    text: text.clone(),
                });
            }
            StreamEvent::Usage {
                input,
                output,
                cache_read,
                cache_write,
            } if streaming => {
                let usage = cairn_core::Usage::reported(*input, *output, *cache_read, *cache_write);
                emitter.emit(EventData::ModelUsage {
                    turn_id,
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                    cost_usd: cairn_provider::cost_usd(&usage, &pricing).unwrap_or(0.0),
                });
            }
            _ => {}
        },
    )
    .await;

    let answer_text = outcome
        .assistant
        .as_ref()
        .map(Message::text)
        .unwrap_or_default();
    let report = usage_report(
        outcome.reported_usage,
        prompt_tokens,
        &answer_text,
        &pricing,
    );
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Durable first, rendered second.
    let mut answers = Vec::new();
    if let Some(assistant) = &outcome.assistant {
        writer.message(assistant).map_err(flush_fail)?;
        answers = transcript::unavailable_tool_results(assistant);
        for message in &answers {
            writer.message(message).map_err(flush_fail)?;
            for block in &message.blocks {
                if let Block::ToolResult {
                    call_id, content, ..
                } = block
                {
                    let name = tool_name(assistant, call_id);
                    let body = content
                        .iter()
                        .map(|b| match b {
                            Block::Text { text } => text.as_str(),
                            _ => "",
                        })
                        .collect::<String>();
                    writer
                        .tool_result(turn_id, call_id, &name, false, &body)
                        .map_err(flush_fail)?;
                }
            }
        }
    }
    if let Some(fault) = &outcome.fault {
        writer
            .error(fault.code().unwrap_or("ERR_GENERIC"), &fault.message)
            .map_err(flush_fail)?;
    }
    writer
        .turn_ended(
            turn_id,
            status_name(&outcome.end),
            Some(report.usage),
            report.cost_usd,
            elapsed_ms,
        )
        .map_err(flush_fail)?;

    render(
        &plan,
        &outcome,
        &report,
        &answers,
        &mut emitter,
        &session_id,
        turn_id,
        elapsed_ms,
        live_text,
    );

    match outcome.end {
        TurnEnd::Completed => Ok(ExitStatus::Ok.code()),
        TurnEnd::Cancelled => Ok(ExitStatus::Cancelled.code()),
        TurnEnd::Failed => Err(model_failure(&plan.live.model_id, outcome.fault)),
    }
}

fn tool_name(assistant: &Message, call_id: &str) -> String {
    assistant
        .blocks
        .iter()
        .find_map(|block| match block {
            Block::ToolCall {
                call_id: id, name, ..
            } if id == call_id => Some(name.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The exit code a finished turn maps to (§7.7).
fn exit_code(end: &TurnEnd) -> i32 {
    match end {
        TurnEnd::Completed => ExitStatus::Ok.code(),
        TurnEnd::Failed => ExitStatus::Provider.code(),
        TurnEnd::Cancelled => ExitStatus::Cancelled.code(),
    }
}

#[allow(clippy::too_many_arguments)]
fn render(
    plan: &Plan,
    outcome: &TurnOutcome,
    report: &UsageReport,
    answers: &[Message],
    emitter: &mut Emitter,
    session_id: &str,
    turn_id: u64,
    elapsed_ms: u64,
    streamed_live: bool,
) {
    let text = outcome
        .assistant
        .as_ref()
        .map(Message::text)
        .unwrap_or_default();
    match plan.format {
        OutputFormat::Text => {
            if outcome.end == TurnEnd::Completed {
                if streamed_live {
                    if !text.ends_with('\n') && !text.is_empty() {
                        println!();
                    }
                } else if !text.is_empty() {
                    println!("{}", text.trim_end_matches('\n'));
                }
            }
        }
        OutputFormat::Json => {
            let error = outcome.fault.as_ref().map(|fault| {
                serde_json::json!({
                    "code": fault.code(),
                    "message": fault.message,
                })
            });
            let tool_calls: Vec<serde_json::Value> = outcome
                .assistant
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter_map(|block| match block {
                    Block::ToolCall {
                        call_id,
                        name,
                        input,
                        parse_error,
                        ..
                    } => Some(serde_json::json!({
                        "call_id": call_id,
                        "name": name,
                        "input": input,
                        "parse_error": parse_error,
                        "executed": false,
                    })),
                    _ => None,
                })
                .collect();
            let mut messages: Vec<&Message> = outcome.assistant.iter().collect();
            messages.extend(answers.iter());
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "status": status_name(&outcome.end),
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "model": plan.live.model_id,
                    "stop": outcome.stop.map(stop_name),
                    "messages": messages,
                    "tool_calls": tool_calls,
                    "usage": {
                        "input": report.usage.input,
                        "output": report.usage.output,
                        "cache_read": report.usage.cache_read,
                        "cache_write": report.usage.cache_write,
                        "estimated": report.usage.estimated,
                    },
                    "cost_usd": report.cost_usd,
                    "plan": null,
                    "error": error,
                    "exit_code": exit_code(&outcome.end),
                })
            );
        }
        OutputFormat::StreamJson => {
            if let Some(assistant) = &outcome.assistant {
                emitter.emit(EventData::MessageAppended {
                    turn_id,
                    message: assistant.clone(),
                });
            }
            if let Some(fault) = &outcome.fault {
                emitter.emit(EventData::ModelError {
                    turn_id,
                    code: fault.code().unwrap_or("ERR_GENERIC").to_string(),
                    http_status: None,
                    retryable: fault.fault.retryable(),
                    attempt: fault.retries_spent.saturating_add(1),
                });
            }
            emitter.emit(EventData::TurnEnded {
                turn_id,
                status: turn_status(&outcome.end),
                duration_ms: elapsed_ms,
                cost_usd: report.cost_usd.unwrap_or(0.0),
            });
        }
    }
}

/// A failed turn: the fault's stable code with exit 3 (T-CLI-010).
#[must_use]
pub fn model_failure(model_id: &str, fault: Option<ProviderError>) -> Fail {
    match fault {
        Some(error) => Fail::new(
            error.code().unwrap_or("ERR_GENERIC"),
            ExitStatus::Provider,
            format!("model `{model_id}` failed: {}", error.message),
            Some("see `cairn doctor` for connectivity and credential checks".to_string()),
        ),
        None => Fail::new(
            "ERR_GENERIC",
            ExitStatus::Provider,
            format!("model `{model_id}` failed without detail"),
            None,
        ),
    }
}
