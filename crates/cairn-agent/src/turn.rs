//! One model turn (SPEC §8.1's `AwaitingModel → Streaming → AppendingMessage`
//! path), M1 shape: no tool execution, so a turn is "send the history, fold
//! the stream into one assistant message".
//!
//! The fold follows §4.7's turn boundary: a `MessageStart` opens a new
//! attempt and everything gathered since the previous one is dropped, and an
//! attempt that ended in `Finish { stop: Error }` never reaches the session
//! (REQ-PROV-009). Provider faults, tool-call assembly and cancellation are
//! all decided here so the CLI only renders.

use std::sync::Arc;

use cairn_core::cancel::CancellationToken;
use cairn_core::message::{Block, Message, Role, StopReason, Usage};
use cairn_provider::{
    stream_with_retry, Compact, ModelRequest, Provider, ProviderError, RetryBudget, StreamEvent,
    ToolCallAssembler,
};
use futures::StreamExt;

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    /// The model finished; [`TurnOutcome::assistant`] holds its message.
    Completed,
    /// Every attempt failed. Nothing was committed — in particular no tool
    /// result is invented for a call the failed attempt had started (T-PROV-007).
    Failed,
    /// The token was raised; the stream ended silently and nothing was kept.
    Cancelled,
}

/// What [`run_turn`] hands back.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub end: TurnEnd,
    /// The committed assistant message: `Some` exactly when `end` is
    /// [`TurnEnd::Completed`].
    pub assistant: Option<Message>,
    pub stop: Option<StopReason>,
    /// The provider's own numbers, if it sent a complete set.
    pub reported_usage: Option<Usage>,
    /// The terminal fault of a [`TurnEnd::Failed`] turn, with its stable code.
    pub fault: Option<ProviderError>,
    /// Attempts whose partial content was discarded before the winning one.
    pub discarded_attempts: u32,
}

/// One attempt's gathered content.
#[derive(Default)]
struct Draft {
    blocks: Vec<Block>,
    assembler: ToolCallAssembler,
    usage: Option<Usage>,
    finish: Option<StopReason>,
}

impl Draft {
    fn push_text(&mut self, text: &str) {
        if let Some(Block::Text { text: last }) = self.blocks.last_mut() {
            last.push_str(text);
        } else {
            self.blocks.push(Block::Text {
                text: text.to_string(),
            });
        }
    }

    fn push_reasoning(&mut self, text: &str) {
        if let Some(Block::Reasoning { text: last, .. }) = self.blocks.last_mut() {
            last.push_str(text);
        } else {
            self.blocks.push(Block::Reasoning {
                text: text.to_string(),
                signature: None,
            });
        }
    }

    fn sign_reasoning(&mut self, signature: &str) {
        if let Some(Block::Reasoning {
            signature: slot, ..
        }) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|block| matches!(block, Block::Reasoning { .. }))
        {
            *slot = Some(signature.to_string());
        }
    }
}

/// Run one turn against `request`, calling `on_event` with every raw
/// [`StreamEvent`] as it arrives (for rendering — the fold below does not
/// depend on it). `turn_id` stamps the committed message.
pub async fn run_turn(
    provider: Arc<dyn Provider>,
    request: ModelRequest,
    turn_id: u64,
    cancel: CancellationToken,
    budget: RetryBudget,
    compact: Option<Compact>,
    mut on_event: impl FnMut(&StreamEvent) + Send,
) -> TurnOutcome {
    let mut stream = Box::pin(stream_with_retry(
        provider.clone(),
        request,
        cancel.clone(),
        budget,
        compact,
    ));
    let mut draft = Draft::default();
    let mut started = false;
    let mut discarded = 0;
    while let Some(event) = stream.next().await {
        on_event(&event);
        match event {
            StreamEvent::Ping => {}
            StreamEvent::MessageStart { .. } => {
                // A second start means the previous attempt did not win.
                if started {
                    discarded += 1;
                }
                draft = Draft::default();
                started = true;
            }
            StreamEvent::TextDelta { text } => draft.push_text(&text),
            StreamEvent::ReasoningDelta { text } => draft.push_reasoning(&text),
            StreamEvent::ReasoningSignature { signature } => draft.sign_reasoning(&signature),
            StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallDelta { .. }
            | StreamEvent::ToolCallEnd { .. } => {
                if let Some(block) = draft.assembler.push(&event) {
                    draft.blocks.push(block);
                }
            }
            StreamEvent::Usage {
                input,
                output,
                cache_read,
                cache_write,
            } => draft.usage = Some(Usage::reported(input, output, cache_read, cache_write)),
            StreamEvent::Finish { stop } => draft.finish = Some(stop),
        }
    }
    drop(stream);

    let clean_finish = matches!(draft.finish, Some(stop) if stop != StopReason::Error);
    if !clean_finish {
        if cancel.is_cancelled() {
            return TurnOutcome {
                end: TurnEnd::Cancelled,
                assistant: None,
                stop: None,
                reported_usage: None,
                fault: None,
                discarded_attempts: discarded,
            };
        }
        return TurnOutcome {
            end: TurnEnd::Failed,
            assistant: None,
            stop: None,
            reported_usage: None,
            fault: provider.take_last_error(),
            discarded_attempts: discarded + u32::from(started),
        };
    }
    let stop = draft.finish;
    let mut message = Message::new(Role::Assistant, draft.blocks, turn_id);
    message.usage = draft.usage;
    TurnOutcome {
        end: TurnEnd::Completed,
        assistant: Some(message),
        stop,
        reported_usage: draft.usage,
        fault: None,
        discarded_attempts: discarded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::registry::ProviderKind;
    use cairn_provider::{steps_from_json, Capabilities, MockProvider, ProviderFault, Step};

    fn request() -> ModelRequest {
        ModelRequest::new("mock/m", Vec::new(), 100)
    }

    fn mock(kind: ProviderKind, steps: Vec<Step>) -> Arc<MockProvider> {
        Arc::new(MockProvider::new(kind, Capabilities::baseline(), steps))
    }

    async fn turn(provider: Arc<MockProvider>) -> TurnOutcome {
        run_turn(
            provider,
            request(),
            1,
            CancellationToken::new(),
            RetryBudget::new(),
            None,
            |_| {},
        )
        .await
    }

    fn chunk(text: &str) -> Step {
        Step::Payload(format!(
            r#"{{"id":"c","model":"m","choices":[{{"index":0,"delta":{{"content":"{text}"}},"finish_reason":null}}]}}"#
        ))
    }

    fn stop() -> Vec<Step> {
        vec![
            Step::Payload(
                r#"{"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.into(),
            ),
            Step::Payload(
                r#"{"usage":{"prompt_tokens":5,"completion_tokens":2},"choices":[]}"#.into(),
            ),
            Step::Payload("[DONE]".into()),
        ]
    }

    fn text_of(message: &Message) -> String {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_text_turn_commits_one_assistant_message() {
        let mut steps = vec![chunk("Hel"), chunk("lo")];
        steps.extend(stop());
        let outcome = turn(mock(ProviderKind::Openai, steps)).await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        let message = outcome.assistant.expect("committed");
        assert_eq!(text_of(&message), "Hello");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.turn_id, 1);
        assert_eq!(outcome.reported_usage, Some(Usage::reported(5, 2, 0, 0)));
        assert_eq!(outcome.stop, Some(StopReason::EndTurn));
        assert_eq!(outcome.discarded_attempts, 0);
    }

    /// T-PROV-001's shape through the loop: text, then a tool call whose
    /// arguments arrived in two deltas, in stream order.
    #[tokio::test]
    async fn a_tool_turn_keeps_stream_order_and_assembles_the_call() {
        let steps = steps_from_json(include_str!(
            "../../../assets/cassettes/anthropic-tool-use.json"
        ))
        .expect("cassette");
        let outcome = turn(mock(ProviderKind::Anthropic, steps)).await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(outcome.stop, Some(StopReason::ToolUse));
        let blocks = outcome.assistant.expect("committed").blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0],
            Block::Text {
                text: "On it.".to_string()
            }
        );
        assert!(matches!(
            &blocks[1],
            Block::ToolCall { call_id, name, input, parse_error: None, .. }
                if call_id == "toolu_1" && name == "get_weather"
                    && input == &serde_json::json!({"city": "Paris"})
        ));
    }

    /// T-PROV-033 through the turn: two 500s are invisible to the result.
    #[tokio::test(start_paused = true)]
    async fn retried_setup_errors_leave_a_clean_turn() {
        let steps = steps_from_json(include_str!(
            "../../../assets/cassettes/openai-500-then-ok.json"
        ))
        .expect("cassette");
        let outcome = turn(mock(ProviderKind::Openai, steps)).await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(text_of(&outcome.assistant.expect("committed")), "Recovered");
    }

    /// REQ-PROV-009 / §4.7: a stream that dies after content is replayed
    /// whole; the partial text never reaches the committed message.
    #[tokio::test(start_paused = true)]
    async fn a_mid_stream_failure_discards_the_partial_attempt() {
        let mut steps = vec![
            chunk("par"),
            Step::StreamError {
                fault: ProviderFault::Unreachable,
                message: "reset".into(),
            },
            chunk("whole"),
        ];
        steps.extend(stop());
        let outcome = turn(mock(ProviderKind::Openai, steps)).await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(text_of(&outcome.assistant.expect("committed")), "whole");
        assert_eq!(outcome.discarded_attempts, 1);
    }

    /// T-PROV-007: the provider fails after a tool call began, on every
    /// attempt. The turn aborts with the fault's code; no assistant message
    /// and so no fabricated tool result is committed.
    #[tokio::test(start_paused = true)]
    async fn t_prov_007_failure_after_a_tool_call_commits_nothing() {
        let attempt = || {
            vec![
                Step::Payload(
                    r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#.into(),
                ),
                Step::StreamError {
                    fault: ProviderFault::Unreachable,
                    message: "reset".into(),
                },
            ]
        };
        let steps: Vec<Step> = (0..6).flat_map(|_| attempt()).collect();
        let outcome = turn(mock(ProviderKind::Openai, steps)).await;
        assert_eq!(outcome.end, TurnEnd::Failed);
        assert!(outcome.assistant.is_none(), "nothing is committed");
        assert_eq!(
            outcome.fault.as_ref().and_then(ProviderError::code),
            Some("E-PROV-NET")
        );
    }

    #[tokio::test]
    async fn a_fatal_setup_error_fails_the_turn_with_its_code() {
        let outcome = turn(mock(
            ProviderKind::Openai,
            vec![Step::SetupError {
                fault: ProviderFault::Auth,
                message: "bad key".into(),
            }],
        ))
        .await;
        assert_eq!(outcome.end, TurnEnd::Failed);
        assert_eq!(
            outcome.fault.as_ref().and_then(ProviderError::code),
            Some("E-PROV-AUTH")
        );
    }

    /// Unparseable tool arguments keep the call, flagged, so the loop can
    /// answer `E-TOOL-BADJSON` instead of executing (§4.3).
    #[tokio::test]
    async fn unparseable_arguments_are_flagged_not_dropped() {
        let steps = vec![
            Step::Payload(
                r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"edit_file","arguments":"{\"path\":\"a\",\"old_"}}]},"finish_reason":null}]}"#.into(),
            ),
            Step::Payload(
                r#"{"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#.into(),
            ),
            Step::Payload("[DONE]".into()),
        ];
        let outcome = turn(mock(ProviderKind::Openai, steps)).await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        let blocks = outcome.assistant.expect("committed").blocks;
        assert!(matches!(
            &blocks[..],
            [Block::ToolCall {
                parse_error: Some(_),
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn a_raised_token_ends_the_turn_cancelled_with_nothing_kept() {
        let mut steps = vec![chunk("never seen")];
        steps.extend(stop());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = run_turn(
            mock(ProviderKind::Openai, steps),
            request(),
            1,
            cancel,
            RetryBudget::new(),
            None,
            |_| {},
        )
        .await;
        assert_eq!(outcome.end, TurnEnd::Cancelled);
        assert!(outcome.assistant.is_none());
        assert!(outcome.fault.is_none());
    }

    #[tokio::test]
    async fn the_callback_sees_every_raw_event() {
        let mut steps = vec![chunk("x")];
        steps.extend(stop());
        let mut seen = 0;
        run_turn(
            mock(ProviderKind::Openai, steps),
            request(),
            1,
            CancellationToken::new(),
            RetryBudget::new(),
            None,
            |_| seen += 1,
        )
        .await;
        assert!(seen >= 3, "start, text, usage, finish: saw {seen}");
    }
}
