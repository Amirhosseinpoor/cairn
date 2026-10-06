//! `write_file`, `edit_file` and `multi_edit` through the whole pipeline.

mod common;

use std::sync::Arc;
use std::time::Duration;

use cairn_core::cancel::CancellationToken;
use cairn_core::Mode;
use cairn_tools::{Answer, SyntaxCheck, SyntaxProblem, SyntaxVerdict, ToolCall};
use common::{assert_model_visible, code, data, Fixture, Options, Scripted};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// A fixture where every write is pre-approved, so the tests are about the
/// tools and not the prompt.
fn fixture() -> Fixture {
    with_syntax(None)
}

fn with_syntax(syntax: Option<Arc<dyn SyntaxCheck>>) -> Fixture {
    let approver = Scripted::with(&[Answer::Session; 64]);
    Fixture::build(Options {
        approver: Some(approver),
        syntax,
        ..Options::default()
    })
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn disk(fx: &Fixture, rel: &str) -> Vec<u8> {
    std::fs::read(fx.path(rel)).expect("file")
}

/// A stand-in validator: the text `BROKEN` is a syntax error on the line it
/// appears on; `SLOW` takes longer than the budget.
struct Fake;

impl SyntaxCheck for Fake {
    fn check(&self, _path: &str, _before: &str, after: &str) -> SyntaxVerdict {
        if after.contains("SLOW") {
            std::thread::sleep(Duration::from_millis(1500));
        }
        match after.lines().position(|l| l.contains("BROKEN")) {
            Some(index) => SyntaxVerdict::Invalid(SyntaxProblem {
                line: index + 1,
                column: 5,
                expected: Some("`;`".into()),
                found: Some("`BROKEN`".into()),
                snippet: after.lines().nth(index).unwrap_or("").to_string(),
            }),
            None if after.contains("PARSES") => SyntaxVerdict::Valid,
            None => SyntaxVerdict::Unchecked,
        }
    }
}

fn fake() -> Arc<dyn SyntaxCheck> {
    Arc::new(Fake)
}

// ----------------------------------------------------------- write_file

#[tokio::test]
async fn write_file_creates_a_file_and_its_directories() {
    let fx = fixture();
    let r = fx
        .call(
            "write_file",
            json!({"path": "src/new/mod.rs", "content": "fn a() {}\nfn b() {}\n"}),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["created"], true);
    assert_eq!(
        (d["lines_added"].as_u64(), d["lines_removed"].as_u64()),
        (Some(2), Some(0))
    );
    assert_eq!(d["bytes_written"], 20);
    assert_eq!(disk(&fx, "src/new/mod.rs"), b"fn a() {}\nfn b() {}\n");
    assert_eq!(d["sha256"], sha(b"fn a() {}\nfn b() {}\n").as_str());
    assert!(
        r.paths_written.iter().any(|p| p.ends_with("mod.rs")),
        "the change is reported for checkpoints"
    );
}

#[tokio::test]
async fn create_dirs_false_refuses_a_missing_parent() {
    let fx = fixture();
    let r = fx
        .call(
            "write_file",
            json!({"path": "nodir/x.txt", "content": "x", "create_dirs": false}),
        )
        .await;
    assert_eq!(code(&r), "E-FS-NOPARENT");
    assert_model_visible(&r);
    assert!(!fx.path("nodir").exists());
}

/// T-TOOL-004: endings and BOM are the existing file's, whatever the model
/// typed.
#[tokio::test]
async fn t_tool_004_an_overwrite_keeps_the_files_line_endings_and_bom() {
    let fx = fixture();
    fx.write("crlf.txt", "one\r\ntwo\r\n");
    fx.write("lf.txt", "one\ntwo\n");
    fx.write("bom.txt", b"\xEF\xBB\xBFone\r\ntwo\r\n");
    for (name, want) in [
        ("crlf.txt", &b"A\r\nB\r\n"[..]),
        ("lf.txt", b"A\nB\n"),
        ("bom.txt", b"\xEF\xBB\xBFA\r\nB\r\n"),
    ] {
        // Read first, so the write is not stale.
        fx.call("read_file", json!({"path": name})).await;
        let r = fx
            .call("write_file", json!({"path": name, "content": "A\nB\n"}))
            .await;
        assert!(r.ok, "{name}: {}", r.envelope);
        assert_eq!(disk(&fx, name), want, "{name}");
    }
    // A model that sends CRLF itself is normalised, not doubled.
    fx.call("read_file", json!({"path": "lf.txt"})).await;
    fx.call(
        "write_file",
        json!({"path": "lf.txt", "content": "x\r\ny\r\n"}),
    )
    .await;
    assert_eq!(disk(&fx, "lf.txt"), b"x\ny\n");
    // A brand-new file follows `line_endings` (LF in this fixture).
    fx.call(
        "write_file",
        json!({"path": "fresh.txt", "content": "a\nb\n"}),
    )
    .await;
    assert_eq!(disk(&fx, "fresh.txt"), b"a\nb\n");
}

#[cfg(unix)]
#[tokio::test]
async fn permissions_survive_an_overwrite() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    fx.write("run.sh", "#!/bin/sh\necho old\n");
    std::fs::set_permissions(fx.path("run.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    fx.call("read_file", json!({"path": "run.sh"})).await;
    assert!(
        fx.call(
            "write_file",
            json!({"path": "run.sh", "content": "#!/bin/sh\necho new\n"})
        )
        .await
        .ok
    );
    let mode = std::fs::metadata(fx.path("run.sh"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    // …and no temp file is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&fx.root)
        .expect("dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn a_write_over_a_file_changed_since_it_was_read_is_stale() {
    let fx = fixture();
    fx.write("a.txt", "original\n");
    fx.call("read_file", json!({"path": "a.txt"})).await;
    std::fs::write(fx.path("a.txt"), "someone else's change\n").expect("external edit");
    let r = fx
        .call("write_file", json!({"path": "a.txt", "content": "mine\n"}))
        .await;
    assert_eq!(code(&r), "E-FS-STALE");
    assert!(r.envelope["error"]["recovery"]
        .as_str()
        .expect("r")
        .contains("Re-read"));
    assert_eq!(
        disk(&fx, "a.txt"),
        b"someone else's change\n",
        "nothing was overwritten"
    );
}

#[tokio::test]
async fn expected_sha256_is_checked_both_ways() {
    let fx = fixture();
    fx.write("a.txt", "v1\n");
    let wrong = fx
        .call(
            "write_file",
            json!({"path": "a.txt", "content": "v2\n", "expected_sha256": sha(b"other")}),
        )
        .await;
    assert_eq!(code(&wrong), "E-FS-STALE");
    let right = fx
        .call(
            "write_file",
            json!({"path": "a.txt", "content": "v2\n", "expected_sha256": sha(b"v1\n")}),
        )
        .await;
    assert!(right.ok, "{}", right.envelope);
    // A hash for a file that is not there cannot match.
    let none = fx
        .call(
            "write_file",
            json!({"path": "ghost.txt", "content": "x", "expected_sha256": sha(b"v1\n")}),
        )
        .await;
    assert_eq!(code(&none), "E-FS-STALE");
    assert!(!fx.path("ghost.txt").exists());
}

#[tokio::test]
async fn consecutive_writes_need_no_re_read() {
    let fx = fixture();
    assert!(
        fx.call("write_file", json!({"path": "a.txt", "content": "one\n"}))
            .await
            .ok
    );
    assert!(
        fx.call("write_file", json!({"path": "a.txt", "content": "two\n"}))
            .await
            .ok,
        "the tool's own write is what it last saw"
    );
    assert_eq!(disk(&fx, "a.txt"), b"two\n");
}

#[tokio::test]
async fn write_file_failures_are_model_visible() {
    let fx = fixture();
    fx.write("dir/x", "x");
    fx.write("blob.bin", b"\0\0\0binary");
    for (input, want) in [
        (json!({"path": "dir", "content": "x"}), "E-FS-DIR"),
        (json!({"path": "blob.bin", "content": "x"}), "E-FS-BINARY"),
        (
            json!({"path": ".env", "content": "TOKEN=1"}),
            "E-FS-PROTECTED",
        ),
        (
            json!({"path": ".git/config", "content": "x"}),
            "E-FS-PROTECTED",
        ),
        (
            json!({"path": "../outside.txt", "content": "x"}),
            "E-FS-ESCAPE",
        ),
        (
            json!({"path": "a.txt", "content": "x".repeat(400_001)}),
            "E-TOOL-TOOBIG",
        ),
        (json!({"path": "a.txt"}), "E-TOOL-BADSCHEMA"),
    ] {
        let r = fx.call("write_file", input.clone()).await;
        assert_eq!(
            code(&r),
            want,
            "{}",
            input.to_string().chars().take(80).collect::<String>()
        );
        assert_model_visible(&r);
    }
}

#[tokio::test]
async fn write_file_reports_but_does_not_refuse_a_file_that_does_not_parse() {
    let fx = with_syntax(Some(fake()));
    let r = fx
        .call(
            "write_file",
            json!({"path": "fixture.json", "content": "BROKEN on purpose\n"}),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["syntax_ok"], false);
    assert!(d["syntax_warning"]
        .as_str()
        .expect("w")
        .contains("does not parse"));
    assert_eq!(
        disk(&fx, "fixture.json"),
        b"BROKEN on purpose\n",
        "a deliberate fixture is still written"
    );
}

// ------------------------------------------------- modes and approvals

#[tokio::test]
async fn write_tools_are_refused_in_plan_mode_and_ask_in_build() {
    let plan = Fixture::new(Mode::Plan);
    for (tool, input) in [
        ("write_file", json!({"path": "a.txt", "content": "x"})),
        (
            "edit_file",
            json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        ),
        (
            "multi_edit",
            json!({"path": "a.txt", "edits": [{"old_string": "x", "new_string": "y"}]}),
        ),
    ] {
        let r = plan.call(tool, input).await;
        assert_eq!(code(&r), "E-PERM-MODE", "{tool}");
        assert!(!plan
            .executor
            .registry()
            .definitions(Mode::Plan)
            .iter()
            .any(|d| d.name == tool));
    }
    assert!(!plan.path("a.txt").exists());

    // Build asks; the default headless approver declines.
    let build = Fixture::new(Mode::Build);
    let r = build
        .call("write_file", json!({"path": "a.txt", "content": "x"}))
        .await;
    assert_eq!(code(&r), "E-PERM-DENIED");
    assert!(!build.path("a.txt").exists());
    // The approval carries the model's exact input (§9.7 mitigation 7).
    let approver = Scripted::with(&[Answer::Once]);
    let asked = Fixture::build(Options {
        approver: Some(Arc::clone(&approver) as Arc<dyn cairn_tools::Approver>),
        ..Options::default()
    });
    assert!(asked
        .call(
            "edit_file",
            json!({"path": "missing.txt", "old_string": "alpha", "new_string": "omega"})
        )
        .await
        .envelope["error"]["code"]
        .is_string());
    let request = approver.asked.lock().expect("asked")[0].clone();
    assert_eq!(request.summary, "missing.txt");
    assert_eq!(request.detail["input"]["old_string"], "alpha");
    assert_eq!(request.detail["input"]["new_string"], "omega");
}

// ------------------------------------------------------------ edit_file

#[tokio::test]
async fn edit_file_replaces_and_reports_everything_the_spec_lists() {
    let fx = fixture();
    fx.write("src/lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
    let r = fx.call("edit_file", json!({"path": "src/lib.rs", "old_string": "fn two() {}", "new_string": "fn two(x: u32) {}"})).await;
    let d = data(&r);
    assert_eq!(d["occurrences"], 1);
    assert_eq!(d["replaced"], 1);
    assert_eq!(
        (d["start_line"].as_u64(), d["end_line"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(d["fuzzy_used"], false);
    assert_eq!(d["fuzzy_score"], Value::Null);
    assert_eq!(
        d["sha256_before"],
        sha(b"fn one() {}\nfn two() {}\nfn three() {}\n").as_str()
    );
    assert_eq!(d["sha256_after"], sha(&disk(&fx, "src/lib.rs")).as_str());
    assert_eq!(d["syntax_ok"], Value::Null, "no validator configured");
    assert_eq!(
        disk(&fx, "src/lib.rs"),
        b"fn one() {}\nfn two(x: u32) {}\nfn three() {}\n"
    );
}

/// T-EDIT-001, 002, 004 through the tool, with the messages §6.3.1 fixes.
#[tokio::test]
async fn t_edit_001_002_004_the_error_messages() {
    let fx = fixture();
    let mut body = String::new();
    for i in 1..=600 {
        body.push_str(if [42, 118, 501].contains(&i) {
            "dup();\n"
        } else {
            "other();\n"
        });
    }
    fx.write("big.rs", &body);
    fx.write("small.txt", "alpha\nbeta\n");

    let none = fx
        .call(
            "edit_file",
            json!({"path": "small.txt", "old_string": "zzz_absent", "new_string": "x"}),
        )
        .await;
    assert_eq!(code(&none), "E-EDIT-NOMATCH");
    assert_model_visible(&none);

    let many = fx
        .call(
            "edit_file",
            json!({"path": "big.rs", "old_string": "dup();", "new_string": "x();"}),
        )
        .await;
    assert_eq!(code(&many), "E-EDIT-AMBIGUOUS");
    assert_eq!(
        many.envelope["error"]["message"],
        "Pattern matched 3 times (expected 1) at lines 42, 118, 501. Add surrounding context or set replace_all."
    );
    assert_eq!(
        many.envelope["error"]["recovery"],
        "Include 1-3 lines of unique surrounding context, or pass expect_occurrences=N, or replace_all:true."
    );

    let same = fx
        .call(
            "edit_file",
            json!({"path": "small.txt", "old_string": "alpha", "new_string": "alpha"}),
        )
        .await;
    assert_eq!(code(&same), "E-EDIT-NOCHANGE");
    assert_eq!(disk(&fx, "small.txt"), b"alpha\nbeta\n", "nothing changed");
}

#[tokio::test]
async fn replace_all_and_expect_occurrences_work_through_the_tool() {
    let fx = fixture();
    fx.write("a.txt", "foo\nfoo\nfoo\n");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
        )
        .await;
    assert_eq!(data(&r)["occurrences"], 3);
    assert_eq!(disk(&fx, "a.txt"), b"bar\nbar\nbar\n");
    fx.write("b.txt", "x\ny\n");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "b.txt", "old_string": "x", "new_string": "z", "expect_occurrences": 2}),
        )
        .await;
    assert_eq!(code(&r), "E-EDIT-NOMATCH", "T-EDIT-019");
}

#[tokio::test]
async fn fuzzy_matching_is_reported_and_can_be_switched_off() {
    let fx = fixture();
    fx.write("a.txt", "head\n\tindented();\ntail\n");
    let off = fx.call("edit_file", json!({"path": "a.txt", "old_string": "    indented();", "new_string": "x();", "fuzzy": "off"})).await;
    assert_eq!(code(&off), "E-EDIT-NOMATCH", "T-EDIT-017");
    let on = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "    indented();", "new_string": "x();"}),
        )
        .await;
    let d = data(&on);
    assert_eq!(d["fuzzy_used"], true);
    assert!(d["fuzzy_score"].as_f64().expect("score") >= 0.92);
    assert_eq!(disk(&fx, "a.txt"), b"head\nx();\ntail\n");
}

#[tokio::test]
async fn a_modest_fuzzy_score_carries_w_edit_fuzzy_with_the_matched_text() {
    let fx = fixture();
    fx.write("a.rs", "fn run() {\n    let a = load_configuration();\n    let sum = compute(first, second);\n    log(a);\n}\n");
    let r = fx.call("edit_file", json!({
        "path": "a.rs",
        "old_string": "fn run() {\n    let a = load_configuration();\n    let total = compute(first, second);\n    log(a);\n}",
        "new_string": "fn run() {}"
    })).await;
    let warning = &data(&r)["warnings"][0];
    assert_eq!(warning["code"], "W-EDIT-FUZZY");
    assert!(
        warning["message"]
            .as_str()
            .expect("m")
            .starts_with("fuzzy match ("),
        "{warning}"
    );
    assert!(warning["matched_text"]
        .as_str()
        .expect("t")
        .contains("let sum"));
}

/// T-EDIT-020 and T-EDIT-025 through the tool.
#[tokio::test]
async fn t_edit_020_025_crlf_and_bom_survive_an_edit() {
    let fx = fixture();
    fx.write("w.txt", b"\xEF\xBB\xBFfirst\r\nsecond\r\nthird\r\n");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "w.txt", "old_string": "first\nsecond", "new_string": "ONE\nTWO"}),
        )
        .await;
    assert!(r.ok, "{}", r.envelope);
    assert_eq!(disk(&fx, "w.txt"), b"\xEF\xBB\xBFONE\r\nTWO\r\nthird\r\n");
}

/// T-EDIT-003: the file changed elsewhere; the edit still applies and says so.
#[tokio::test]
async fn t_edit_003_a_change_elsewhere_is_stale_but_safe() {
    let fx = fixture();
    fx.write("a.txt", "top\nTARGET\nbottom\n");
    fx.call("read_file", json!({"path": "a.txt"})).await;
    std::fs::write(fx.path("a.txt"), "top changed\nTARGET\nbottom\n").expect("external edit");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "TARGET", "new_string": "DONE"}),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["stale_but_safe"], true);
    assert_eq!(
        disk(&fx, "a.txt"),
        b"top changed\nDONE\nbottom\n",
        "both changes are present"
    );
}

/// §6.3.3 stage 2: the span itself changed — `E-EDIT-STALE`, with the diff.
#[tokio::test]
async fn t_tool_stale_span_changed_is_e_edit_stale_with_a_diff() {
    let fx = fixture();
    fx.write("a.txt", "keep\nTARGET\nkeep\n");
    fx.call("read_file", json!({"path": "a.txt"})).await;
    std::fs::write(fx.path("a.txt"), "keep\nsomething else entirely\nkeep\n")
        .expect("external edit");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "TARGET", "new_string": "DONE"}),
        )
        .await;
    assert_eq!(code(&r), "E-EDIT-STALE");
    assert_model_visible(&r);
    let message = r.envelope["error"]["message"].as_str().expect("m");
    assert!(
        message.contains("-TARGET") && message.contains("+something else entirely"),
        "{message}"
    );
    assert!(r.envelope["error"]["recovery"]
        .as_str()
        .expect("r")
        .contains("Re-read it, then retry"));
    assert_eq!(disk(&fx, "a.txt"), b"keep\nsomething else entirely\nkeep\n");
}

#[tokio::test]
async fn edit_file_failures() {
    let fx = fixture();
    fx.write("dir/x", "x");
    fx.write("blob.bin", b"\0\0\0");
    for (input, want) in [
        (
            json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
            "E-FS-NOTFOUND",
        ),
        (
            json!({"path": "dir", "old_string": "a", "new_string": "b"}),
            "E-FS-DIR",
        ),
        (
            json!({"path": "blob.bin", "old_string": "a", "new_string": "b"}),
            "E-FS-BINARY",
        ),
        (
            json!({"path": ".env", "old_string": "a", "new_string": "b"}),
            "E-FS-PROTECTED",
        ),
        (
            json!({"path": "x", "old_string": "", "new_string": "b"}),
            "E-TOOL-BADSCHEMA",
        ),
        (
            json!({"path": "x", "old_string": "a", "new_string": "b", "fuzzy": "wild"}),
            "E-TOOL-BADSCHEMA",
        ),
        (
            json!({"path": "x", "old_string": "a".repeat(20_001), "new_string": "b"}),
            "E-TOOL-TOOBIG",
        ),
    ] {
        let r = fx.call("edit_file", input.clone()).await;
        assert_eq!(
            code(&r),
            want,
            "{}",
            input.to_string().chars().take(80).collect::<String>()
        );
        assert_model_visible(&r);
    }
}

// ------------------------------------------------- syntax validation

/// T-EDIT-021: a breaking edit is refused and the file is byte-identical.
#[tokio::test]
async fn t_edit_021_a_syntax_error_rolls_back_byte_for_byte() {
    let fx = with_syntax(Some(fake()));
    let original = "fn a() {}\nfn b() {}\n";
    fx.write("a.rs", original);
    let r = fx
        .call(
            "edit_file",
            json!({"path": "a.rs", "old_string": "fn b() {}", "new_string": "fn b() { BROKEN"}),
        )
        .await;
    assert_eq!(code(&r), "E-EDIT-SYNTAX");
    assert_model_visible(&r);
    assert_eq!(r.envelope["data"]["line"], 2);
    assert_eq!(r.envelope["data"]["column"], 5);
    assert!(r.envelope["error"]["message"]
        .as_str()
        .expect("m")
        .contains("fn b() { BROKEN"));
    assert!(r.envelope["error"]["recovery"]
        .as_str()
        .expect("r")
        .contains("left unchanged"));
    assert_eq!(
        sha(&disk(&fx, "a.rs")),
        sha(original.as_bytes()),
        "REQ-TOOL-014"
    );
    assert!(std::fs::read_dir(&fx.root)
        .expect("dir")
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().contains(".tmp")));
}

#[tokio::test]
async fn a_checked_edit_reports_syntax_ok_true() {
    let fx = with_syntax(Some(fake()));
    fx.write("a.txt", "x PARSES\n");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        )
        .await;
    assert_eq!(data(&r)["syntax_ok"], true);
}

/// T-EDIT-024 / REQ-TOOL-015: validation past its budget does not block.
#[tokio::test]
async fn t_edit_024_a_slow_validator_writes_unchecked_with_a_warning() {
    let fx = with_syntax(Some(fake()));
    fx.write("big.txt", "start\n");
    let started = std::time::Instant::now();
    let r = fx
        .call(
            "edit_file",
            json!({"path": "big.txt", "old_string": "start", "new_string": "SLOW start"}),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["syntax_ok"], Value::Null);
    assert_eq!(d["warnings"][0]["code"], "W-EDIT-TIMEOUT");
    assert_eq!(disk(&fx, "big.txt"), b"SLOW start\n");
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "the pipeline did not wait out the validator"
    );
}

// ----------------------------------------------------------- multi_edit

#[tokio::test]
async fn multi_edit_applies_all_edits_in_order() {
    let fx = fixture();
    fx.write("a.txt", "one\ntwo\nthree\n");
    let r = fx
        .call(
            "multi_edit",
            json!({"path": "a.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "three", "new_string": "3"},
            ]}),
        )
        .await;
    let d = data(&r);
    assert_eq!(d["applied"], 2);
    assert_eq!(d["failed_index"], Value::Null);
    assert_eq!(disk(&fx, "a.txt"), b"1\ntwo\n3\n");
}

/// T-EDIT-005.
#[tokio::test]
async fn t_edit_005_overlapping_edits_conflict_and_nothing_is_written() {
    let fx = fixture();
    fx.write("a.txt", "alpha beta gamma\n");
    let r = fx
        .call(
            "multi_edit",
            json!({"path": "a.txt", "edits": [
                {"old_string": "alpha beta", "new_string": "A"},
                {"old_string": "beta gamma", "new_string": "B"},
            ]}),
        )
        .await;
    assert_eq!(code(&r), "E-EDIT-CONFLICT");
    assert_model_visible(&r);
    assert_eq!(disk(&fx, "a.txt"), b"alpha beta gamma\n");
}

/// T-EDIT-006.
#[tokio::test]
async fn t_edit_006_the_failing_edit_is_named_and_the_file_is_untouched() {
    let fx = fixture();
    fx.write("a.txt", "one\ntwo\nthree\n");
    let r = fx
        .call(
            "multi_edit",
            json!({"path": "a.txt", "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "missing", "new_string": "x"},
                {"old_string": "three", "new_string": "3"},
            ]}),
        )
        .await;
    assert_eq!(code(&r), "E-EDIT-PARTIAL");
    assert_eq!(r.envelope["data"]["failed_index"], 1);
    assert_eq!(r.envelope["data"]["first_failed_index"], 1);
    assert_model_visible(&r);
    assert_eq!(disk(&fx, "a.txt"), b"one\ntwo\nthree\n");
}

/// T-EDIT-021 for `multi_edit` (REQ-TOOL-006).
#[tokio::test]
async fn multi_edit_validates_the_final_text_before_writing() {
    let fx = with_syntax(Some(fake()));
    let original = "a\nb\n";
    fx.write("a.rs", original);
    let r = fx
        .call(
            "multi_edit",
            json!({"path": "a.rs", "edits": [
                {"old_string": "a", "new_string": "A"},
                {"old_string": "b", "new_string": "BROKEN"},
            ]}),
        )
        .await;
    assert_eq!(code(&r), "E-EDIT-SYNTAX");
    assert_eq!(
        disk(&fx, "a.rs"),
        original.as_bytes(),
        "even the first, valid edit is not kept"
    );
}

// --------------------------------------------------------- concurrency

/// §6.6: writes to one path queue, so two edits to different lines of the
/// same file both land — no lost update.
#[tokio::test]
async fn concurrent_edits_to_one_file_do_not_lose_each_other() {
    let fx = fixture();
    fx.write("a.txt", "a\nb\nc\nd\n");
    let calls: Vec<ToolCall> = [("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")]
        .iter()
        .map(|(old, new)| ToolCall {
            call_id: format!("c_{old}"),
            name: "edit_file".into(),
            input: json!({"path": "a.txt", "old_string": old, "new_string": new}),
        })
        .collect();
    let (results, _) = fx
        .executor
        .run_batch(calls, &fx.env, &CancellationToken::new())
        .await;
    assert!(results.iter().all(|r| r.ok), "{results:?}");
    assert_eq!(disk(&fx, "a.txt"), b"A\nB\nC\nD\n");
}

#[tokio::test]
async fn the_pipeline_reports_the_paths_a_write_changed() {
    let fx = fixture();
    fx.write("a.txt", "x\n");
    let r = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "x", "new_string": "y"}),
        )
        .await;
    assert_eq!(r.paths_written, vec![std::path::PathBuf::from("a.txt")]);
    let failed = fx
        .call(
            "edit_file",
            json!({"path": "a.txt", "old_string": "nope", "new_string": "y"}),
        )
        .await;
    assert!(
        failed.paths_written.is_empty(),
        "a failed edit changed nothing"
    );
}

/// The registry now carries all seven tools with the §6.1 metadata.
#[tokio::test]
async fn t_tool_001_write_tool_metadata_matches_the_table() {
    let fx = fixture();
    let table = fx.executor.registry().table();
    let row = |n: &str| {
        table
            .iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("{n}"))
            .clone()
    };
    for (name, seconds, kib, idem) in [
        ("write_file", 20, 8, "Retryable"),
        ("edit_file", 20, 8, "NonIdempotent"),
        ("multi_edit", 30, 16, "NonIdempotent"),
    ] {
        let r = row(name);
        assert_eq!(
            (r.class, r.side_effect, r.idempotency, r.serial),
            ("Write", "Write", idem, false),
            "{name}"
        );
        assert_eq!(r.timeout_ms, seconds * 1000, "{name}");
        assert_eq!(r.max_output_bytes, kib * 1024, "{name}");
    }
}
