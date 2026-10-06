//! `write_file` (SPEC §6.2.2): create a file or replace one wholesale.

use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{object_schema, parse};
use super::fsio::{
    check_syntax, line_delta, load, resolve_for_write, sha256_hex, staleness, syntax_ok,
    write_atomic, Staleness,
};
use crate::edit::encode_like;
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, SideEffect, SyntaxVerdict, ToolContext,
    ToolError, ToolOutput,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
    content: String,
    create_dirs: Option<bool>,
    expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create a file, or replace one entirely, with the given content. Prefer edit_file for \
         changes to an existing file. An existing file's line endings and BOM are preserved. \
         Pass expected_sha256 (from read_file) to refuse the write if the file has changed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["path", "content"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "maxLength": 400_000},
                "create_dirs": {"type": "boolean", "default": true},
                "expected_sha256": {"type": ["string", "null"], "default": null,
                    "description": "If set, the file must currently hash to this or the write fails with E-FS-STALE"}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "path": {"type": "string"}, "bytes_written": {"type": "integer"},
            "sha256": {"type": "string"}, "created": {"type": "boolean"},
            "lines_added": {"type": "integer"}, "lines_removed": {"type": "integer"}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Write
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Retryable
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(20)
    }

    fn max_output_bytes(&self) -> u32 {
        8 * 1024
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(|p| PathArg {
                field: "path",
                value: p.to_string(),
                access: Access::Write,
            })
            .into_iter()
            .collect()
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: Input = parse("write_file", input)?;
            write(&input, &ctx).await
        }
        .boxed()
    }
}

async fn write(input: &Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let resolved = resolve_for_write(ctx, &input.path)?;
    let shown = resolved.display();
    let existing = load(&resolved.abs, &shown)?;

    let new_text = input.content.replace("\r\n", "\n");
    let (old_text, bom, eol, mode) = if let Some(file) = &existing {
        // The model overwrote what it did not see, or saw an old copy.
        if let Staleness::Changed { .. } = staleness(
            ctx,
            &resolved.abs,
            &file.sha256,
            input.expected_sha256.as_deref(),
        ) {
            return Err(ToolError::new(
                codes::FS_STALE,
                format!("`{shown}` changed since you last saw it."),
            )
            .recovery("Re-read the file; it changed since you saw it."));
        }
        (
            file.doc.text.as_str(),
            file.doc.had_bom,
            file.doc.eol,
            file.mode,
        )
    } else {
        if input.expected_sha256.is_some() {
            return Err(ToolError::new(
                codes::FS_STALE,
                format!("`{shown}` does not exist, so it cannot match the hash you gave."),
            )
            .recovery("Drop expected_sha256 to create the file."));
        }
        ("", false, ctx.line_endings.for_new_files(), None)
    };

    // §6.3.6's rollback is specified for *edits*. A whole-file write may be
    // deliberately malformed (a test fixture for a parser, a template), so it
    // is checked and reported, never refused.
    let verdict = check_syntax(ctx, &shown, old_text, &new_text).await;
    let bytes = encode_like(&new_text, bom, eol);
    write_atomic(
        &resolved.abs,
        &bytes,
        mode,
        input.create_dirs.unwrap_or(true),
    )?;
    let sha = sha256_hex(&bytes);
    ctx.file_state
        .record(resolved.abs.clone(), sha.clone(), Some(&new_text));
    let (added, removed) = line_delta(old_text, &new_text);
    let mut data = json!({
        "path": shown,
        "bytes_written": bytes.len(),
        "sha256": sha,
        "created": existing.is_none(),
        "lines_added": added,
        "lines_removed": removed,
        "syntax_ok": syntax_ok(&verdict),
    });
    if let SyntaxVerdict::Invalid(problem) = &verdict {
        data["syntax_ok"] = json!(false);
        data["syntax_warning"] = json!(format!(
            "line {}, column {}: the file was written, but it does not parse",
            problem.line, problem.column
        ));
    }
    Ok(ToolOutput::new(data))
}
