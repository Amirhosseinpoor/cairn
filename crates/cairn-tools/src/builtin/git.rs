//! `git_status`, `git_diff` and `git_commit` (SPEC §6.2.12–§6.2.14).

use std::path::{Path, PathBuf};
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_git::ops::{self, CommitRequest, GitError, Scope};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{object_schema, parse};
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, SideEffect, ToolContext, ToolError, ToolOutput,
};

fn failure(e: GitError) -> ToolError {
    let mut error = ToolError::new(e.code, e.message);
    error.recovery = e.recovery;
    error
}

fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> BoxFuture<'static, Result<T, ToolError>> {
    async move {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|e| ToolError::new(codes::GIT_CMD, format!("the git task failed: {e}")))?
    }
    .boxed()
}

fn to_output<T: serde::Serialize>(value: &T) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput {
        data: serde_json::to_value(value).map_err(|e| {
            ToolError::new(codes::GIT_CMD, format!("could not encode the result: {e}"))
        })?,
        truncated: false,
    })
}

fn dir_path_arg(input: &Value) -> Vec<PathArg> {
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

// ------------------------------------------------------------- git_status

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathInput {
    path: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GitStatus;

impl Tool for GitStatus {
    fn name(&self) -> &'static str {
        "git_status"
    }

    fn description(&self) -> &'static str {
        "Show the state of the git working tree: current branch, upstream and how far ahead or \
         behind, staged, unstaged and untracked files, merge conflicts, and whether a rebase is \
         in progress."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {"path": {"type": "string", "default": "."}}
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "branch": {"type": ["string", "null"]},
            "upstream": {"type": ["string", "null"]},
            "ahead": {"type": "integer"},
            "behind": {"type": "integer"},
            "staged": {"type": "array"},
            "unstaged": {"type": "array"},
            "untracked": {"type": "array"},
            "conflicts": {"type": "array"},
            "clean": {"type": "boolean"},
            "detached": {"type": "boolean"},
            "rebase_in_progress": {"type": "boolean"}
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
        32 * 1024
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        dir_path_arg(input)
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: PathInput = parse("git_status", input)?;
            let dir = resolve(&ctx, input.path.as_deref())?;
            blocking(move || to_output(&ops::status(&dir).map_err(failure)?)).await
        }
        .boxed()
    }
}

fn resolve(ctx: &ToolContext, path: Option<&str>) -> Result<PathBuf, ToolError> {
    Ok(ctx
        .boundary
        .resolve(path.unwrap_or("."), &ctx.cwd, Access::Read)?
        .abs)
}

// -------------------------------------------------------------- git_diff

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffInput {
    path: Option<String>,
    scope: Option<String>,
    commit: Option<String>,
    unified_lines: Option<u32>,
    max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GitDiff;

impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn description(&self) -> &'static str {
        "Show a unified diff: `working` (unstaged), `staged`, `all` (everything since HEAD, the \
         default) or `commit` (what one commit changed; pass `commit`). Untracked files are \
         included in `working` and `all`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "default": "."},
                "scope": {"enum": ["working", "staged", "all", "commit"], "default": "all"},
                "commit": {"type": ["string", "null"], "default": null},
                "unified_lines": {"type": "integer", "minimum": 0, "maximum": 20, "default": 3},
                "max_bytes": {"type": "integer", "minimum": 1024, "maximum": 1_048_576, "default": 131_072}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "diff": {"type": "string"},
            "files_changed": {"type": "integer"},
            "insertions": {"type": "integer"},
            "deletions": {"type": "integer"},
            "truncated": {"type": "boolean"}
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
        Duration::from_secs(20)
    }

    fn max_output_bytes(&self) -> u32 {
        128 * 1024
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        dir_path_arg(input)
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: DiffInput = parse("git_diff", input)?;
            let dir = resolve(&ctx, input.path.as_deref())?;
            let scope = match input.scope.as_deref() {
                Some("working") => Scope::Working,
                Some("staged") => Scope::Staged,
                Some("commit") => Scope::Commit,
                _ => Scope::All,
            };
            blocking(move || {
                let report = ops::diff(
                    &dir,
                    scope,
                    input.commit.as_deref(),
                    input.unified_lines.unwrap_or(3),
                    input.max_bytes.unwrap_or(131_072),
                )
                .map_err(failure)?;
                Ok(ToolOutput {
                    truncated: report.truncated,
                    data: serde_json::to_value(&report).map_err(|e| {
                        ToolError::new(codes::GIT_CMD, format!("could not encode the diff: {e}"))
                    })?,
                })
            })
            .await
        }
        .boxed()
    }
}

// ------------------------------------------------------------ git_commit

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitInput {
    message: String,
    all: Option<bool>,
    paths: Option<Vec<String>>,
    amend: Option<bool>,
    allow_empty: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GitCommit;

impl Tool for GitCommit {
    fn name(&self) -> &'static str {
        "git_commit"
    }

    fn description(&self) -> &'static str {
        "Create a git commit from what is staged. `paths` stages those files first (including \
         deletions); `all: true` stages every modified tracked file. Files under `.cairn/` and \
         files matched by `.cairnignore` are never included unless named in `paths`. Git hooks \
         run; a failing hook is reported with its output. A checkpoint is taken first so the \
         work before the commit can still be undone."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["message"],
            "additionalProperties": false,
            "properties": {
                "message": {"type": "string", "minLength": 1, "maxLength": 8000},
                "all": {"type": "boolean", "default": false},
                "paths": {"type": "array", "items": {"type": "string"}, "maxItems": 500, "default": []},
                "amend": {"type": "boolean", "default": false},
                "allow_empty": {"type": "boolean", "default": false}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "sha": {"type": "string"},
            "short_sha": {"type": "string"},
            "message": {"type": "string"},
            "files": {"type": "integer"},
            "insertions": {"type": "integer"},
            "deletions": {"type": "integer"}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Write
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::NonIdempotent
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn max_output_bytes(&self) -> u32 {
        16 * 1024
    }

    fn requires_serial(&self) -> bool {
        true
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        input
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|p| PathArg {
                field: "paths",
                value: p.to_string(),
                access: Access::Read,
            })
            .collect()
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: CommitInput = parse("git_commit", input)?;
            blocking(move || commit(&input, &ctx)).await
        }
        .boxed()
    }
}

/// `abs` relative to the repository's working directory, `/`-separated.
fn repo_relative(root: &Path, abs: &Path) -> Option<String> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    abs.strip_prefix(&root)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

fn in_cairn_dir(rel: &str) -> bool {
    rel == ".cairn" || rel.starts_with(".cairn/")
}

fn commit(input: &CommitInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let repo = ops::open(&ctx.workspace_root).map_err(failure)?;
    let root = repo.workdir().map(Path::to_path_buf).ok_or_else(|| {
        ToolError::new(codes::GIT_NOREPO, "a bare repository has no working tree.")
    })?;
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
    drop(repo);

    // REQ-TOOL-007: what `.cairn/` and `.cairnignore` keep out of commits.
    let excluded = |rel: &str| {
        let abs = root_canon.join(rel);
        in_cairn_dir(rel) || ctx.ignore.cairnignored(&abs, false)
    };
    let mut explicit: Vec<String> = Vec::new();
    for requested in input.paths.as_deref().unwrap_or_default() {
        let resolved = ctx.boundary.resolve(requested, &ctx.cwd, Access::Read)?;
        let rel = repo_relative(&root, &resolved.abs).ok_or_else(|| {
            ToolError::new(
                codes::GIT_CMD,
                format!("`{requested}` is outside the repository."),
            )
        })?;
        // `.cairn/` is only committed when it is named outright.
        if ctx.ignore.cairnignored(&root_canon.join(&rel), false) {
            return Err(ToolError::new(
                codes::GIT_CMD,
                format!("`{requested}` is excluded by .cairnignore and cannot be committed."),
            )
            .recovery("Remove it from `paths`, or ask the user to change .cairnignore."));
        }
        explicit.push(rel);
    }
    let mut to_stage: Vec<String> = explicit.clone();
    if input.all == Some(true) {
        let changed = ops::tracked_changes(&root).map_err(failure)?;
        to_stage.extend(changed.into_iter().filter(|p| !excluded(p)));
    }
    to_stage.sort();
    to_stage.dedup();
    ops::stage(&root, &to_stage).map_err(failure)?;

    // What is now staged must not include anything kept out of commits.
    let staged = ops::staged_paths(&root).map_err(failure)?;
    let offending: Vec<&String> = staged
        .iter()
        .filter(|p| {
            let named = explicit
                .iter()
                .any(|e| e == *p || p.starts_with(&format!("{e}/")));
            let cairn = in_cairn_dir(p) && !named;
            let ignored = ctx.ignore.cairnignored(&root_canon.join(p.as_str()), false);
            cairn || ignored
        })
        .collect();
    if !offending.is_empty() {
        return Err(ToolError::new(
            codes::GIT_CMD,
            format!(
                "staged files that must not be committed: {}",
                offending
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .recovery(
            "Unstage them (`git restore --staged <file>`) or ask the user; then commit again.",
        ));
    }

    // §9.8: a checkpoint before each commit, so what came before it can be
    // undone (with `--hard`).
    let head = ops::head_short(&root).unwrap_or_default();
    ctx.observer.checkpoint(&format!("pre-commit:{head}"));

    let report = ops::commit(
        &root,
        &CommitRequest {
            message: input.message.clone(),
            all: input.all == Some(true),
            paths: explicit,
            amend: input.amend == Some(true),
            allow_empty: input.allow_empty == Some(true),
        },
    )
    .map_err(failure)?;
    to_output(&report)
}
