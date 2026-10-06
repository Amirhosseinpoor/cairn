//! A workspace on disk plus a wired-up executor, for the tool tests.
#![allow(dead_code, reason = "each test file uses a different part")]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::event::EventData;
use cairn_core::redact::Redactor;
use cairn_core::{Mode, SessionId};
use cairn_perm::{PolicyFiles, RulePolicy};
use cairn_sandbox::PathChecksOnly;
use cairn_search::{IgnoreEngine, IgnoreOptions};
use cairn_tools::{
    builtin, Answer, ApprovalRequest, Approver, Boundary, CallEnv, EventSink, Executor,
    ExecutorParts, Registry, Tool, ToolCall, ToolResult,
};
use futures::future::BoxFuture;
use serde_json::{json, Value};

/// Events, kept in order.
#[derive(Default)]
pub struct Collect(pub Mutex<Vec<EventData>>);

impl EventSink for Collect {
    fn emit(&self, event: EventData) {
        self.0.lock().expect("events").push(event);
    }
}

impl Collect {
    pub fn kinds(&self) -> Vec<&'static str> {
        self.0
            .lock()
            .expect("events")
            .iter()
            .map(EventData::kind)
            .collect()
    }
}

/// An approver that gives scripted answers (denying once the script runs
/// out) and remembers what it was asked.
#[derive(Default)]
pub struct Scripted {
    pub answers: Mutex<VecDeque<Answer>>,
    pub asked: Mutex<Vec<ApprovalRequest>>,
}

impl Scripted {
    pub fn with(answers: &[Answer]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().copied().collect()),
            asked: Mutex::default(),
        })
    }
}

impl Approver for Scripted {
    fn ask(&self, request: ApprovalRequest) -> BoxFuture<'_, Answer> {
        self.asked.lock().expect("asked").push(request);
        let answer = self
            .answers
            .lock()
            .expect("answers")
            .pop_front()
            .unwrap_or(Answer::Deny);
        Box::pin(async move { answer })
    }
}

/// An approver that never answers.
pub struct Silent;

impl Approver for Silent {
    fn ask(&self, _request: ApprovalRequest) -> BoxFuture<'_, Answer> {
        Box::pin(std::future::pending())
    }
}

pub struct Fixture {
    pub tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub executor: Arc<Executor>,
    pub events: Arc<Collect>,
    pub approver: Arc<Scripted>,
    pub env: CallEnv,
    pub policy: Arc<RulePolicy>,
}

pub struct Options {
    pub mode: Mode,
    pub approver: Option<Arc<dyn Approver>>,
    pub extra_tools: Vec<Arc<dyn Tool>>,
    pub builtin: bool,
    pub approval_timeout: Duration,
    pub syntax: Option<Arc<dyn cairn_tools::SyntaxCheck>>,
    pub observer: Option<Arc<dyn cairn_git::WriteObserver>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Build,
            approver: None,
            extra_tools: Vec::new(),
            builtin: true,
            approval_timeout: Duration::from_secs(600),
            syntax: None,
            observer: None,
        }
    }
}

impl Fixture {
    pub fn new(mode: Mode) -> Self {
        Self::build(Options {
            mode,
            ..Options::default()
        })
    }

    pub fn build(options: Options) -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let base = tmp.path().canonicalize().expect("canonical");
        let root = base.join("ws");
        std::fs::create_dir_all(root.join(".cairn")).expect("workspace");
        std::fs::create_dir_all(base.join("home")).expect("home");

        let mut registry = Registry::new();
        if options.builtin {
            builtin::register_all(&mut registry).expect("registers");
        }
        for tool in options.extra_tools {
            registry.register(tool).expect("registers");
        }
        let policy = Arc::new(
            RulePolicy::new(
                options.mode,
                PolicyFiles {
                    project: Some(root.join(".cairn").join("permissions.json")),
                    user: None,
                },
                Some(base.join("home").to_string_lossy().into_owned()),
                false,
            )
            .expect("policy"),
        );
        let boundary = Arc::new(
            Boundary::new(&root, &[], false, Some(base.join("home")), &[], false)
                .expect("boundary"),
        );
        let ignore = Arc::new(IgnoreEngine::new(
            boundary.root(),
            &IgnoreOptions::default(),
        ));
        let events = Arc::new(Collect::default());
        let scripted = Scripted::with(&[]);
        let approver: Arc<dyn Approver> = options
            .approver
            .unwrap_or_else(|| Arc::clone(&scripted) as Arc<dyn Approver>);
        let executor = Arc::new(Executor::new(ExecutorParts {
            registry: Arc::new(registry),
            policy: Arc::clone(&policy) as Arc<dyn cairn_perm::PermissionPolicy>,
            approver,
            sandbox: Arc::new(PathChecksOnly),
            events: Arc::clone(&events) as Arc<dyn EventSink>,
            boundary: Arc::clone(&boundary),
            ignore,
            redactor: Arc::new(Redactor::default()),
            syntax: options.syntax,
            line_endings: cairn_tools::LineEndings::Lf,
            observer: options.observer,
            approval_timeout: options.approval_timeout,
        }));
        let env = CallEnv {
            session_id: SessionId::new(),
            turn_id: 1,
            cwd: boundary.root().to_path_buf(),
            mode: options.mode,
        };
        Self {
            tmp,
            root: boundary.root().to_path_buf(),
            executor,
            events,
            approver: scripted,
            env,
            policy,
        }
    }

    pub fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let full = self.root.join(rel);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
        std::fs::write(full, content).expect("file");
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub async fn call(&self, tool: &str, input: Value) -> ToolResult {
        self.call_with(tool, input, &CancellationToken::new()).await
    }

    pub async fn call_with(
        &self,
        tool: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let call = ToolCall {
            call_id: format!("call_{tool}"),
            name: tool.to_string(),
            input,
        };
        self.executor.run(&call, &self.env, cancel, 0).await
    }
}

/// Every failure a model sees must say what went wrong and what to do
/// (REQ-TOOL-019).
pub fn assert_model_visible(result: &ToolResult) {
    assert!(!result.ok, "{result:?}");
    let error = &result.envelope["error"];
    let code = error["code"].as_str().expect("a code");
    assert!(
        regex::Regex::new(r"^[EW]-[A-Z]+-[A-Z]+$")
            .expect("regex")
            .is_match(code),
        "malformed code {code}"
    );
    assert!(
        !error["message"].as_str().unwrap_or("").is_empty(),
        "{error}"
    );
    assert!(
        !error["recovery"].as_str().unwrap_or("").is_empty(),
        "no recovery on {code}: {error}"
    );
    assert_eq!(result.envelope["ok"], false);
}

pub fn data(result: &ToolResult) -> &Value {
    assert!(result.ok, "{}", result.envelope);
    &result.envelope["data"]
}

pub fn code(result: &ToolResult) -> &str {
    result.envelope["error"]["code"].as_str().unwrap_or("")
}

pub fn lines(n: usize) -> String {
    use std::fmt::Write as _;
    (1..=n).fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "line {i}");
        out
    })
}

pub fn read(path: &str) -> Value {
    json!({ "path": path })
}
