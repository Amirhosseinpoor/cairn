//! §4.3 partial tool-argument assembly: per-`index` buffers, the parse, and the
//! (a)/(b)/(c) repairs. Pure — no I/O, no timers.

use std::collections::BTreeMap;

use cairn_core::message::Block;
use serde_json::Value;

use crate::StreamEvent;

/// §4.3's brace-closing limit: deeper nesting is not repaired.
const MAX_REPAIR_DEPTH: usize = 8;

/// Parse a finished argument buffer. An empty buffer is `{}`; otherwise the
/// text is parsed as is, then once more after §4.3's repairs. The error is the
/// *original* parse failure, which is what the model should be told.
///
/// # Errors
/// The `serde_json` message for the unrepaired text.
pub fn parse_tool_args(buffer: &str) -> Result<Value, String> {
    if buffer.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let original = match serde_json::from_str(buffer) {
        Ok(value) => return Ok(value),
        Err(error) => error.to_string(),
    };
    repair(buffer)
        .and_then(|fixed| serde_json::from_str(&fixed).ok())
        .ok_or(original)
}

/// Apply (a) close unbalanced braces/brackets (depth ≤ 8), (b) strip trailing
/// commas, (c) `NaN`/`Infinity` → `null`, in one pass outside string literals.
/// `None` when nothing changed or nesting exceeds the limit.
fn repair(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + MAX_REPAIR_DEPTH);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            rest = &rest[c.len_utf8()..];
            continue;
        }
        if let Some(word) = ["NaN", "-Infinity", "Infinity"]
            .iter()
            .find(|word| rest.starts_with(**word))
        {
            out.push_str("null");
            rest = &rest[word.len()..];
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => {
                stack.push(if c == '{' { '}' } else { ']' });
                if stack.len() > MAX_REPAIR_DEPTH {
                    return None;
                }
            }
            '}' | ']' => {
                strip_trailing_comma(&mut out);
                if stack.pop() != Some(c) {
                    return None;
                }
            }
            _ => {}
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    if in_string {
        return None;
    }
    strip_trailing_comma(&mut out);
    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    (out != text).then_some(out)
}

fn strip_trailing_comma(out: &mut String) {
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    if out.ends_with(',') {
        out.pop();
    }
}

#[derive(Debug)]
struct Open {
    id: String,
    name: String,
    args: String,
}

/// Folds `ToolCallStart`/`Delta`/`End` into finished [`Block::ToolCall`]s.
/// Calls still open when the stream is cut are never emitted (REQ-PROV-009).
#[derive(Debug, Default)]
pub struct ToolCallAssembler {
    open: BTreeMap<u32, Open>,
}

impl ToolCallAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one event; a finished call comes back on its `ToolCallEnd`. A
    /// `ToolCallEnd` for an index never opened yields nothing.
    pub fn push(&mut self, event: &StreamEvent) -> Option<Block> {
        match event {
            StreamEvent::ToolCallStart { index, id, name } => {
                self.open.insert(
                    *index,
                    Open {
                        id: id.clone(),
                        name: name.clone(),
                        args: String::new(),
                    },
                );
                None
            }
            StreamEvent::ToolCallDelta { index, args_delta } => {
                self.open
                    .entry(*index)
                    .or_insert_with(|| Open {
                        id: format!("synthetic-{index}"),
                        name: String::new(),
                        args: String::new(),
                    })
                    .args
                    .push_str(args_delta);
                None
            }
            StreamEvent::ToolCallEnd { index } => {
                let call = self.open.remove(index)?;
                let (input, parse_error) = match parse_tool_args(&call.args) {
                    Ok(value) => (value, None),
                    Err(message) => (Value::Null, Some(message)),
                };
                Some(Block::ToolCall {
                    call_id: call.id,
                    name: call.name,
                    input,
                    partial: false,
                    parse_error,
                })
            }
            _ => None,
        }
    }

    /// Calls opened and not yet ended.
    #[must_use]
    pub fn open_calls(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// T-PROV-030: trailing comma + unbalanced brace repair and execute.
    #[test]
    fn t_prov_030_trailing_comma_and_open_brace() {
        assert_eq!(parse_tool_args(r#"{"a":1,"#), Ok(json!({"a": 1})));
        assert_eq!(
            parse_tool_args(r#"{"a":[1,2,],"b":{"c":3,"#),
            Ok(json!({"a": [1, 2], "b": {"c": 3}}))
        );
    }

    /// T-PROV-031: `NaN` becomes `null`; what stays invalid is `E-TOOL-BADJSON`.
    #[test]
    fn t_prov_031_nan_and_infinity_become_null() {
        assert_eq!(
            parse_tool_args(r#"{"x":NaN,"y":-Infinity,"z":Infinity}"#),
            Ok(json!({"x": null, "y": null, "z": null}))
        );
        assert!(parse_tool_args(r#"{"x":NaN "y"}"#).is_err());
    }

    /// Literals inside strings are data, not numbers to repair.
    #[test]
    fn repairs_leave_string_contents_alone() {
        assert_eq!(
            parse_tool_args(r#"{"s":"NaN, }"#),
            Err("EOF while parsing a string at line 1 column 12".to_string())
        );
        assert_eq!(
            parse_tool_args(r#"{"s":"NaN, \" ]","#),
            Ok(json!({"s": "NaN, \" ]"}))
        );
    }

    /// T-PROV-002: a stream cut mid-key stays a parse error.
    #[test]
    fn t_prov_002_truncated_arguments_are_not_invented() {
        assert!(parse_tool_args(r#"{"path":"a.rs","old_"#).is_err());
        assert!(parse_tool_args(r#"{"path":"a.rs","old_str":"x"#).is_err());
    }

    #[test]
    fn empty_buffer_is_empty_object() {
        assert_eq!(parse_tool_args(""), Ok(json!({})));
        assert_eq!(parse_tool_args("  \n"), Ok(json!({})));
    }

    #[test]
    fn nesting_past_depth_eight_is_not_closed() {
        let deep = format!("{}1", "[".repeat(9));
        assert!(parse_tool_args(&deep).is_err());
        let ok = format!("{}1", "[".repeat(8));
        assert!(parse_tool_args(&ok).is_ok());
    }

    #[test]
    fn mismatched_closer_is_not_repaired() {
        assert!(parse_tool_args(r#"{"a":[1}"#).is_err());
    }

    fn feed(events: &[StreamEvent]) -> (ToolCallAssembler, Vec<Block>) {
        let mut assembler = ToolCallAssembler::new();
        let blocks = events.iter().filter_map(|e| assembler.push(e)).collect();
        (assembler, blocks)
    }

    #[test]
    fn a_call_assembles_from_split_deltas() {
        let (_, blocks) = feed(&[
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "read_file".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args_delta: r#"{"path":"#.into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args_delta: r#""a.rs"}"#.into(),
            },
            StreamEvent::ToolCallEnd { index: 0 },
        ]);
        assert_eq!(
            blocks,
            vec![Block::ToolCall {
                call_id: "c1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
                partial: false,
                parse_error: None,
            }]
        );
    }

    #[test]
    fn unknown_index_opens_a_synthetic_call() {
        let (_, blocks) = feed(&[
            StreamEvent::ToolCallDelta {
                index: 3,
                args_delta: "{}".into(),
            },
            StreamEvent::ToolCallEnd { index: 3 },
        ]);
        assert!(matches!(
            &blocks[..],
            [Block::ToolCall { call_id, name, .. }] if call_id == "synthetic-3" && name.is_empty()
        ));
    }

    #[test]
    fn unparseable_args_keep_the_call_with_parse_error() {
        let (_, blocks) = feed(&[
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "edit_file".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args_delta: r#"{"path":"a.rs","old_"#.into(),
            },
            StreamEvent::ToolCallEnd { index: 0 },
        ]);
        assert!(matches!(
            &blocks[..],
            [Block::ToolCall {
                parse_error: Some(_),
                partial: false,
                ..
            }]
        ));
    }

    /// A cut stream never emits its open call.
    #[test]
    fn open_calls_are_never_emitted() {
        let (assembler, blocks) = feed(&[
            StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args_delta: r#"{"cmd":"ls"#.into(),
            },
        ]);
        assert!(blocks.is_empty());
        assert_eq!(assembler.open_calls(), 1);
    }

    #[test]
    fn parallel_calls_interleave_by_index() {
        let (_, blocks) = feed(&[
            StreamEvent::ToolCallStart {
                index: 0,
                id: "a".into(),
                name: "x".into(),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "b".into(),
                name: "y".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                args_delta: r#"{"n":2}"#.into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                args_delta: r#"{"n":1}"#.into(),
            },
            StreamEvent::ToolCallEnd { index: 1 },
            StreamEvent::ToolCallEnd { index: 0 },
        ]);
        let ids: Vec<_> = blocks
            .iter()
            .map(|b| match b {
                Block::ToolCall { call_id, input, .. } => (call_id.clone(), input.clone()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                ("b".to_string(), json!({"n": 2})),
                ("a".to_string(), json!({"n": 1}))
            ]
        );
    }
}
