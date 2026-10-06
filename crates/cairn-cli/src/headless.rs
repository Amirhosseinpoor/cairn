//! `cairn run` — one headless turn over a persisted session (SPEC §7.7).
//!
//! The order is the one §8.1 draws: open (or continue) the session, make the
//! user's prompt durable, run the loop — each iteration made durable as it
//! completes — then render. Rendering never decides anything: the exit code
//! and the JSON document are derived from the loop's [`LoopOutcome`].

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use std::sync::{Arc, Mutex};

use cairn_agent::lifecycle::{run_loop, Iteration, LoopConfig, LoopEnd, LoopHooks, LoopOutcome};
use cairn_agent::prompt::{system_prompt, PromptVars};
use cairn_agent::transcript::{self, status, Recovery, ResumeState, SessionWriter};
use cairn_agent::usage::usage_report;
use cairn_core::cancel::CancellationToken;
use cairn_core::error::{codes, ExitStatus};
use cairn_core::event::{Event, EventData, TurnStatus};
use cairn_core::message::{Block, Message, Role, StopReason, Usage};
use cairn_core::{CairnError, Mode, SessionId};
use cairn_provider::{estimate_tokens, ModelRequest, ProviderError, StreamEvent};
use cairn_session::{Header, Store};
use cairn_tools::{CallEnv, EventSink};

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
    /// The loaded configuration, for the tool layer and the loop limits.
    pub config: cairn_config::Config,
    pub paths: cairn_config::Paths,
    /// The operating mode this turn runs in.
    pub run_mode: Mode,
    /// `--allow-ask`: approvals come from stdin.
    pub allow_ask: bool,
    /// `--max-iterations`.
    pub max_iterations: Option<u32>,
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

/// Writes §3.5 envelopes to stdout — and only envelopes (REQ-MODE-012). One
/// emitter is shared by the model stream and the tool layer, so `seq` counts
/// every event of the turn in the order it happened.
struct Emitter {
    session: String,
    seq: Mutex<u64>,
    enabled: bool,
}

impl Emitter {
    fn publish(&self, data: EventData) {
        if !self.enabled {
            return;
        }
        let mut seq = self.seq.lock().expect("event seq");
        *seq += 1;
        let event = Event::new(data, *seq, Some(self.session.clone()));
        println!(
            "{}",
            serde_json::to_string(&event).expect("events serialise")
        );
    }
}

impl EventSink for Emitter {
    fn emit(&self, event: EventData) {
        self.publish(event);
    }
}

/// Text and JSON modes: one line per tool on stderr (§7.7 "progress to
/// stderr"), silent under `--quiet`.
struct Progress {
    quiet: bool,
}

impl EventSink for Progress {
    fn emit(&self, event: EventData) {
        if self.quiet {
            return;
        }
        match event {
            EventData::ToolStarted { name, input, .. } => {
                let shown = crate::output::ellipsize(&input.to_string(), 100);
                eprintln!("[tool] {name} {shown}");
            }
            EventData::ToolFinished {
                name,
                status,
                duration_ms,
                error,
                ..
            } => match error {
                Some(code) => eprintln!("[tool] {name} → {code} ({duration_ms} ms)"),
                None => eprintln!("[tool] {name} → {status:?} ({duration_ms} ms)"),
            },
            _ => {}
        }
    }
}

fn turn_status_of(end: &LoopEnd) -> TurnStatus {
    match end {
        LoopEnd::Completed => TurnStatus::Ok,
        LoopEnd::ProviderFailed
        | LoopEnd::MaxTokens
        | LoopEnd::ContentFilter
        | LoopEnd::ContextFull { .. } => TurnStatus::Error,
        LoopEnd::Cancelled => TurnStatus::Cancelled,
        LoopEnd::Guardrail { .. } => TurnStatus::Guardrail,
        LoopEnd::Denied { .. } => TurnStatus::Denied,
    }
}

fn status_name(end: &LoopEnd) -> &'static str {
    match end {
        LoopEnd::Completed => status::OK,
        LoopEnd::ProviderFailed
        | LoopEnd::MaxTokens
        | LoopEnd::ContentFilter
        | LoopEnd::ContextFull { .. } => status::ERROR,
        LoopEnd::Cancelled => status::CANCELLED,
        LoopEnd::Guardrail { .. } => status::GUARDRAIL,
        LoopEnd::Denied { .. } => status::DENIED,
    }
}

/// §7.7's `exit_code` for each way a turn can end.
fn exit_code(end: &LoopEnd) -> i32 {
    match end {
        LoopEnd::Completed => ExitStatus::Ok.code(),
        LoopEnd::ProviderFailed | LoopEnd::ContentFilter => ExitStatus::Provider.code(),
        LoopEnd::MaxTokens | LoopEnd::ContextFull { .. } => ExitStatus::Generic.code(),
        LoopEnd::Cancelled => ExitStatus::Cancelled.code(),
        LoopEnd::Guardrail { .. } => ExitStatus::Guardrail.code(),
        LoopEnd::Denied { .. } => ExitStatus::Permission.code(),
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

/// Persists each iteration the moment the loop completes it, and renders the
/// model stream as it arrives.
struct Persist<'a> {
    writer: &'a mut SessionWriter,
    emitter: Arc<Emitter>,
    live_text: bool,
    stream_json: bool,
    turn_id: u64,
    pricing: cairn_core::registry::Pricing,
    quiet: bool,
}

impl LoopHooks for Persist<'_> {
    fn compacted(
        &mut self,
        change: &cairn_agent::lifecycle::Compacted<'_>,
    ) -> Result<(), CairnError> {
        // Durable before the next request relies on it.
        for record in change.records {
            self.writer.compaction(record)?;
        }
        let (before, after, dropped, summarised, summary_tokens) =
            cairn_agent::compaction::event_numbers(change.before, change.after, change.records);
        self.emitter.publish(EventData::CompactionPerformed {
            before_tokens: before,
            after_tokens: after,
            messages_dropped: dropped,
            messages_summarized: summarised,
            summary_tokens,
        });
        if !self.quiet {
            crate::output::info_line(&cairn_agent::compaction::notice(before, after));
            if change.trimmed {
                crate::output::warn_line(
                    codes::CTX_COMPACT,
                    "summaries were not enough; the oldest messages were dropped",
                );
            }
        }
        Ok(())
    }

    fn on_stream(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta { text } if self.live_text => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            StreamEvent::TextDelta { text } if self.stream_json => {
                self.emitter.publish(EventData::ModelDelta {
                    turn_id: self.turn_id,
                    text: text.clone(),
                });
            }
            StreamEvent::ReasoningDelta { text } if self.stream_json => {
                self.emitter.publish(EventData::ModelReasoning {
                    turn_id: self.turn_id,
                    text: text.clone(),
                });
            }
            StreamEvent::Usage {
                input,
                output,
                cache_read,
                cache_write,
            } if self.stream_json => {
                let usage = Usage::reported(*input, *output, *cache_read, *cache_write);
                self.emitter.publish(EventData::ModelUsage {
                    turn_id: self.turn_id,
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                    cost_usd: cairn_provider::cost_usd(&usage, &self.pricing).unwrap_or(0.0),
                });
            }
            _ => {}
        }
    }

    fn commit(&mut self, iteration: &Iteration) -> Result<(), CairnError> {
        self.writer.message(&iteration.assistant)?;
        if let Some(message) = &iteration.tool_message {
            self.writer.message(message)?;
        }
        for result in &iteration.results {
            self.writer.tool_result(
                self.turn_id,
                &result.call_id,
                &result.name,
                result.ok,
                &result.text(),
                result.duration_ms,
                result.truncated,
            )?;
        }
        Ok(())
    }

    fn on_assistant(&mut self, message: &Message) {
        if self.stream_json {
            self.emitter.publish(EventData::MessageAppended {
                turn_id: self.turn_id,
                message: message.clone(),
            });
        }
    }
}

/// §9.8: the checkpoint store, unless checkpoints are off or the mode cannot
/// write anyway.
fn open_checkpoints(plan: &Plan, session_id: &str) -> Option<Arc<cairn_git::Checkpointer>> {
    if !plan.config.checkpoint.enabled || !plan.run_mode.allows_side_effects() {
        return None;
    }
    let cc = &plan.config.checkpoint;
    Some(Arc::new(cairn_git::Checkpointer::open(
        &plan.workspace,
        session_id,
        cairn_git::Limits {
            per_session: cc.keep_per_session as usize,
            global: cc.keep_total as usize,
            max_total_bytes: cc.max_total_bytes,
            ..cairn_git::Limits::default()
        },
    )))
}

/// Snapshot before the turn. A failure is `W-CHK-FAIL`, never fatal.
fn begin_checkpoint(ck: &cairn_git::Checkpointer, turn_id: u64, emitter: &Emitter, quiet: bool) {
    match ck.begin_turn(turn_id, "turn") {
        Ok(made) => emitter.publish(EventData::CheckpointCreated {
            checkpoint_id: made.id,
            r#ref: made.ref_name.unwrap_or_default(),
            bytes: made.bytes,
            files: u32::try_from(made.files).unwrap_or(u32::MAX),
        }),
        Err(e) if !quiet => crate::output::warn_line(
            cairn_core::error::codes::W_CHK_FAIL,
            &format!("no checkpoint for this turn: {}", e.message),
        ),
        Err(_) => {}
    }
}

/// Persist what the turn wrote and surface anything the store warned about.
fn end_checkpoint(ck: &cairn_git::Checkpointer, quiet: bool) {
    ck.end_turn();
    if !quiet {
        for warning in ck.take_warnings() {
            crate::output::warn_line(warning.code, &warning.message);
        }
    }
}

/// Write the guardrail trip and provider fault, if any, before the turn ends.
fn record_faults(
    writer: &mut SessionWriter,
    emitter: &Emitter,
    outcome: &LoopOutcome,
) -> Result<(), Fail> {
    if let LoopEnd::Guardrail {
        rule,
        limit,
        actual,
    } = &outcome.end
    {
        writer
            .guardrail(rule, *limit, *actual)
            .map_err(flush_fail)?;
        // REQ-MODE-009: the trip is announced before the turn ends.
        emitter.publish(EventData::GuardrailTrip {
            rule: (*rule).to_string(),
            limit: serde_json::json!(limit),
            actual: serde_json::json!(actual),
        });
    }
    if let Some(fault) = &outcome.fault {
        writer
            .error(fault.code().unwrap_or("ERR_GENERIC"), &fault.message)
            .map_err(flush_fail)?;
    }
    Ok(())
}

/// §5.7: the merged `AGENTS.md` text, cut to fit the system-prompt share of
/// the window (`W-CTX-SYSPROMPT`), with `W-CTX-ALIAS` warnings shown.
fn load_instructions(
    plan: &Plan,
    budget: Option<&cairn_context::budget::Budget>,
) -> Option<String> {
    let redactor = crate::log::redactor_always(&plan.config);
    let active = [plan.workspace.clone()];
    let loaded = cairn_context::instructions::load(
        &cairn_context::instructions::Locations {
            workspace: &plan.workspace,
            config_home: Some(&plan.paths.config_home),
            active_dirs: &active,
        },
        &redactor,
    );
    if !plan.quiet {
        for warning in &loaded.warnings {
            crate::output::warn_line(warning.code, &warning.message);
        }
    }
    if loaded.text.trim().is_empty() {
        return None;
    }
    let Some(budget) = budget else {
        return Some(loaded.text);
    };
    // The prompt around the instructions takes some of the system share.
    let room = budget.system.saturating_sub(1_500);
    let (text, cut) = cairn_context::budget::truncate_instructions(&loaded.text, room);
    if cut && !plan.quiet {
        crate::output::warn_line(
            codes::CTX_SYSPROMPT,
            "project instructions were cut to fit the system prompt budget",
        );
    }
    Some(text)
}

/// Run one headless turn; returns the process exit code (0 / 7) or the
/// failure to report (3 provider, 4 guardrail, 6 denied, 9 session, 13
/// flush, ...).
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
    let stream_json = plan.format == OutputFormat::StreamJson;

    let emitter = Arc::new(Emitter {
        session: session_id.clone(),
        seq: Mutex::new(0),
        enabled: stream_json,
    });
    let sink: Arc<dyn EventSink> = if stream_json {
        Arc::clone(&emitter) as Arc<dyn EventSink>
    } else {
        Arc::new(Progress { quiet: plan.quiet })
    };
    let checkpoints = open_checkpoints(&plan, &session_id);
    let executor = crate::toolkit::build(&crate::toolkit::Wiring {
        observer: checkpoints
            .clone()
            .map(|c| c as Arc<dyn cairn_git::WriteObserver>),
        config: &plan.config,
        paths: &plan.paths,
        workspace: &plan.workspace,
        mode: plan.run_mode,
        events: sink,
        allow_ask: plan.allow_ask,
        quiet: plan.quiet,
    })?;

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

    // The system prompt is rendered fresh each turn and is not part of the
    // stored conversation.
    let tools = executor.registry().definitions(plan.run_mode);
    let project = plan
        .workspace
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let root = plan.workspace.to_string_lossy();
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let shell = std::env::var("SHELL").unwrap_or_default();
    let window = plan.live.provider.capabilities().max_context;
    let token_budget = (window > 0).then(|| cairn_context::budget::Budget::for_window(window));
    let instructions = load_instructions(&plan, token_budget.as_ref());
    let system = system_prompt(&PromptVars {
        mode: plan.run_mode,
        workspace_root: &root,
        project_name: &project,
        platform: std::env::consts::OS,
        shell: &shell,
        date_utc: &date,
        model_id: &plan.live.model_id,
        tools: &tools,
        instructions: instructions.as_deref(),
    });
    let mut messages = vec![Message::new(
        Role::System,
        vec![Block::Text { text: system }],
        0,
    )];
    messages.extend(state.messages.iter().cloned());
    messages.extend(context);
    let mut request = ModelRequest::new(plan.live.model_id.clone(), messages, plan.live.max_tokens);
    request.temperature = plan.live.temperature;
    let prompt_tokens: u32 = request
        .messages
        .iter()
        .map(|m| estimate_tokens(&m.text()))
        .sum();

    // Text streams live only for a person watching; a pipe gets the committed
    // answer once, so a replayed attempt can never double up in a script.
    let live_text = plan.format == OutputFormat::Text && std::io::stdout().is_terminal();
    if stream_json {
        emitter.publish(if created {
            EventData::SessionCreated {
                session_id: session_id.clone(),
                mode: plan.run_mode.as_str().to_string(),
                model: plan.live.model_id.clone(),
                workspace: plan.workspace.to_string_lossy().into_owned(),
            }
        } else {
            EventData::SessionResumed {
                session_id: session_id.clone(),
                from_record: 0,
            }
        });
        emitter.publish(EventData::TurnStarted {
            turn_id,
            prompt: plan.prompt.clone().unwrap_or_default(),
        });
        emitter.publish(EventData::ModelRequest {
            turn_id,
            provider: plan.live.provider.id().to_string(),
            model: plan.live.model_id.clone(),
            estimated_input_tokens: prompt_tokens,
            cache_hit_tokens: 0,
        });
    }

    let defaults = LoopConfig {
        max_iterations: plan.config.auto.max_iterations,
        max_tool_calls: plan.config.auto.max_tool_calls,
        deny_ending_after: plan.config.permissions.deny_ending_turn_after,
        budget: token_budget,
    };
    let config = LoopConfig {
        max_iterations: plan.max_iterations.unwrap_or(defaults.max_iterations),
        ..defaults
    };
    let env = CallEnv {
        session_id: SessionId::new(),
        turn_id,
        cwd: plan.workspace.clone(),
        mode: plan.run_mode,
    };
    if let Some(ck) = &checkpoints {
        begin_checkpoint(ck, turn_id, &emitter, plan.quiet);
    }
    let outcome = {
        let mut hooks = Persist {
            writer: &mut writer,
            emitter: Arc::clone(&emitter),
            live_text,
            stream_json,
            turn_id,
            pricing: plan.live.pricing,
            quiet: plan.quiet,
        };
        run_loop(
            Arc::clone(&plan.live.provider),
            request,
            &executor,
            &env,
            &cancel,
            &plan.live.budget,
            config,
            &mut hooks,
        )
        .await
        .map_err(flush_fail)?
    };

    if let Some(ck) = &checkpoints {
        end_checkpoint(ck, plan.quiet);
    }
    let (usage, cost_usd) = total_usage(&outcome, prompt_tokens, &plan.live.pricing);
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record_faults(&mut writer, &emitter, &outcome)?;
    writer
        .turn_ended(
            turn_id,
            status_name(&outcome.end),
            Some(usage),
            cost_usd,
            elapsed_ms,
        )
        .map_err(flush_fail)?;

    render(
        &plan,
        &outcome,
        usage,
        cost_usd,
        &emitter,
        &session_id,
        turn_id,
        elapsed_ms,
        live_text,
    );
    finish(&plan, outcome)
}

/// Sum usage over every model call: the provider's numbers where it gave
/// them, the §4.8 estimate where it did not (flagged), and no cost unless
/// *every* call could be priced (REQ-PROV-012: unknown, never zero).
fn total_usage(
    outcome: &LoopOutcome,
    prompt_tokens: u32,
    pricing: &cairn_core::registry::Pricing,
) -> (Usage, Option<f64>) {
    let mut total = Usage::reported(0, 0, 0, 0);
    let mut cost = Some(0.0_f64);
    for (index, iteration) in outcome.iterations.iter().enumerate() {
        // Only the first call's prompt is the user's; later prompts repeat it
        // plus tool traffic, so a missing report there is estimated from the
        // text produced, not recounted.
        let prompt = if index == 0 { prompt_tokens } else { 0 };
        let report = usage_report(
            iteration.reported_usage,
            prompt,
            &iteration.assistant.text(),
            pricing,
        );
        total.input = total.input.saturating_add(report.usage.input);
        total.output = total.output.saturating_add(report.usage.output);
        total.cache_read = total.cache_read.saturating_add(report.usage.cache_read);
        total.cache_write = total.cache_write.saturating_add(report.usage.cache_write);
        total.estimated |= report.usage.estimated;
        cost = match (cost, report.cost_usd) {
            (Some(sum), Some(this)) => Some(sum + this),
            _ => None,
        };
    }
    if outcome.iterations.is_empty() {
        cost = None;
    }
    (total, cost)
}

#[allow(clippy::too_many_arguments)]
fn render(
    plan: &Plan,
    outcome: &LoopOutcome,
    usage: Usage,
    cost_usd: Option<f64>,
    emitter: &Emitter,
    session_id: &str,
    turn_id: u64,
    elapsed_ms: u64,
    streamed_live: bool,
) {
    let texts: Vec<String> = outcome
        .iterations
        .iter()
        .map(|i| i.assistant.text())
        .filter(|t| !t.trim().is_empty())
        .collect();
    match plan.format {
        OutputFormat::Text => {
            let text = texts.join("\n");
            if streamed_live {
                if !text.is_empty() && !text.ends_with('\n') {
                    println!();
                }
            } else if !text.is_empty() {
                println!("{}", text.trim_end_matches('\n'));
            }
        }
        OutputFormat::Json => {
            let error = outcome.fault.as_ref().map(|fault| {
                serde_json::json!({
                    "code": fault.code(),
                    "message": fault.message,
                })
            });
            let mut messages: Vec<&Message> = Vec::new();
            let mut tool_calls = Vec::new();
            for iteration in &outcome.iterations {
                messages.push(&iteration.assistant);
                if let Some(message) = &iteration.tool_message {
                    messages.push(message);
                }
                for block in &iteration.assistant.blocks {
                    let Block::ToolCall {
                        call_id,
                        name,
                        input,
                        ..
                    } = block
                    else {
                        continue;
                    };
                    let result = iteration.results.iter().find(|r| &r.call_id == call_id);
                    tool_calls.push(serde_json::json!({
                        "call_id": call_id,
                        "name": name,
                        "input": input,
                        "ok": result.map(|r| r.ok),
                        "error_code": result.and_then(|r| r.error_code),
                        "duration_ms": result.map(|r| r.duration_ms),
                        "truncated": result.map(|r| r.truncated),
                    }));
                }
            }
            let guardrail = match &outcome.end {
                LoopEnd::Guardrail {
                    rule,
                    limit,
                    actual,
                } => Some(serde_json::json!({
                    "rule": rule, "limit": limit, "actual": actual,
                })),
                _ => None,
            };
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "status": status_name(&outcome.end),
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "model": plan.live.model_id,
                    "stop": outcome.iterations.last().and_then(|i| i.stop).map(stop_name),
                    "messages": messages,
                    "tool_calls": tool_calls,
                    "usage": {
                        "input": usage.input,
                        "output": usage.output,
                        "cache_read": usage.cache_read,
                        "cache_write": usage.cache_write,
                        "estimated": usage.estimated,
                    },
                    "cost_usd": cost_usd,
                    "plan": null,
                    "guardrail": guardrail,
                    "error": error,
                    "exit_code": exit_code(&outcome.end),
                })
            );
        }
        OutputFormat::StreamJson => {
            if let Some(fault) = &outcome.fault {
                emitter.publish(EventData::ModelError {
                    turn_id,
                    code: fault.code().unwrap_or("ERR_GENERIC").to_string(),
                    http_status: None,
                    retryable: fault.fault.retryable(),
                    attempt: fault.retries_spent.saturating_add(1),
                });
            }
            emitter.publish(EventData::TurnEnded {
                turn_id,
                status: turn_status_of(&outcome.end),
                duration_ms: elapsed_ms,
                cost_usd: cost_usd.unwrap_or(0.0),
            });
        }
    }
}

/// Turn the loop's ending into the process result (§8.3's exit column).
fn finish(plan: &Plan, outcome: LoopOutcome) -> Result<i32, Fail> {
    match outcome.end {
        LoopEnd::Completed => Ok(ExitStatus::Ok.code()),
        LoopEnd::Cancelled => Ok(ExitStatus::Cancelled.code()),
        LoopEnd::ProviderFailed => Err(model_failure(&plan.live.model_id, outcome.fault)),
        LoopEnd::Guardrail {
            rule,
            limit,
            actual,
        } => Err(Fail::new(
            "ERR_GUARDRAIL",
            ExitStatus::Guardrail,
            format!("guardrail `{rule}` tripped: {actual} against a limit of {limit}"),
            Some(match rule {
                "max_iterations" => {
                    "raise --max-iterations or auto.max_iterations if the task needs it".to_string()
                }
                _ => "raise the matching `auto.*` limit if the task needs it".to_string(),
            }),
        )),
        LoopEnd::Denied { consecutive } => Err(Fail::new(
            codes::PERM_DENIED,
            ExitStatus::Permission,
            format!("{consecutive} tool calls in a row were denied; the turn was stopped"),
            Some(
                "grant what it needs (`.cairn/permissions.json`, or --allow-ask) and run again"
                    .to_string(),
            ),
        )),
        LoopEnd::MaxTokens => Err(Fail::new(
            codes::LOOP_MAXTOKENS,
            ExitStatus::Generic,
            "the model ran out of output tokens before finishing",
            Some("raise `max_output` for this model, or split the task".to_string()),
        )),
        LoopEnd::ContextFull { message } => Err(Fail::new(
            codes::CTX_COMPACT,
            ExitStatus::Generic,
            format!("the conversation no longer fits the model's window: {message}"),
            Some("start a new session, or use a model with a larger context window".to_string()),
        )),
        LoopEnd::ContentFilter => Err(Fail::new(
            codes::PROV_FILTER,
            ExitStatus::Provider,
            "the provider blocked the response (content filter)",
            Some("rephrase the request".to_string()),
        )),
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
