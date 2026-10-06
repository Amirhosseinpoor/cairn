//! Compaction through the loop and on its own (T-CTX-011, -012, -016..-018,
//! -021; SPEC §5.4, §5.6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cairn_agent::compaction::{compact, Job, Outcome};
use cairn_agent::lifecycle::{run_loop, Compacted, Iteration, LoopConfig, LoopEnd, LoopHooks};
use cairn_agent::transcript::{resume_state, SessionWriter};
use cairn_context::budget::{history_tokens, Budget};
use cairn_context::compact::{is_summary, Trigger};
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
use cairn_session::record::Header;
use cairn_session::Store;
use cairn_tools::{
    builtin, Boundary, CallEnv, DenyAll, Executor, ExecutorParts, NullSink, Registry,
};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};

enum Step {
    Say(String),
    FailContext,
    FailAuth,
    Hang,
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
        self.requests.lock().unwrap().clone()
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
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        self.requests.lock().unwrap().push(req);
        let step = self.script.lock().unwrap().pop_front();
        async move {
            match step {
                Some(Step::Say(text)) => Ok(futures::stream::iter(vec![
                    StreamEvent::MessageStart {
                        model: "m".into(),
                        id: "1".into(),
                    },
                    StreamEvent::TextDelta { text },
                    StreamEvent::Usage {
                        input: 10,
                        output: 5,
                        cache_read: 0,
                        cache_write: 0,
                    },
                    StreamEvent::Finish {
                        stop: StopReason::EndTurn,
                    },
                ])
                .boxed()),
                Some(Step::FailContext) => {
                    Err(ProviderError::new(ProviderFault::ContextLength, "too long"))
                }
                Some(Step::FailAuth) => Err(ProviderError::new(ProviderFault::Auth, "no")),
                Some(Step::Hang) => {
                    // Wait until the caller gives up.
                    while !cancel.is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Ok(futures::stream::iter(Vec::new()).boxed())
                }
                None => panic!("more model calls than the script has"),
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
        self.last.lock().unwrap().take()
    }
    fn record_last_error(&self, error: ProviderError) {
        *self.last.lock().unwrap() = Some(error);
    }
}

/// `n` alternating messages of about 150 tokens each, all from turn 1.
fn long_history(n: usize) -> Vec<Message> {
    (0..n)
        .map(|i| {
            let body = format!("message {i}: {}", "lorem ipsum dolor sit amet ".repeat(22));
            Message::new(
                if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                vec![Block::Text { text: body }],
                1,
            )
        })
        .collect()
}

fn job<'a>(
    provider: &Arc<Scripted>,
    budget: &'a Budget,
    retry: &'a RetryBudget,
    trigger: Trigger,
) -> Job<'a> {
    Job {
        provider: Arc::clone(provider) as Arc<dyn Provider>,
        model: "m",
        retry,
        budget,
        current_turn: 2,
        trigger,
        first_id: 1,
        fixed_tokens: 0,
    }
}

/// A budget whose history share is nil, so only the twelve newest stay.
fn tight(window: u32) -> Budget {
    Budget {
        history: 0,
        ..Budget::for_window(window)
    }
}

fn summary_text(request: &ModelRequest) -> String {
    request.messages[0].text()
}

#[tokio::test]
async fn t_ctx_017_a_compaction_replaces_old_messages_with_one_summary() {
    let provider = Scripted::new(vec![
        Step::Say("## Goal\nship it\n".into()),
        Step::Say("## Goal\nand more\n".into()),
    ]);
    let budget = tight(100_000);
    let retry = RetryBudget::default();
    let history = long_history(40);
    let out = compact(
        &job(&provider, &budget, &retry, Trigger::Threshold),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await;
    let Outcome::Compacted {
        messages,
        records,
        trimmed,
    } = out
    else {
        panic!("expected a compaction, got {out:?}");
    };
    assert!(!trimmed);
    assert_eq!(records.len(), 1);
    // 28 older messages go to the summariser as chunks of 24 and 4.
    assert_eq!(records[0].replaced.len(), 28);
    assert!(is_summary(&messages[0]));
    assert_eq!(messages.len(), 40 - 28 + 1);
    assert_eq!(
        messages[1].id, history[28].id,
        "the recent twelve are untouched"
    );
    assert!(history_tokens(&messages) < history_tokens(&history));
    assert!(messages[0].text().contains("## Goal\nship it"));
    // The summariser got the exact §5.6.1 prompt, at temperature 0.
    let asked = provider.requests();
    assert_eq!(asked.len(), 2, "one request per chunk of at most 24");
    assert!(summary_text(&asked[0]).starts_with(cairn_context::compact::SUMMARY_PROMPT));
    assert_eq!(asked[0].temperature, Some(0.0));
    assert_eq!(asked[0].max_tokens, 2000);
    assert!(summary_text(&asked[0]).contains("message 0:"));
    assert!(!summary_text(&asked[0]).contains("message 39:"));
}

#[tokio::test]
async fn the_users_instructions_ride_along() {
    let provider = Scripted::new(vec![Step::Say("s".into()), Step::Say("s".into())]);
    let budget = tight(100_000);
    let retry = RetryBudget::default();
    compact(
        &job(
            &provider,
            &budget,
            &retry,
            Trigger::User(Some("keep the todo list".into())),
        ),
        &long_history(40),
        Some("keep the todo list"),
        &CancellationToken::new(),
    )
    .await;
    assert!(summary_text(&provider.requests()[0])
        .contains("Additional instructions from the user: keep the todo list"));
}

/// T-CTX-016 / REQ-CTX-014.
#[tokio::test]
async fn t_ctx_016_cancelling_a_compaction_keeps_nothing_and_is_prompt() {
    let provider = Scripted::new(vec![Step::Hang]);
    let budget = tight(100_000);
    let retry = RetryBudget::default();
    let token = CancellationToken::new();
    let flip = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flip.cancel();
    });
    let started = Instant::now();
    let out = compact(
        &job(&provider, &budget, &retry, Trigger::Threshold),
        &long_history(40),
        None,
        &token,
    )
    .await;
    assert!(matches!(out, Outcome::Cancelled), "{out:?}");
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_short_conversation_has_nothing_to_compact() {
    let provider = Scripted::new(vec![]);
    let budget = Budget::for_window(100_000);
    let retry = RetryBudget::default();
    let out = compact(
        &job(&provider, &budget, &retry, Trigger::User(None)),
        &long_history(8),
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(out, Outcome::Nothing), "{out:?}");
    assert!(provider.requests().is_empty());
}

/// T-CTX-021: summaries that do not shrink anything run out the rounds.
#[tokio::test]
async fn t_ctx_021_three_rounds_without_enough_saving_trim_the_oldest() {
    // Each summary is as large as what it replaced.
    let big = "word ".repeat(2_000);
    let provider = Scripted::new((0..12).map(|_| Step::Say(big.clone())).collect());
    let budget = Budget {
        history: 0,
        ..Budget::for_window(8_000)
    };
    let retry = RetryBudget::default();
    let history = long_history(60);
    let out = compact(
        &job(&provider, &budget, &retry, Trigger::Reserve),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await;
    let Outcome::Compacted {
        messages,
        records,
        trimmed,
    } = out
    else {
        panic!("{out:?}");
    };
    assert!(trimmed, "the rounds ran out");
    assert!(records.len() <= 3);
    assert!(history_tokens(&messages) <= budget.compaction_threshold() || messages.len() <= 1);
}

#[tokio::test]
async fn a_summariser_that_fails_reports_its_code() {
    let provider = Scripted::new(vec![Step::FailAuth]);
    let budget = tight(100_000);
    let retry = RetryBudget::default();
    let out = compact(
        &job(&provider, &budget, &retry, Trigger::Threshold),
        &long_history(40),
        None,
        &CancellationToken::new(),
    )
    .await;
    let Outcome::Failed { code, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(code, "E-PROV-AUTH");
}

// ------------------------------------------------------------- the loop

#[derive(Default)]
struct Hooks {
    compactions: Vec<(usize, usize, usize, bool)>,
}

impl LoopHooks for Hooks {
    fn on_stream(&mut self, _event: &StreamEvent) {}
    fn commit(&mut self, _iteration: &Iteration) -> Result<(), CairnError> {
        Ok(())
    }
    fn compacted(&mut self, change: &Compacted<'_>) -> Result<(), CairnError> {
        self.compactions.push((
            change.before.len(),
            change.after.len(),
            change.records.len(),
            change.trimmed,
        ));
        Ok(())
    }
}

struct World {
    _tmp: tempfile::TempDir,
    executor: Executor,
    env: CallEnv,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("ws");
    std::fs::create_dir_all(root.join(".cairn")).unwrap();
    std::fs::create_dir_all(base.join("home")).unwrap();
    let mut registry = Registry::new();
    builtin::register_all(&mut registry).unwrap();
    let boundary =
        Arc::new(Boundary::new(&root, &[], false, Some(base.join("home")), &[], false).unwrap());
    let executor = Executor::new(ExecutorParts {
        registry: Arc::new(registry),
        policy: Arc::new(
            RulePolicy::new(Mode::Build, PolicyFiles::default(), None, false).unwrap(),
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
        questioner: None,
        approval_timeout: Duration::from_secs(1),
    });
    let env = CallEnv {
        session_id: SessionId::new(),
        turn_id: 2,
        cwd: boundary.root().to_path_buf(),
        mode: Mode::Build,
    };
    World {
        _tmp: tmp,
        executor,
        env,
    }
}

fn request(history: Vec<Message>) -> ModelRequest {
    let mut messages = vec![Message::new(
        Role::System,
        vec![Block::Text {
            text: "system".into(),
        }],
        0,
    )];
    messages.extend(history);
    messages.push(Message::new(
        Role::User,
        vec![Block::Text {
            text: "now do the next thing".into(),
        }],
        2,
    ));
    ModelRequest::new("m", messages, 500)
}

fn config(window: u32) -> LoopConfig {
    LoopConfig {
        budget: Some(Budget {
            history: 0,
            ..Budget::for_window(window)
        }),
        ..LoopConfig::default()
    }
}

/// T-CTX-017: C-1 fires at 80% and the real request is the compacted one.
#[tokio::test]
async fn t_ctx_017_the_loop_compacts_at_eighty_percent_before_asking_the_model() {
    let w = world();
    // 80 messages of ~170 tokens plus the tool list against a 20,000 window
    // (usable 17,600, threshold 14,080).
    let provider = Scripted::new(vec![
        Step::Say("## Goal\nx".into()),
        Step::Say("## Goal\ny".into()),
        Step::Say("## Goal\nz".into()),
        Step::Say("done".into()),
    ]);
    let mut hooks = Hooks::default();
    let outcome = run_loop(
        Arc::clone(&provider) as Arc<dyn Provider>,
        request(long_history(80)),
        &w.executor,
        &w.env,
        &CancellationToken::new(),
        &RetryBudget::default(),
        config(20_000),
        &mut hooks,
    )
    .await
    .expect("runs");
    assert_eq!(outcome.end, LoopEnd::Completed);
    assert_eq!(hooks.compactions.len(), 1);
    let (before, after, records, trimmed) = hooks.compactions[0];
    assert_eq!((before, records, trimmed), (81, 1, false));
    assert!(after < before);
    // The answer was requested over the compacted history.
    let requests = provider.requests();
    let last = requests.last().unwrap();
    assert_eq!(last.messages[0].role, Role::System);
    assert!(is_summary(&last.messages[1]));
    assert!(last.messages.len() < 82);
    assert_eq!(
        last.messages.last().unwrap().text(),
        "now do the next thing"
    );
}

/// T-CTX-011 / T-CTX-012: a request that cannot be made to fit is not sent.
#[tokio::test]
async fn t_ctx_012_an_oversized_request_that_cannot_shrink_is_never_sent() {
    let w = world();
    // One enormous message: nothing older to summarise.
    let huge = Message::new(
        Role::User,
        vec![Block::Text {
            text: "x ".repeat(20_000),
        }],
        2,
    );
    let mut req = request(vec![]);
    req.messages.insert(1, huge);
    let provider = Scripted::new(vec![]);
    let mut hooks = Hooks::default();
    let outcome = run_loop(
        Arc::clone(&provider) as Arc<dyn Provider>,
        req,
        &w.executor,
        &w.env,
        &CancellationToken::new(),
        &RetryBudget::default(),
        config(4_000),
        &mut hooks,
    )
    .await
    .expect("runs");
    assert!(
        matches!(outcome.end, LoopEnd::ContextFull { .. }),
        "{:?}",
        outcome.end
    );
    assert_eq!(outcome.model_calls, 0);
    assert!(provider.requests().is_empty(), "the request was not sent");
}

/// C-3: the provider says the context is too long; compact once, resend.
#[tokio::test]
async fn a_provider_context_error_triggers_one_compaction_and_a_resend() {
    let w = world();
    // The window is generous, so only the provider's complaint can trigger it.
    // The provider layer resends a context error once as it is; the second
    // failure is what reaches the loop.
    let provider = Scripted::new(vec![
        Step::FailContext,
        Step::FailContext,
        Step::Say("## Goal\nz".into()),
        Step::Say("## Goal\nz2".into()),
        Step::Say("answer".into()),
    ]);
    let mut hooks = Hooks::default();
    let outcome = run_loop(
        Arc::clone(&provider) as Arc<dyn Provider>,
        request(long_history(40)),
        &w.executor,
        &w.env,
        &CancellationToken::new(),
        &RetryBudget::default(),
        config(200_000),
        &mut hooks,
    )
    .await
    .expect("runs");
    assert_eq!(outcome.end, LoopEnd::Completed, "{:?}", outcome.fault);
    assert_eq!(hooks.compactions.len(), 1);
    assert_eq!(outcome.final_text(), "answer");
}

#[tokio::test]
async fn cancelling_during_a_loop_compaction_ends_the_turn_cancelled() {
    let w = world();
    let provider = Scripted::new(vec![Step::Hang]);
    let token = CancellationToken::new();
    let flip = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        flip.cancel();
    });
    let mut hooks = Hooks::default();
    let outcome = run_loop(
        Arc::clone(&provider) as Arc<dyn Provider>,
        request(long_history(40)),
        &w.executor,
        &w.env,
        &token,
        &RetryBudget::default(),
        config(6_500),
        &mut hooks,
    )
    .await
    .expect("runs");
    assert_eq!(outcome.end, LoopEnd::Cancelled);
    assert!(hooks.compactions.is_empty(), "no partial summary");
}

// -------------------------------------------------------- session records

/// T-CTX-018: a resumed session sees the compacted history, and undoing
/// restores what was there before.
#[tokio::test]
async fn t_ctx_018_compactions_are_recorded_replayed_and_undoable() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    let header = Header::new("ses_1", "/w", "build", "m");
    let mut writer = SessionWriter::create(store.clone(), &header).unwrap();
    let history = long_history(40);
    writer.turn_started(1).unwrap();
    for m in &history {
        writer.message(m).unwrap();
    }
    writer.turn_ended(1, "ok", None, None, 0).unwrap();

    let provider = Scripted::new(vec![
        Step::Say("## Goal\nx".into()),
        Step::Say("## Goal\ny".into()),
    ]);
    let budget = tight(100_000);
    let retry = RetryBudget::default();
    let Outcome::Compacted {
        messages, records, ..
    } = compact(
        &job(&provider, &budget, &retry, Trigger::User(None)),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    else {
        panic!("expected compaction")
    };
    for r in &records {
        writer.compaction(r).unwrap();
    }

    let file = store.load(writer.path()).unwrap();
    let state = resume_state(&file);
    assert_eq!(state.original_messages.len(), 40);
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(state.messages.len(), messages.len());
    assert!(is_summary(&state.messages[0]));
    assert_eq!(state.messages[0].text(), messages[0].text());

    writer.compaction_undone(records[0].id).unwrap();
    let state = resume_state(&store.load(writer.path()).unwrap());
    assert!(state.compactions.is_empty());
    assert_eq!(state.messages.len(), 40, "the original messages are back");
    assert!(!state.messages.iter().any(is_summary));
}
