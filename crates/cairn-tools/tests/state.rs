//! `todo_write` and `ask_user` (SPEC §6.2.16, §6.2.17).

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cairn_core::cancel::CancellationToken;
use cairn_core::Mode;
use cairn_tools::{Question, Questioner, Reply};
use common::{assert_model_visible, code, data, Fixture, Options};
use futures::future::BoxFuture;
use serde_json::{json, Value};

fn todos(items: &[(&str, &str)]) -> Value {
    json!({"todos": items.iter().map(|(id, status)| json!({
        "id": id, "content": format!("do {id}"), "status": status
    })).collect::<Vec<_>>()})
}

fn saved(fx: &Fixture) -> Value {
    serde_json::from_slice(&std::fs::read(fx.path(".cairn/todos.json")).expect("file"))
        .expect("json")
}

#[tokio::test]
async fn todo_write_saves_the_list_and_counts_it() {
    let fx = Fixture::build(Options::default());
    let r = fx
        .call(
            "todo_write",
            todos(&[
                ("a", "completed"),
                ("b", "in_progress"),
                ("c", "pending"),
                ("d", "pending"),
            ]),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["todos_saved"], 4);
    assert_eq!(d["completed"], 1);
    assert_eq!(d["pending"], 3, "open work: pending plus in progress");
    assert_eq!(d["plan_id"], Value::Null);
    let file = saved(&fx);
    assert_eq!(file["todos"].as_array().unwrap().len(), 4);
    assert_eq!(file["todos"][1]["status"], "in_progress");
    assert!(file["session_id"].as_str().is_some());
}

#[tokio::test]
async fn each_call_replaces_the_whole_list() {
    let fx = Fixture::build(Options::default());
    fx.call("todo_write", todos(&[("a", "pending"), ("b", "pending")]))
        .await;
    let r = fx.call("todo_write", todos(&[("c", "completed")])).await;
    assert_eq!(data(&r)["todos_saved"], 1);
    let file = saved(&fx);
    assert_eq!(file["todos"].as_array().unwrap().len(), 1);
    assert_eq!(file["todos"][0]["id"], "c");
}

#[tokio::test]
async fn bad_lists_are_refused_with_a_way_forward() {
    let fx = Fixture::build(Options::default());
    let r = fx
        .call("todo_write", todos(&[("a", "pending"), ("a", "pending")]))
        .await;
    assert_eq!(code(&r), "E-TODO-DUPLICATE");
    assert_model_visible(&r);
    let r = fx
        .call(
            "todo_write",
            todos(&[("a", "in_progress"), ("b", "in_progress")]),
        )
        .await;
    assert_eq!(code(&r), "E-TODO-STATUS");
    assert_model_visible(&r);
    // Nothing was written by either.
    assert!(!fx.path(".cairn/todos.json").exists());
    // An unknown status or a bad id is the schema's business.
    let r = fx.call("todo_write", todos(&[("a", "maybe")])).await;
    assert_eq!(code(&r), "E-TOOL-BADSCHEMA");
    let r = fx
        .call("todo_write", todos(&[("Bad Id!", "pending")]))
        .await;
    assert_eq!(code(&r), "E-TOOL-BADSCHEMA");
}

#[tokio::test]
async fn todo_write_is_allowed_in_every_mode_without_asking() {
    for mode in Mode::ALL {
        let fx = Fixture::build(Options {
            mode,
            ..Options::default()
        });
        let r = fx.call("todo_write", todos(&[("a", "pending")])).await;
        assert!(r.ok, "{mode:?}: {}", r.envelope);
        assert!(fx.approver.asked.lock().unwrap().is_empty(), "{mode:?}");
        let offered: Vec<String> = fx
            .executor
            .registry()
            .definitions(mode)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(offered.contains(&"todo_write".to_string()), "{mode:?}");
        assert!(offered.contains(&"ask_user".to_string()), "{mode:?}");
    }
}

#[tokio::test]
async fn a_plan_id_announces_each_step() {
    let fx = Fixture::build(Options::default());
    let mut input = todos(&[("a", "completed"), ("b", "pending")]);
    input["plan_id"] = json!("plan_1");
    let r = fx.call("todo_write", input).await;
    assert_eq!(data(&r)["plan_id"], "plan_1");
    let steps: Vec<(u32, String)> = fx
        .events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            cairn_core::event::EventData::PlanStep { step, status, .. } => {
                Some((*step, status.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        [(1, "completed".to_string()), (2, "pending".to_string())]
    );
}

// ---------------------------------------------------------------- ask_user

/// Answers from a script; remembers the questions.
struct Scripted {
    replies: Mutex<Vec<Option<Reply>>>,
    asked: Mutex<Vec<Question>>,
}

impl Scripted {
    fn new(replies: Vec<Option<Reply>>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies),
            asked: Mutex::default(),
        })
    }
}

impl Questioner for Scripted {
    fn ask(&self, question: Question) -> BoxFuture<'_, Option<Reply>> {
        self.asked.lock().unwrap().push(question);
        let next = self.replies.lock().unwrap().pop().flatten();
        Box::pin(async move { next })
    }
}

struct Never;

impl Questioner for Never {
    fn ask(&self, _question: Question) -> BoxFuture<'_, Option<Reply>> {
        Box::pin(std::future::pending())
    }
}

fn asking(q: Arc<dyn Questioner>) -> Fixture {
    Fixture::build(Options {
        questioner: Some(q),
        ..Options::default()
    })
}

fn reply(answer: &str, selected: Option<usize>) -> Reply {
    Reply {
        answer: answer.to_string(),
        selected_option: selected,
    }
}

#[tokio::test]
async fn ask_user_returns_what_the_person_said() {
    let q = Scripted::new(vec![Some(reply("use postgres", None))]);
    let fx = asking(Arc::clone(&q) as Arc<dyn Questioner>);
    let r = fx
        .call("ask_user", json!({"question": "Which database?"}))
        .await;
    let d = data(&r);
    assert_eq!(d["answer"], "use postgres");
    assert_eq!(d["selected_option"], Value::Null);
    assert_eq!(d["source"], "user");
    let asked = q.asked.lock().unwrap();
    assert_eq!(asked[0].text, "Which database?");
    assert!(asked[0].allow_free_text);
    // No approval prompt for asking a question.
    assert!(fx.approver.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn choosing_an_option_returns_its_text_and_number() {
    let q = Scripted::new(vec![Some(reply("", Some(2)))]);
    let fx = asking(q);
    let r = fx
        .call(
            "ask_user",
            json!({"question": "Which?", "options": ["sqlite", "postgres", "mysql"], "allow_free_text": false}),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["answer"], "postgres");
    assert_eq!(d["selected_option"], 2);
}

#[tokio::test]
async fn free_text_is_refused_when_only_options_are_allowed() {
    let q = Scripted::new(vec![Some(reply("something else", None))]);
    let fx = asking(q);
    let r = fx
        .call(
            "ask_user",
            json!({"question": "Which?", "options": ["a", "b"], "allow_free_text": false}),
        )
        .await;
    assert_eq!(code(&r), "E-ASK-NOINPUT");
    assert_model_visible(&r);
}

#[tokio::test]
async fn nobody_to_ask_is_e_ask_noinput() {
    let fx = Fixture::build(Options::default());
    let r = fx
        .call("ask_user", json!({"question": "Anyone there?"}))
        .await;
    assert_eq!(code(&r), "E-ASK-NOINPUT");
    assert_model_visible(&r);

    // A channel that closes without an answer is the same thing.
    let fx = asking(Scripted::new(vec![None]));
    let r = fx.call("ask_user", json!({"question": "Anyone?"})).await;
    assert_eq!(code(&r), "E-ASK-NOINPUT");
}

#[tokio::test]
async fn an_unanswered_question_times_out() {
    let fx = asking(Arc::new(Never));
    let started = Instant::now();
    let r = fx
        .call(
            "ask_user",
            json!({"question": "Hello?", "timeout_ms": 1000}),
        )
        .await;
    assert_eq!(code(&r), "E-ASK-TIMEOUT");
    assert_model_visible(&r);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancelling_abandons_the_question() {
    let fx = asking(Arc::new(Never));
    let token = CancellationToken::new();
    let flip = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        flip.cancel();
    });
    let r = fx
        .call_with("ask_user", json!({"question": "Hello?"}), &token)
        .await;
    assert_eq!(code(&r), "E-TOOL-CANCELLED");
}
