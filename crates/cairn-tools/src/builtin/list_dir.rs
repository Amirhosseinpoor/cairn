//! `list_dir` (SPEC §6.2.5).

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_search::{classify, language_for, walk, Content, Kind, WalkOptions, SNIFF_BYTES};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{io_error, object_schema, parse};
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, SideEffect, ToolContext, ToolError, ToolOutput,
};

/// §6.2.5: entries listed per directory.
const PER_LEVEL: usize = 2000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    path: Option<String>,
    depth: Option<usize>,
    include_hidden: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ListDir;

impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> &'static str {
        "List the entries of a directory (default: the workspace root), optionally several \
         levels deep. Each entry says whether it is a file, directory or symlink, its size, \
         whether it is binary or excluded by ignore rules, and its language."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "default": "."},
                "depth": {"type": "integer", "minimum": 1, "maximum": 4, "default": 1},
                "include_hidden": {"type": "boolean", "default": false}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "path": {"type": "string"},
            "entries": {"type": "array"},
            "truncated": {"type": "boolean"},
            "entry_count": {"type": "integer"}
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
            let input: Input = parse("list_dir", input)?;
            tokio::task::spawn_blocking(move || list(&input, &ctx))
                .await
                .map_err(|e| ToolError::new(codes::FS_PERM, format!("list task failed: {e}")))?
        }
        .boxed()
    }
}

fn is_binary(path: &std::path::Path, size: u64) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buffer = vec![0u8; SNIFF_BYTES];
    let Ok(read) = file.read(&mut buffer) else {
        return false;
    };
    buffer.truncate(read);
    !matches!(classify(&buffer, size), Content::Text { .. })
}

fn list(input: &Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let requested = input.path.as_deref().unwrap_or(".");
    let resolved = ctx.boundary.resolve(requested, &ctx.cwd, Access::Read)?;
    let shown = resolved.display();
    let meta = std::fs::metadata(&resolved.abs).map_err(|e| io_error(&shown, &e))?;
    if !meta.is_dir() {
        return Err(ToolError::new(
            codes::FS_DIR,
            format!("`{shown}` is a file, not a directory."),
        )
        .recovery("Use read_file for files."));
    }
    let walked = walk(
        &ctx.ignore,
        &resolved.abs,
        &WalkOptions {
            max_depth: input.depth.unwrap_or(1).clamp(1, 4),
            include_hidden: input.include_hidden.unwrap_or(false),
            respect_ignore: false,
            mark_ignored: true,
            ..WalkOptions::default()
        },
    )
    .map_err(|e| ToolError::new(codes::FS_PERM, format!("cannot list `{shown}`: {e}")))?;

    // Group by parent so the per-level cap applies to each directory.
    let mut per_dir: BTreeMap<String, usize> = BTreeMap::new();
    let mut truncated = false;
    let mut entries = Vec::new();
    for entry in &walked.entries {
        let parent = entry
            .rel
            .rsplit_once('/')
            .map_or(String::new(), |(p, _)| p.to_string());
        let n = per_dir.entry(parent).or_insert(0);
        *n += 1;
        if *n > PER_LEVEL {
            truncated = true;
            continue;
        }
        let name = entry.rel.rsplit('/').next().unwrap_or(&entry.rel);
        let binary = entry.kind == Kind::File && is_binary(&entry.path, entry.size);
        entries.push(json!({
            "name": name,
            "path": entry.rel,
            "type": entry.kind.as_str(),
            "size": entry.size,
            "binary": binary,
            "ignored": entry.ignored,
            "language": (entry.kind == Kind::File)
                .then(|| language_for(&entry.rel, None))
                .flatten(),
        }));
    }
    truncated |= walked.capped;
    Ok(ToolOutput::new(json!({
        "path": shown,
        "entry_count": entries.len(),
        "entries": entries,
        "truncated": truncated,
    }))
    .truncated(truncated))
}
