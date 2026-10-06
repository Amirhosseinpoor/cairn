//! Helpers the built-in tools share.

use std::path::Path;

use cairn_core::error::codes;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::types::ToolError;

/// Decode a validated input into its typed form. Validation already ran, so a
/// failure here is a mismatch between a tool's schema and its struct — a bug
/// in this crate, reported rather than panicked on.
pub fn parse<T: DeserializeOwned>(tool: &str, input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| {
        ToolError::new(
            codes::TOOL_BADSCHEMA,
            format!("`{tool}` could not read its input: {e}"),
        )
    })
}

/// Map an I/O failure on `shown` to the filesystem codes of §6.2.1.
#[must_use]
pub fn io_error(shown: &str, error: &std::io::Error) -> ToolError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => {
            ToolError::new(codes::FS_NOTFOUND, format!("`{shown}` does not exist."))
                .recovery("Check the path with list_dir.")
        }
        ErrorKind::PermissionDenied => ToolError::new(
            codes::FS_PERM,
            format!("`{shown}` is not accessible to this user."),
        )
        .recovery("File is not readable by this user."),
        _ => ToolError::new(codes::FS_PERM, format!("`{shown}`: {error}")),
    }
}

/// `path` as a model should see it.
#[must_use]
pub fn show(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).map_or_else(
        |_| path.to_string_lossy().replace('\\', "/"),
        |rel| {
            let text = rel.to_string_lossy().replace('\\', "/");
            if text.is_empty() {
                ".".to_string()
            } else {
                text
            }
        },
    )
}

/// The JSON-Schema stub every tool's output schema starts from.
#[must_use]
pub fn object_schema(properties: &Value) -> Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
    })
}
