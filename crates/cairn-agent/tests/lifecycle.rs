//! The §8.2 loop against a scripted provider and the real read-only tools.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cairn_agent::lifecycle::{run_loop, Iteration, LoopConfig, LoopEnd, LoopHooks, LoopOutcome};
use cairn_core::cancel::CancellationToken;
use cairn_core::message::{Block, Message, Role, StopReason};
use cairn_core::redact::Redactor;
use cairn_core::{CairnError, Mode, SessionId};
use cairn_perm::{PolicyFiles, RulePolicy};
use cairn_provider::{
    Capabilities, ModelRequest, Provider, ProviderError, ProviderFault, ProviderHealth, ProviderId,
    RetryBudget, StreamEvent, TokenCount,
};
use cairn_sandbox::PathChecksOnly;
use cairn_search::{IgnoreEngine, IgnoreOptions};
use cairn_tools::{
    builtin, Boundary, CallEnv, DenyAll, Executor, ExecutorParts, NullSink, Registry,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use serde_json::{json, Value};

/// One scripted model call.
enum Step {
    Events(Vec<StreamEvent>),
    Fail(ProviderFault),
}

struct Scripted {
    id: ProviderId,
    script: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    last: Mutex<Option<ProviderError>>,
}

impl Scripted {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId::new("scripted"),
            script: Mutex::new(steps.into()),
            requests: Mutex::default(),
            last: Mutex::default(),
        })
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl Provider for Scripted {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::baseline()
    }
    fn stream(
        &self,
        req: ModelRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.requests.lock().expect("requests").push(req);
        let step = self.script.lock().expect("script").pop_front();
        async move {
            match step {
                Some(Step::Events(events)) => Ok(futures::stream::iter(events).boxed()),
                Some(Step::Fail(fault)) => Err(ProviderError::new(fault, "scripted failure")),
                None => panic!("the loop made more model calls than the script has"),
            }
        }
        .boxed()
    }
    fn count_tokens<'a>(
        &'a self,
        _: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
        async { Ok(TokenCount::estimate(0)) }.boxed()
    }
    fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
    fn take_last_error(&self) -> Option<ProviderError> {
        self.last.lock().expect("last").take()
    }
    fn record_last_error(&self, error: ProviderError) {
        *self.last.lock().expect("last") = Some(error);
    }
}

fn text(t: &str, stop: StopReason) -> Step {
    Step::Events(vec![
        StreamEvent::MessageStart {
            model: "m".into(),
            id: "1".into(),
        },
        StreamEvent::TextDelta { text: t.into() },
        StreamEvent::Usage {
            input: 10,
            output: 5,
            cache_read: 0,
            cache_write: 0,
        },
        StreamEvent::Finish { stop },
    ])
}

fn calls(specs: &[(&str, &str, &str)]) -> Step {
    let mut events = vec![StreamEvent::MessageStart {
        model: "m".into(),
        id: "1".into(),
    }];
    for (i, (id, name, args)) in specs.iter().enumerate() {
        let index = u32::try_from(i).expect("small");
        events.push(StreamEvent::ToolCallStart {
            index,
            id: (*id).into(),
            name: (*name).into(),
        });
        events.push(StreamEvent::ToolCallDelta {
            index,
            args_delta: (*args).into(),
        });
        events.push(StreamEvent::ToolCallEnd { index });
    }
    events.push(StreamEvent::Finish {
        stop: StopReason::ToolUse,
    });
    Step::Events(events)
}

#[derive(Default)]
struct Hooks {
    committed: Vec<Iteration>,
    streamed: usize,
    assistant_seen: usize,
    fail_on: Option<usize>,
}

impl LoopHooks for Hooks {
    fn on_stream(&mut self, _event: &StreamEvent) {
        self.streamed += 1;
    }
    fn on_assistant(&mut self, _message: &Message) {
        self.assistant_seen += 1;
    }
    fn commit(&mut self, iteration: &Iteration) -> Result<(), CairnError> {
        if self.fail_on == Some(self.committed.len()) {
            return Err(CairnError::new("E-SESS-FLUSH", "disk full"));
        }
        self.committed.push(iteration.clone());
        Ok(())
    }
}

struct World {
    _tmp: tempfile::TempDir,
    executor: Executor,
    env: CallEnv,
}

fn world(mode: Mode) -> World {
    let tmp = tempfile::tempdir().expect("tmp");
    let base = tmp.path().canonicalize().expect("canonical");
    let root = base.join("ws");
    std::fs::create_dir_all(root.join(".cairn")).expect("ws");
    std::fs::create_dir_all(base.join("home")).expect("home");
    std::fs::write(root.join("a.txt"), "alpha\nbeta\n").expect("file");
    std::fs::write(root.join("b.txt"), "gamma\n").expect("file");
    std::fs::write(root.join(".env"), "TOKEN=1\n").expect("env");
    let mut registry = Registry::new();
    builtin::register_all(&mut registry).expect("registers");
    let boundary = Arc::new(
        Boundary::new(&root, &[], false, Some(base.join("home")), &[], false).expect("boundary"),
    );
    let executor = Executor::new(ExecutorParts {
        registry: Arc::new(registry),
        policy: Arc::new(
            RulePolicy::new(mode, PolicyFiles::default(), None, false).expect("policy"),
        ),
        approver: Arc::new(DenyAll),
        sandbox: Arc::new(PathChecksOnly),
        events: Arc::new(NullSink),
        ignore: Arc::new(IgnoreEngine::new(
            boundary.root(),
            &IgnoreOptions::default(),
        )),
        boundary: Arc::clone(&boundary),
        redactor: Arc::new(Redactor::default()),
        syntax: None,
        line_endings: cairn_tools::LineEndings::Lf,
        observer: None,
        approval_timeout: std::time::Duration::from_secs(1),
    });
    let env = CallEnv {
        session_id: SessionId::new(),
        turn_id: 1,
        cwd: boundary.root().to_path_buf(),
        mode,
    };
    World {
        _tmp: tmp,
        executor,
        env,
    }
}

async fn run(
    world: &World,
    provider: &Arc<Scripted>,
    config: LoopConfig,
    hooks: &mut Hooks,
) -> LoopOutcome {
    run_with(world, provider, config, hooks, &CancellationToken::new())
        .await
        .expect("loop runs")
}

async fn run_with(
    world: &World,
    provider: &Arc<Scripted>,
    config: LoopConfig,
    hooks: &mut Hooks,
    cancel: &CancellationToken,
) -> Result<LoopOutcome, CairnError> {
    let request = ModelRequest::new(
        "m",
        vec![
            Message::new(Role::System, vec![Block::Text { text: "sys".into() }], 0),
            Message::user("go", 1),
        ],
        1000,
    );
    run_loop(
        Arc::clone(provider) as Arc<dyn Provider>,
        request,
        &world.executor,
        &world.env,
        cancel,
        &RetryBudget::new(),
        config,
        hooks,
    )
    .await
}

fn tool_result_text(iteration: &Iteration) -> Vec<String> {
    iteration
        .tool_message
        .as_ref()
        .expect("tool message")
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .map(|c| match c {
                        Block::Text { text } => text.clone(),
                        _ => String::new(),
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------ tests

/// §8.2 steps 4–7: the model reads a file, sees the result in its next
/// request, and answers.
#[tokio::test]
async fn a_tool_call_runs_and_its_result_reaches_the_next_model_call() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[("c1", "read_file", r#"{"path":"a.txt"}"#)]),
        text("It says alpha.", StopReason::EndTurn),
    ]);
    let mut hooks = Hooks::default();
    let out = run(&w, &p, LoopConfig::default(), &mut hooks).await;

    assert_eq!(out.end, LoopEnd::Completed);
    assert_eq!((out.model_calls, out.tool_calls), (2, 1));
    assert_eq!(out.final_text(), "It says alpha.");
    assert_eq!(hooks.committed.len(), 2, "every iteration was committed");
    assert_eq!(
        hooks.assistant_seen, 2,
        "each assistant message was announced"
    );
    assert!(hooks.streamed > 0);

    let reqs = p.requests();
    assert_eq!(reqs.len(), 2);
    let names: Vec<_> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "edit_file",
            "glob",
            "grep",
            "list_dir",
            "multi_edit",
            "read_file",
            "write_file"
        ]
    );
    // The second request carries the assistant's call and the tool's answer.
    let second = &reqs[1].messages;
    assert_eq!(second.len(), 4);
    assert_eq!(second[2].role, Role::Assistant);
    assert_eq!(second[3].role, Role::Tool);
    let results = tool_result_text(&hooks.committed[0]);
    let envelope: Value = serde_json::from_str(&results[0]).expect("envelope JSON");
    assert_eq!(envelope["ok"], true);
    assert!(envelope["data"]["content"]
        .as_str()
        .expect("content")
        .contains("1\talpha"));
}

#[tokio::test]
async fn several_calls_in_one_message_come_back_in_one_ordered_message() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[
            ("c1", "read_file", r#"{"path":"a.txt"}"#),
            ("c2", "read_file", r#"{"path":"b.txt"}"#),
            ("c3", "list_dir", "{}"),
        ]),
        text("done", StopReason::EndTurn),
    ]);
    let mut hooks = Hooks::default();
    let out = run(&w, &p, LoopConfig::default(), &mut hooks).await;
    assert_eq!(out.tool_calls, 3);
    let ids: Vec<_> = hooks.committed[0]
        .tool_message
        .as_ref()
        .expect("message")
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
}

/// REQ-TOOL-019: a failing tool is a result, and the loop goes on.
#[tokio::test]
async fn a_failed_call_is_reported_to_the_model_and_the_turn_continues() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[
            ("c1", "read_file", r#"{"path":"missing.txt"}"#),
            ("c2", "no_such_tool", "{}"),
        ]),
        text("I could not read it.", StopReason::EndTurn),
    ]);
    let mut hooks = Hooks::default();
    let out = run(&w, &p, LoopConfig::default(), &mut hooks).await;
    assert_eq!(out.end, LoopEnd::Completed);
    let results = tool_result_text(&hooks.committed[0]);
    assert!(results[0].contains("E-FS-NOTFOUND") && results[0].contains("recovery"));
    assert!(results[1].contains("E-TOOL-BADSCHEMA") && results[1].contains("read_file"));
    let blocks = &hooks.committed[0].tool_message.as_ref().expect("m").blocks;
    assert!(blocks
        .iter()
        .all(|b| matches!(b, Block::ToolResult { is_error: true, .. })));
}

/// §4.3: arguments that never parsed are answered with `E-TOOL-BADJSON`
/// without running anything.
#[tokio::test]
async fn unparseable_arguments_are_answered_without_dispatch() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[("c1", "read_file", r#"{"path":"a.txt","li"#)]),
        text("sorry", StopReason::EndTurn),
    ]);
    let mut hooks = Hooks::default();
    run(&w, &p, LoopConfig::default(), &mut hooks).await;
    let r = &tool_result_text(&hooks.committed[0])[0];
    assert!(r.contains("E-TOOL-BADJSON"), "{r}");
    assert!(r.contains("well-formed JSON"));
}

/// T-CLI-011's engine: an iteration cap stops a model that never stops.
#[tokio::test]
async fn the_iteration_guardrail_trips_after_the_configured_number_of_calls() {
    let w = world(Mode::Build);
    let step = || calls(&[("c", "list_dir", "{}")]);
    let p = Scripted::new(vec![step(), step(), step()]);
    let mut hooks = Hooks::default();
    let out = run(
        &w,
        &p,
        LoopConfig {
            max_iterations: 3,
            ..LoopConfig::default()
        },
        &mut hooks,
    )
    .await;
    assert_eq!(
        out.end,
        LoopEnd::Guardrail {
            rule: "max_iterations",
            limit: 3,
            actual: 4
        }
    );
    assert_eq!(out.model_calls, 3);
    assert_eq!(
        hooks.committed.len(),
        3,
        "everything before the trip is committed"
    );
}

#[tokio::test]
async fn the_tool_call_budget_is_checked_before_the_calls_run() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![calls(&[
        ("a", "list_dir", "{}"),
        ("b", "list_dir", "{}"),
        ("c", "list_dir", "{}"),
    ])]);
    let mut hooks = Hooks::default();
    let out = run(
        &w,
        &p,
        LoopConfig {
            max_tool_calls: 2,
            ..LoopConfig::default()
        },
        &mut hooks,
    )
    .await;
    assert_eq!(
        out.end,
        LoopEnd::Guardrail {
            rule: "max_tool_calls",
            limit: 2,
            actual: 3
        }
    );
    assert!(hooks.committed.is_empty(), "nothing ran, nothing to commit");
    assert_eq!(out.tool_calls, 0);
}

/// §8.3 T-5: repeated denials end the turn; a success in between resets the
/// count.
#[tokio::test]
async fn consecutive_permission_denials_end_the_turn() {
    let w = world(Mode::Build);
    let denied = || calls(&[("c", "read_file", r#"{"path":".env"}"#)]);
    let p = Scripted::new(vec![denied(), denied(), denied()]);
    let mut hooks = Hooks::default();
    let out = run(&w, &p, LoopConfig::default(), &mut hooks).await;
    assert_eq!(out.end, LoopEnd::Denied { consecutive: 3 });
    assert_eq!(out.model_calls, 3);
    assert!(tool_result_text(&hooks.committed[2])[0].contains("E-FS-PROTECTED"));

    let reset = Scripted::new(vec![
        denied(),
        denied(),
        calls(&[("c", "read_file", r#"{"path":"a.txt"}"#)]),
        denied(),
        denied(),
        text("fine", StopReason::EndTurn),
    ]);
    let mut hooks = Hooks::default();
    let out = run(&w, &reset, LoopConfig::default(), &mut hooks).await;
    assert_eq!(
        out.end,
        LoopEnd::Completed,
        "a success in the middle resets the run"
    );
}

#[tokio::test]
async fn max_tokens_and_content_filter_are_terminal_with_their_own_ends() {
    let w = world(Mode::Build);
    let mut hooks = Hooks::default();
    let out = run(
        &w,
        &Scripted::new(vec![text("cut off mid-", StopReason::MaxTokens)]),
        LoopConfig::default(),
        &mut hooks,
    )
    .await;
    assert_eq!(out.end, LoopEnd::MaxTokens);
    assert_eq!(
        out.final_text(),
        "cut off mid-",
        "the partial answer is kept"
    );
    let out = run(
        &w,
        &Scripted::new(vec![text("", StopReason::ContentFilter)]),
        LoopConfig::default(),
        &mut Hooks::default(),
    )
    .await;
    assert_eq!(out.end, LoopEnd::ContentFilter);
}

#[tokio::test]
async fn a_provider_failure_after_a_tool_round_keeps_what_was_committed() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[("c1", "read_file", r#"{"path":"a.txt"}"#)]),
        Step::Fail(ProviderFault::Auth),
    ]);
    let mut hooks = Hooks::default();
    let out = run(&w, &p, LoopConfig::default(), &mut hooks).await;
    assert_eq!(out.end, LoopEnd::ProviderFailed);
    assert_eq!(
        out.fault.as_ref().and_then(ProviderError::code),
        Some("E-PROV-AUTH")
    );
    assert_eq!(hooks.committed.len(), 1, "the tool round is durable");
}

#[tokio::test]
async fn a_raised_token_ends_the_loop_cancelled() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[("c", "list_dir", "{}")]),
        text("never", StopReason::EndTurn),
    ]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let out = run_with(
        &w,
        &p,
        LoopConfig::default(),
        &mut Hooks::default(),
        &cancel,
    )
    .await
    .expect("runs");
    assert_eq!(out.end, LoopEnd::Cancelled);
}

#[tokio::test]
async fn a_commit_that_cannot_be_written_stops_the_loop_with_its_error() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![
        calls(&[("c", "list_dir", "{}")]),
        text("never", StopReason::EndTurn),
    ]);
    let mut hooks = Hooks {
        fail_on: Some(0),
        ..Hooks::default()
    };
    let err = run_with(
        &w,
        &p,
        LoopConfig::default(),
        &mut hooks,
        &CancellationToken::new(),
    )
    .await
    .expect_err("flush failure");
    assert_eq!(err.code, "E-SESS-FLUSH");
    assert_eq!(
        p.requests().len(),
        1,
        "no second model call after a failed commit"
    );
}

#[tokio::test]
async fn a_plain_answer_is_one_iteration_with_no_tool_message() {
    let w = world(Mode::Build);
    let mut hooks = Hooks::default();
    let out = run(
        &w,
        &Scripted::new(vec![text("hello", StopReason::EndTurn)]),
        LoopConfig::default(),
        &mut hooks,
    )
    .await;
    assert_eq!(out.end, LoopEnd::Completed);
    assert!(out.iterations[0].tool_message.is_none());
    assert_eq!((out.model_calls, out.tool_calls), (1, 0));
    assert_eq!(out.iterations[0].reported_usage.map(|u| u.input), Some(10));
}

/// REQ-MODE-001 through the loop: what the model is offered depends on mode.
#[tokio::test]
async fn the_request_carries_exactly_the_tools_the_mode_offers() {
    for mode in [Mode::Plan, Mode::Build] {
        let w = world(mode);
        let p = Scripted::new(vec![text("ok", StopReason::EndTurn)]);
        run(&w, &p, LoopConfig::default(), &mut Hooks::default()).await;
        let offered = p.requests()[0].tools.len();
        assert_eq!(offered, w.executor.registry().definitions(mode).len());
        assert!(offered >= 4, "{mode:?}");
    }
}

#[tokio::test]
async fn tool_schemas_reach_the_provider_unchanged() {
    let w = world(Mode::Build);
    let p = Scripted::new(vec![text("ok", StopReason::EndTurn)]);
    run(&w, &p, LoopConfig::default(), &mut Hooks::default()).await;
    let read = p.requests()[0]
        .tools
        .iter()
        .find(|t| t.name == "read_file")
        .cloned()
        .expect("read_file");
    assert_eq!(read.input_schema["required"], json!(["path"]));
    assert_eq!(read.input_schema["additionalProperties"], false);
}
