//! `todo_write` and `ask_user` (SPEC §6.2.16, §6.2.17): the two tools that
//! talk about the work rather than do it.

use std::collections::BTreeSet;
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::event::EventData;
use futures::future::BoxFuture;
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{object_schema, parse};
use super::fsio::write_atomic;
use crate::tool::Tool;
use crate::types::{
    Access, Idempotency, PathArg, PermissionClass, Question, SideEffect, ToolContext, ToolError,
    ToolOutput,
};

/// Where the todo list lives, relative to the workspace.
pub const TODOS_PATH: &str = ".cairn/todos.json";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Todo {
    id: String,
    content: String,
    status: String,
    #[serde(rename = "activeForm")]
    active_form: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoInput {
    todos: Vec<Todo>,
    plan_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TodoWrite;

impl Tool for TodoWrite {
    fn name(&self) -> &'static str {
        "todo_write"
    }

    fn description(&self) -> &'static str {
        "Record the task list for this work. Each call REPLACES the whole list, so always send \
         every item. Statuses: pending, in_progress, completed, failed, cancelled. Keep at most \
         one item in_progress. Use it for work with three or more steps, and update it as you \
         go."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["todos"],
            "additionalProperties": false,
            "properties": {
                "todos": {
                    "type": "array", "minItems": 1, "maxItems": 100,
                    "items": {
                        "type": "object",
                        "required": ["id", "content", "status"],
                        "additionalProperties": false,
                        "properties": {
                            "id": {"type": "string", "pattern": "^[a-z0-9_.-]{1,64}$"},
                            "content": {"type": "string", "minLength": 1, "maxLength": 500},
                            "status": {"enum": ["pending", "in_progress", "completed", "failed", "cancelled"]},
                            "activeForm": {"type": "string", "maxLength": 120}
                        }
                    }
                },
                "plan_id": {"type": ["string", "null"], "default": null}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "todos_saved": {"type": "integer"},
            "completed": {"type": "integer"},
            "pending": {"type": "integer"},
            "plan_id": {"type": ["string", "null"]}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::WriteState
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Retryable
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn max_output_bytes(&self) -> u32 {
        32 * 1024
    }

    fn path_args(&self, _input: &Value) -> Vec<PathArg> {
        vec![PathArg {
            field: "todos",
            value: TODOS_PATH.to_string(),
            access: Access::Write,
        }]
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: TodoInput = parse("todo_write", input)?;
            tokio::task::spawn_blocking(move || save(&input, &ctx))
                .await
                .map_err(|e| {
                    ToolError::new(codes::STATE_PERM, format!("the save task failed: {e}"))
                })?
        }
        .boxed()
    }
}

fn save(input: &TodoInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let mut seen = BTreeSet::new();
    for todo in &input.todos {
        if !seen.insert(todo.id.as_str()) {
            return Err(ToolError::new(
                codes::TODO_DUPLICATE,
                format!("the id `{}` appears more than once.", todo.id),
            )
            .recovery("Give every todo its own id and send the whole list again."));
        }
    }
    let in_progress = input
        .todos
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();
    if in_progress > 1 {
        return Err(ToolError::new(
            codes::TODO_STATUS,
            format!("{in_progress} todos are in_progress; only one may be."),
        )
        .recovery("Mark the others pending or completed, then send the whole list again."));
    }
    let count = |status: &str| input.todos.iter().filter(|t| t.status == status).count();
    let completed = count("completed");
    let pending = count("pending") + in_progress;

    let path = ctx.workspace_root.join(TODOS_PATH);
    let doc = json!({
        "session_id": ctx.session_id.to_string(),
        "plan_id": input.plan_id,
        "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "todos": input.todos.iter().map(|t| json!({
            "id": t.id, "content": t.content, "status": t.status, "activeForm": t.active_form,
        })).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| {
        ToolError::new(codes::STATE_PERM, format!("could not encode the list: {e}"))
    })?;
    write_atomic(&path, &bytes, None, true).map_err(|e| {
        ToolError::new(codes::STATE_PERM, e.message)
            .recovery("The todo list could not be saved; carry on without it.")
    })?;
    if let Some(plan_id) = &input.plan_id {
        for (i, todo) in input.todos.iter().enumerate() {
            ctx.events.emit(EventData::PlanStep {
                plan_id: plan_id.clone(),
                step: u32::try_from(i + 1).unwrap_or(u32::MAX),
                status: todo.status.clone(),
            });
        }
    }
    Ok(ToolOutput {
        data: json!({
            "todos_saved": input.todos.len(),
            "completed": completed,
            "pending": pending,
            "in_progress": in_progress,
            "failed": count("failed"),
            "cancelled": count("cancelled"),
            "plan_id": input.plan_id,
        }),
        truncated: false,
    })
}

// -------------------------------------------------------------- ask_user

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskInput {
    question: String,
    options: Option<Vec<String>>,
    allow_free_text: Option<bool>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AskUser;

impl Tool for AskUser {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ask the person a question and wait for the answer. Use it when a decision is genuinely \
         theirs (a choice between approaches, a missing requirement), not for things you can \
         find out yourself. Offer `options` when the answers are a short list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["question"],
            "additionalProperties": false,
            "properties": {
                "question": {"type": "string", "minLength": 1, "maxLength": 4000},
                "options": {"type": "array", "maxItems": 8, "items": {"type": "string", "maxLength": 200}, "default": []},
                "allow_free_text": {"type": "boolean", "default": true},
                "timeout_ms": {"type": "integer", "minimum": 1000, "maximum": 3_600_000, "default": 3_600_000}
            }
        })
    }

    fn output_schema(&self) -> Value {
        object_schema(&json!({
            "answer": {"type": "string"},
            "selected_option": {"type": ["integer", "null"]},
            "source": {"type": "string"}
        }))
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Ask
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Safe
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(3600)
    }

    fn max_timeout(&self) -> Duration {
        Duration::from_secs(3600 + 5)
    }

    fn max_output_bytes(&self) -> u32 {
        8 * 1024
    }

    fn requires_serial(&self) -> bool {
        true
    }

    fn execute(
        &self,
        input: Value,
        ctx: ToolContext,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async move {
            let input: AskInput = parse("ask_user", input)?;
            let Some(questioner) = ctx.questioner.clone() else {
                return Err(ToolError::new(
                    codes::ASK_NOINPUT,
                    "there is nobody to ask: this run has no terminal and no input channel.",
                )
                .recovery("Make the best decision you can and say what you assumed."));
            };
            let wait = Duration::from_millis(input.timeout_ms.unwrap_or(3_600_000));
            let question = Question {
                text: input.question,
                options: input.options.unwrap_or_default(),
                allow_free_text: input.allow_free_text.unwrap_or(true),
            };
            let free = question.allow_free_text;
            let options = question.options.clone();
            let asked = tokio::time::timeout(wait, questioner.ask(question));
            let cancelled = async {
                loop {
                    if cancel.is_cancelled() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            };
            let reply = tokio::select! {
                r = asked => r,
                () = cancelled => {
                    return Err(ToolError::new(codes::TOOL_CANCELLED, "the question was cancelled"));
                }
            };
            let reply = match reply {
                Err(_) => {
                    return Err(ToolError::new(
                        codes::ASK_TIMEOUT,
                        format!(
                            "No answer after {} minutes; returning 'cancelled'.",
                            wait.as_secs() / 60
                        ),
                    )
                    .recovery("Proceed on your best judgement and say what you assumed."));
                }
                Ok(None) => {
                    return Err(ToolError::new(
                        codes::ASK_NOINPUT,
                        "the question could not be put to anyone.",
                    )
                    .recovery("Make the best decision you can and say what you assumed."));
                }
                Ok(Some(reply)) => reply,
            };
            // An option number stands for the option's text; free text is
            // only accepted when the question allowed it.
            let answer = match (reply.selected_option, reply.answer.is_empty()) {
                (Some(n), true) => options.get(n - 1).cloned().unwrap_or_default(),
                (Some(_) | None, false) if !free && reply.selected_option.is_none() => {
                    return Err(ToolError::new(
                        codes::ASK_NOINPUT,
                        "a free-text answer was given to a question that only accepts the options.",
                    )
                    .recovery("Ask again, or pick one of the options yourself."));
                }
                _ => reply.answer,
            };
            Ok(ToolOutput {
                data: json!({
                    "answer": answer,
                    "selected_option": reply.selected_option,
                    "source": "user",
                }),
                truncated: false,
            })
        }
        .boxed()
    }
}
