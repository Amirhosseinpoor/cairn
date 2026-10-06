//! T-CHK-*: checkpoints and undo (SPEC §9.8).

use std::path::Path;
use std::process::Command;

use cairn_core::error::codes;
use cairn_git::{Checkpointer, Limits, RestorePolicy, Target, WriteObserver};
use sha2::{Digest, Sha256};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.log\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    dir
}

fn write(ck: &Checkpointer, dir: &Path, rel: &str, text: &str) {
    let abs = dir.canonicalize().unwrap().join(rel);
    ck.before_write(&abs);
    if let Some(p) = abs.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&abs, text).unwrap();
    ck.after_write(&abs, &hex::encode(Sha256::digest(text.as_bytes())));
}

fn read(dir: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(rel)).ok()
}

#[test]
fn t_chk_001_undo_restores_what_cairn_wrote_in_a_git_repo() {
    let dir = repo();
    let ck = Checkpointer::open(dir.path(), "sess-1", Limits::default());
    assert!(ck.uses_git());
    ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "two\n");
    write(&ck, dir.path(), "new.txt", "fresh\n");
    ck.end_turn();
    let done = ck
        .undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap();
    assert_eq!(done.files, vec!["a.txt".to_string(), "new.txt".to_string()]);
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
    assert_eq!(read(dir.path(), "new.txt"), None);
}

#[test]
fn t_chk_002_non_git_directory_restores_exact_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..=255).collect();
    std::fs::write(dir.path().join("bin.dat"), &bytes).unwrap();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    assert!(!ck.uses_git());
    ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "bin.dat", "text now");
    write(&ck, dir.path(), "sub/made.txt", "x");
    ck.undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap();
    assert_eq!(std::fs::read(dir.path().join("bin.dat")).unwrap(), bytes);
    assert!(!dir.path().join("sub/made.txt").exists());
}

#[test]
fn t_chk_010_the_users_index_and_status_are_untouched() {
    let dir = repo();
    std::fs::write(dir.path().join("untracked.txt"), "u").unwrap();
    std::fs::write(dir.path().join("a.txt"), "dirty\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    std::fs::write(dir.path().join("a.txt"), "dirtier\n").unwrap();
    let before = git(dir.path(), &["status", "--porcelain"]);
    let index_before = std::fs::read(dir.path().join(".git/index")).unwrap();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    for turn in 0..10 {
        ck.begin_turn(turn, "turn").unwrap();
    }
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), before);
    assert_eq!(
        std::fs::read(dir.path().join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(git(dir.path(), &["stash", "list"]), "");
}

#[test]
fn t_chk_011_checkpoint_refs_hold_untracked_files_and_diff_shows_the_turn() {
    let dir = repo();
    std::fs::write(dir.path().join("untracked.txt"), "u").unwrap();
    let ck = Checkpointer::open(dir.path(), "sessionabc", Limits::default());
    let first = ck.begin_turn(1, "turn").unwrap();
    let refs = git(dir.path(), &["for-each-ref", "refs/cairn/checkpoints/"]);
    assert!(refs.contains("refs/cairn/checkpoints/sessiona/"), "{refs}");
    let tree = git(
        dir.path(),
        &[
            "ls-tree",
            "-r",
            "--name-only",
            first.ref_name.as_deref().unwrap(),
        ],
    );
    assert!(tree.contains("untracked.txt"), "{tree}");
    write(&ck, dir.path(), "a.txt", "two\n");
    let changes = ck.diff(&first.id).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "a.txt");
}

#[test]
fn t_chk_012_a_moved_ref_is_refused() {
    let dir = repo();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    let made = ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "two\n");
    ck.end_turn();
    // Point the ref at an unrelated commit with a different tree.
    std::fs::write(dir.path().join("other.txt"), "o").unwrap();
    git(dir.path(), &["add", "other.txt"]);
    git(dir.path(), &["commit", "-qm", "other"]);
    git(
        dir.path(),
        &["update-ref", made.ref_name.as_deref().unwrap(), "HEAD"],
    );
    let err = ck
        .undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap_err();
    assert_eq!(err.code, codes::CHK_HASH);
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("two\n"));
}

#[test]
fn t_chk_013_a_user_edit_is_a_conflict_and_full_overrides() {
    let dir = repo();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "cairn\n");
    ck.end_turn();
    std::fs::write(dir.path().join("a.txt"), "user edit\n").unwrap();
    let err = ck
        .undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap_err();
    assert_eq!(err.code, codes::CHK_MERGE);
    assert_eq!(err.paths, vec!["a.txt".to_string()]);
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("user edit\n"));
    ck.undo(&Target::Last, RestorePolicy::Full, false).unwrap();
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
}

#[test]
fn t_chk_014_files_cairn_never_touched_are_left_alone() {
    let dir = repo();
    std::fs::write(dir.path().join("mine.txt"), "mine").unwrap();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "two\n");
    std::fs::write(dir.path().join("mine.txt"), "mine, edited").unwrap();
    ck.undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap();
    assert_eq!(
        read(dir.path(), "mine.txt").as_deref(),
        Some("mine, edited")
    );
}

#[test]
fn t_chk_015_ignored_files_cairn_overwrites_come_back() {
    let dir = repo();
    std::fs::write(dir.path().join("ignored.log"), "precious").unwrap();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "ignored.log", "clobbered");
    ck.undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap();
    assert_eq!(read(dir.path(), "ignored.log").as_deref(), Some("precious"));
}

#[test]
fn t_chk_016_redo_reapplies_and_a_new_turn_clears_it() {
    let dir = repo();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "two\n");
    write(&ck, dir.path(), "b.txt", "bee\n");
    ck.end_turn();
    ck.undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap();
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
    ck.redo().unwrap();
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("two\n"));
    assert_eq!(read(dir.path(), "b.txt").as_deref(), Some("bee\n"));

    ck.begin_turn(2, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "three\n");
    ck.end_turn();
    ck.undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap();
    ck.begin_turn(3, "turn").unwrap();
    assert!(ck.redo().is_err(), "a new checkpoint clears the redo stack");
}

#[test]
fn t_chk_017_undoing_an_older_turn_undoes_the_later_ones_too() {
    let dir = repo();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    let first = ck.begin_turn(1, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "two\n");
    ck.begin_turn(2, "turn").unwrap();
    write(&ck, dir.path(), "a.txt", "three\n");
    write(&ck, dir.path(), "c.txt", "see\n");
    ck.end_turn();
    ck.undo(
        &Target::Seq(first.seq),
        RestorePolicy::CairnFilesOnly,
        false,
    )
    .unwrap();
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
    assert_eq!(read(dir.path(), "c.txt"), None);
    assert!(ck.list().unwrap().is_empty());
}

#[test]
fn t_chk_018_going_back_past_a_commit_needs_hard() {
    let dir = repo();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    ck.begin_turn(1, "pre-commit:abc123").unwrap();
    write(&ck, dir.path(), "a.txt", "two\n");
    ck.end_turn();
    let err = ck
        .undo(&Target::Last, RestorePolicy::CairnFilesOnly, false)
        .unwrap_err();
    assert_eq!(err.code, codes::CHK_FAIL);
    assert!(err.recovery.unwrap().contains("--hard"));
    ck.undo(&Target::Last, RestorePolicy::CairnFilesOnly, true)
        .unwrap();
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("one\n"));
}

#[test]
fn t_chk_020_a_thousand_changed_files_snapshot_quickly() {
    let dir = repo();
    for i in 0..1000 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("{i}")).unwrap();
    }
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    let started = std::time::Instant::now();
    ck.begin_turn(1, "turn").unwrap();
    let took = started.elapsed();
    // REQ-SAFE-017's 200 ms. Windows runners create files several times more
    // slowly (Defender scans every one), so the bound there is a sanity check.
    let budget = if cfg!(windows) { 3000 } else { 200 };
    assert!(took.as_millis() <= budget, "{took:?}");
}

#[test]
fn t_chk_021_retention_keeps_the_newest_per_session() {
    let dir = tempfile::tempdir().unwrap();
    let limits = Limits {
        per_session: 3,
        ..Limits::default()
    };
    let ck = Checkpointer::open(dir.path(), "s", limits);
    for turn in 0..6 {
        ck.begin_turn(turn, "turn").unwrap();
    }
    let turns: Vec<u64> = ck.list().unwrap().iter().map(|c| c.turn).collect();
    assert_eq!(turns, vec![3, 4, 5]);
}

#[test]
fn t_chk_022_the_copy_cap_disables_the_turn_with_e_chk_disk() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{i}")), "x").unwrap();
    }
    let limits = Limits {
        fs_max_files: 3,
        ..Limits::default()
    };
    let ck = Checkpointer::open(dir.path(), "s", limits);
    ck.begin_turn(1, "turn").unwrap();
    for i in 0..5 {
        write(&ck, dir.path(), &format!("f{i}"), "y");
    }
    let warnings = ck.take_warnings();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, codes::CHK_DISK);
}

#[test]
fn t_chk_023_a_read_only_git_dir_fails_the_checkpoint_not_the_work() {
    let dir = repo();
    let objects = dir.path().join(".git");
    // Make the store unwritable by putting a file where its directory goes.
    std::fs::write(objects.join("cairn"), "in the way").unwrap();
    let ck = Checkpointer::open(dir.path(), "s", Limits::default());
    let err = ck.begin_turn(1, "turn").unwrap_err();
    assert_eq!(err.code, codes::CHK_FAIL);
    // Writing still works without a checkpoint.
    write(&ck, dir.path(), "a.txt", "two\n");
    assert_eq!(read(dir.path(), "a.txt").as_deref(), Some("two\n"));
}
