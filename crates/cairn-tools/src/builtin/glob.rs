//! `glob` (SPEC §6.2.6).

use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_search::{GlobError, GlobOptions};
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
    max_results: Option<usize>,
    respect_ignore: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Glob;

impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern (`**`, `*`, `?`, `{a,b}`, `[abc]`), relative to `path`. \
         Results are workspace-relative, oldest first. Ignored files are excluded unless \
         `respect_ignore` is false."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["pattern"],
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "maxLength": 500},
                "path": {"type": "string", "default": "."},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 500},
                "respect_ignore": {"type": "boolean", "default": true}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "matches": {"type": "array", "items": {"type": "string"}},
            "count": {"type": "integer"},
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
        Duration::from_secs(15)
    }

    fn max_output_bytes(&self) -> u32 {
        64 * 1024
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
            let input: Input = parse("glob", input)?;
            tokio::task::spawn_blocking(move || run(&input, &ctx))
                .await
                .map_err(|e| ToolError::new(codes::FS_PERM, format!("glob task failed: {e}")))?
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
    // REQ-CTX-002: an excluded directory is unreadable, not merely unlisted.
    if respect && ctx.ignore.is_ignored(&resolved.abs, resolved.abs.is_dir()) {
        return Err(ToolError::new(
            codes::FS_IGNORED,
            format!("`{shown}` is excluded by the ignore rules."),
        )
        .recovery("Search a path that is not ignored, or pass respect_ignore:false."));
    }
    let found = cairn_search::glob(
        &ctx.ignore,
        &resolved.abs,
        &input.pattern,
        GlobOptions {
            max_results: input.max_results.unwrap_or(500),
            respect_ignore: respect,
        },
    )
    .map_err(|e| match e {
        GlobError::Syntax(why) => ToolError::new(
            codes::GLOB_SYNTAX,
            format!("`{}` is not a valid glob: {why}", input.pattern),
        )
        .recovery("Use globset syntax: **, *, ?, {a,b}, [abc]."),
        GlobError::NotFound => {
            io_error(&shown, &std::io::Error::from(std::io::ErrorKind::NotFound))
        }
        GlobError::Cap => ToolError::new(
            codes::GLOB_CAP,
            "The scan reached its entry limit before finding any match.",
        )
        .recovery("Narrow `path` to a subdirectory and try again."),
        GlobError::Unreadable(why) => {
            ToolError::new(codes::FS_PERM, format!("cannot search `{shown}`: {why}"))
        }
    })?;
    Ok(ToolOutput::new(json!({
        "matches": found.matches,
        "count": found.count,
        "truncated": found.truncated,
        "duration_ms": found.duration_ms,
    }))
    .truncated(found.truncated))
}
