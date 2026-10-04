//! Unified internal message model (SPEC §4.1).

use crate::ids::MessageId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Message roles (SPEC §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Image media types accepted by the vision path (SPEC §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Png,
    Jpeg,
    Gif,
    Webp,
}

/// Content blocks (SPEC §4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    Image {
        media_type: MediaType,
        data_b64: String,
        alt: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: serde_json::Value,
        /// `true` while the provider stream is still assembling `input`.
        partial: bool,
        parse_error: Option<String>,
    },
    ToolResult {
        call_id: String,
        content: Vec<Block>,
        is_error: bool,
    },
    ThinkingPlaceholder {
        text: String,
    },
}

/// Token usage attached to assistant messages (SPEC §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
    /// `false` once the provider reported real numbers (REQ-PROV-011).
    pub estimated: bool,
}

/// Why the model stopped (SPEC §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    ContentFilter,
    Cancelled,
    Error,
}

/// A message (SPEC §4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub blocks: Vec<Block>,
    /// RFC3339 timestamp; schema expresses it as a string (SPEC §3.5 style).
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
    /// Set for assistant messages only.
    pub usage: Option<Usage>,
    /// Turn that produced the message.
    pub turn_id: u64,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, blocks: Vec<Block>, turn_id: u64) -> Self {
        Self {
            id: MessageId::new(),
            role,
            blocks,
            created_at: Utc::now(),
            usage: None,
            turn_id,
        }
    }

    #[must_use]
    pub fn user(text: impl Into<String>, turn_id: u64) -> Self {
        Self::new(Role::User, vec![Block::Text { text: text.into() }], turn_id)
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>, turn_id: u64) -> Self {
        Self::new(
            Role::Assistant,
            vec![Block::Text { text: text.into() }],
            turn_id,
        )
    }

    /// Concatenated plain text of all `Text` blocks.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.blocks {
            if let Block::Text { text } = b {
                out.push_str(text);
            }
        }
        out
    }

    /// Every tool call in this message (SPEC §4.1 invariants, REQ-PROV-001).
    #[must_use]
    pub fn tool_calls(&self) -> Vec<(&str, &str)> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall {
                    call_id,
                    name,
                    partial,
                    ..
                } if !*partial => Some((call_id.as_str(), name.as_str())),
                _ => None,
            })
            .collect()
    }

    /// `true` when this tool message satisfies exactly one call id (REQ-PROV-001).
    #[must_use]
    pub fn tool_result_matches(&self, call_id: &str) -> bool {
        let mut found = 0;
        for b in &self.blocks {
            if let Block::ToolResult { call_id: cid, .. } = b {
                if cid == call_id {
                    found += 1;
                }
            }
        }
        found == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_serialize_as_spec() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), r#""system""#);
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), r#""tool""#);
    }

    #[test]
    fn block_kinds_are_tagged() {
        let b = Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command":"true"}),
            partial: false,
            parse_error: None,
        };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["kind"], "tool_call");
        assert_eq!(v["name"], "bash");
        let back: Block = serde_json::from_value(v).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn tool_call_invariants() {
        let m = Message::new(
            Role::Assistant,
            vec![
                Block::Text {
                    text: "doing it".into(),
                },
                Block::ToolCall {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                    partial: false,
                    parse_error: None,
                },
            ],
            3,
        );
        assert_eq!(m.tool_calls(), vec![("c1", "read_file")]);
        assert_eq!(m.text(), "doing it");

        let r = Message::new(
            Role::Tool,
            vec![Block::ToolResult {
                call_id: "c1".into(),
                content: vec![Block::Text {
                    text: "file body".into(),
                }],
                is_error: false,
            }],
            3,
        );
        assert!(r.tool_result_matches("c1"));
        assert!(!r.tool_result_matches("c2"));
    }

    #[test]
    fn usage_estimated_flag_defaults_false_after_provider_report() {
        let u = Usage::default();
        assert!(!u.estimated);
    }
}
