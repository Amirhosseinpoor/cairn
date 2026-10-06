//! Input validation against a tool's JSON Schema (SPEC §6.5 steps 1–2).
//!
//! One validation pass, two outcomes. A *shape* problem — wrong type, missing
//! field, unknown field — is `E-TOOL-BADSCHEMA`. A *size* problem — a string
//! or array over its declared maximum — is `E-TOOL-TOOBIG`, because the right
//! recovery differs: fix the call versus send less. When both are present the
//! shape error is reported, since fixing it may change the sizes.

use std::fmt::Write as _;

use cairn_core::error::codes;
use jsonschema::error::ValidationErrorKind;
use serde_json::Value;

use crate::types::ToolError;

/// A compiled schema.
pub struct Compiled {
    validator: jsonschema::Validator,
}

impl std::fmt::Debug for Compiled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Compiled(..)")
    }
}

/// The whole input may not exceed this many bytes of JSON, whatever the
/// per-field limits say (a defence against a schema with no `maxLength`).
pub const MAX_INPUT_BYTES: usize = 1_048_576;

/// How many violations one error message lists.
const MAX_REPORTED: usize = 5;

impl Compiled {
    /// Compile `schema` (draft 2020-12).
    ///
    /// # Errors
    /// The compiler's message when the schema itself is invalid.
    pub fn new(schema: &Value) -> Result<Self, String> {
        jsonschema::draft202012::new(schema)
            .map(|validator| Self { validator })
            .map_err(|e| e.to_string())
    }

    /// Validate `input`.
    ///
    /// # Errors
    /// `E-TOOL-BADSCHEMA` or `E-TOOL-TOOBIG`, with JSON-pointer paths.
    pub fn check(&self, tool: &str, input: &Value) -> Result<(), ToolError> {
        if input.to_string().len() > MAX_INPUT_BYTES {
            return Err(ToolError::new(
                codes::TOOL_TOOBIG,
                format!("the input to `{tool}` is larger than {MAX_INPUT_BYTES} bytes"),
            )
            .recovery("Send a smaller input; split the work across several calls."));
        }
        let mut shape = Vec::new();
        let mut size = Vec::new();
        for error in self.validator.iter_errors(input) {
            let at = error.instance_path().to_string();
            let at = if at.is_empty() { "/".to_string() } else { at };
            let line = format!("{at}: {error}");
            match error.kind() {
                ValidationErrorKind::MaxLength { .. }
                | ValidationErrorKind::MaxItems { .. }
                | ValidationErrorKind::MaxProperties { .. } => size.push(line),
                _ => shape.push(line),
            }
        }
        let list = |lines: &[String]| {
            let shown = lines.iter().take(MAX_REPORTED).cloned().collect::<Vec<_>>();
            let more = lines.len().saturating_sub(MAX_REPORTED);
            let mut text = shown.join("; ");
            if more > 0 {
                let _ = write!(text, "; and {more} more");
            }
            text
        };
        if !shape.is_empty() {
            return Err(ToolError::new(
                codes::TOOL_BADSCHEMA,
                format!(
                    "the input to `{tool}` does not match its schema — {}",
                    list(&shape)
                ),
            )
            .recovery("Fix the fields named above and call the tool again."));
        }
        if !size.is_empty() {
            return Err(ToolError::new(
                codes::TOOL_TOOBIG,
                format!(
                    "the input to `{tool}` has a field over its limit — {}",
                    list(&size)
                ),
            )
            .recovery("Shorten the field, or split the work across several calls."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Compiled {
        Compiled::new(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["path"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "maxLength": 8},
                "limit": {"type": "integer", "minimum": 1, "maximum": 10},
                "tags": {"type": "array", "maxItems": 2, "items": {"type": "string"}}
            }
        }))
        .expect("compiles")
    }

    #[test]
    fn a_valid_input_passes() {
        assert!(schema()
            .check("t", &json!({"path": "a.rs", "limit": 3}))
            .is_ok());
    }

    #[test]
    fn shape_errors_are_badschema_with_pointer_paths() {
        let s = schema();
        for (input, needle) in [
            (json!({}), "path"),
            (json!({"path": 5}), "/path"),
            (json!({"path": "a", "limit": 0}), "/limit"),
            (json!({"path": "a", "limit": "x"}), "/limit"),
            (json!({"path": "a", "extra": 1}), "extra"),
            (json!("not an object"), "object"),
        ] {
            let err = s.check("t", &input).expect_err("rejected");
            assert_eq!(err.code, "E-TOOL-BADSCHEMA", "{input}");
            assert!(err.message.contains(needle), "{input}: {}", err.message);
            assert!(err.recovery.is_some());
        }
    }

    #[test]
    fn size_errors_are_toobig() {
        let s = schema();
        let err = s
            .check("t", &json!({"path": "far-too-long"}))
            .expect_err("long");
        assert_eq!(err.code, "E-TOOL-TOOBIG");
        assert!(err.message.contains("/path"), "{}", err.message);
        let err = s
            .check("t", &json!({"path": "a", "tags": ["a", "b", "c"]}))
            .expect_err("many");
        assert_eq!(err.code, "E-TOOL-TOOBIG");
    }

    #[test]
    fn a_shape_error_outranks_a_size_error() {
        let err = schema()
            .check("t", &json!({"path": "far-too-long", "limit": 0}))
            .expect_err("both");
        assert_eq!(err.code, "E-TOOL-BADSCHEMA");
    }

    #[test]
    fn an_enormous_input_is_toobig_whatever_the_schema() {
        let s = Compiled::new(&json!({"type": "object"})).expect("compiles");
        let big = "x".repeat(MAX_INPUT_BYTES + 1);
        assert_eq!(
            s.check("t", &json!({"a": big})).expect_err("big").code,
            "E-TOOL-TOOBIG"
        );
    }

    #[test]
    fn many_violations_are_summarised_not_dumped() {
        let s = Compiled::new(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}, "b": {"type": "string"}, "c": {"type": "string"},
                           "d": {"type": "string"}, "e": {"type": "string"}, "f": {"type": "string"},
                           "g": {"type": "string"}}
        }))
        .expect("compiles");
        let err = s
            .check("t", &json!({"a":1,"b":1,"c":1,"d":1,"e":1,"f":1,"g":1}))
            .expect_err("rejected");
        assert!(err.message.contains("and 2 more"), "{}", err.message);
    }

    #[test]
    fn an_invalid_schema_is_reported_not_swallowed() {
        assert!(Compiled::new(&json!({"type": "nonsense"})).is_err());
    }
}
