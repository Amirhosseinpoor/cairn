//! `git_status`, `git_diff` and `git_commit` (T-GIT-001..009, T-TOOL-006).
#![cfg(unix)]

mod common;

use std::fmt::Write as _;
use std::process::Command;
use std::sync::{Arc, Mutex};

use cairn_core::Mode;
use cairn_tools::Answer;
use common::{assert_model_visible, code, data, Fixture, Options, Scripted};
use serde_json::{json, Value};

fn git(fx: &Fixture, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(&fx.root)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[derive(Default)]
struct Snapshots(Mutex<Vec<String>>);

impl cairn_git::WriteObserver for Snapshots {
    fn before_write(&self, _abs: &std::path::Path) {}
    fn after_write(&self, _abs: &std::path::Path, _sha256: &str) {}
    fn checkpoint(&self, label: &str) {
        self.0.lock().unwrap().push(label.to_string());
    }
}

fn repo_with(mode: Mode, observer: Option<Arc<dyn cairn_git::WriteObserver>>) -> Fixture {
    let fx = Fixture::build(Options {
        mode,
        approver: Some(Scripted::with(&[Answer::Session; 64])),
        observer,
        ..Options::default()
    });
    git(&fx, &["init", "-q", "-b", "main"]);
    git(&fx, &["config", "user.name", "T"]);
    git(&fx, &["config", "user.email", "t@t"]);
    fx.write("src/lib.rs", "fn one() {}\n");
    fx.write("README.md", "# demo\n");
    fx.write(".cairn/notes.md", "notes\n");
    git(&fx, &["add", "src", "README.md", ".cairn/notes.md"]);
    git(&fx, &["commit", "-qm", "init"]);
    fx
}

fn repo() -> Fixture {
    repo_with(Mode::Build, None)
}

#[tokio::test]
async fn t_git_001_status_reports_branch_and_every_kind_of_change() {
    let fx = repo();
    fx.write("src/lib.rs", "fn one() {}\nfn two() {}\n");
    fx.write("staged.rs", "x\n");
    git(&fx, &["add", "staged.rs"]);
    fx.write("new.rs", "y\n");
    let d = data(&fx.call("git_status", json!({})).await).clone();
    assert_eq!(d["branch"], "main");
    assert_eq!(d["clean"], false);
    assert_eq!(d["detached"], false);
    assert_eq!(d["rebase_in_progress"], false);
    assert_eq!(d["staged"], json!(["A staged.rs"]));
    assert_eq!(d["unstaged"], json!(["M src/lib.rs"]));
    assert_eq!(d["untracked"], json!(["new.rs"]));
    assert_eq!(d["conflicts"], json!([]));
    assert_eq!(d["ahead"], 0);

    git(&fx, &["add", "-A"]);
    git(&fx, &["commit", "-qm", "more"]);
    let d = data(&fx.call("git_status", json!({})).await).clone();
    assert_eq!(d["clean"], true);
}

#[tokio::test]
async fn t_git_001_status_follows_an_upstream_and_a_detached_head() {
    let fx = repo();
    // A local "remote" to be ahead of.
    let remote = fx.tmp.path().join("remote.git");
    Command::new("git")
        .args(["init", "-q", "--bare", remote.to_str().unwrap()])
        .output()
        .unwrap();
    git(&fx, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&fx, &["push", "-q", "-u", "origin", "main"]);
    fx.write("a.txt", "a\n");
    git(&fx, &["add", "a.txt"]);
    git(&fx, &["commit", "-qm", "ahead"]);
    let d = data(&fx.call("git_status", json!({})).await).clone();
    assert_eq!(d["upstream"], "origin/main");
    assert_eq!(d["ahead"], 1);
    assert_eq!(d["behind"], 0);
    git(&fx, &["checkout", "-q", "--detach"]);
    let d = data(&fx.call("git_status", json!({})).await).clone();
    assert_eq!(d["detached"], true);
    assert_eq!(d["branch"], Value::Null);
}

#[tokio::test]
async fn t_git_002_outside_a_repository_is_e_git_norepo() {
    let fx = Fixture::build(Options::default());
    let r = fx.call("git_status", json!({})).await;
    assert_eq!(code(&r), "E-GIT-NOREPO");
    assert_model_visible(&r);
    let r = fx.call("git_diff", json!({})).await;
    assert_eq!(code(&r), "E-GIT-NOREPO");
}

#[tokio::test]
async fn t_git_003_diff_scopes() {
    let fx = repo();
    fx.write("src/lib.rs", "fn one() {}\nfn two() {}\n");
    git(&fx, &["add", "src/lib.rs"]);
    fx.write("src/lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
    fx.write("fresh.txt", "brand new\n");

    let diff = |scope: &str| {
        let fx = &fx;
        let scope = scope.to_string();
        async move { fx.call("git_diff", json!({"scope": scope})).await }
    };
    let staged = diff("staged").await;
    let d = data(&staged);
    assert_eq!(d["insertions"], 1);
    assert!(d["diff"].as_str().unwrap().contains("+fn two() {}"));
    assert!(!d["diff"].as_str().unwrap().contains("three"));

    let working = diff("working").await;
    let d = data(&working);
    assert!(d["diff"].as_str().unwrap().contains("+fn three() {}"));
    assert!(
        d["diff"].as_str().unwrap().contains("brand new"),
        "untracked content"
    );

    let all = diff("all").await;
    let d = data(&all);
    assert_eq!(d["files_changed"], 2);
    assert!(d["diff"].as_str().unwrap().contains("+fn two() {}"));
    assert!(d["diff"].as_str().unwrap().contains("+fn three() {}"));
    assert_eq!(d["truncated"], false);

    // One commit against its parent.
    git(&fx, &["add", "-A"]);
    git(&fx, &["commit", "-qm", "second"]);
    let r = fx
        .call("git_diff", json!({"scope": "commit", "commit": "HEAD"}))
        .await;
    let d = data(&r);
    assert_eq!(d["files_changed"], 2);
    assert!(d["diff"].as_str().unwrap().starts_with("diff --git"));
    // The first commit has no parent: everything in it is an addition.
    let r = fx
        .call("git_diff", json!({"scope": "commit", "commit": "HEAD~1"}))
        .await;
    assert_eq!(data(&r)["files_changed"], 3);
}

#[tokio::test]
async fn t_git_003_a_path_narrows_the_diff_and_context_lines_apply() {
    let fx = repo();
    fx.write("src/lib.rs", "a\nb\nc\nd\ne\nf\ng\nh\n");
    git(&fx, &["add", "-A"]);
    git(&fx, &["commit", "-qm", "wide"]);
    fx.write("src/lib.rs", "a\nb\nc\nd\nE\nf\ng\nh\n");
    fx.write("README.md", "# changed\n");
    let r = fx
        .call("git_diff", json!({"path": "src", "unified_lines": 1}))
        .await;
    let d = data(&r);
    assert_eq!(d["files_changed"], 1);
    let text = d["diff"].as_str().unwrap();
    assert!(text.contains("-e\n+E"), "{text}");
    assert!(!text.contains("README"));
    assert!(!text.contains(" b\n"), "one line of context only: {text}");
}

#[tokio::test]
async fn t_git_004_diff_errors_are_specific() {
    let fx = repo();
    let r = fx.call("git_diff", json!({})).await;
    assert_eq!(code(&r), "E-GIT-NODIFF");
    assert_model_visible(&r);
    let r = fx
        .call(
            "git_diff",
            json!({"scope": "commit", "commit": "no-such-rev"}),
        )
        .await;
    assert_eq!(code(&r), "E-GIT-BADREV");
    assert_model_visible(&r);
    let r = fx.call("git_diff", json!({"scope": "commit"})).await;
    assert_eq!(code(&r), "E-GIT-BADREV");
}

#[tokio::test]
async fn t_git_004_a_large_diff_is_cut_at_a_line_and_flagged() {
    let fx = repo();
    let mut big = String::new();
    for i in 0..2000 {
        writeln!(big, "line number {i}").unwrap();
    }
    fx.write("big.txt", &big);
    let r = fx.call("git_diff", json!({"max_bytes": 2048})).await;
    let d = data(&r);
    assert_eq!(d["truncated"], true);
    let text = d["diff"].as_str().unwrap();
    assert!(text.len() <= 2048);
    assert!(text.ends_with('\n'));
    assert_eq!(d["insertions"], 2000, "counts are for the whole diff");
}

#[tokio::test]
async fn t_git_005_commit_with_paths_and_the_report() {
    let observer = Arc::new(Snapshots::default());
    let fx = repo_with(
        Mode::Build,
        Some(Arc::clone(&observer) as Arc<dyn cairn_git::WriteObserver>),
    );
    fx.write("src/lib.rs", "fn one() {}\nfn two() {}\n");
    fx.write("other.rs", "untouched\n");
    let r = fx
        .call(
            "git_commit",
            json!({"message": "feat: two\n\nbody", "paths": ["src/lib.rs"]}),
        )
        .await;
    let d = data(&r).clone();
    assert_eq!(d["message"], "feat: two");
    assert_eq!(d["files"], 1);
    assert_eq!(d["insertions"], 1);
    assert_eq!(d["deletions"], 0);
    assert_eq!(d["short_sha"].as_str().unwrap().len(), 7);
    assert!(d["sha"]
        .as_str()
        .unwrap()
        .starts_with(d["short_sha"].as_str().unwrap()));
    assert_eq!(git(&fx, &["log", "-1", "--format=%H"]).trim(), d["sha"]);
    // Only the named path went in.
    let status = git(&fx, &["status", "--porcelain"]);
    assert!(status.contains("?? other.rs"), "{status}");
    // §9.8: a checkpoint was taken before the commit.
    let labels = observer.0.lock().unwrap().clone();
    assert_eq!(labels.len(), 1);
    assert!(labels[0].starts_with("pre-commit:"), "{labels:?}");
}

#[tokio::test]
async fn t_git_005_paths_stage_deletions_and_new_files() {
    let fx = repo();
    std::fs::remove_file(fx.path("README.md")).unwrap();
    fx.write("added.rs", "new\n");
    let r = fx
        .call(
            "git_commit",
            json!({"message": "tidy", "paths": ["README.md", "added.rs"]}),
        )
        .await;
    assert_eq!(data(&r)["files"], 2);
    assert_eq!(git(&fx, &["status", "--porcelain"]).trim(), "");
}

#[tokio::test]
async fn t_git_006_nothing_staged_and_no_identity() {
    let fx = repo();
    let r = fx.call("git_commit", json!({"message": "nothing"})).await;
    assert_eq!(code(&r), "E-GIT-EMPTY");
    assert!(r.envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Nothing staged"));
    assert_model_visible(&r);

    let r = fx
        .call(
            "git_commit",
            json!({"message": "empty on purpose", "allow_empty": true}),
        )
        .await;
    assert!(r.ok, "{}", r.envelope);

    git(&fx, &["config", "user.name", ""]);
    fx.write("x.rs", "x\n");
    let r = fx
        .call("git_commit", json!({"message": "x", "paths": ["x.rs"]}))
        .await;
    assert_eq!(code(&r), "E-GIT-NOCFG");
}

#[tokio::test]
async fn t_git_007_a_failing_hook_is_reported_with_its_output() {
    use std::os::unix::fs::PermissionsExt;
    let fx = repo();
    let hook = fx.path(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'lint failed: trailing whitespace' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    fx.write("a.rs", "a\n");
    let r = fx
        .call("git_commit", json!({"message": "a", "paths": ["a.rs"]}))
        .await;
    assert_eq!(code(&r), "E-GIT-PRECOMMIT");
    let error = &r.envelope["error"];
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("trailing whitespace"));
    assert!(error["recovery"]
        .as_str()
        .unwrap()
        .contains("Fix the issue"));
}

#[tokio::test]
async fn t_git_008_unmerged_paths_block_a_commit() {
    let fx = repo();
    git(&fx, &["checkout", "-q", "-b", "side"]);
    fx.write("src/lib.rs", "fn side() {}\n");
    git(&fx, &["commit", "-qam", "side"]);
    git(&fx, &["checkout", "-q", "main"]);
    fx.write("src/lib.rs", "fn main_side() {}\n");
    git(&fx, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(&fx.root)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the merge must conflict");
    let d = data(&fx.call("git_status", json!({})).await).clone();
    assert_eq!(d["conflicts"], json!(["src/lib.rs"]));
    let r = fx.call("git_commit", json!({"message": "resolve"})).await;
    assert_eq!(code(&r), "E-GIT-CONFLICT");
    assert_model_visible(&r);
}

#[tokio::test]
async fn t_git_009_a_held_index_lock_is_e_git_lock() {
    let fx = repo();
    fx.write("a.rs", "a\n");
    std::fs::write(fx.path(".git/index.lock"), "").unwrap();
    let r = fx
        .call("git_commit", json!({"message": "a", "paths": ["a.rs"]}))
        .await;
    assert_eq!(code(&r), "E-GIT-LOCK");
    assert!(r.envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains("index.lock"));
}

#[tokio::test]
async fn t_tool_006_cairn_state_is_not_committed_unless_named() {
    let fx = repo();
    fx.write("src/lib.rs", "fn one() {}\nfn changed() {}\n");
    fx.write(".cairn/notes.md", "notes, edited\n");
    let r = fx
        .call("git_commit", json!({"message": "work", "all": true}))
        .await;
    let d = data(&r);
    assert_eq!(d["files"], 1, "only src/lib.rs");
    let status = git(&fx, &["status", "--porcelain"]);
    assert!(status.contains(" M .cairn/notes.md"), "{status}");

    // Named outright, it goes in.
    let r = fx
        .call(
            "git_commit",
            json!({"message": "state", "paths": [".cairn/notes.md"]}),
        )
        .await;
    assert_eq!(data(&r)["files"], 1);
    assert_eq!(git(&fx, &["status", "--porcelain"]).trim(), "");
}

#[tokio::test]
async fn t_tool_006_what_the_user_already_staged_cannot_smuggle_cairn_state_in() {
    let fx = repo();
    fx.write(".cairn/notes.md", "staged by hand\n");
    git(&fx, &["add", ".cairn/notes.md"]);
    fx.write("src/lib.rs", "fn one() {}\nfn x() {}\n");
    let r = fx
        .call(
            "git_commit",
            json!({"message": "m", "paths": ["src/lib.rs"]}),
        )
        .await;
    assert_eq!(code(&r), "E-GIT-CMD");
    assert!(r.envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains(".cairn/notes.md"));
}

#[tokio::test]
async fn cairnignored_files_are_never_committed() {
    let fx = repo();
    fx.write(".cairnignore", "generated/\n");
    fx.write("generated/out.rs", "g\n");
    let r = fx
        .call(
            "git_commit",
            json!({"message": "m", "paths": ["generated/out.rs"]}),
        )
        .await;
    assert_eq!(code(&r), "E-GIT-CMD");
    assert!(r.envelope["error"]["message"]
        .as_str()
        .unwrap()
        .contains(".cairnignore"));
}

#[tokio::test]
async fn a_protected_file_cannot_be_named_in_paths() {
    let fx = repo();
    fx.write(".env", "TOKEN=1\n");
    let r = fx
        .call("git_commit", json!({"message": "m", "paths": [".env"]}))
        .await;
    assert_eq!(code(&r), "E-FS-PROTECTED");
}

#[tokio::test]
async fn plan_mode_reads_git_but_never_commits() {
    let fx = repo_with(Mode::Plan, None);
    let r = fx.call("git_status", json!({})).await;
    assert!(r.ok, "{}", r.envelope);
    fx.write("a.rs", "a\n");
    let r = fx
        .call("git_commit", json!({"message": "a", "paths": ["a.rs"]}))
        .await;
    assert_eq!(code(&r), "E-PERM-MODE");
    assert_eq!(git(&fx, &["log", "--oneline"]).lines().count(), 1);
}

#[tokio::test]
async fn amend_rewrites_the_last_commit() {
    let fx = repo();
    fx.write("a.rs", "a\n");
    let r = fx
        .call(
            "git_commit",
            json!({"message": "first try", "paths": ["a.rs"]}),
        )
        .await;
    assert!(r.ok);
    let r = fx
        .call(
            "git_commit",
            json!({"message": "second try", "amend": true}),
        )
        .await;
    assert_eq!(data(&r)["message"], "second try");
    assert_eq!(git(&fx, &["log", "--oneline"]).lines().count(), 2);
}
