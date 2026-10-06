//! The §6.5 pipeline and §6.6 parallel policy, with fake tools so each rule
//! can be provoked on its own.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::Mode;
use cairn_perm::{Effect, PermissionPolicy, PermissionRequest, Scope};
use cairn_tools::{
    Access, Answer, Idempotency, PathArg, PermissionClass, RequestInfo, SideEffect, Tool, ToolCall,
    ToolContext, ToolError, ToolOutput,
};
use common::{assert_model_visible, code, Fixture, Options, Scripted, Silent};
use futures::future::BoxFuture;
use futures::FutureExt;
use serde_json::{json, Value};

type Log = Arc<Mutex<Vec<String>>>;

#[derive(Clone)]
enum Behavior {
    Ok(Value),
    Fail(&'static str),
    Sleep(Duration),
    Pending,
    Huge(usize),
    Message(String),
    /// Tracks how many run at once.
    Gauge(Arc<AtomicUsize>, Arc<AtomicUsize>, Duration),
}

#[derive(Clone)]
struct Fake {
    name: &'static str,
    class: PermissionClass,
    effect: SideEffect,
    serial: bool,
    timeout: Duration,
    max_output: u32,
    behavior: Behavior,
    log: Log,
}

impl Fake {
    fn new(name: &'static str, class: PermissionClass, effect: SideEffect, log: &Log) -> Self {
        Self {
            name,
            class,
            effect,
            serial: false,
            timeout: Duration::from_secs(5),
            max_output: 64 * 1024,
            behavior: Behavior::Ok(json!({"done": true})),
            log: Arc::clone(log),
        }
    }

    fn reader(name: &'static str, log: &Log) -> Self {
        Self::new(name, PermissionClass::Read, SideEffect::None, log)
    }

    fn writer(name: &'static str, log: &Log) -> Self {
        Self::new(name, PermissionClass::Write, SideEffect::Write, log)
    }

    fn with(mut self, behavior: Behavior) -> Self {
        self.behavior = behavior;
        self
    }
}

impl Tool for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "a fake tool"
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{
            "path":{"type":"string"},"command":{"type":"string"},"tag":{"type":"string"}}})
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn permission_class(&self) -> PermissionClass {
        self.class
    }
    fn side_effect(&self) -> SideEffect {
        self.effect
    }
    fn idempotency(&self) -> Idempotency {
        Idempotency::Safe
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
    fn max_output_bytes(&self) -> u32 {
        self.max_output
    }
    fn requires_serial(&self) -> bool {
        self.serial
    }
    fn path_args(&self, input: &Value) -> Vec<PathArg> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(|p| PathArg {
                field: "path",
                value: p.to_string(),
                access: if self.effect == SideEffect::Write {
                    Access::Write
                } else {
                    Access::Read
                },
            })
            .into_iter()
            .collect()
    }
    fn request_info(&self, input: &Value) -> RequestInfo {
        RequestInfo {
            command: input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_string),
            url: None,
        }
    }
    fn execute(
        &self,
        input: Value,
        _ctx: ToolContext,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        let me = self.clone();
        async move {
            let tag = input
                .get("tag")
                .and_then(Value::as_str)
                .unwrap_or(me.name)
                .to_string();
            me.log.lock().expect("log").push(format!("start:{tag}"));
            let outcome = match &me.behavior {
                Behavior::Ok(v) => Ok(ToolOutput::new(v.clone())),
                Behavior::Fail(c) => Err(ToolError::new(c, "it failed").recovery("try again")),
                Behavior::Sleep(d) => {
                    tokio::time::sleep(*d).await;
                    Ok(ToolOutput::new(json!({"slept": true})))
                }
                Behavior::Pending => std::future::pending().await,
                Behavior::Huge(n) => Ok(ToolOutput::new(json!({"blob": "z".repeat(*n)}))),
                Behavior::Message(m) => {
                    Err(ToolError::new(codes::FS_PERM, m.clone()).recovery("none"))
                }
                Behavior::Gauge(now, max, d) => {
                    let c = now.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(c, Ordering::SeqCst);
                    tokio::time::sleep(*d).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                    Ok(ToolOutput::new(json!({})))
                }
            };
            me.log.lock().expect("log").push(format!("end:{tag}"));
            outcome
        }
        .boxed()
    }
}

struct Open;
impl Tool for Open {
    fn name(&self) -> &'static str {
        "open_tool"
    }
    fn description(&self) -> &'static str {
        "d"
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object"})
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
        Duration::from_secs(1)
    }
    fn max_output_bytes(&self) -> u32 {
        1
    }
    fn execute(
        &self,
        _: Value,
        _: ToolContext,
        _: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        async { Ok(ToolOutput::new(json!({}))) }.boxed()
    }
}

fn log() -> Log {
    Log::default()
}

fn entries(log: &Log) -> Vec<String> {
    log.lock().expect("log").clone()
}

fn position(log: &[String], what: &str) -> usize {
    log.iter()
        .position(|e| e == what)
        .unwrap_or_else(|| panic!("{what} not in {log:?}"))
}

/// A fixture with `tools` registered. Invented tool names have no built-in
/// rule (so Build would ask, and nobody answers); outside plan mode they get
/// a session allow so the test can reach the rule it is about.
fn fixture_with(mode: Mode, tools: Vec<Fake>) -> Fixture {
    let fx = Fixture::build(Options {
        mode,
        builtin: false,
        extra_tools: tools
            .into_iter()
            .map(|t| Arc::new(t) as Arc<dyn Tool>)
            .collect(),
        ..Options::default()
    });
    if mode != Mode::Plan {
        fx.policy
            .remember(
                &PermissionRequest::new("tool:*", mode),
                Effect::Allow,
                Scope::Session,
            )
            .expect("rule");
    }
    fx
}

// -------------------------------------------------------- registration

#[tokio::test]
async fn an_unknown_tool_is_a_model_visible_error_that_lists_what_exists() {
    let l = log();
    let fx = fixture_with(
        Mode::Build,
        vec![Fake::reader("alpha", &l), Fake::reader("beta", &l)],
    );
    let r = fx.call("gamma", json!({})).await;
    assert_eq!(code(&r), "E-TOOL-BADSCHEMA");
    assert_model_visible(&r);
    let recovery = r.envelope["error"]["recovery"].as_str().expect("recovery");
    assert!(
        recovery.contains("alpha") && recovery.contains("beta"),
        "{recovery}"
    );

    let none = Fixture::build(Options {
        builtin: false,
        ..Options::default()
    });
    let r = none.call("anything", json!({})).await;
    assert!(r.envelope["error"]["recovery"]
        .as_str()
        .unwrap_or("")
        .contains("answer in text"));
}

#[test]
fn registration_enforces_the_contract() {
    use cairn_tools::{Registry, RegistryError};
    let l = log();
    let mut reg = Registry::new();
    reg.register(Arc::new(Fake::reader("ok_tool", &l)))
        .expect("registers");
    assert_eq!(
        reg.register(Arc::new(Fake::reader("ok_tool", &l))),
        Err(RegistryError::Duplicate("ok_tool"))
    );
    for bad in ["Upper", "x", "has-dash", "9start", ""] {
        let mut fake = Fake::reader("placeholder", &l);
        fake.name = Box::leak(bad.to_string().into_boxed_str());
        assert!(
            matches!(reg.register(Arc::new(fake)), Err(RegistryError::BadName(_))),
            "{bad:?}"
        );
    }
    // An open input schema is refused.
    let err = reg.register(Arc::new(Open)).expect_err("open schema");
    assert!(err.to_string().contains("additionalProperties"), "{err}");
}

// ------------------------------------------------------------- modes

/// REQ-TOOL-002 / T-TOOL-002: in plan mode a write tool is *absent* from the
/// request, and a call to it anyway is `E-PERM-MODE` — the turn goes on.
#[tokio::test]
async fn t_tool_002_plan_mode_omits_write_tools_and_refuses_a_direct_call() {
    let l = log();
    let state = Fake::new(
        "todo_write",
        PermissionClass::WriteState,
        SideEffect::Write,
        &l,
    );
    let fx = fixture_with(
        Mode::Plan,
        vec![
            Fake::reader("read_file", &l),
            Fake::writer("write_file", &l),
            Fake::new("bash", PermissionClass::Execute, SideEffect::Execute, &l),
            state,
        ],
    );
    let offered: Vec<String> = fx
        .executor
        .registry()
        .definitions(Mode::Plan)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(
        offered,
        ["read_file", "todo_write"],
        "write and execute tools are absent"
    );
    let build: Vec<String> = fx
        .executor
        .registry()
        .definitions(Mode::Build)
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(build.len(), 4);

    let r = fx.call("write_file", json!({"path": "a.txt"})).await;
    assert_eq!(code(&r), "E-PERM-MODE");
    assert_eq!(
        r.envelope["error"]["message"],
        "Plan mode is read-only. This action was not executed."
    );
    assert_model_visible(&r);
    assert!(r.denied);
    assert!(entries(&l).is_empty(), "the tool body never ran");
    assert!(fx.events.kinds().contains(&"permission.denied"));
}

// ------------------------------------------------------- approvals

#[tokio::test]
async fn ask_runs_the_tool_when_approved_once_and_asks_again_next_time() {
    let l = log();
    let approver = Scripted::with(&[Answer::Once, Answer::Deny]);
    let fx = Fixture::build(Options {
        builtin: false,
        extra_tools: vec![Arc::new(Fake::writer("edit_file", &l))],
        approver: Some(Arc::clone(&approver) as Arc<dyn cairn_tools::Approver>),
        ..Options::default()
    });
    let r = fx.call("edit_file", json!({"path": "src/a.rs"})).await;
    assert!(r.ok, "{}", r.envelope);
    let asked = approver.asked.lock().expect("asked").clone();
    assert_eq!(
        asked[0].summary, "src/a.rs",
        "the exact path, not a description"
    );
    assert_eq!(asked[0].rule_id, "D3");

    // `Once` remembers nothing: the second call asks again, and is declined.
    let second = fx.call("edit_file", json!({"path": "src/a.rs"})).await;
    assert_eq!(code(&second), "E-PERM-DENIED");
    assert!(second.envelope["error"]["message"]
        .as_str()
        .expect("m")
        .contains("declined"));
    assert_eq!(approver.asked.lock().expect("asked").len(), 2);
    let kinds = fx.events.kinds();
    assert!(kinds.contains(&"approval.requested") && kinds.contains(&"approval.answered"));
}

/// T-PERM-005 through the pipeline: "always" writes the rule, and the next
/// call is not asked.
#[tokio::test]
async fn always_persists_a_rule_and_stops_asking() {
    let l = log();
    let approver = Scripted::with(&[Answer::Always]);
    let fx = Fixture::build(Options {
        builtin: false,
        extra_tools: vec![Arc::new(Fake::new(
            "bash",
            PermissionClass::Execute,
            SideEffect::Execute,
            &l,
        ))],
        approver: Some(Arc::clone(&approver) as Arc<dyn cairn_tools::Approver>),
        ..Options::default()
    });
    let input = json!({"command": "npm test"});
    assert!(fx.call("bash", input.clone()).await.ok);
    assert!(fx.call("bash", input).await.ok, "no second question");
    assert_eq!(approver.asked.lock().expect("asked").len(), 1);

    let file = fx.root.join(".cairn").join("permissions.json");
    let text = std::fs::read_to_string(&file).expect("rule file");
    assert!(text.contains("npm test"), "{text}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).expect("meta").permissions().mode() & 0o777,
            0o600
        );
    }
    // A different command still asks.
    assert_eq!(
        code(&fx.call("bash", json!({"command": "rm -rf build"})).await),
        "E-PERM-DENIED"
    );
}

#[tokio::test]
async fn session_and_deny_always_answers() {
    let l = log();
    let approver = Scripted::with(&[Answer::Session, Answer::DenyAlways]);
    let fx = Fixture::build(Options {
        builtin: false,
        extra_tools: vec![Arc::new(Fake::new(
            "bash",
            PermissionClass::Execute,
            SideEffect::Execute,
            &l,
        ))],
        approver: Some(Arc::clone(&approver) as Arc<dyn cairn_tools::Approver>),
        ..Options::default()
    });
    let a = json!({"command": "make build"});
    assert!(fx.call("bash", a.clone()).await.ok);
    assert!(
        fx.call("bash", a).await.ok,
        "allowed for the session without asking"
    );
    assert!(
        !fx.root.join(".cairn").join("permissions.json").exists(),
        "a session answer writes no file"
    );

    let b = json!({"command": "make deploy"});
    assert_eq!(code(&fx.call("bash", b.clone()).await), "E-PERM-DENIED");
    // Now denied by a stored rule — the approver is not consulted again.
    assert_eq!(code(&fx.call("bash", b).await), "E-PERM-DENIED");
    assert_eq!(approver.asked.lock().expect("asked").len(), 2);
    assert!(
        std::fs::read_to_string(fx.root.join(".cairn/permissions.json"))
            .expect("file")
            .contains("\"deny\"")
    );
}

/// T-PERM-013: an unanswered approval times out as `E-PERM-TIMEOUT`.
#[tokio::test]
async fn t_perm_013_an_unanswered_approval_times_out() {
    let l = log();
    let fx = Fixture::build(Options {
        builtin: false,
        extra_tools: vec![Arc::new(Fake::writer("edit_file", &l))],
        approver: Some(Arc::new(Silent)),
        approval_timeout: Duration::from_millis(60),
        ..Options::default()
    });
    let r = fx.call("edit_file", json!({"path": "a.txt"})).await;
    assert_eq!(code(&r), "E-PERM-TIMEOUT");
    assert_model_visible(&r);
    assert!(entries(&l).is_empty());
}

/// T-PERM-010: `rm -rf` in build asks (D5), and runs once approved; a
/// read-only command in auto runs without asking (D4).
#[tokio::test]
async fn t_perm_010_default_rules_drive_bash_decisions() {
    let l = log();
    let approver = Scripted::with(&[Answer::Once]);
    let build = Fixture::build(Options {
        builtin: false,
        extra_tools: vec![Arc::new(Fake::new(
            "bash",
            PermissionClass::Execute,
            SideEffect::Execute,
            &l,
        ))],
        approver: Some(Arc::clone(&approver) as Arc<dyn cairn_tools::Approver>),
        ..Options::default()
    });
    assert!(
        build
            .call("bash", json!({"command": "rm -rf /tmp/x"}))
            .await
            .ok
    );
    assert_eq!(approver.asked.lock().expect("asked").len(), 1);

    let auto = Fixture::build(Options {
        mode: Mode::Auto,
        builtin: false,
        extra_tools: vec![Arc::new(Fake::new(
            "bash",
            PermissionClass::Execute,
            SideEffect::Execute,
            &l,
        ))],
        ..Options::default()
    });
    assert!(auto.call("bash", json!({"command": "git status"})).await.ok);
    assert!(auto.approver.asked.lock().expect("asked").is_empty());
    assert_eq!(
        code(&auto.call("bash", json!({"command": "rm -rf /tmp/x"})).await),
        "E-PERM-DENIED",
        "ask with nobody to answer is a refusal"
    );
}

/// D12: a protected path is refused before permission is even consulted —
/// even where a rule (or `auto-unsafe`) would allow everything.
#[tokio::test]
async fn protected_paths_are_refused_in_every_mode() {
    let l = log();
    for mode in Mode::ALL {
        let fx = Fixture::build(Options {
            mode,
            builtin: false,
            extra_tools: vec![Arc::new(Fake::writer("write_file", &l))],
            ..Options::default()
        });
        fx.policy
            .remember(
                &PermissionRequest::new("write_file", mode).path(".env"),
                Effect::Allow,
                Scope::Session,
            )
            .expect("rule");
        for path in [
            ".env",
            "keys/server.pem",
            ".git/config",
            ".cairn/permissions.json",
        ] {
            let r = fx.call("write_file", json!({"path": path})).await;
            assert_eq!(code(&r), "E-FS-PROTECTED", "{mode:?} {path}");
            assert!(r.denied);
            assert_model_visible(&r);
        }
    }
    assert!(entries(&l).is_empty(), "no tool body ever ran");
}

/// REQ-SAFE-004 / T-PERM-006: there is no tool through which a model can add
/// a permission rule, and nothing it can write reaches the rule file.
#[tokio::test]
async fn t_perm_006_a_model_cannot_grant_itself_permission() {
    let fx = Fixture::new(Mode::AutoUnsafe);
    for name in fx.executor.registry().names() {
        assert!(
            !name.contains("permission") && !name.contains("allow") && !name.contains("rule"),
            "{name}"
        );
    }
    let l = log();
    let writer = fixture_with(Mode::AutoUnsafe, vec![Fake::writer("write_file", &l)]);
    let r = writer
        .call("write_file", json!({"path": ".cairn/permissions.json"}))
        .await;
    assert_eq!(code(&r), "E-FS-PROTECTED");
}

// ------------------------------------------------- execute: limits

#[tokio::test]
async fn a_slow_tool_times_out_and_a_raised_token_cancels() {
    let l = log();
    let mut slow = Fake::reader("slow", &l).with(Behavior::Pending);
    slow.timeout = Duration::from_millis(80);
    let fx = fixture_with(
        Mode::Build,
        vec![slow, Fake::reader("hang", &l).with(Behavior::Pending)],
    );
    let r = fx.call("slow", json!({})).await;
    assert_eq!(code(&r), "E-TOOL-TIMEOUT");
    assert_model_visible(&r);
    assert_eq!(r.status, cairn_core::event::ToolStatus::Timeout);

    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        stopper.cancel();
    });
    let started = Instant::now();
    let r = fx.call_with("hang", json!({}), &cancel).await;
    assert_eq!(code(&r), "E-TOOL-CANCELLED");
    assert_eq!(r.status, cairn_core::event::ToolStatus::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(2));

    // Already cancelled: the tool body never starts.
    let before = entries(&l).len();
    let done = CancellationToken::new();
    done.cancel();
    assert_eq!(
        code(&fx.call_with("hang", json!({}), &done).await),
        "E-TOOL-CANCELLED"
    );
    assert_eq!(entries(&l).len(), before);
}

#[tokio::test]
async fn output_over_the_cap_is_truncated_but_stays_valid() {
    let l = log();
    let mut huge = Fake::reader("huge", &l).with(Behavior::Huge(500_000));
    huge.max_output = 8 * 1024;
    let fx = fixture_with(Mode::Build, vec![huge]);
    let r = fx.call("huge", json!({})).await;
    assert!(r.ok && r.truncated);
    assert!(r.text().len() <= 8 * 1024, "{}", r.text().len());
    assert_eq!(r.envelope["truncated"], true);
    assert!(
        r.envelope["data"]["blob"]
            .as_str()
            .expect("blob")
            .contains("[line truncated]"),
        "a single 500 KB line is cut by the 2,000-character line rule"
    );
}

#[tokio::test]
async fn tool_errors_pass_through_with_their_code_and_are_redacted() {
    let l = log();
    let fx = fixture_with(
        Mode::Build,
        vec![
            Fake::reader("fails", &l).with(Behavior::Fail("E-FS-PERM")),
            Fake::reader("leaks", &l).with(Behavior::Message(
                "token = sk-abcdefghijklmnopqrstuvwxyz0123456789 rejected".into(),
            )),
        ],
    );
    let r = fx.call("fails", json!({})).await;
    assert_eq!(code(&r), "E-FS-PERM");
    assert_eq!(r.envelope["error"]["recovery"], "try again");
    let leaked = fx.call("leaks", json!({})).await;
    assert!(
        !leaked
            .text()
            .contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"),
        "{}",
        leaked.text()
    );
}

// ------------------------------------------------------ §6.6 policy

/// T-TOOL-014: one failing call among parallel ones does not cancel the rest.
#[tokio::test]
async fn t_tool_014_one_failure_does_not_cancel_its_siblings() {
    let l = log();
    let fx = fixture_with(
        Mode::Build,
        vec![
            Fake::reader("ok_a", &l),
            Fake::reader("bad", &l).with(Behavior::Fail("E-FS-PERM")),
            Fake::reader("ok_b", &l).with(Behavior::Sleep(Duration::from_millis(30))),
            Fake::reader("ok_c", &l),
        ],
    );
    let calls = ["ok_a", "bad", "ok_b", "ok_c"]
        .iter()
        .map(|n| ToolCall {
            call_id: format!("c_{n}"),
            name: (*n).to_string(),
            input: json!({}),
        })
        .collect();
    let (results, burst) = fx
        .executor
        .run_batch(calls, &fx.env, &CancellationToken::new())
        .await;
    assert!(!burst);
    let ok: Vec<bool> = results.iter().map(|r| r.ok).collect();
    assert_eq!(ok, [true, false, true, true]);
    let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        ["ok_a", "bad", "ok_b", "ok_c"],
        "results keep call order"
    );
}

/// T-TOOL-013: 20 calls — 16 are dispatched at once, the rest wait, and the
/// loop is told to warn the model.
#[tokio::test]
async fn t_tool_013_a_burst_beyond_sixteen_is_queued() {
    let l = log();
    let now = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let probe = Fake::reader("probe", &l).with(Behavior::Gauge(
        Arc::clone(&now),
        Arc::clone(&max),
        Duration::from_millis(40),
    ));
    let fx = fixture_with(Mode::Build, vec![probe]);
    let calls: Vec<ToolCall> = (0..20)
        .map(|i| ToolCall {
            call_id: format!("c{i}"),
            name: "probe".into(),
            input: json!({"tag": format!("p{i}")}),
        })
        .collect();
    let (results, burst) = fx
        .executor
        .run_batch(calls, &fx.env, &CancellationToken::new())
        .await;
    assert!(burst, "W-TOOL-BURST applies");
    assert_eq!(results.len(), 20);
    assert!(results.iter().all(|r| r.ok));
    assert_eq!(
        max.load(Ordering::SeqCst),
        8,
        "never more than 8 parallel-safe calls at once"
    );
    // The last four only started after the first sixteen had all finished.
    let log = entries(&l);
    let last_group_start = position(&log, "start:p16");
    for i in 0..16 {
        assert!(
            position(&log, &format!("end:p{i}")) < last_group_start,
            "p{i}"
        );
    }
}

/// T-TOOL-012 / REQ-TOOL-021: reads overlap, writers to different paths
/// overlap, writers to one path queue, and a serial tool waits for every
/// write already in flight.
#[tokio::test]
async fn t_tool_012_ordering_between_reads_writers_and_a_serial_tool() {
    let l = log();
    let nap = Behavior::Sleep(Duration::from_millis(60));
    let mut bash = Fake::new("bash", PermissionClass::Execute, SideEffect::Execute, &l)
        .with(Behavior::Sleep(Duration::from_millis(10)));
    bash.serial = true;
    let fx = fixture_with(
        Mode::AutoUnsafe,
        vec![
            Fake::reader("read_file", &l).with(Behavior::Sleep(Duration::from_millis(30))),
            Fake::writer("edit_file", &l).with(nap),
            bash,
        ],
    );
    let call = |name: &str, tag: &str, path: Option<&str>| ToolCall {
        call_id: format!("c_{tag}"),
        name: name.to_string(),
        input: path.map_or_else(
            || json!({"tag": tag, "command": "make"}),
            |p| json!({"tag": tag, "path": p}),
        ),
    };
    let calls = vec![
        call("read_file", "r1", Some("a.txt")),
        call("read_file", "r2", Some("b.txt")),
        call("edit_file", "wa", Some("a.txt")),
        call("edit_file", "wb", Some("b.txt")),
        call("edit_file", "wa2", Some("a.txt")),
        call("bash", "sh", None),
    ];
    let (results, _) = fx
        .executor
        .run_batch(calls, &fx.env, &CancellationToken::new())
        .await;
    assert!(results.iter().all(|r| r.ok), "{results:?}");
    let log = entries(&l);
    assert!(
        position(&log, "start:r2") < position(&log, "end:r1"),
        "reads overlap: {log:?}"
    );
    assert!(
        position(&log, "start:wb") < position(&log, "end:wa"),
        "different paths overlap: {log:?}"
    );
    assert!(
        position(&log, "start:wa2") > position(&log, "end:wa"),
        "one path queues: {log:?}"
    );
    for w in ["end:wa", "end:wb", "end:wa2"] {
        assert!(
            position(&log, "start:sh") > position(&log, w),
            "the serial tool waits for {w}: {log:?}"
        );
    }
}

// ------------------------------------------------------- events, perf

#[tokio::test]
async fn a_call_publishes_started_then_finished() {
    let l = log();
    let fx = fixture_with(Mode::Build, vec![Fake::reader("alpha", &l)]);
    assert!(fx.call("alpha", json!({})).await.ok);
    assert_eq!(fx.events.kinds(), ["tool.started", "tool.finished"]);
    let events = fx.events.0.lock().expect("events");
    let cairn_core::EventData::ToolFinished { status, error, .. } = &events[1] else {
        panic!("tool.finished")
    };
    assert_eq!(*status, cairn_core::event::ToolStatus::Ok);
    assert!(error.is_none());
}

/// T-TOOL-010: a crafted `../../etc/passwd` is refused in under 20 ms at the
/// 95th percentile over 1,000 calls (REQ-TOOL-018).
#[tokio::test]
async fn t_tool_010_pipeline_steps_one_to_seven_are_fast() {
    let l = log();
    let fx = fixture_with(Mode::Build, vec![Fake::reader("read_file", &l)]);
    let mut times = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let started = Instant::now();
        let r = fx
            .call("read_file", json!({"path": "../../etc/passwd"}))
            .await;
        times.push(started.elapsed());
        assert_eq!(code(&r), "E-FS-ESCAPE");
    }
    times.sort();
    let p95 = times[949];
    assert!(p95 < Duration::from_millis(20), "p95 = {p95:?}");
}
