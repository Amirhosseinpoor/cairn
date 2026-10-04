//! The data that crosses the `Provider` boundary: the §3.4 signatures' named
//! types, plus the §4.9 registry id and the §4.8 token count.

use std::fmt;

use serde::{Deserialize, Serialize};

use cairn_core::message::{Block, Message, StopReason, Usage};

/// A key of §4.9's `providers` map, e.g. `anthropic`.
///
/// Not an `id_newtype`: those mint ULIDs, and a provider key is a fixed,
/// human-typed name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// A provider key. §4.9's five kinds are `anthropic`, `openai`,
    /// `openai_compatible`, `ollama` and `vllm`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The key as written in config and in a model id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Split a registry model id (`anthropic/claude-sonnet-4-5`) into its
    /// provider key and the model name. Returns `None` when the id carries no
    /// `/`, which §4.9 never permits.
    #[must_use]
    pub fn split_model_id(model_id: &str) -> Option<(Self, &str)> {
        let (provider, model) = model_id.split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some((Self::new(provider), model))
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl AsRef<str> for ProviderId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// §3.4 `Capabilities` — what an adapter can do, as advertised to §4.2's
/// branching and §4.6's prompt-fallback decision.
///
/// `max_context` and `max_output` come from the model entry, not the §4.9
/// `capabilities` object, which carries only the seven booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tool_calling: bool,
    pub streaming: bool,
    /// Native reasoning/thinking content (§4.2's row 3).
    pub reasoning: bool,
    /// `cache_control` breakpoints / prompt caching (§4.2 row 4).
    pub prompt_cache: bool,
    pub vision: bool,
    pub parallel_tool_calls: bool,
    /// Strict structured outputs.
    pub json_schema_strict: bool,
    pub max_context: u32,
    pub max_output: u32,
}

impl Capabilities {
    /// The §4.2 baseline every adapter starts from before the registry
    /// overrides the two limits.
    ///
    /// Only `streaming` is asserted here, because §3.4 makes it the one thing
    /// every `Provider` guarantees. Everything else is a claim about a
    /// specific model, so it comes from §4.2's matrix or §4.9's registry —
    /// never from a default that might make §4.6 pick prompt fallback for an
    /// adapter that does support native tool calls.
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            tool_calling: false,
            streaming: true,
            reasoning: false,
            prompt_cache: false,
            vision: false,
            parallel_tool_calls: false,
            json_schema_strict: false,
            max_context: 0,
            max_output: 0,
        }
    }

    /// A §4.9 model row turned into §3.4's view of it (T-PROV-003:
    /// `capabilities()` equals the registry row).
    ///
    /// Seven booleans from the row's `capabilities` object, two limits from
    /// the row's `context_window` and `max_output` — which is why the struct
    /// has nine fields and the JSON object has seven.
    #[must_use]
    pub const fn from_entry(entry: &cairn_core::registry::ModelEntry) -> Self {
        let flags = entry.capabilities;
        Self {
            tool_calling: flags.tool_calling,
            streaming: flags.streaming,
            reasoning: flags.reasoning,
            prompt_cache: flags.prompt_cache,
            vision: flags.vision,
            parallel_tool_calls: flags.parallel_tool_calls,
            json_schema_strict: flags.json_schema_strict,
            max_context: entry.context_window,
            max_output: entry.max_output,
        }
    }
}

/// §3.4 `StreamEvent` — the only shape a caller ever sees, whatever the wire
/// format was. Adapters map SSE `content_block_delta`, `delta`,
/// `response.output_text.delta` and Ollama's NDJSON onto these (§4.2 row 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        model: String,
        id: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: u32,
        args_delta: String,
    },
    ToolCallEnd {
        index: u32,
    },
    Usage {
        input: u32,
        output: u32,
        cache_read: u32,
        cache_write: u32,
    },
    Finish {
        stop: StopReason,
    },
    Ping,
}

/// §4.8 token accounting, in the shape [`Usage`] already uses.
///
/// Deliberately an alias rather than a second struct: §4.8's arithmetic runs
/// on one number line, and REQ-PROV-011's `estimated` flag only carries
/// information if the estimate and the provider's eventual report share it.
/// `count_tokens` returns an input count with `output` and the cache fields at
/// 0.
pub type TokenCount = Usage;

/// §3.4 `health()` — cheap and non-network: "could a call be attempted now?",
/// not "is the service up?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProviderHealth {
    /// Credentials found (§4.10) and the model id resolves (§4.9).
    Ready,
    /// Nothing in §4.10's lookup order answered.
    NoCredentials,
    /// The model id is absent from the registry and `models.<id>` is unset
    /// (REQ-PROV-013).
    UnknownModel,
    /// Present but unusable: a bad `base_url`, unparsable config, and so on.
    Misconfigured { reason: String },
}

/// The model-facing half of a tool (§3.4, §4.6).
///
/// `cairn-provider` MUST NOT import `cairn-tools` (§3.2), so an executable
/// tool is projected into this value before the call; the projection is what
/// §4.6's prompt fallback re-serialises.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    /// Model-facing, ≤ 2000 chars (§3.4).
    pub description: String,
    /// JSON Schema draft 2020-12 (§3.4).
    pub input_schema: serde_json::Value,
}

/// One model call, in the provider-neutral form §4.4 then shapes per adapter.
///
/// There is no `provider` field: `Provider::stream` is a method on the
/// adapter, which already knows which §4.9 entry it is, and `id()` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    /// The full registry id, e.g. `anthropic/claude-sonnet-4-5`.
    pub model: String,
    /// Conversation in order, including any `Role::System` messages — §4.4's
    /// first row is what decides how each adapter presents those.
    pub messages: Vec<Message>,
    /// Model-facing tool list (§4.6).
    pub tools: Vec<ToolSpec>,
    /// Required by Anthropic (§4.4), honoured by all five.
    pub max_tokens: u32,
    /// `None` leaves the provider's own default alone.
    pub temperature: Option<f64>,
    /// Strict structured-output schema; only meaningful when
    /// `capabilities.json_schema_strict` (§4.2).
    pub response_format: Option<serde_json::Value>,
    /// Turn that issued the call — carried into §12.1's trace.
    pub turn_id: u64,
}

impl ModelRequest {
    /// A request with the mandatory fields set and the optionals left unset.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>, max_tokens: u32) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            max_tokens,
            temperature: None,
            response_format: None,
            turn_id: 0,
        }
    }

    /// The system-role messages, in order (§4.4 row 1).
    pub fn system_blocks(&self) -> impl Iterator<Item = &Block> {
        self.messages
            .iter()
            .filter(|message| message.role == cairn_core::message::Role::System)
            .flat_map(|message| message.blocks.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::message::Role;

    /// §4.9's model ids are `provider/name`; anything without a `/` is not a
    /// registry id at all.
    #[test]
    fn model_ids_split_on_the_first_slash() {
        assert_eq!(
            ProviderId::split_model_id("anthropic/claude-sonnet-4-5"),
            Some((ProviderId::new("anthropic"), "claude-sonnet-4-5"))
        );
        // The model name may itself contain slashes.
        assert_eq!(
            ProviderId::split_model_id("ollama/qwen2.5-coder:14b"),
            Some((ProviderId::new("ollama"), "qwen2.5-coder:14b"))
        );
        assert_eq!(
            ProviderId::split_model_id("vllm/meta/Llama-3"),
            Some((ProviderId::new("vllm"), "meta/Llama-3"))
        );
        assert_eq!(ProviderId::split_model_id("claude-sonnet-4-5"), None);
        assert_eq!(ProviderId::split_model_id("/lonely"), None);
        assert_eq!(ProviderId::split_model_id("lonely/"), None);
    }

    /// §3.4's `Capabilities` is a value type: it round-trips, so the registry
    /// and the trace agree on what an adapter advertised.
    #[test]
    fn capabilities_round_trip() {
        let mut capabilities = Capabilities::baseline();
        capabilities.tool_calling = true;
        capabilities.reasoning = true;
        capabilities.max_context = 200_000;
        capabilities.max_output = 64_000;

        let json = serde_json::to_value(capabilities).expect("serialize");
        assert_eq!(json["max_context"], 200_000);
        assert_eq!(json["tool_calling"], true);
        let back: Capabilities = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, capabilities);
    }

    /// §12.1's `--trace` writes the "full `StreamEvent` sequence"; the tag and
    /// the field names are what make it replayable, so pin them.
    #[test]
    fn stream_events_serialize_with_a_stable_tag() {
        let json = serde_json::to_value(StreamEvent::TextDelta {
            text: "hi".to_string(),
        })
        .expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({"type": "text_delta", "text": "hi"})
        );

        let json = serde_json::to_value(StreamEvent::Finish {
            stop: StopReason::ToolUse,
        })
        .expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({"type": "finish", "stop": "tool_use"})
        );

        let json = serde_json::to_value(StreamEvent::Ping).expect("serialize");
        assert_eq!(json, serde_json::json!({"type": "ping"}));

        let json = serde_json::to_value(StreamEvent::ToolCallDelta {
            index: 2,
            args_delta: "{\"path\"".to_string(),
        })
        .expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({"type": "tool_call_delta", "index": 2, "args_delta": "{\"path\""})
        );
    }

    /// Every variant round-trips: a trace replayed through §12.1 must not lose
    /// a variant the serializer does not know.
    #[test]
    fn every_stream_event_round_trips() {
        let events = vec![
            StreamEvent::MessageStart {
                model: "m".into(),
                id: "i".into(),
            },
            StreamEvent::TextDelta { text: "t".into() },
            StreamEvent::ReasoningDelta { text: "r".into() },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args_delta: "{}".into(),
            },
            StreamEvent::ToolCallEnd { index: 0 },
            StreamEvent::Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
            },
            StreamEvent::Finish {
                stop: StopReason::EndTurn,
            },
            StreamEvent::Ping,
        ];
        assert_eq!(events.len(), 9, "§3.4 lists nine variants");
        for event in events {
            let json = serde_json::to_value(&event).expect("serialize");
            let back: StreamEvent = serde_json::from_value(json).expect("deserialize");
            assert_eq!(back, event);
        }
    }

    /// A request's optional fields default to "leave it to the provider", and
    /// the system-role extraction is what §4.4's first row operates on.
    #[test]
    fn a_request_extracts_its_system_blocks() {
        let mut request = ModelRequest::new("anthropic/claude-sonnet-4-5", vec![], 4096);
        assert_eq!(request.temperature, None);
        assert_eq!(request.response_format, None);
        assert!(request.tools.is_empty());

        request.messages = vec![
            Message::new(
                Role::System,
                vec![Block::Text {
                    text: "be brief".into(),
                }],
                1,
            ),
            Message::user("hello", 1),
        ];
        let system: Vec<String> = request
            .system_blocks()
            .map(|block| match block {
                Block::Text { text } => text.clone(),
                other => panic!("unexpected system block {other:?}"),
            })
            .collect();
        assert_eq!(system, ["be brief"]);
    }

    /// `TokenCount` is `Usage`, which is what keeps REQ-PROV-011's flag
    /// meaningful: an estimate and a provider report are the same type, and
    /// only the flag tells them apart.
    #[test]
    fn a_token_count_is_a_usage_with_the_estimated_flag() {
        let count = TokenCount::estimate(1_024);
        assert!(count.estimated, "estimates start flagged (REQ-PROV-011)");
        assert_eq!(count.output, 0, "a count says nothing about output");

        let reported = TokenCount::reported(900, 120, 64, 0);
        assert!(!reported.estimated);
        assert_eq!(reported.output, 120);
    }
}
