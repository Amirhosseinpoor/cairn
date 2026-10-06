//! `grep` (SPEC §6.2.7).

use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_search::{GrepError, GrepOptions};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{io_error, object_schema, parse};
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, SideEffect, ToolContext, ToolError, ToolOutput,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    case_insensitive: Option<bool>,
    multiline: Option<bool>,
    context_lines: Option<usize>,
    max_results: Option<usize>,
    max_file_size_kb: Option<u64>,
    respect_ignore: Option<bool>,
    include_binary: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Grep;

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a Rust regular expression. Returns path, line, column and \
         the matching line, with optional context. Binary and ignored files are skipped \
         (binary files are counted). Narrow with `path` and `glob` for speed."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["pattern"],
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "maxLength": 500, "description": "Rust regex"},
                "path": {"type": "string", "default": "."},
                "glob": {"type": ["string", "null"], "default": null, "description": "e.g. *.rs"},
                "case_insensitive": {"type": "boolean", "default": false},
                "multiline": {"type": "boolean", "default": false},
                "context_lines": {"type": ["integer", "null"], "minimum": 0, "maximum": 5, "default": 0},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 200},
                "max_file_size_kb": {"type": "integer", "default": 1024},
                "respect_ignore": {"type": "boolean", "default": true},
                "include_binary": {"type": "boolean", "default": false}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "matches": {"type": "array"},
            "match_count": {"type": "integer"},
            "files_searched": {"type": "integer"},
            "binary_skipped": {"type": "integer"},
            "truncated": {"type": "boolean"},
            "duration_ms": {"type": "integer"}
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
        Duration::from_secs(30)
    }

    fn max_output_bytes(&self) -> u32 {
        128 * 1024
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        vec![PathArg {
            field: "path",
            value: input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string(),
            access: Access::Read,
        }]
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: Input = parse("grep", input)?;
            tokio::task::spawn_blocking(move || run(&input, &ctx))
                .await
                .map_err(|e| ToolError::new(codes::GREP_WALK, format!("grep task failed: {e}")))?
        }
        .boxed()
    }
}

fn run(input: &Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let resolved =
        ctx.boundary
            .resolve(input.path.as_deref().unwrap_or("."), &ctx.cwd, Access::Read)?;
    let shown = resolved.display();
    let respect = input.respect_ignore.unwrap_or(true);
    // T-PERM-020: naming an excluded file or directory is `E-FS-IGNORED`.
    if respect && ctx.ignore.is_ignored(&resolved.abs, resolved.abs.is_dir()) {
        return Err(ToolError::new(
            codes::FS_IGNORED,
            format!("`{shown}` is excluded by the ignore rules."),
        )
        .recovery("Ignored paths are not searchable; ask the user to include it."));
    }
    let found = cairn_search::grep(
        &ctx.ignore,
        &resolved.abs,
        &GrepOptions {
            pattern: input.pattern.clone(),
            glob: input.glob.clone(),
            case_insensitive: input.case_insensitive.unwrap_or(false),
            multiline: input.multiline.unwrap_or(false),
            context_lines: input.context_lines.unwrap_or(0).min(5),
            max_results: input.max_results.unwrap_or(200),
            max_file_size_kb: input.max_file_size_kb.unwrap_or(1024),
            respect_ignore: respect,
            include_binary: input.include_binary.unwrap_or(false),
        },
    )
    .map_err(|e| match e {
        GrepError::Syntax(why) => ToolError::new(
            codes::REGEX_SYNTAX,
            format!(
                "`{}` is not a valid regular expression: {why}",
                input.pattern
            ),
        )
        .recovery(
            "Fix the pattern (Rust regex syntax); escape literal punctuation with a backslash.",
        ),
        GrepError::TooBig(why) => ToolError::new(
            codes::REGEX_TOOBIG,
            format!("The pattern compiles to something too large: {why}"),
        )
        .recovery("Use a simpler pattern, or search for a literal."),
        GrepError::GlobSyntax(why) => ToolError::new(
            codes::GLOB_SYNTAX,
            format!("The `glob` filter is not valid: {why}"),
        )
        .recovery("Use globset syntax, e.g. *.rs"),
        GrepError::NotFound => {
            io_error(&shown, &std::io::Error::from(std::io::ErrorKind::NotFound))
        }
        GrepError::Walk(why) => {
            ToolError::new(codes::GREP_WALK, format!("cannot search `{shown}`: {why}"))
        }
        GrepError::Cap => ToolError::new(
            codes::GREP_CAP,
            "The scan reached its entry limit before finding any match.",
        )
        .recovery("Narrow `path` or `glob` and try again."),
    })?;
    let matches: Vec<Value> = found
        .matches
        .iter()
        .map(|m| {
            json!({
                "path": m.path, "line": m.line, "column": m.column, "text": m.text,
                "before": m.before, "after": m.after,
            })
        })
        .collect();
    Ok(ToolOutput::new(json!({
        "matches": matches,
        "match_count": found.match_count,
        "files_searched": found.files_searched,
        "binary_skipped": found.binary_skipped,
        "truncated": found.truncated,
        "duration_ms": found.duration_ms,
    }))
    .truncated(found.truncated))
}
