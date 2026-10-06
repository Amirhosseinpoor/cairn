//! The turn lifecycle (SPEC §8.2): model call → tool calls → results → model
//! call, until the model stops, a guardrail trips, or the turn is cancelled.
//!
//! Each iteration is made durable through [`LoopHooks::commit`] *before* the
//! next one starts, so a crash leaves committed tool results on disk and the
//! §8.7 recovery has something to rebuild from (REQ-LOOP-006).

use std::sync::Arc;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::message::{Block, Message, Role, StopReason, Usage};
use cairn_core::CairnError;
use cairn_provider::{ModelRequest, Provider, ProviderError, RetryBudget, StreamEvent, ToolSpec};
use cairn_tools::{CallEnv, Executor, ToolCall, ToolError, ToolResult};

use crate::turn::{run_turn, TurnEnd};

/// §7.5 / §8.3 limits the loop enforces itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopConfig {
    /// Model calls per turn (`auto.max_iterations`).
    pub max_iterations: u32,
    /// Tool calls per turn (`auto.max_tool_calls`).
    pub max_tool_calls: u32,
    /// Consecutive denied calls that end the turn (`permissions.deny_ending_turn_after`, T-5).
    pub deny_ending_after: u32,
    /// The token budget (§5.4); `None` sends whatever there is.
    pub budget: Option<cairn_context::budget::Budget>,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 40,
            max_tool_calls: 120,
            deny_ending_after: 3,
            budget: None,
        }
    }
}

/// Why the loop stopped (§8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEnd {
    /// T-1: the model ended its turn.
    Completed,
    /// T-6: the provider failed for good (the fault is on the outcome).
    ProviderFailed,
    /// T-7.
    Cancelled,
    /// T-4: a §7.5 guardrail tripped.
    Guardrail {
        rule: &'static str,
        limit: u64,
        actual: u64,
    },
    /// T-5: repeated permission denials.
    Denied { consecutive: u32 },
    /// T-3: the model ran out of output tokens mid-answer.
    MaxTokens,
    /// T-13.
    ContentFilter,
    /// The request cannot be made to fit (`E-CTX-COMPACT`).
    ContextFull { message: String },
}

/// One committed iteration.
#[derive(Debug, Clone)]
pub struct Iteration {
    pub assistant: Message,
    /// The tool-result message that followed, when the assistant called tools.
    pub tool_message: Option<Message>,
    pub results: Vec<ToolResult>,
    /// The provider's own usage for this model call, if it sent one.
    pub reported_usage: Option<Usage>,
    pub stop: Option<StopReason>,
}

/// What a whole turn came to.
#[derive(Debug)]
pub struct LoopOutcome {
    pub end: LoopEnd,
    pub fault: Option<ProviderError>,
    pub iterations: Vec<Iteration>,
    /// Model calls started (including one that failed or was cancelled).
    pub model_calls: u32,
    pub tool_calls: u32,
    /// Earlier attempts discarded by §4.7's replay, summed.
    pub discarded_attempts: u32,
}

impl LoopOutcome {
    /// The text of the final assistant message.
    #[must_use]
    pub fn final_text(&self) -> String {
        self.iterations
            .last()
            .map(|i| i.assistant.text())
            .unwrap_or_default()
    }
}

/// What the loop reports and persists as it goes.
pub trait LoopHooks: Send {
    /// A raw model stream event (for rendering).
    fn on_stream(&mut self, event: &StreamEvent);
    /// The model has finished one message; tools (if any) have not started.
    fn on_assistant(&mut self, _message: &Message) {}
    /// An iteration is complete and must be made durable now.
    ///
    /// # Errors
    /// `E-SESS-FLUSH` when it cannot be; the loop stops with that error.
    fn commit(&mut self, iteration: &Iteration) -> Result<(), CairnError>;
    /// History was compacted (§5.6). Persist the records before the next
    /// request relies on them.
    ///
    /// # Errors
    /// `E-SESS-FLUSH` when they cannot be made durable.
    fn compacted(&mut self, _change: &Compacted<'_>) -> Result<(), CairnError> {
        Ok(())
    }
}

/// What a compaction changed, for [`LoopHooks::compacted`].
#[derive(Debug)]
pub struct Compacted<'a> {
    pub before: &'a [Message],
    pub after: &'a [Message],
    pub records: &'a [cairn_context::compact::Record],
    /// Rounds ran out and the oldest messages were dropped.
    pub trimmed: bool,
}

fn tool_specs(executor: &Executor, mode: cairn_core::Mode) -> Vec<ToolSpec> {
    executor
        .registry()
        .definitions(mode)
        .into_iter()
        .map(|d| ToolSpec {
            name: d.name,
            description: d.description,
            input_schema: d.input_schema,
        })
        .collect()
}

fn tool_result_message(turn_id: u64, results: &[ToolResult]) -> Message {
    let blocks = results
        .iter()
        .map(|r| Block::ToolResult {
            call_id: r.call_id.clone(),
            content: vec![Block::Text { text: r.text() }],
            is_error: !r.ok,
        })
        .collect();
    Message::new(Role::Tool, blocks, turn_id)
}

/// Run one assistant message's tool calls. Calls whose arguments never
/// parsed are answered here (§4.3); the rest go through the pipeline together
/// so §6.6's policy applies.
async fn run_calls(
    executor: &Executor,
    env: &CallEnv,
    cancel: &CancellationToken,
    calls: &[(String, String, serde_json::Value, Option<String>)],
) -> Vec<ToolResult> {
    let mut dispatch = Vec::new();
    let mut slots: Vec<Option<ToolResult>> = Vec::with_capacity(calls.len());
    for (call_id, name, input, parse_error) in calls {
        if let Some(why) = parse_error {
            let error = ToolError::new(
                codes::TOOL_BADJSON,
                format!("the arguments to `{name}` are not valid JSON: {why}"),
            )
            .recovery("Send the call again with well-formed JSON arguments.");
            slots.push(Some(ToolResult::rejected(call_id, name, &error)));
        } else {
            dispatch.push(ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            slots.push(None);
        }
    }
    let (ran, _burst) = executor.run_batch(dispatch, env, cancel).await;
    let mut ran = ran.into_iter();
    slots
        .into_iter()
        .map(|slot| slot.unwrap_or_else(|| ran.next().expect("one result per dispatched call")))
        .collect()
}

/// Whether a request fits, after compacting if it did not.
enum Fit {
    Ok,
    Cancelled,
    Full(String),
}

/// Distinct turns since the last summary, for C-4.
fn turns_since_summary(history: &[Message]) -> u32 {
    let start = history
        .iter()
        .rposition(cairn_context::compact::is_summary)
        .map_or(0, |i| i + 1);
    let turns: std::collections::BTreeSet<u64> =
        history[start..].iter().map(|m| m.turn_id).collect();
    u32::try_from(turns.len()).unwrap_or(u32::MAX)
}

/// §5.4/§5.6: make sure `request` fits its window, compacting when a trigger
/// fires (`forced` is C-3's provider-reported overflow). REQ-CTX-010: a
/// request that still does not fit is not sent.
#[allow(
    clippy::too_many_arguments,
    reason = "one parameter per collaborator the §3.3 diagram names"
)]
async fn make_room(
    provider: &Arc<dyn Provider>,
    request: &mut ModelRequest,
    budget: &cairn_context::budget::Budget,
    retry: &RetryBudget,
    env: &CallEnv,
    cancel: &CancellationToken,
    hooks: &mut dyn LoopHooks,
    forced: Option<cairn_context::compact::Trigger>,
    state: &mut cairn_context::compact::State,
) -> Result<Fit, CairnError> {
    use cairn_context::budget::{history_tokens, verify_budget};
    use cairn_context::compact::{automatic_trigger, Trigger};

    let start = usize::from(
        request
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System),
    );
    let fixed = request.messages[..start]
        .iter()
        .map(cairn_context::budget::message_tokens)
        .sum::<u32>()
        + cairn_core::tokens::estimate_tokens(
            &serde_json::to_string(&request.tools).unwrap_or_default(),
        );
    let total = |request: &ModelRequest| fixed + history_tokens(&request.messages[start..]);
    state.turns_since = turns_since_summary(&request.messages[start..]);
    let trigger = forced.or_else(|| {
        let tokens = total(request);
        if verify_budget(tokens, budget).is_err() {
            Some(Trigger::Reserve)
        } else {
            automatic_trigger(tokens, budget, state)
        }
    });
    let Some(trigger) = trigger else {
        return Ok(Fit::Ok);
    };
    let before: Vec<Message> = request.messages[start..].to_vec();
    let job = crate::compaction::Job {
        provider: Arc::clone(provider),
        model: &request.model,
        retry,
        budget,
        current_turn: env.turn_id,
        trigger,
        first_id: 1 + u32::try_from(
            before
                .iter()
                .filter(|m| cairn_context::compact::is_summary(m))
                .count(),
        )
        .unwrap_or(0),
        fixed_tokens: fixed,
    };
    let still_fits = |request: &ModelRequest| verify_budget(total(request), budget).is_ok();
    match crate::compaction::compact(&job, &before, None, cancel).await {
        crate::compaction::Outcome::Cancelled => Ok(Fit::Cancelled),
        crate::compaction::Outcome::Nothing => Ok(if still_fits(request) {
            Fit::Ok
        } else {
            Fit::Full("there is nothing left to compact and the request is still too large".into())
        }),
        crate::compaction::Outcome::Failed { code, message } => Ok(if still_fits(request) {
            Fit::Ok
        } else {
            Fit::Full(format!("{code}: {message}"))
        }),
        crate::compaction::Outcome::Compacted {
            messages,
            records,
            trimmed,
        } => {
            let mut rebuilt: Vec<Message> = request.messages[..start].to_vec();
            rebuilt.extend(messages.iter().cloned());
            request.messages = rebuilt;
            hooks.compacted(&Compacted {
                before: &before,
                after: &messages,
                records: &records,
                trimmed,
            })?;
            state.turns_since = 0;
            state.last_reduction = records
                .last()
                .map(cairn_context::compact::Record::reduction);
            Ok(if still_fits(request) {
                Fit::Ok
            } else {
                Fit::Full("the request is too large even after compaction".into())
            })
        }
    }
}

/// Run the turn described by `request` (whose `messages` hold the system
/// prompt, the history and the new user message) to its end.
///
/// # Errors
/// A [`CairnError`] when [`LoopHooks::commit`] cannot make an iteration
/// durable.
#[allow(
    clippy::too_many_arguments,
    reason = "one parameter per collaborator the §3.3 diagram names"
)]
pub async fn run_loop(
    provider: Arc<dyn Provider>,
    mut request: ModelRequest,
    executor: &Executor,
    env: &CallEnv,
    cancel: &CancellationToken,
    budget: &RetryBudget,
    config: LoopConfig,
    hooks: &mut dyn LoopHooks,
) -> Result<LoopOutcome, CairnError> {
    request.tools = tool_specs(executor, env.mode);
    request.turn_id = env.turn_id;
    let mut outcome = LoopOutcome {
        end: LoopEnd::Completed,
        fault: None,
        iterations: Vec::new(),
        model_calls: 0,
        tool_calls: 0,
        discarded_attempts: 0,
    };
    let mut denied_in_a_row = 0_u32;
    let mut compaction_state = cairn_context::compact::State::default();
    let mut overflow_retried = false;

    loop {
        // §7.5: evaluated at every increment of the iteration counter.
        if outcome.model_calls >= config.max_iterations {
            outcome.end = LoopEnd::Guardrail {
                rule: "max_iterations",
                limit: u64::from(config.max_iterations),
                actual: u64::from(outcome.model_calls) + 1,
            };
            return Ok(outcome);
        }
        if let Some(window) = config.budget {
            match make_room(
                &provider,
                &mut request,
                &window,
                budget,
                env,
                cancel,
                hooks,
                None,
                &mut compaction_state,
            )
            .await?
            {
                Fit::Ok => {}
                Fit::Cancelled => {
                    outcome.end = LoopEnd::Cancelled;
                    return Ok(outcome);
                }
                Fit::Full(message) => {
                    outcome.end = LoopEnd::ContextFull { message };
                    return Ok(outcome);
                }
            }
        }
        outcome.model_calls += 1;

        let turn = run_turn(
            Arc::clone(&provider),
            request.clone(),
            env.turn_id,
            cancel.clone(),
            budget.clone(),
            None,
            |event| hooks.on_stream(event),
        )
        .await;
        outcome.discarded_attempts += turn.discarded_attempts;
        match turn.end {
            TurnEnd::Cancelled => {
                outcome.end = LoopEnd::Cancelled;
                return Ok(outcome);
            }
            TurnEnd::Failed => {
                // C-3: the provider says the context was too long — compact
                // once and try again.
                let overflow = turn
                    .fault
                    .as_ref()
                    .and_then(cairn_provider::ProviderError::code)
                    == Some(cairn_core::error::codes::PROV_CONTEXT);
                if let (true, false, Some(window)) = (overflow, overflow_retried, config.budget) {
                    overflow_retried = true;
                    let fit = make_room(
                        &provider,
                        &mut request,
                        &window,
                        budget,
                        env,
                        cancel,
                        hooks,
                        Some(cairn_context::compact::Trigger::ProviderError),
                        &mut compaction_state,
                    )
                    .await?;
                    if matches!(fit, Fit::Ok) {
                        outcome.model_calls -= 1;
                        continue;
                    }
                }
                outcome.end = LoopEnd::ProviderFailed;
                outcome.fault = turn.fault;
                return Ok(outcome);
            }
            TurnEnd::Completed => {}
        }
        let assistant = turn.assistant.expect("a completed turn has a message");
        hooks.on_assistant(&assistant);
        let calls: Vec<(String, String, serde_json::Value, Option<String>)> = assistant
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall {
                    call_id,
                    name,
                    input,
                    parse_error,
                    ..
                } => Some((
                    call_id.clone(),
                    name.clone(),
                    input.clone(),
                    parse_error.clone(),
                )),
                _ => None,
            })
            .collect();

        if calls.is_empty() {
            let end = match turn.stop {
                Some(StopReason::MaxTokens) => LoopEnd::MaxTokens,
                Some(StopReason::ContentFilter) => LoopEnd::ContentFilter,
                _ => LoopEnd::Completed,
            };
            let iteration = Iteration {
                assistant,
                tool_message: None,
                results: Vec::new(),
                reported_usage: turn.reported_usage,
                stop: turn.stop,
            };
            hooks.commit(&iteration)?;
            request.messages.push(iteration.assistant.clone());
            outcome.iterations.push(iteration);
            outcome.end = end;
            return Ok(outcome);
        }

        // §7.5: the budget for tool calls is checked before any of them run.
        let would_be = outcome.tool_calls + u32::try_from(calls.len()).unwrap_or(u32::MAX);
        if would_be > config.max_tool_calls {
            outcome.end = LoopEnd::Guardrail {
                rule: "max_tool_calls",
                limit: u64::from(config.max_tool_calls),
                actual: u64::from(would_be),
            };
            return Ok(outcome);
        }
        outcome.tool_calls = would_be;

        let results = run_calls(executor, env, cancel, &calls).await;

        let tool_message = tool_result_message(env.turn_id, &results);
        let iteration = Iteration {
            assistant,
            tool_message: Some(tool_message),
            results,
            reported_usage: turn.reported_usage,
            stop: turn.stop,
        };
        // Durable first: the next model call may take minutes.
        hooks.commit(&iteration)?;
        request.messages.push(iteration.assistant.clone());
        if let Some(message) = &iteration.tool_message {
            request.messages.push(message.clone());
        }

        let cancelled = iteration
            .results
            .iter()
            .any(|r| r.error_code == Some(codes::TOOL_CANCELLED));
        for result in &iteration.results {
            if result.denied {
                denied_in_a_row += 1;
            } else if result.ok {
                denied_in_a_row = 0;
            }
        }
        outcome.iterations.push(iteration);

        if cancelled || cancel.is_cancelled() {
            outcome.end = LoopEnd::Cancelled;
            return Ok(outcome);
        }
        if denied_in_a_row >= config.deny_ending_after {
            outcome.end = LoopEnd::Denied {
                consecutive: denied_in_a_row,
            };
            return Ok(outcome);
        }
    }
}
