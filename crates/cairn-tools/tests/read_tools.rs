//! `read_file`, `list_dir`, `glob` and `grep` through the whole pipeline.

mod common;

use cairn_core::Mode;
use common::{assert_model_visible, code, data, lines, read, Fixture};
use serde_json::json;

fn fixture() -> Fixture {
    Fixture::new(Mode::Build)
}

// ------------------------------------------------------------ read_file

/// T-TOOL-003: a 300-line file with `limit: 10`.
#[tokio::test]
async fn t_tool_003_read_file_numbers_lines_and_a_short_range_is_not_truncation() {
    let fx = fixture();
    fx.write("a.txt", lines(300));
    let r = fx
        .call("read_file", json!({"path": "a.txt", "limit": 10}))
        .await;
    let d = data(&r);
    assert_eq!(d["start_line"], 1);
    assert_eq!(d["end_line"], 10);
    assert_eq!(d["total_lines"], 300);
    assert_eq!(d["truncated"], false);
    let content = d["content"].as_str().expect("content");
    assert_eq!(content.lines().count(), 10);
    assert!(content.starts_with("1\tline 1\n"), "{content}");
    assert!(content.ends_with("10\tline 10\n"), "{content}");
    assert_eq!(d["path"], "a.txt");
    assert_eq!(d["language"], serde_json::Value::Null);
    assert_eq!(r.envelope["ok"], true);
    assert_eq!(r.envelope["truncated"], false);
    assert!(r.envelope["bytes"].as_u64().expect("bytes") > 0);
}

#[tokio::test]
async fn offset_selects_a_range_and_numbers_stay_absolute() {
    let fx = fixture();
    fx.write("a.txt", lines(50));
    let r = fx
        .call(
            "read_file",
            json!({"path": "a.txt", "offset": 21, "limit": 5}),
        )
        .await;
    let d = data(&r);
    assert_eq!(
        (d["start_line"].as_u64(), d["end_line"].as_u64()),
        (Some(21), Some(25))
    );
    assert!(d["content"]
        .as_str()
        .expect("c")
        .starts_with("21\tline 21\n"));
    // Past the end: an empty, honest answer rather than an error.
    let past = fx
        .call("read_file", json!({"path": "a.txt", "offset": 999}))
        .await;
    let d = data(&past);
    assert_eq!(d["content"], "");
    assert_eq!(d["total_lines"], 50);
}

#[tokio::test]
async fn a_trailing_newline_does_not_add_a_line_and_an_empty_file_has_none() {
    let fx = fixture();
    fx.write("two.txt", "a\nb\n");
    fx.write("noeol.txt", "a\nb");
    fx.write("empty.txt", "");
    assert_eq!(
        data(&fx.call("read_file", read("two.txt")).await)["total_lines"],
        2
    );
    assert_eq!(
        data(&fx.call("read_file", read("noeol.txt")).await)["total_lines"],
        2
    );
    let empty = fx.call("read_file", read("empty.txt")).await;
    assert_eq!(data(&empty)["total_lines"], 0);
    assert_eq!(data(&empty)["content"], "");
}

#[tokio::test]
async fn crlf_files_are_reported_and_read_as_lines_without_the_carriage_return() {
    let fx = fixture();
    fx.write("w.txt", "one\r\ntwo\r\nthree\r\n");
    let d = fx.call("read_file", read("w.txt")).await;
    let d = data(&d);
    assert_eq!(d["crlf"], true);
    assert_eq!(d["content"], "1\tone\n2\ttwo\n3\tthree\n");
    fx.write("m.txt", "a\r\nb\nc\nd\n");
    assert_eq!(
        data(&fx.call("read_file", read("m.txt")).await)["crlf"],
        false,
        "majority decides"
    );
}

#[tokio::test]
async fn a_bom_is_noted_and_not_part_of_the_first_line() {
    let fx = fixture();
    fx.write("b.txt", b"\xEF\xBB\xBFhello\nworld\n");
    let r = fx.call("read_file", read("b.txt")).await;
    let d = data(&r);
    assert_eq!(d["bom"], true);
    assert_eq!(d["content"], "1\thello\n2\tworld\n");
}

#[tokio::test]
async fn the_hash_is_of_the_raw_bytes_and_is_remembered_for_stale_detection() {
    use sha2::{Digest, Sha256};
    let fx = fixture();
    fx.write("s.txt", "hello\n");
    let r = fx.call("read_file", read("s.txt")).await;
    let want = hex::encode(Sha256::digest(b"hello\n"));
    assert_eq!(data(&r)["sha256"], want.as_str());
    assert_eq!(
        fx.executor.file_state().last_seen(&fx.path("s.txt")),
        Some(want),
        "§6.3.3's file_state"
    );
}

#[tokio::test]
async fn language_comes_from_the_extension() {
    let fx = fixture();
    fx.write("src/lib.rs", "fn main() {}\n");
    assert_eq!(
        data(&fx.call("read_file", read("src/lib.rs")).await)["language"],
        "rust"
    );
}

#[tokio::test]
async fn the_default_call_is_capped_at_2000_lines_and_says_so() {
    let fx = fixture();
    fx.write("big.txt", lines(3000));
    let r = fx.call("read_file", read("big.txt")).await;
    let d = data(&r);
    assert_eq!(d["end_line"], 2000);
    assert_eq!(d["total_lines"], 3000);
    assert_eq!(d["truncated"], true);
    // The next page picks up where the cap cut.
    let next = fx
        .call("read_file", json!({"path": "big.txt", "offset": 2001}))
        .await;
    assert_eq!(data(&next)["end_line"], 3000);
    assert_eq!(data(&next)["truncated"], false);
}

#[tokio::test]
async fn the_byte_cap_cuts_long_lines_short_of_the_line_cap() {
    let fx = fixture();
    let long = "x".repeat(1900);
    fx.write("wide.txt", format!("{long}\n").repeat(500));
    let r = fx.call("read_file", read("wide.txt")).await;
    let d = data(&r);
    assert_eq!(d["truncated"], true);
    assert!(d["end_line"].as_u64().expect("end") < 500);
    assert!(d["content"].as_str().expect("c").len() <= 200 * 1024);
}

// -------------------------------------------------- read_file: failures

#[tokio::test]
async fn t_tool_011_every_read_failure_is_model_visible_with_a_recovery() {
    let fx = fixture();
    fx.write("dir/inner.txt", "x");
    fx.write("blob.bin", b"\0\0\0\0binary\0");
    fx.write("utf16.txt", [0xFF, 0xFE, b'h', 0, b'i', 0]);
    fx.write("latin1.txt", b"caf\xE9 au lait, plain ascii otherwise\n");
    fx.write(".gitignore", "secret-gen.txt\n");
    fx.write("secret-gen.txt", "generated");
    fx.write(".env", "TOKEN=abc");
    let mut late_bad = vec![b'a'; 9000];
    late_bad.extend_from_slice(b"\xFF\xFE bad tail \xC3\x28\n");
    fx.write("late.txt", late_bad);
    fx.write("huge.txt", vec![b'a'; 9 * 1024 * 1024]);

    let cases: Vec<(serde_json::Value, &str)> = vec![
        (read("nope.txt"), "E-FS-NOTFOUND"),
        (read("dir"), "E-FS-DIR"),
        (read("blob.bin"), "E-FS-BINARY"),
        (read("utf16.txt"), "E-FS-ENCODING"),
        (read("latin1.txt"), "E-FS-ENCODING"),
        (read("late.txt"), "E-FS-ENCODING"),
        (read("huge.txt"), "E-FS-TOOBIG"),
        (read("secret-gen.txt"), "E-FS-IGNORED"),
        (read(".env"), "E-FS-PROTECTED"),
        (read("../outside.txt"), "E-FS-ESCAPE"),
        (read("/etc/hostname"), "E-FS-ESCAPE"),
        (read(""), "E-FS-BADPATH"),
        (json!({}), "E-TOOL-BADSCHEMA"),
        (json!({"path": "a", "limit": 5000}), "E-TOOL-BADSCHEMA"),
        (json!({"path": "a", "offset": 0}), "E-TOOL-BADSCHEMA"),
        (json!({"path": "a", "bogus": 1}), "E-TOOL-BADSCHEMA"),
        (json!({"path": 5}), "E-TOOL-BADSCHEMA"),
    ];
    for (input, want) in cases {
        let r = fx.call("read_file", input.clone()).await;
        assert_eq!(code(&r), want, "{input}: {}", r.envelope);
        assert_model_visible(&r);
    }
}

#[tokio::test]
async fn a_file_over_1_mib_needs_an_explicit_range() {
    let fx = fixture();
    fx.write("mid.txt", "x\n".repeat(700_000));
    let whole = fx.call("read_file", read("mid.txt")).await;
    assert_eq!(code(&whole), "E-FS-TOOBIG");
    assert_model_visible(&whole);
    let ranged = fx
        .call("read_file", json!({"path": "mid.txt", "limit": 5}))
        .await;
    assert_eq!(data(&ranged)["end_line"], 5);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_that_leaves_the_workspace_cannot_be_read() {
    let fx = fixture();
    let outside = fx.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("secret.txt"), "top secret").expect("file");
    std::os::unix::fs::symlink(&outside, fx.path("link")).expect("link");
    let r = fx.call("read_file", read("link/secret.txt")).await;
    assert_eq!(code(&r), "E-FS-ESCAPE");
    assert!(!r.text().contains("top secret"));
}

// ------------------------------------------- output hygiene (REQ-TOOL-020)

/// T-SEC-011: secrets in a file never reach the model.
#[tokio::test]
async fn t_sec_011_secrets_in_file_content_are_redacted_before_the_model_sees_them() {
    let fx = fixture();
    fx.write(
        "config.py",
        "API_KEY = \"sk-abcdefghijklmnopqrstuvwxyz0123456789\"\npassword: hunter2hunter2\nuse = 1\n",
    );
    let r = fx.call("read_file", read("config.py")).await;
    let text = r.text();
    assert!(
        !text.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"),
        "{text}"
    );
    assert!(!text.contains("hunter2hunter2"), "{text}");
    assert!(text.contains("REDACTED"), "{text}");
    assert!(text.contains("use = 1"), "ordinary code is untouched");
}

/// §9.7 mitigation 8: hidden characters are stripped and reported.
#[tokio::test]
async fn hidden_characters_are_stripped_and_flagged() {
    let fx = fixture();
    fx.write("tricky.txt", "ignore\u{200B}previous\u{200D}text\n");
    let r = fx.call("read_file", read("tricky.txt")).await;
    assert!(data(&r)["content"]
        .as_str()
        .expect("c")
        .contains("ignoreprevioustext"));
    assert!(r.envelope["warning"]
        .as_str()
        .unwrap_or("")
        .starts_with("W-INJ-OBSCURE"));
}

// ------------------------------------------------------------- list_dir

#[tokio::test]
async fn list_dir_describes_entries() {
    let fx = fixture();
    fx.write("src/main.rs", "fn main() {}\n");
    fx.write("README.md", "# hi\n");
    fx.write("image.bin", b"\0\0\0");
    fx.write(".gitignore", "*.log\n");
    fx.write("run.log", "x");
    let r = fx.call("list_dir", json!({})).await;
    let d = data(&r);
    let entries = d["entries"].as_array().expect("entries");
    let by_name = |n: &str| {
        entries
            .iter()
            .find(|e| e["name"] == n)
            .unwrap_or_else(|| panic!("{n} in {entries:?}"))
    };
    assert_eq!(by_name("src")["type"], "dir");
    assert_eq!(by_name("README.md")["language"], "markdown");
    assert_eq!(by_name("image.bin")["binary"], true);
    assert_eq!(
        by_name("run.log")["ignored"],
        true,
        "ignored entries are shown, flagged"
    );
    assert!(
        !entries.iter().any(|e| e["name"] == "main.rs"),
        "depth 1 does not list children"
    );
}

#[tokio::test]
async fn list_dir_depth_hidden_and_ignored_directories() {
    let fx = fixture();
    fx.write("a/b/c/d.txt", "x");
    fx.write(".hidden/x", "x");
    fx.write("target/debug/x", "x");
    let shallow = fx.call("list_dir", json!({})).await;
    let names: Vec<_> = data(&shallow)["entries"]
        .as_array()
        .expect("e")
        .iter()
        .map(|e| e["path"].as_str().expect("p").to_string())
        .collect();
    assert!(names.contains(&"a".to_string()));
    assert!(
        !names.iter().any(|n| n.starts_with(".hidden")),
        "hidden is off by default: {names:?}"
    );

    let deep = fx
        .call("list_dir", json!({"depth": 3, "include_hidden": true}))
        .await;
    let paths: Vec<_> = data(&deep)["entries"]
        .as_array()
        .expect("e")
        .iter()
        .map(|e| e["path"].as_str().expect("p").to_string())
        .collect();
    assert!(paths.contains(&"a/b/c".to_string()), "{paths:?}");
    assert!(
        !paths.contains(&"a/b/c/d.txt".to_string()),
        "depth 3 stops at c"
    );
    assert!(paths.contains(&".hidden".to_string()));
    assert!(
        paths.contains(&"target".to_string()),
        "an ignored directory is listed…"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with("target/")),
        "…but never entered"
    );
}

#[tokio::test]
async fn list_dir_caps_each_directory_at_2000_entries() {
    let fx = fixture();
    for i in 0..2100 {
        fx.write(&format!("many/f{i:04}.txt"), "");
    }
    let r = fx.call("list_dir", json!({"path": "many"})).await;
    let d = data(&r);
    assert_eq!(d["entry_count"], 2000);
    assert_eq!(d["truncated"], true);
}

#[tokio::test]
async fn list_dir_failures() {
    let fx = fixture();
    fx.write("f.txt", "x");
    for (input, want) in [
        (json!({"path": "f.txt"}), "E-FS-DIR"),
        (json!({"path": "missing"}), "E-FS-NOTFOUND"),
        (json!({"path": "../.."}), "E-FS-ESCAPE"),
        (json!({"depth": 9}), "E-TOOL-BADSCHEMA"),
    ] {
        let r = fx.call("list_dir", input.clone()).await;
        assert_eq!(code(&r), want, "{input}");
        assert_model_visible(&r);
    }
}

// ----------------------------------------------------------------- glob

#[tokio::test]
async fn glob_finds_files_and_reports_counts() {
    let fx = fixture();
    fx.write("src/a.rs", "");
    fx.write("src/b.rs", "");
    fx.write("docs/c.md", "");
    fx.write("target/x.rs", "");
    let r = fx.call("glob", json!({"pattern": "**/*.rs"})).await;
    let d = data(&r);
    let mut found: Vec<_> = d["matches"]
        .as_array()
        .expect("m")
        .iter()
        .map(|m| m.as_str().expect("s"))
        .collect();
    found.sort_unstable();
    assert_eq!(found, ["src/a.rs", "src/b.rs"], "target/ is ignored");
    assert_eq!(d["count"], 2);
    assert_eq!(d["truncated"], false);
    let all = fx
        .call(
            "glob",
            json!({"pattern": "**/*.rs", "respect_ignore": false}),
        )
        .await;
    assert_eq!(data(&all)["count"], 3);
}

#[tokio::test]
async fn glob_failures() {
    let fx = fixture();
    fx.write(".gitignore", "gen/\n");
    fx.write("gen/x", "x");
    for (input, want) in [
        (json!({"pattern": "[unclosed"}), "E-GLOB-SYNTAX"),
        (json!({"pattern": "*", "path": "missing"}), "E-FS-NOTFOUND"),
        (json!({"pattern": "*", "path": "gen"}), "E-FS-IGNORED"),
        (json!({"pattern": "*", "path": "/etc"}), "E-FS-ESCAPE"),
        (json!({"pattern": "x".repeat(501)}), "E-TOOL-TOOBIG"),
        (json!({}), "E-TOOL-BADSCHEMA"),
    ] {
        let r = fx.call("glob", input.clone()).await;
        assert_eq!(code(&r), want, "{input}");
        assert_model_visible(&r);
    }
}

// ----------------------------------------------------------------- grep

#[tokio::test]
async fn grep_returns_matches_with_context_and_counts_binaries() {
    let fx = fixture();
    fx.write("src/a.rs", "fn one() {}\nfn parse() {}\nfn three() {}\n");
    fx.write("blob.bin", b"\0parse\0");
    let r = fx
        .call("grep", json!({"pattern": "fn parse", "context_lines": 1}))
        .await;
    let d = data(&r);
    assert_eq!(d["match_count"], 1);
    let m = &d["matches"][0];
    assert_eq!(
        (m["path"].as_str(), m["line"].as_u64(), m["column"].as_u64()),
        (Some("src/a.rs"), Some(2), Some(1))
    );
    assert_eq!(m["before"], json!(["fn one() {}"]));
    assert_eq!(m["after"], json!(["fn three() {}"]));
    assert_eq!(d["binary_skipped"], 1);
}

/// T-PERM-020: grep on an ignored file.
#[tokio::test]
async fn t_perm_020_grep_on_an_ignored_path_is_refused() {
    let fx = fixture();
    fx.write(".gitignore", "private.txt\n");
    fx.write("private.txt", "needle");
    let r = fx
        .call("grep", json!({"pattern": "needle", "path": "private.txt"}))
        .await;
    assert_eq!(code(&r), "E-FS-IGNORED");
    assert_model_visible(&r);
    // A repository-wide search simply does not see it.
    let all = fx.call("grep", json!({"pattern": "needle"})).await;
    assert_eq!(data(&all)["match_count"], 0);
}

#[tokio::test]
async fn grep_failures() {
    let fx = fixture();
    fx.write("a.txt", "x");
    for (input, want) in [
        (json!({"pattern": "(unclosed"}), "E-REGEX-SYNTAX"),
        (json!({"pattern": "x", "glob": "["}), "E-GLOB-SYNTAX"),
        (json!({"pattern": "x", "path": "missing"}), "E-FS-NOTFOUND"),
        (json!({"pattern": "x", "path": ".."}), "E-FS-ESCAPE"),
        (json!({"pattern": "x".repeat(501)}), "E-TOOL-TOOBIG"),
        (
            json!({"pattern": "x", "context_lines": 9}),
            "E-TOOL-BADSCHEMA",
        ),
    ] {
        let r = fx.call("grep", input.clone()).await;
        assert_eq!(code(&r), want, "{input}");
        assert_model_visible(&r);
    }
}

#[tokio::test]
async fn grep_output_is_capped_but_stays_valid_json() {
    let fx = fixture();
    fx.write(
        "big.txt",
        format!("{}\n", "needle ".repeat(300)).repeat(400),
    );
    let r = fx
        .call("grep", json!({"pattern": "needle", "max_results": 5000}))
        .await;
    assert!(r.ok);
    assert!(r.truncated, "128 KiB cap applied");
    assert!(r.text().len() <= 128 * 1024 + 1024, "{}", r.text().len());
    let _: serde_json::Value = serde_json::from_str(&r.text()).expect("still JSON");
}

// ---------------------------------------------------------- T-TOOL-001

/// T-TOOL-001 (for the tools that exist so far): the table is live
/// registration, and equals §6.1 exactly.
#[tokio::test]
async fn t_tool_001_the_table_comes_from_registration() {
    let fx = fixture();
    let table = fx.executor.registry().table();
    let row = |n: &str| {
        table
            .iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("{n}"))
            .clone()
    };
    for (name, timeout_s, max_kib) in [
        ("read_file", 10, 200),
        ("list_dir", 10, 64),
        ("glob", 15, 64),
        ("grep", 30, 128),
    ] {
        let r = row(name);
        assert_eq!(
            (r.class, r.side_effect, r.idempotency, r.serial),
            ("Read", "None", "Safe", false),
            "{name}"
        );
        assert_eq!(r.timeout_ms, timeout_s * 1000, "{name}");
        assert_eq!(r.max_output_bytes, max_kib * 1024, "{name}");
    }
}
