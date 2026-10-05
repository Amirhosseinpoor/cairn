//! §4.2/§4.3 — the per-adapter wire decoder: one payload in, §3.4's
//! [`StreamEvent`]s out.
//!
//! A *payload* is what a transport hands over after framing: the `data:` field
//! of an SSE event for Anthropic and the `OpenAI` family, or one NDJSON line for
//! Ollama. Framing itself belongs to `cairn-sse`; this module only knows how
//! each §4.2 column spells a message.
//!
//! Three rules shape the whole thing:
//!
//! * **[`WireDecoder::finish`] owns the ending.** `Finish` is never produced by
//!   `decode`, because `OpenAI`'s `stream_options.include_usage` chunk arrives
//!   *after* `finish_reason` and §3.4's contract is that `Usage` precedes
//!   `Finish`. The transport calls `finish()` at end of stream.
//! * **One `Usage`, and only when it is complete.** REQ-PROV-011 says the
//!   §4.8 estimate stands until the provider reports real numbers, so a
//!   half-merged report (Anthropic splits input and output across two events)
//!   is worth nothing and is not emitted.
//! * **Nothing is invented at the end.** A stream cut mid-tool-call emits no
//!   `ToolCallEnd`, because an argument buffer that was never finished must not
//!   reach `serde_json::from_str` and run (§4.3).

use std::collections::BTreeSet;

use serde_json::Value;

use cairn_core::message::StopReason;
use cairn_core::registry::ProviderKind;

use crate::error::{ProviderError, ProviderFault};
use crate::types::StreamEvent;

/// §4.3's malformed-chunk rule: five or more unparseable payloads in one
/// stream abort it with `E-PROV-MALFORMED`.
const MALFORMED_LIMIT: u32 = 5;

/// What the provider has said about token counts so far.
///
/// Anthropic splits it: `message_start` carries input and cache, `message_delta`
/// carries the final output. `OpenAI` and Ollama report both at once. Neither
/// shape is a `Usage` until [`WireDecoder::usage_event`] sees all of it.
#[derive(Debug, Clone)]
struct PendingUsage {
    input: Option<u32>,
    output: Option<u32>,
    cache_read: u32,
    cache_write: u32,
}

/// Decode one provider's stream into §3.4's [`StreamEvent`]s (§4.2, §4.3).
///
/// A decoder covers exactly one stream: it remembers what has been started,
/// which tool-call indices are open, and how many payloads failed to parse.
/// [`WireDecoder::finish`] closes it, and is the only place `Finish` comes
/// from.
#[derive(Debug)]
pub struct WireDecoder {
    kind: ProviderKind,
    malformed: u32,
    /// A `MessageStart` has been emitted (§4.3 tolerates a repeat).
    started: bool,
    /// The protocol's own terminator arrived: `[DONE]`, `message_stop`, or
    /// Ollama's `done`. Everything after it is ignored rather than counted
    /// (T-PROV-028).
    terminated: bool,
    /// At least one tool call was seen, which §4.4's last row uses to upgrade
    /// an `EndTurn` to `ToolUse` — Ollama reports `done_reason: "stop"` even
    /// when it just handed over a call.
    saw_tool: bool,
    usage_emitted: bool,
    finished: bool,
    finish_stop: Option<StopReason>,
    usage: Option<PendingUsage>,
    /// Indices with a `ToolCallStart` out and no `ToolCallEnd` yet.
    open_tools: BTreeSet<u32>,
    /// Next index to mint for a protocol that has none (Ollama).
    next_index: u32,
}

impl WireDecoder {
    /// A decoder for one provider's stream format (§4.2's columns).
    #[must_use]
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            malformed: 0,
            started: false,
            terminated: false,
            saw_tool: false,
            usage_emitted: false,
            finished: false,
            finish_stop: None,
            usage: None,
            open_tools: BTreeSet::new(),
            next_index: 0,
        }
    }

    /// Decode one payload into zero or more [`StreamEvent`]s.
    ///
    /// `Err` means the stream is over: an in-band provider error (§4.5's rows
    /// arrive in the body as often as in the status line) or enough malformed
    /// payloads to trip §4.3's limit. `Ok(vec![])` is the normal case for a
    /// payload worth nothing — a keep-alive, `[DONE]`, an event name the
    /// provider made up (T-PROV-026).
    pub fn decode(&mut self, payload: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.terminated {
            return Ok(Vec::new());
        }
        let payload = payload.trim();
        if payload.is_empty() {
            // A blank `data:` line is a keep-alive, not a message that failed
            // to parse: §4.3's counter is about JSON that broke.
            return Ok(Vec::new());
        }
        if payload == "[DONE]" {
            self.terminated = true;
            return Ok(Vec::new());
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(err) => return self.record_malformed(&err.to_string()),
        };
        if let Some((fault, message)) = inband_error(&value) {
            return Err(ProviderError::new(fault, message));
        }
        Ok(match self.kind {
            ProviderKind::Anthropic => self.anthropic(&value),
            ProviderKind::Ollama => self.ollama(&value),
            ProviderKind::Openai | ProviderKind::OpenaiCompatible | ProviderKind::Vllm => {
                self.openai(&value)
            }
        })
    }

    /// End of stream: close what the provider said is complete, then emit the
    /// one [`StreamEvent::Finish`] of the stream.
    ///
    /// `Usage` has already gone out during `decode` — that is the point of
    /// holding `Finish` back — and an argument buffer with no completion signal
    /// is left open, so a truncated tool call is never handed to a tool.
    ///
    /// Calling this twice is safe; the second call has nothing left to say.
    #[must_use]
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = self.close_open_tools();
        if let Some(stop) = self.final_stop() {
            out.push(StreamEvent::Finish { stop });
        }
        out
    }

    /// How many payloads failed to parse (§4.3), for the `warn` the transport
    /// logs before it aborts at the §4.3 limit of five.
    #[must_use]
    pub fn malformed_events(&self) -> u32 {
        self.malformed
    }

    // ------------------------------------------------------------- plumbing

    fn record_malformed(&mut self, detail: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        self.malformed += 1;
        if self.malformed < MALFORMED_LIMIT {
            return Ok(Vec::new());
        }
        let seen = self.malformed;
        Err(ProviderError::new(
            ProviderFault::MalformedStream,
            format!("{seen} malformed events in one stream, aborting ({detail})"),
        ))
    }

    /// A `MessageStart` for a protocol that names its model at the top level
    /// (`OpenAI`, Ollama). Ollama has no message id, so §4.2's row gets `""`.
    fn start_from(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        let Some(model) = value.get("model").and_then(Value::as_str) else {
            return;
        };
        self.started = true;
        out.push(StreamEvent::MessageStart {
            model: model.to_string(),
            id: value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }

    fn mint_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        index
    }

    /// Remember an index the provider chose, so a minted one cannot collide
    /// with it later in the same stream.
    fn note_index(&mut self, index: u32) {
        self.next_index = self.next_index.max(index.saturating_add(1));
    }

    /// §4.3: "Deltas for an unknown `index` MUST open a synthetic
    /// `ToolCallStart`". The id is minted because the provider omitted it and
    /// the name is left empty because nothing in the stream knows it yet.
    fn synthetic_start(&mut self, index: u32) -> StreamEvent {
        self.note_index(index);
        self.open_tools.insert(index);
        self.saw_tool = true;
        StreamEvent::ToolCallStart {
            index,
            id: synthetic_id(index),
            name: String::new(),
        }
    }

    fn close_tool(&mut self, index: u32) -> Option<StreamEvent> {
        self.open_tools
            .remove(&index)
            .then_some(StreamEvent::ToolCallEnd { index })
    }

    /// The stop reason the consumer should see, once §4.4's last row has had
    /// its say. `None` only for a stream that stopped mid-sentence.
    fn final_stop(&self) -> Option<StopReason> {
        let stop = match self.finish_stop {
            Some(stop) => stop,
            None if self.terminated => StopReason::EndTurn,
            None => return None,
        };
        Some(match stop {
            StopReason::EndTurn if self.saw_tool => StopReason::ToolUse,
            other => other,
        })
    }

    /// Close every open tool call, but only for a response the provider
    /// called complete. A stream cut at `max_tokens` leaves the buffer open
    /// and the tool unexecuted (§4.3, and REQ-PROV-009's "no fabricated
    /// result").
    fn close_open_tools(&mut self) -> Vec<StreamEvent> {
        let completed = matches!(
            self.final_stop(),
            Some(StopReason::EndTurn | StopReason::ToolUse)
        );
        if !completed {
            return Vec::new();
        }
        let open: Vec<u32> = self.open_tools.iter().copied().collect();
        open.into_iter()
            .filter_map(|index| self.close_tool(index))
            .collect()
    }

    /// Emit the one `Usage` of this stream, once both halves are in.
    fn usage_event(&mut self) -> Option<StreamEvent> {
        if self.usage_emitted {
            return None;
        }
        let pending = self.usage.as_ref()?;
        let input = pending.input?;
        let output = pending.output?;
        let cache_read = pending.cache_read;
        let cache_write = pending.cache_write;
        self.usage = None;
        self.usage_emitted = true;
        Some(StreamEvent::Usage {
            input,
            output,
            cache_read,
            cache_write,
        })
    }

    // ------------------------------------------------------------- Anthropic

    fn anthropic(&mut self, value: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => self.anthropic_start(value, &mut out),
            Some("content_block_start") => self.anthropic_block_start(value, &mut out),
            Some("content_block_delta") => self.anthropic_delta(value, &mut out),
            Some("content_block_stop") => {
                if let Some(index) = num_at(value, "index") {
                    if let Some(event) = self.close_tool(index) {
                        out.push(event);
                    }
                }
            }
            Some("message_delta") => self.anthropic_message_delta(value, &mut out),
            Some("message_stop") => self.terminated = true,
            Some("ping") => out.push(StreamEvent::Ping),
            // Unknown event names are ignored, stream kept (§4.3). This is
            // also where a payload declaring itself an error would land, but
            // `decode` has already turned that into an `Err`.
            _ => {}
        }
        out
    }

    fn anthropic_start(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        let message = value.get("message").unwrap_or(&Value::Null);
        let model = str_at(message, "model").unwrap_or_default();
        let id = str_at(message, "id").unwrap_or_default();
        let usage = message.get("usage");
        // Input and cache are known here; the output in `message_start` is a
        // placeholder (Anthropic seeds it with 1) and is replaced by
        // `message_delta`.
        self.usage = Some(PendingUsage {
            input: usage.and_then(|u| num_at(u, "input_tokens")),
            output: None,
            cache_read: usage
                .and_then(|u| num_at(u, "cache_read_input_tokens"))
                .unwrap_or(0),
            cache_write: usage
                .and_then(|u| num_at(u, "cache_creation_input_tokens"))
                .unwrap_or(0),
        });
        self.started = true;
        out.push(StreamEvent::MessageStart {
            model: model.to_string(),
            id: id.to_string(),
        });
    }

    fn anthropic_block_start(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        let index = match num_at(value, "index") {
            Some(index) => {
                self.note_index(index);
                index
            }
            None => self.mint_index(),
        };
        let block = value.get("content_block").unwrap_or(&Value::Null);
        if str_at(block, "type") != Some("tool_use") {
            // `text`, `thinking`, `image`, `redacted_thinking`: their content
            // arrives as deltas, and a block start is not a message on its own.
            return;
        }
        let id = str_at(block, "id").map_or_else(|| synthetic_id(index), str::to_string);
        let name = str_at(block, "name").unwrap_or_default().to_string();
        out.push(StreamEvent::ToolCallStart { index, id, name });
        self.saw_tool = true;
        self.open_tools.insert(index);
        // §4.3's buffer starts empty, so `input: {}` — the normal case — adds
        // nothing and the deltas that follow append cleanly. Anything else the
        // provider put here is argument text we do not have, and forwarding it
        // is the only way to avoid handing the tool an empty `{}`.
        if let Some(input) = block.get("input") {
            let empty_object = input.as_object().is_some_and(serde_json::Map::is_empty);
            if !input.is_null() && !empty_object {
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    args_delta: input.to_string(),
                });
            }
        }
    }

    fn anthropic_delta(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        let delta = value.get("delta").unwrap_or(&Value::Null);
        match str_at(delta, "type") {
            Some("text_delta") => {
                if let Some(text) = str_at(delta, "text") {
                    out.push(StreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            Some("thinking_delta") => {
                if let Some(text) = str_at(delta, "thinking") {
                    out.push(StreamEvent::ReasoningDelta {
                        text: text.to_string(),
                    });
                }
            }
            // §4.2's Anthropic column: `thinking` blocks come back with a
            // signature the next request has to echo, so it is its own event
            // rather than a delta of the reasoning text.
            Some("signature_delta") => {
                if let Some(signature) = str_at(delta, "signature") {
                    out.push(StreamEvent::ReasoningSignature {
                        signature: signature.to_string(),
                    });
                }
            }
            Some("input_json_delta") => {
                let index = match num_at(value, "index") {
                    Some(index) => {
                        self.note_index(index);
                        index
                    }
                    None => self.mint_index(),
                };
                if !self.open_tools.contains(&index) {
                    out.push(self.synthetic_start(index));
                }
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    args_delta: str_at(delta, "partial_json")
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            // Unknown delta types are ignored, stream kept (§4.3).
            _ => {}
        }
    }

    fn anthropic_message_delta(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        if let Some(stop) = value
            .get("delta")
            .and_then(|delta| str_at(delta, "stop_reason"))
        {
            self.finish_stop = Some(anthropic_stop(stop));
        }
        let output = value
            .get("usage")
            .and_then(|usage| num_at(usage, "output_tokens"));
        if let (Some(pending), Some(output)) = (self.usage.as_mut(), output) {
            pending.output = Some(output);
        }
        if let Some(event) = self.usage_event() {
            out.push(event);
        }
        out.extend(self.close_open_tools());
    }

    // ------------------------------------------------------- OpenAI family

    fn openai(&mut self, value: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        self.start_from(value, &mut out);

        // `stream_options.include_usage` sends a final chunk whose `choices`
        // is empty: input, output and the cached prefix, all at once.
        if let Some(usage) = value.get("usage").filter(|entry| entry.is_object()) {
            self.usage = Some(PendingUsage {
                input: num_at(usage, "prompt_tokens"),
                output: num_at(usage, "completion_tokens"),
                cache_read: usage
                    .get("prompt_tokens_details")
                    .and_then(|details| num_at(details, "cached_tokens"))
                    .unwrap_or(0),
                // `OpenAI` bills cached input as a discount rather than a
                // separate write count, so there is nothing to report.
                cache_write: 0,
            });
            if let Some(event) = self.usage_event() {
                out.push(event);
            }
        }

        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return out;
        };
        let delta = choice.get("delta").unwrap_or(&Value::Null);

        if let Some(text) = str_at(delta, "content").filter(|text| !text.is_empty()) {
            out.push(StreamEvent::TextDelta {
                text: text.to_string(),
            });
        }
        let reasoning = ["reasoning_content", "reasoning"]
            .into_iter()
            .find_map(|key| str_at(delta, key))
            .filter(|text| !text.is_empty());
        if let Some(text) = reasoning {
            out.push(StreamEvent::ReasoningDelta {
                text: text.to_string(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.openai_tool_call(call, &mut out);
            }
        }
        if let Some(reason) = str_at(choice, "finish_reason") {
            self.finish_stop = Some(openai_stop(reason));
            out.extend(self.close_open_tools());
        }
        out
    }

    fn openai_tool_call(&mut self, call: &Value, out: &mut Vec<StreamEvent>) {
        let function = call.get("function").unwrap_or(&Value::Null);
        let id = str_at(call, "id").unwrap_or_default();
        let name = str_at(function, "name").unwrap_or_default();
        let args = str_at(function, "arguments").unwrap_or_default();

        let index = match num_at(call, "index") {
            Some(index) => {
                self.note_index(index);
                index
            }
            None => self.mint_index(),
        };
        if !self.open_tools.contains(&index) {
            if id.is_empty() && name.is_empty() {
                out.push(self.synthetic_start(index));
            } else {
                self.open_tools.insert(index);
                self.saw_tool = true;
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
        }
        // `arguments` are string fragments in arrival order, including the
        // first one, so appending is always right — a chunk that repeats the
        // id on a call already open contributes only its fragment.
        if !args.is_empty() {
            out.push(StreamEvent::ToolCallDelta {
                index,
                args_delta: args.to_string(),
            });
        }
    }

    // ---------------------------------------------------------------- Ollama

    fn ollama(&mut self, value: &Value) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        self.start_from(value, &mut out);

        if let Some(message) = value.get("message") {
            if let Some(text) = str_at(message, "content").filter(|text| !text.is_empty()) {
                out.push(StreamEvent::TextDelta {
                    text: text.to_string(),
                });
            }
            if let Some(text) = str_at(message, "thinking").filter(|text| !text.is_empty()) {
                out.push(StreamEvent::ReasoningDelta {
                    text: text.to_string(),
                });
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.ollama_tool_call(call, &mut out);
                }
            }
        }

        if value.get("done").and_then(Value::as_bool) == Some(true) {
            if let Some(reason) = str_at(value, "done_reason") {
                self.finish_stop = Some(ollama_stop(reason));
            }
            let input = num_at(value, "prompt_eval_count");
            let output = num_at(value, "eval_count");
            if let (Some(input), Some(output)) = (input, output) {
                self.usage = Some(PendingUsage {
                    input: Some(input),
                    output: Some(output),
                    cache_read: 0,
                    cache_write: 0,
                });
            }
            if let Some(event) = self.usage_event() {
                out.push(event);
            }
            self.terminated = true;
            out.extend(self.close_open_tools());
        }
        out
    }

    /// Ollama hands a tool call over whole — no index, no id, no partial
    /// arguments — so one line becomes a start, the assembled arguments, and
    /// an end, which is what §4.3's assembly rule wants to see anyway.
    fn ollama_tool_call(&mut self, call: &Value, out: &mut Vec<StreamEvent>) {
        let function = call.get("function").unwrap_or(&Value::Null);
        let index = self.mint_index();
        out.push(StreamEvent::ToolCallStart {
            index,
            id: synthetic_id(index),
            name: str_at(function, "name").unwrap_or_default().to_string(),
        });
        self.saw_tool = true;
        let arguments = match function.get("arguments") {
            Some(Value::String(fragments)) => fragments.clone(),
            // Object form: serialise it whole, which is exactly what the
            // consumer's buffer would have accumulated.
            Some(other) => other.to_string(),
            None => String::new(),
        };
        out.push(StreamEvent::ToolCallDelta {
            index,
            args_delta: arguments,
        });
        out.push(StreamEvent::ToolCallEnd { index });
    }
}

// ------------------------------------------------------------------ helpers

fn synthetic_id(index: u32) -> String {
    format!("synthetic-{index}")
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn num_at(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// §4.2's Anthropic column for `stop_reason`.
fn anthropic_stop(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::ContentFilter,
        // `end_turn`, `stop_sequence`, `pause_turn`, and whatever the next
        // API revision adds: unknown reasons stay an `EndTurn`, which
        // [`WireDecoder::final_stop`] upgrades if tools were seen.
        _ => StopReason::EndTurn,
    }
}

/// §4.2's `OpenAI` column for `finish_reason` (and the proxies that copy it).
fn openai_stop(reason: &str) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::ContentFilter,
        _ => StopReason::EndTurn,
    }
}

/// Ollama's `done_reason`.
fn ollama_stop(reason: &str) -> StopReason {
    match reason {
        "length" => StopReason::MaxTokens,
        // The model being swapped out, not the turn ending.
        "load" | "unload" => StopReason::Error,
        // `stop` — what Ollama reports even after handing over a tool call —
        // and anything unrecognized land here.
        _ => StopReason::EndTurn,
    }
}

/// Recognise a provider's in-band error envelope (§4.5).
///
/// Every column disagrees about the shape: Anthropic writes
/// `{"type":"error","error":{"type":"…"}}`, the `OpenAI` family writes
/// `{"error":{"message":…,"code":…}}` with the status echoed in `code`, some
/// proxies put `status` at the top level, and Ollama's is a bare string. A
/// payload with none of those keys is a message, not an error, and is left to
/// the caller.
fn inband_error(value: &Value) -> Option<(ProviderFault, String)> {
    let error = value.get("error");
    let declared = str_at(value, "type") == Some("error");
    // Only a failure-range status names an error: a `200` echoed in the body
    // is a message that happens to carry a status, not a fault.
    let status = ["status", "code"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
        .or_else(|| {
            error
                .and_then(|entry| entry.get("code"))
                .and_then(Value::as_u64)
        })
        .filter(|status| *status >= 400);
    if error.is_none() && !declared && status.is_none() {
        return None;
    }

    let message = error
        .and_then(|entry| {
            entry
                .as_str()
                .map_or_else(|| str_at(entry, "message"), Some)
        })
        .or_else(|| str_at(value, "message"))
        .unwrap_or("provider reported an error")
        .to_string();

    if let Some(status) = status {
        let fault = ProviderFault::from_status(u16::try_from(status).unwrap_or(u16::MAX))
            .unwrap_or(ProviderFault::Server);
        return Some((fault, message));
    }

    let names = [
        error.and_then(|entry| str_at(entry, "code")),
        error.and_then(|entry| str_at(entry, "type")),
        str_at(value, "type").filter(|kind| *kind != "error"),
    ];
    let fault = names
        .into_iter()
        .flatten()
        .find_map(fault_from_name)
        .unwrap_or(ProviderFault::Server);
    Some((fault, message))
}

/// Map an `OpenAI` `error.code` or an Anthropic `error.type` onto §4.5's rows.
/// Anything unrecognised is `None`, and the caller treats "a provider that
/// errored in a way we have no row for" as `Server`.
fn fault_from_name(name: &str) -> Option<ProviderFault> {
    Some(match name {
        "authentication_error" | "invalid_api_key" => ProviderFault::Auth,
        "permission_error" | "permission_denied" => ProviderFault::Forbidden,
        "not_found_error" | "model_not_found" | "model_not_supported" => ProviderFault::NoModel,
        "timeout_error" | "request_timeout" => ProviderFault::Timeout,
        "rate_limit_error" | "rate_limit_exceeded" | "insufficient_quota" => {
            ProviderFault::RateLimited
        }
        "request_too_large" | "payload_too_large" => ProviderFault::PayloadTooLarge,
        "context_length_exceeded" => ProviderFault::ContextLength,
        "content_filter" => ProviderFault::ContentFilter,
        "invalid_request_error" | "invalid_prompt" => ProviderFault::BadRequest,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::message::StopReason;
    use cairn_core::registry::ProviderKind;

    /// Feed payloads through one decoder, then finish, and return everything
    /// in order. Most tests below assert on this whole vector rather than one
    /// event, because the interesting §4.3 guarantees are about ordering —
    /// `Usage` before `Finish`, deltas between their `Start` and `End`.
    fn drain(kind: ProviderKind, payloads: &[&str]) -> Vec<StreamEvent> {
        let mut decoder = WireDecoder::new(kind);
        let mut events = Vec::new();
        for payload in payloads {
            events.extend(decoder.decode(payload).expect("payload decodes"));
        }
        events.extend(decoder.finish());
        events
    }

    /// T-PROV-001's shape: a tool-bearing Anthropic SSE stream decodes to one
    /// `MessageStart`, text `Delta`s, one closed `ToolCall`, a merged `Usage`,
    /// and a `Finish` — in that order.
    #[test]
    fn an_anthropic_text_turn_decodes_end_to_end() {
        let events = drain(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":25,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    model: "claude-x".to_string(),
                    id: "msg_1".to_string(),
                },
                StreamEvent::TextDelta {
                    text: "Hello".to_string(),
                },
                StreamEvent::TextDelta {
                    text: " world".to_string(),
                },
                // The input half came from `message_start`, the output half
                // from `message_delta` — one event, both numbers.
                StreamEvent::Usage {
                    input: 25,
                    output: 15,
                    cache_read: 0,
                    cache_write: 0,
                },
                StreamEvent::Finish {
                    stop: StopReason::EndTurn,
                },
            ]
        );
    }

    /// §4.2's Anthropic column: a `thinking` block's signature rides beside
    /// the reasoning text, never inside it, because the next request must be
    /// able to echo exactly one value.
    #[test]
    fn anthropic_thinking_keeps_its_signature_apart() {
        let events = drain(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me think"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
            ],
        );
        assert!(events.contains(&StreamEvent::ReasoningDelta {
            text: "let me think".to_string(),
        }));
        assert!(events.contains(&StreamEvent::ReasoningSignature {
            signature: "sig-abc".to_string(),
        }));
    }

    /// T-PROV-026: event and delta names the decoder does not know are
    /// ignored — and, critically, never counted as malformed.
    #[test]
    fn an_unknown_event_is_ignored_not_counted() {
        let mut decoder = WireDecoder::new(ProviderKind::Anthropic);
        let events = decoder
            .decode(r#"{"type":"frobnicate","payload":{"type":"mystery_delta"}}"#)
            .expect("unknown events are fine");
        assert!(events.is_empty());
        assert_eq!(decoder.malformed_events(), 0);
    }

    /// In-band errors abort the stream with §4.5's fault, for all three
    /// shapes: Anthropic's typed envelope, an `OpenAI` numeric `code`, an
    /// `OpenAI` string `code`, and Ollama's bare string.
    #[test]
    fn an_in_band_error_fails_the_stream() {
        let mut decoder = WireDecoder::new(ProviderKind::Anthropic);
        let err = decoder
            .decode(r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#)
            .expect_err("an error payload aborts");
        assert_eq!(err.fault, ProviderFault::Server);
        assert_eq!(err.message, "busy");

        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        let err = decoder
            .decode(
                r#"{"error":{"message":"Slow down","type":"invalid_request_error","code":429}}"#,
            )
            .expect_err("429 in the body is 429");
        assert_eq!(err.fault, ProviderFault::RateLimited);

        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        let err = decoder
            .decode(r#"{"error":{"message":"too long","type":"invalid_request_error","code":"context_length_exceeded"}}"#)
            .expect_err("the string code refines the type");
        assert_eq!(err.fault, ProviderFault::ContextLength);

        let mut decoder = WireDecoder::new(ProviderKind::Ollama);
        let err = decoder
            .decode(r#"{"error":"pull model first"}"#)
            .expect_err("Ollama's string form aborts");
        assert_eq!(err.fault, ProviderFault::Server);
        assert_eq!(err.message, "pull model first");
    }

    /// T-PROV-025: five malformed payloads abort the stream. Four are dropped
    /// and counted; the fifth raises `E-PROV-MALFORMED`.
    #[test]
    fn five_malformed_payloads_abort_the_stream() {
        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        for n in 1..MALFORMED_LIMIT {
            let events = decoder
                .decode("{not json")
                .expect("below the limit a bad payload is dropped");
            assert!(events.is_empty());
            assert_eq!(decoder.malformed_events(), n);
        }
        let err = decoder
            .decode("{also not json")
            .expect_err("the fifth one aborts");
        assert_eq!(err.fault, ProviderFault::MalformedStream);
        assert_eq!(decoder.malformed_events(), MALFORMED_LIMIT);
    }

    /// §4.3's counter is about JSON that broke. A blank `data:` line and the
    /// `[DONE]` terminator are protocol, and neither may spend the budget.
    #[test]
    fn keep_alive_and_done_are_not_malformed() {
        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        for payload in ["", "   ", "[DONE]"] {
            decoder.decode(payload).expect("protocol, not damage");
        }
        assert_eq!(decoder.malformed_events(), 0);
    }

    /// T-PROV-001's sibling, `OpenAI`-shaped: the `finish_reason` chunk does
    /// not end the stream — the `include_usage` chunk arrives *after* it — so
    /// `Usage` is decoded first and `Finish` still comes last.
    #[test]
    fn openai_reports_usage_after_finish_but_orders_it_first() {
        let events = drain(
            ProviderKind::Openai,
            &[
                r#"{"id":"chatcmpl-1","model":"gpt-x","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
                r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_7","type":"function","function":{"name":"get_weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
                r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"{"usage":{"prompt_tokens":30,"completion_tokens":12,"prompt_tokens_details":{"cached_tokens":5}},"choices":[]}"#,
                "[DONE]",
            ],
        );
        let usage_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Usage { .. }))
            .expect("usage decoded");
        let finish_at = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Finish { .. }))
            .expect("finish decoded");
        assert!(usage_at < finish_at, "{events:?}");
        assert_eq!(
            events[usage_at],
            StreamEvent::Usage {
                input: 30,
                output: 12,
                cache_read: 5,
                cache_write: 0,
            }
        );
        assert_eq!(
            events[finish_at],
            StreamEvent::Finish {
                stop: StopReason::ToolUse,
            }
        );
        assert!(
            events.contains(&StreamEvent::ToolCallEnd { index: 0 }),
            "the provider completed the response, so the call closes: {events:?}"
        );
    }

    /// T-PROV-002's shape: the stream stops mid-tool-call — no finish signal,
    /// no terminator. `finish()` must not invent the missing `ToolCallEnd`,
    /// because an unfinished argument buffer must never reach a tool, and
    /// must not invent a `Finish` either: the turn did not end.
    #[test]
    fn a_truncated_stream_leaves_tools_open_and_never_finishes() {
        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        let mut events = decoder
            .decode(r#"{"id":"c","model":"gpt-x","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"f","arguments":"{\"a\":"}}]},"finish_reason":null}]}"#)
            .expect("partial tool call decodes");
        events.extend(decoder.finish());
        assert!(events.contains(&StreamEvent::ToolCallStart {
            index: 0,
            id: "call_9".to_string(),
            name: "f".to_string(),
        }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallEnd { .. })),
            "no completion signal, no End: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Finish { .. })),
            "the turn did not end: {events:?}"
        );
    }

    /// §4.3: a delta for an index with no `ToolCallStart` opens a synthetic
    /// one. The id is minted (`synthetic-{index}`) and the name stays empty —
    /// nothing in the stream has said it yet.
    #[test]
    fn a_delta_for_an_unknown_index_opens_a_synthetic_start() {
        let events = drain(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart {
                    index: 2,
                    id: "synthetic-2".to_string(),
                    name: String::new(),
                },
                StreamEvent::ToolCallDelta {
                    index: 2,
                    args_delta: "{\"a\":1}".to_string(),
                },
            ]
        );
    }

    /// Proxies disagree about the reasoning key: `reasoning_content` (vLLM
    /// and friends) or `reasoning` (some `OpenAI` builds). Both are reasoning
    /// text, not tool arguments.
    #[test]
    fn proxy_reasoning_fields_become_reasoning_deltas() {
        let first = drain(
            ProviderKind::OpenaiCompatible,
            &[
                r#"{"model":"m","choices":[{"index":0,"delta":{"reasoning_content":"because"},"finish_reason":null}]}"#,
            ],
        );
        let second = drain(
            ProviderKind::Vllm,
            &[
                r#"{"model":"m","choices":[{"index":0,"delta":{"reasoning":"because"},"finish_reason":null}]}"#,
            ],
        );
        for events in [first, second] {
            assert!(events.contains(&StreamEvent::ReasoningDelta {
                text: "because".to_string(),
            }));
        }
    }

    /// Ollama speaks NDJSON rather than SSE, but the events are the same:
    /// one `MessageStart` with no id, text deltas, one merged `Usage`, one
    /// `Finish`.
    #[test]
    fn ollama_lines_decode_to_the_same_events() {
        let events = drain(
            ProviderKind::Ollama,
            &[
                r#"{"model":"llama3.1","message":{"role":"assistant","content":"Hello"},"done":false}"#,
                r#"{"model":"llama3.1","message":{"role":"assistant","content":" world"},"done":false}"#,
                r#"{"model":"llama3.1","message":{"role":"assistant"},"done":true,"done_reason":"stop","prompt_eval_count":40,"eval_count":9}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    model: "llama3.1".to_string(),
                    id: String::new(),
                },
                StreamEvent::TextDelta {
                    text: "Hello".to_string(),
                },
                StreamEvent::TextDelta {
                    text: " world".to_string(),
                },
                StreamEvent::Usage {
                    input: 40,
                    output: 9,
                    cache_read: 0,
                    cache_write: 0,
                },
                StreamEvent::Finish {
                    stop: StopReason::EndTurn,
                },
            ]
        );
    }

    /// One Ollama line carries a whole tool call — name and complete object
    /// arguments included — so it becomes a start, the assembled arguments,
    /// and an end on the spot. And `done_reason: "stop"` after a tool call
    /// means the turn stopped *on the tool* (§4.4's last row).
    #[test]
    fn an_ollama_tool_call_completes_in_one_line_and_upgrades_the_stop() {
        let events = drain(
            ProviderKind::Ollama,
            &[
                r#"{"model":"llama3.1","message":{"role":"assistant","tool_calls":[{"function":{"name":"get_weather","arguments":{"city":"Paris"}}}]},"done":false}"#,
                r#"{"model":"llama3.1","message":{"role":"assistant"},"done":true,"done_reason":"stop","prompt_eval_count":40,"eval_count":9}"#,
            ],
        );
        assert!(events.contains(&StreamEvent::ToolCallStart {
            index: 0,
            id: "synthetic-0".to_string(),
            name: "get_weather".to_string(),
        }));
        assert!(events.contains(&StreamEvent::ToolCallDelta {
            index: 0,
            args_delta: "{\"city\":\"Paris\"}".to_string(),
        }));
        assert!(events.contains(&StreamEvent::ToolCallEnd { index: 0 }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::ToolUse,
        }));
    }

    /// A repeated `message_start` does not restart the stream: one
    /// `MessageStart`, and the first event's usage row is the one the
    /// eventual `Usage` is merged from.
    #[test]
    fn only_one_message_start_is_emitted() {
        let events = drain(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"message_start","message":{"id":"a","model":"m","usage":{"input_tokens":10}}}"#,
                r#"{"type":"message_start","message":{"id":"b","model":"m","usage":{"input_tokens":99}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        let starts: Vec<&StreamEvent> = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::MessageStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "{events:?}");
        assert!(events.contains(&StreamEvent::Usage {
            input: 10,
            output: 3,
            cache_read: 0,
            cache_write: 0,
        }));
    }

    /// `finish()` flushes once: the second call has already said everything
    /// and returns nothing, rather than emitting a second `Finish`.
    #[test]
    fn finish_flushes_only_once() {
        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        decoder
            .decode(r#"{"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#)
            .expect("stop decodes");
        decoder.decode("[DONE]").expect("terminator decodes");
        let first = decoder.finish();
        assert_eq!(
            first,
            vec![StreamEvent::Finish {
                stop: StopReason::EndTurn,
            }]
        );
        assert!(decoder.finish().is_empty());
    }

    /// T-PROV-028: everything after the terminator is ignored — never
    /// decoded, never counted, however broken it is.
    #[test]
    fn garbage_after_done_is_ignored_not_counted() {
        let mut decoder = WireDecoder::new(ProviderKind::Openai);
        decoder.decode("[DONE]").expect("terminator decodes");
        let events = decoder.decode("{not json").expect("post-terminator noise");
        assert!(events.is_empty());
        assert_eq!(decoder.malformed_events(), 0);
        assert_eq!(
            decoder.finish(),
            vec![StreamEvent::Finish {
                stop: StopReason::EndTurn,
            }],
            "the terminator still counts as a normal ending"
        );
    }

    /// A `content_block_start` that carries its arguments inline (rather than
    /// as deltas) is forwarded, not dropped — otherwise the tool would run
    /// with an empty `{}` while the provider had said what it wanted.
    #[test]
    fn inline_input_is_forwarded_not_dropped() {
        let events = drain(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{"a":1}}}"#,
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "t1".to_string(),
                    name: "f".to_string(),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    args_delta: "{\"a\":1}".to_string(),
                },
            ]
        );
    }

    /// `max_tokens` ends the response without completing it: the turn still
    /// gets a `Finish` (the consumer must know why it stopped), but an open
    /// tool call gets no `ToolCallEnd` — its arguments are truncated and must
    /// not execute.
    #[test]
    fn max_tokens_finishes_the_turn_but_never_executes_partial_args() {
        let events = drain(
            ProviderKind::Anthropic,
            &[
                r#"{"type":"message_start","message":{"id":"a","model":"m","usage":{"input_tokens":10}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":5}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::MaxTokens,
        }));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallEnd { .. })),
            "truncated arguments never become a call: {events:?}"
        );
    }
}
