//! `edit_file` and `multi_edit` (SPEC §6.2.3, §6.2.4).

use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{object_schema, parse};
use super::fsio::{
    check_syntax, load, resolve_for_write, sha256_hex, stale_edit_error, staleness, syntax_error,
    syntax_ok, write_observed, Staleness,
};
use crate::edit::{apply, apply_multi, Applied, EditError, EditSpec, Fuzzy, MultiError};
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, SideEffect, SyntaxVerdict, ToolContext,
    ToolError, ToolOutput,
};

fn fuzzy_of(text: Option<&str>) -> Fuzzy {
    text.and_then(Fuzzy::parse).unwrap_or(Fuzzy::Normal)
}

fn describe_no_match(closest: Option<&crate::edit::Closest>) -> String {
    match closest {
        Some(c) => format!(
            "`old_string` was not found. The nearest text is at line {} (similarity {:.0}%).",
            c.line,
            c.score * 100.0
        ),
        None => "`old_string` was not found in the file.".to_string(),
    }
}

/// Map an engine error to the §6.3.1 messages and recoveries.
fn edit_error(shown: &str, error: &EditError) -> ToolError {
    match error {
        EditError::NoMatch { closest } => ToolError::new(
            codes::EDIT_NOMATCH,
            format!("{} (in `{shown}`)", describe_no_match(closest.as_ref())),
        )
        .recovery(
            "Re-read the file with read_file and copy old_string exactly, including whitespace; \
             include a little surrounding context.",
        ),
        EditError::EmptyPattern => ToolError::new(
            codes::EDIT_NOMATCH,
            "`old_string` is empty, so it matches nowhere in particular.",
        )
        .recovery("Pass the exact text to replace; use write_file to create a file."),
        EditError::NoChange => ToolError::new(
            codes::EDIT_NOCHANGE,
            "`old_string` and `new_string` are identical; nothing would change.",
        )
        .recovery("Make new_string different from old_string, or skip this edit."),
        EditError::Ambiguous {
            lines,
            scores,
            expected,
        } => {
            let at = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let message = if scores.is_empty() {
                format!(
                    "Pattern matched {} times (expected {expected}) at lines {at}. Add surrounding \
                     context or set replace_all.",
                    lines.len()
                )
            } else {
                let both = lines
                    .iter()
                    .zip(scores)
                    .map(|(l, s)| format!("line {l} ({:.0}%)", s * 100.0))
                    .collect::<Vec<_>>()
                    .join(" and ");
                format!("Two places match about equally well: {both}. Add surrounding context.")
            };
            ToolError::new(codes::EDIT_AMBIGUOUS, message).recovery(
                "Include 1-3 lines of unique surrounding context, or pass expect_occurrences=N, \
                 or replace_all:true.",
            )
        }
    }
}

fn fuzzy_warning(done: &Applied) -> Option<Value> {
    done.fuzzy_note.as_ref().map(|note| {
        json!({
            "code": codes::EDIT_FUZZY,
            "message": format!("fuzzy match ({:.0}%)", note.score * 100.0),
            "matched_text": note.matched_text,
        })
    })
}

// ------------------------------------------------------------------ edit_file

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
    expect_occurrences: Option<usize>,
    expected_sha256: Option<String>,
    fuzzy: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EditFile;

fn path_arg(input: &Value) -> Vec<PathArg> {
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

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replace text in an existing file. `old_string` must match exactly once (include \
         surrounding lines to make it unique) unless you pass replace_all or \
         expect_occurrences. Whitespace differences are forgiven (fuzzy matching) and reported. \
         The result is syntax-checked and rolled back if it breaks the file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "minLength": 1, "maxLength": 20_000},
                "new_string": {"type": "string", "maxLength": 20_000},
                "replace_all": {"type": "boolean", "default": false},
                "expect_occurrences": {"type": "integer", "minimum": 1, "default": 1},
                "expected_sha256": {"type": ["string", "null"], "default": null},
                "fuzzy": {"enum": ["off", "normal", "relaxed"], "default": "normal"}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "path": {"type": "string"}, "occurrences": {"type": "integer"},
            "replaced": {"type": "integer"}, "start_line": {"type": "integer"},
            "end_line": {"type": "integer"}, "sha256_before": {"type": "string"},
            "sha256_after": {"type": "string"}, "syntax_ok": {"type": ["boolean", "null"]},
            "fuzzy_used": {"type": "boolean"}, "fuzzy_score": {"type": ["number", "null"]}
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
        Duration::from_secs(20)
    }

    fn max_output_bytes(&self) -> u32 {
        8 * 1024
    }

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        path_arg(input)
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: EditInput = parse("edit_file", input)?;
            edit_one(&input, &ctx).await
        }
        .boxed()
    }
}

async fn edit_one(input: &EditInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let resolved = resolve_for_write(ctx, &input.path)?;
    let shown = resolved.display();
    let Some(file) = load(&resolved.abs, &shown)? else {
        return Err(super::common::io_error(
            &shown,
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .recovery("The file does not exist; use write_file to create it."));
    };
    let spec = EditSpec {
        old: input.old_string.clone(),
        new: input.new_string.clone(),
        replace_all: input.replace_all.unwrap_or(false),
        expect_occurrences: input.expect_occurrences,
        fuzzy: fuzzy_of(input.fuzzy.as_deref()),
    };
    let stale = staleness(
        ctx,
        &resolved.abs,
        &file.sha256,
        input.expected_sha256.as_deref(),
    );
    let (new_text, done) = match apply(&file.doc.text, &spec) {
        Ok(ok) => ok,
        Err(error) => {
            // Stage 2 (§6.3.3): the model's copy is old *and* its text is not
            // there any more — the file moved under it.
            if let (EditError::NoMatch { .. }, Staleness::Changed { seen }) = (&error, &stale) {
                return Err(stale_edit_error(
                    ctx,
                    &resolved.abs,
                    seen,
                    &file.doc.text,
                    &file.sha256,
                ));
            }
            return Err(edit_error(&shown, &error));
        }
    };
    let verdict = check_syntax(ctx, &shown, &file.doc.text, &new_text).await;
    if let SyntaxVerdict::Invalid(problem) = &verdict {
        return Err(syntax_error(&shown, problem));
    }
    let bytes = file.doc.encode(&new_text);
    write_observed(ctx, &resolved.abs, &bytes, file.mode, false)?;
    let sha_after = sha256_hex(&bytes);
    ctx.file_state
        .record(resolved.abs.clone(), sha_after.clone(), Some(&new_text));

    let mut data = json!({
        "path": shown,
        "occurrences": done.occurrences,
        "replaced": done.replaced,
        "start_line": done.start_line,
        "end_line": done.end_line,
        "sha256_before": file.sha256,
        "sha256_after": sha_after,
        "syntax_ok": syntax_ok(&verdict),
        "fuzzy_used": done.fuzzy_used,
        "fuzzy_score": done.fuzzy_score,
    });
    if matches!(stale, Staleness::Changed { .. }) {
        data["stale_but_safe"] = json!(true);
    }
    let mut warnings: Vec<Value> = fuzzy_warning(&done).into_iter().collect();
    if matches!(verdict, SyntaxVerdict::TimedOut) {
        warnings.push(json!({
            "code": codes::EDIT_TIMEOUT,
            "message": "syntax validation timed out; the edit was written unchecked",
        }));
    }
    if !warnings.is_empty() {
        data["warnings"] = Value::Array(warnings);
    }
    Ok(ToolOutput::new(data))
}

// ----------------------------------------------------------------- multi_edit

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubEdit {
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
    fuzzy: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiInput {
    path: String,
    edits: Vec<SubEdit>,
    expected_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MultiEdit;

impl Tool for MultiEdit {
    fn name(&self) -> &'static str {
        "multi_edit"
    }

    fn description(&self) -> &'static str {
        "Apply several edits to one file, all or nothing: they run in order against an in-memory \
         copy, and the file is written only if every edit applies and the result still parses. \
         Edits that target the same text conflict."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["path", "edits"],
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string"},
                "edits": {"type": "array", "minItems": 1, "maxItems": 50, "items": {
                    "type": "object",
                    "required": ["old_string", "new_string"],
                    "additionalProperties": false,
                    "properties": {
                        "old_string": {"type": "string", "minLength": 1},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean", "default": false},
                        "fuzzy": {"enum": ["off", "normal", "relaxed"], "default": "normal"}
                    }
                }},
                "expected_sha256": {"type": ["string", "null"], "default": null}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "path": {"type": "string"}, "applied": {"type": "integer"},
            "failed_index": {"type": ["integer", "null"]},
            "sha256_after": {"type": "string"}, "syntax_ok": {"type": ["boolean", "null"]}
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

    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        path_arg(input)
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: MultiInput = parse("multi_edit", input)?;
            edit_many(&input, &ctx).await
        }
        .boxed()
    }
}

async fn edit_many(input: &MultiInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let resolved = resolve_for_write(ctx, &input.path)?;
    let shown = resolved.display();
    let Some(file) = load(&resolved.abs, &shown)? else {
        return Err(super::common::io_error(
            &shown,
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .recovery("The file does not exist; use write_file to create it."));
    };
    let specs: Vec<EditSpec> = input
        .edits
        .iter()
        .map(|e| EditSpec {
            old: e.old_string.clone(),
            new: e.new_string.clone(),
            replace_all: e.replace_all.unwrap_or(false),
            expect_occurrences: None,
            fuzzy: fuzzy_of(e.fuzzy.as_deref()),
        })
        .collect();
    let stale = staleness(
        ctx,
        &resolved.abs,
        &file.sha256,
        input.expected_sha256.as_deref(),
    );
    let (new_text, done) = match apply_multi(&file.doc.text, &specs) {
        Ok(ok) => ok,
        Err(MultiError::Conflict { a, b }) => {
            return Err(ToolError::new(
                codes::EDIT_CONFLICT,
                format!(
                    "Edits {a} and {b} target overlapping text in `{shown}`; nothing was written."
                ),
            )
            .recovery("Merge those edits into one, or make their old_string values disjoint.")
            .with_data(json!({ "conflicting": [a, b] })));
        }
        Err(MultiError::Partial { index, error }) => {
            if let (EditError::NoMatch { .. }, Staleness::Changed { seen }) = (&error, &stale) {
                return Err(stale_edit_error(
                    ctx,
                    &resolved.abs,
                    seen,
                    &file.doc.text,
                    &file.sha256,
                ));
            }
            let inner = edit_error(&shown, &error);
            return Err(ToolError::new(
                codes::EDIT_PARTIAL,
                format!(
                    "Edit {index} failed ({}: {}); nothing was written.",
                    inner.code, inner.message
                ),
            )
            .recovery(format!(
                "Fix edit {index} and resend the whole list — the file is unchanged. {}",
                inner.recovery.unwrap_or_default()
            ))
            .with_data(json!({ "failed_index": index, "first_failed_index": index })));
        }
    };
    let verdict = check_syntax(ctx, &shown, &file.doc.text, &new_text).await;
    if let SyntaxVerdict::Invalid(problem) = &verdict {
        return Err(syntax_error(&shown, problem));
    }
    let bytes = file.doc.encode(&new_text);
    write_observed(ctx, &resolved.abs, &bytes, file.mode, false)?;
    let sha_after = sha256_hex(&bytes);
    ctx.file_state
        .record(resolved.abs.clone(), sha_after.clone(), Some(&new_text));
    let mut data = json!({
        "path": shown,
        "applied": done.len(),
        "failed_index": Value::Null,
        "sha256_after": sha_after,
        "syntax_ok": syntax_ok(&verdict),
    });
    let mut warnings: Vec<Value> = done.iter().filter_map(fuzzy_warning).collect();
    if matches!(verdict, SyntaxVerdict::TimedOut) {
        warnings.push(json!({
            "code": codes::EDIT_TIMEOUT,
            "message": "syntax validation timed out; the edits were written unchecked",
        }));
    }
    if matches!(stale, Staleness::Changed { .. }) {
        data["stale_but_safe"] = json!(true);
    }
    if !warnings.is_empty() {
        data["warnings"] = Value::Array(warnings);
    }
    Ok(ToolOutput::new(data))
}
