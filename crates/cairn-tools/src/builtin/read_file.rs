//! `read_file` (SPEC §6.2.1): a UTF-8 file or a line range of one, numbered.

use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_search::{classify, language_for, Content, SNIFF_BYTES};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::common::{io_error, object_schema, parse};
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, SideEffect, ToolContext, ToolError, ToolOutput,
};

/// §6.2.1: a call returns at most this many lines…
const MAX_LINES: usize = 2000;
/// …or this many bytes of content.
const MAX_BYTES: usize = 200 * 1024;
/// §5.1: above this a file needs an explicit `offset`/`limit`…
const READ_WHOLE_LIMIT: u64 = 1024 * 1024;
/// …and above this it cannot be read at all.
const MAX_FILE: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    #[allow(dead_code, reason = "validated by the schema; only utf-8 exists")]
    encoding: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file from the workspace, or a range of its lines. Output is \
         line-numbered (`LINE<TAB>text`, 1-based) so you can cite lines. Use `offset` and \
         `limit` for large files (at most 2000 lines or 200 KiB per call)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["path"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "Absolute or workspace-relative path"},
                "offset": {"type": "integer", "minimum": 1, "default": 1, "description": "1-based start line"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 2000},
                "encoding": {"enum": ["utf-8"], "default": "utf-8"}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "path": {"type": "string"}, "absolute_path": {"type": "string"},
            "content": {"type": "string"}, "start_line": {"type": "integer"},
            "end_line": {"type": "integer"}, "total_lines": {"type": "integer"},
            "truncated": {"type": "boolean"}, "encoding": {"type": "string"},
            "binary": {"type": "boolean"}, "sha256": {"type": "string"},
            "crlf": {"type": "boolean"}, "language": {"type": ["string", "null"]}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Read
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Safe
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn max_output_bytes(&self) -> u32 {
        200 * 1024
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| PathArg {
                field: "path",
                value: path.to_string(),
                access: Access::Read,
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
            let input: Input = parse("read_file", input)?;
            tokio::task::spawn_blocking(move || read(&input, &ctx))
                .await
                .map_err(|e| ToolError::new(codes::FS_PERM, format!("read task failed: {e}")))?
        }
        .boxed()
    }
}

fn read(input: &Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    // Re-validated here, not only in the pipeline: the file may have been
    // swapped for a symlink in between (REQ-SAFE-007).
    let resolved = ctx.boundary.resolve(&input.path, &ctx.cwd, Access::Read)?;
    let shown = resolved.display();
    if resolved.protected {
        return Err(ToolError::new(
            codes::FS_PROTECTED,
            format!("`{shown}` is a protected path."),
        ));
    }
    let path = &resolved.abs;
    let meta = std::fs::metadata(path).map_err(|e| io_error(&shown, &e))?;
    if meta.is_dir() {
        return Err(
            ToolError::new(codes::FS_DIR, format!("`{shown}` is a directory."))
                .recovery("Path is a directory; use list_dir."),
        );
    }
    if ctx.ignore.is_ignored(path, false) {
        return Err(ToolError::new(
            codes::FS_IGNORED,
            format!("`{shown}` is excluded by the ignore rules."),
        )
        .recovery("Ignored files are not readable; ask the user to include it."));
    }
    let size = meta.len();
    if size > MAX_FILE {
        return Err(ToolError::new(
            codes::FS_TOOBIG,
            format!("`{shown}` is {size} bytes; the limit is {MAX_FILE}."),
        )
        .recovery("Use grep to find the part you need, or read it through bash."));
    }
    if size > READ_WHOLE_LIMIT && input.offset.is_none() && input.limit.is_none() {
        return Err(ToolError::new(
            codes::FS_TOOBIG,
            format!("`{shown}` is {size} bytes; files over 1 MiB are read in ranges."),
        )
        .recovery("Pass offset and limit to read part of it."));
    }

    let bytes = std::fs::read(path).map_err(|e| io_error(&shown, &e))?;
    match classify(&bytes[..bytes.len().min(SNIFF_BYTES)], size) {
        Content::Binary => {
            return Err(
                ToolError::new(codes::FS_BINARY, format!("`{shown}` is a binary file.")).recovery(
                    "Binary files cannot be read; use bash (`file`, `xxd`) to inspect it.",
                ),
            )
        }
        Content::Utf16 { .. } | Content::NotUtf8 => {
            return Err(
                ToolError::new(codes::FS_ENCODING, format!("`{shown}` is not UTF-8 text."))
                    .recovery("Only UTF-8 files can be read; convert it first."),
            )
        }
        Content::Text { .. } => {}
    }
    let (body, had_bom) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (rest, true),
        None => (&bytes[..], false),
    };
    let text = std::str::from_utf8(body).map_err(|e| {
        ToolError::new(
            codes::FS_ENCODING,
            format!("`{shown}` is not valid UTF-8 (byte {}).", e.valid_up_to()),
        )
        .recovery("Only UTF-8 files can be read; convert it first.")
    })?;

    let sha256 = hex::encode(Sha256::digest(&bytes));
    let normalised = text.replace("\r\n", "\n");
    ctx.file_state
        .record(path.clone(), sha256.clone(), Some(&normalised));

    let crlf_count = text.matches("\r\n").count();
    let lf_count = text.matches('\n').count() - crlf_count;
    let crlf = crlf_count > lf_count;
    let mut lines: Vec<&str> = normalised.split('\n').collect();
    // A trailing newline ends the last line; it does not start another.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let total = lines.len();

    let offset = input.offset.unwrap_or(1).max(1);
    let limit = input.limit.unwrap_or(MAX_LINES).min(MAX_LINES);
    let start = offset - 1;
    let mut content = String::new();
    let mut end_line = start; // exclusive, 0-based == last 1-based line
    let mut byte_capped = false;
    for (i, line) in lines.iter().enumerate().skip(start).take(limit) {
        let numbered = format!("{}\t{}\n", i + 1, line);
        if content.len() + numbered.len() > MAX_BYTES {
            byte_capped = true;
            break;
        }
        content.push_str(&numbered);
        end_line = i + 1;
    }
    // `truncated` means the *cap* cut the answer, not that the caller asked
    // for a short range (T-TOOL-003: `limit: 10` is not truncation).
    let line_capped = limit == MAX_LINES && end_line < total;
    let truncated = byte_capped || line_capped;

    let first_line = lines.first().copied();
    Ok(ToolOutput::new(json!({
        "path": shown,
        "absolute_path": path.to_string_lossy().replace('\\', "/"),
        "content": content,
        "start_line": offset,
        "end_line": end_line.max(offset - 1),
        "total_lines": total,
        "truncated": truncated,
        "encoding": "utf-8",
        "bom": had_bom,
        "binary": false,
        "sha256": sha256,
        "crlf": crlf,
        "language": language_for(&shown, first_line),
    }))
    .truncated(truncated))
}
