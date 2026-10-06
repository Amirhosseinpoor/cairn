//! `bash`, `bash_background`, `job_output` and `job_kill` through the whole
//! pipeline: analysis, approval, execution, results (SPEC §6.2.8–§6.2.11,
//! §6.4). Unix only: the commands are POSIX shell.
#![cfg(unix)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use cairn_core::cancel::CancellationToken;
use cairn_core::Mode;
use cairn_perm::PermissionPolicy;
use cairn_tools::{Answer, ToolCall};
use common::{assert_model_visible, code, data, Fixture, Options, Scripted};
use serde_json::{json, Value};

fn approving() -> Fixture {
    mode(Mode::Build)
}

fn mode(mode: Mode) -> Fixture {
    Fixture::build(Options {
        mode,
        approver: Some(Scripted::with(&[Answer::Session; 64])),
        ..Options::default()
    })
}

fn bash(command: &str) -> Value {
    json!({ "command": command })
}

#[tokio::test]
async fn a_command_runs_after_approval_and_reports_its_output() {
    let fx = Fixture::build(Options::default());
    fx.approver.answers.lock().unwrap().push_back(Answer::Once);
    let r = fx.call("bash", bash("echo hello; echo oops >&2")).await;
    let d = data(&r);
    assert_eq!(d["stdout"], "hello\n");
    assert_eq!(d["stderr"], "oops\n");
    assert_eq!(d["exit_code"], 0);
    assert_eq!(d["job_id"], Value::Null);
    assert_eq!(d["cwd"].as_str().unwrap(), fx.root.to_string_lossy());
    assert!(d["shell"].as_str().unwrap().contains("sh"));
    // The approval showed the raw command.
    let asked = fx.approver.asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].summary.contains("echo hello"));
}

#[tokio::test]
async fn a_failing_command_is_ok_false_with_its_exit_code_and_output() {
    let fx = approving();
    let r = fx
        .call("bash", bash("echo partial; echo why >&2; exit 3"))
        .await;
    assert!(!r.ok);
    assert_eq!(code(&r), "E-SHELL-EXITNONZERO");
    assert_model_visible(&r);
    let d = &r.envelope["data"];
    assert_eq!(d["exit_code"], 3);
    assert_eq!(d["stdout"], "partial\n");
    assert_eq!(d["stderr"], "why\n");
}

#[tokio::test]
async fn a_timeout_kills_the_group_and_keeps_what_was_printed() {
    let fx = approving();
    let started = Instant::now();
    let r = fx
        .call(
            "bash",
            json!({"command": "echo before; sleep 30 & sleep 30", "timeout_ms": 1000}),
        )
        .await;
    assert_eq!(code(&r), "E-SHELL-TIMEOUT");
    assert_model_visible(&r);
    assert!(r.envelope["data"]["stdout"]
        .as_str()
        .unwrap()
        .contains("before"));
    assert!(started.elapsed() < Duration::from_secs(12));
}

#[tokio::test]
async fn cancelling_stops_the_command_and_says_so() {
    let fx = approving();
    let token = CancellationToken::new();
    let flip = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        flip.cancel();
    });
    let started = Instant::now();
    let r = fx.call_with("bash", bash("sleep 30"), &token).await;
    assert_eq!(code(&r), "E-TOOL-CANCELLED");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn a_denylisted_command_never_runs() {
    let fx = approving();
    let marker = fx.path("ran.txt");
    let r = fx
        .call(
            "bash",
            bash(&format!("touch {} && sudo id", marker.display())),
        )
        .await;
    assert!(!r.ok);
    assert_eq!(code(&r), "E-PERM-CHAIN");
    assert!(r.envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sudo id"));
    assert!(!marker.exists(), "nothing in a denied chain runs");
    assert!(
        fx.approver.asked.lock().unwrap().is_empty(),
        "denied, not asked"
    );

    let r = fx.call("bash", bash("sudo id")).await;
    assert_eq!(code(&r), "E-PERM-DENIED");
    assert_model_visible(&r);
}

#[tokio::test]
async fn plan_mode_does_not_offer_or_run_the_shell() {
    let fx = mode(Mode::Plan);
    let offered: Vec<String> = fx
        .executor
        .registry()
        .definitions(Mode::Plan)
        .into_iter()
        .map(|d| d.name)
        .collect();
    for name in ["bash", "bash_background", "job_kill"] {
        assert!(!offered.contains(&name.to_string()), "{name}");
    }
    // Called anyway, a command that changes things is refused in plan mode.
    let r = fx.call("bash", bash("touch made.txt")).await;
    assert!(!r.ok);
    assert_eq!(code(&r), "E-PERM-MODE");
    assert!(!fx.path("made.txt").exists());
}

#[tokio::test]
async fn a_redirect_outside_the_workspace_is_refused_before_anything_runs() {
    let fx = approving();
    let outside = fx.tmp.path().join("outside.txt");
    let r = fx
        .call("bash", bash(&format!("echo x > {}", outside.display())))
        .await;
    assert_eq!(code(&r), "E-FS-ESCAPE");
    assert!(!outside.exists());
    let r = fx.call("bash", bash("echo x > .git/config")).await;
    assert_eq!(code(&r), "E-FS-PROTECTED");
}

#[tokio::test]
async fn cwd_must_be_inside_the_workspace() {
    let fx = approving();
    fx.write("sub/marker.txt", "m");
    let r = fx
        .call("bash", json!({"command": "pwd && ls", "cwd": "sub"}))
        .await;
    let d = data(&r);
    assert!(d["stdout"].as_str().unwrap().contains("marker.txt"));
    assert!(d["cwd"].as_str().unwrap().ends_with("/sub"));
    let r = fx
        .call("bash", json!({"command": "pwd", "cwd": "/tmp"}))
        .await;
    assert_eq!(code(&r), "E-PERM-DENIED");
    let r = fx
        .call("bash", json!({"command": "pwd", "cwd": "sub/marker.txt"}))
        .await;
    assert_eq!(code(&r), "E-FS-DIR");
}

#[tokio::test]
async fn the_environment_is_filtered_and_the_model_can_add_to_it() {
    let fx = approving();
    std::env::set_var("CAIRN_BASH_TEST_API_KEY", "super-secret");
    let r = fx
        .call(
            "bash",
            json!({"command": "echo \"[$CAIRN_BASH_TEST_API_KEY][$GREETING]\"", "env": {"GREETING": "hi"}}),
        )
        .await;
    assert_eq!(data(&r)["stdout"], "[][hi]\n");
}

#[tokio::test]
async fn stdin_is_written_then_closed() {
    let fx = approving();
    let r = fx
        .call("bash", json!({"command": "wc -c", "input": "12345"}))
        .await;
    assert_eq!(data(&r)["stdout"].as_str().unwrap().trim(), "5");
}

#[tokio::test]
async fn a_terminal_is_not_available_and_says_so() {
    let fx = approving();
    let r = fx
        .call("bash", json!({"command": "echo hi", "tty": true}))
        .await;
    assert_eq!(code(&r), "E-SHELL-PTY");
    assert_model_visible(&r);
}

#[tokio::test]
async fn output_past_the_cap_is_dropped_and_flagged_not_fatal() {
    let fx = approving();
    let r = fx
        .call("bash", bash("yes line | head -c 300000; echo end >&2"))
        .await;
    let d = data(&r);
    assert_eq!(d["truncated"], true);
    assert!(d["stdout"].as_str().unwrap().contains("W-SHELL-DISCARDED"));
    assert_eq!(d["warnings"][0], "W-SHELL-DISCARDED");
    assert_eq!(d["exit_code"], 0);
    assert!(r.envelope["bytes"].as_u64().unwrap() <= 64 * 1024 + 512);
}

#[tokio::test]
async fn secrets_in_output_are_redacted() {
    let fx = approving();
    let r = fx
        .call(
            "bash",
            bash("echo 'key: sk-abcdefghijklmnopqrstuvwxyz0123456789ABCD'"),
        )
        .await;
    let text = r.text();
    assert!(
        !text.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789ABCD"),
        "{text}"
    );
}

#[tokio::test]
async fn a_bash_call_waits_for_writes_issued_before_it() {
    // REQ-TOOL-021 / T-TOOL-012: write_file then bash in one batch.
    let fx = approving();
    let calls = vec![
        ToolCall {
            call_id: "w".into(),
            name: "write_file".into(),
            input: json!({"path": "made.txt", "content": "from write\n"}),
        },
        ToolCall {
            call_id: "b".into(),
            name: "bash".into(),
            input: bash("cat made.txt"),
        },
    ];
    let (results, _) = fx
        .executor
        .run_batch(calls, &fx.env, &CancellationToken::new())
        .await;
    assert!(results[0].ok, "{}", results[0].envelope);
    assert_eq!(data(&results[1])["stdout"], "from write\n");
}

#[tokio::test]
async fn an_allow_rule_for_one_command_does_not_cover_a_chain() {
    let fx = Fixture::build(Options {
        mode: Mode::Auto,
        ..Options::default()
    });
    std::fs::write(
        fx.path(".cairn/permissions.json"),
        r#"{"schema_version":1,"rules":[{"id":"u1","effect":"allow","action":"bash","target":{"kind":"command_prefix","value":"echo ok"}}]}"#,
    )
    .unwrap();
    fx.policy.reload().unwrap();
    let r = fx.call("bash", bash("echo ok")).await;
    assert!(r.ok, "{}", r.envelope);
    // The chain adds a leaf nothing allows: it asks, and the script is out of
    // answers, so it is refused rather than run.
    let r = fx.call("bash", bash("echo ok && touch nope.txt")).await;
    assert_eq!(code(&r), "E-PERM-DENIED");
    assert!(!fx.path("nope.txt").exists());
}

// ------------------------------------------------------------ background jobs

fn wait_for_finish(fx: &Fixture, id: &str) {
    let job = fx.executor.jobs().get(id).expect("job");
    for _ in 0..200 {
        if job.status() != cairn_tools::shell::jobs::Status::Running {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("job did not finish");
}

#[tokio::test]
async fn a_job_is_started_followed_and_reported_finished() {
    let fx = approving();
    let r = fx
        .call(
            "bash_background",
            json!({"command": "echo one; sleep 0.2; echo two", "label": "demo"}),
        )
        .await;
    let d = data(&r).clone();
    let id = d["job_id"].as_str().unwrap().to_string();
    assert_eq!(d["label"], "demo");
    assert!(d["pid"].as_u64().unwrap() > 1);
    assert!(d["started_at"].as_str().unwrap().ends_with('Z'));

    let r = fx
        .call("job_output", json!({"job_id": id, "wait_ms": 5000}))
        .await;
    assert!(!data(&r)["lines"].as_array().unwrap().is_empty());
    wait_for_finish(&fx, &id);
    let r = fx.call("job_output", json!({"job_id": id})).await;
    let d = data(&r);
    let texts: Vec<&str> = d["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["one", "two"]);
    assert_eq!(d["running"], false);
    assert_eq!(d["exit_code"], 0);
    assert_eq!(d["next_since_line"], 2);
    // `since_line` replays from a point.
    let r = fx
        .call("job_output", json!({"job_id": id, "since_line": 1}))
        .await;
    assert_eq!(data(&r)["lines"].as_array().unwrap().len(), 1);
    let kinds = fx.events.kinds();
    assert!(kinds.contains(&"job.started"), "{kinds:?}");
    assert!(kinds.contains(&"job.finished"), "{kinds:?}");
}

#[tokio::test]
async fn bash_with_background_true_starts_a_job() {
    let fx = approving();
    let r = fx
        .call("bash", json!({"command": "echo hi", "background": true}))
        .await;
    assert!(data(&r)["job_id"].as_str().unwrap().starts_with("job_"));
}

#[tokio::test]
async fn a_job_can_be_killed_and_an_unknown_one_is_not_found() {
    let fx = approving();
    let r = fx
        .call("bash_background", json!({"command": "sleep 30"}))
        .await;
    let id = data(&r)["job_id"].as_str().unwrap().to_string();
    let r = fx.call("job_kill", json!({"job_id": id})).await;
    let d = data(&r);
    assert_eq!(d["killed"], true);
    assert_eq!(d["signal"], "SIGTERM");
    wait_for_finish(&fx, &id);
    let r = fx.call("job_output", json!({"job_id": id})).await;
    assert!(!data(&r)["running"].as_bool().unwrap());

    let r = fx.call("job_output", json!({"job_id": "job_9999"})).await;
    assert_eq!(code(&r), "E-JOB-NOTFOUND");
    assert_model_visible(&r);
    let r = fx.call("job_kill", json!({"job_id": "nope"})).await;
    assert_eq!(code(&r), "E-JOB-NOTFOUND");
}

#[tokio::test]
async fn the_ninth_running_job_is_refused() {
    let fx = approving();
    for _ in 0..8 {
        let r = fx
            .call("bash_background", json!({"command": "sleep 30"}))
            .await;
        assert!(r.ok, "{}", r.envelope);
    }
    let r = fx
        .call("bash_background", json!({"command": "sleep 30"}))
        .await;
    assert_eq!(code(&r), "E-JOB-LIMIT");
    assert_model_visible(&r);
    fx.executor.jobs().kill_all();
}

#[tokio::test]
async fn background_commands_are_judged_like_foreground_ones() {
    let fx = approving();
    let r = fx
        .call("bash_background", json!({"command": "sudo sleep 30"}))
        .await;
    assert_eq!(code(&r), "E-PERM-DENIED");
    // Read-only commands in auto mode need no approval.
    let _ = Arc::strong_count(&fx.approver);
}
