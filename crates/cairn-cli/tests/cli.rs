//! End-to-end tests over the real binary (SPEC §11.1–§11.3, §14.3.10 §14.3.11).
//!
//! Every case runs with `CAIRN_HOME`/`CAIRN_WORKSPACE` pointed at a private
//! temp directory and the `CAIRN_*` env table cleared, so the developer's own
//! configuration can never change an expected exit code.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

// ------------------------------------------------------------------ fixtures

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    ws: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(home.join("config")).expect("home");
        std::fs::create_dir_all(ws.join(".cairn")).expect("ws");
        Self {
            _tmp: tmp,
            home,
            ws,
        }
    }

    fn user_config(&self) -> PathBuf {
        // Joined component-wise, not `join("config/config.toml")`: `Path::join`
        // keeps the separator inside the literal, so on Windows this read
        // `…\home\config/config.toml` while `cairn config path` printed the
        // all-backslash form the loader built — two spellings of one file, and
        // the substring check below missed.
        self.home.join("config").join("config.toml")
    }

    fn project_config(&self) -> PathBuf {
        self.ws.join(".cairn").join("config.toml")
    }

    /// A `cairn` invocation isolated from the process environment.
    fn cairn(&self) -> Command {
        // `cargo_bin_cmd!` rather than `Command::cargo_bin`, which assert_cmd
        // deprecated in 2.1 (SPEC §15.2 pins assert_cmd ≥ 2.1).
        let mut cmd = assert_cmd::cargo_bin_cmd!("cairn");
        cmd.env("CAIRN_HOME", &self.home);
        cmd.env("CAIRN_WORKSPACE", &self.ws);
        cmd.current_dir(&self.ws);
        for (var, _, _) in cairn_config::ENV_KEYS {
            cmd.env_remove(var);
        }
        // Non-config env the loader/doctor also read (SPEC §11.3, §12.3 row 24).
        for var in [
            "CAIRN_HOME",
            "CAIRN_WORKSPACE",
            "CAIRN_CONFIG",
            "CAIRN_RT_THREADS",
            "CAIRN_TMP",
            "CAIRN_SCREEN_READER",
            "CAIRN_SHELL",
            "CAIRN_INSTALL_DIR",
            "CAIRN_TEST_CLOCK",
            "CAIRN_CASSETTE_MODE",
            "CAIRN_LIVE",
            "CAIRN_ALLOW_UNREDACTED_EXPORT",
            "CAIRN_LOG_HTTP",
            "CAIRN_LOG_SSE",
            "NO_COLOR",
        ] {
            cmd.env_remove(var);
        }
        // Re-apply the two the fixture controls.
        cmd.env("CAIRN_HOME", &self.home);
        cmd.env("CAIRN_WORKSPACE", &self.ws);
        // Deterministic credential rows (SPEC §4.10 lookup order).
        for var in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OLLAMA_API_KEY",
            "VLLM_API_KEY",
            "CAIRN_ANTHROPIC_API_KEY",
            "CAIRN_OPENAI_API_KEY",
            "CAIRN_OLLAMA_API_KEY",
            "CAIRN_VLLM_API_KEY",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }
}

// ------------------------------------------------------------ T-CLI-001/002

/// Every exit-code path reachable without a provider (SPEC §11.2).
#[test]
fn t_cli_001_exit_code_paths() {
    let fx = Fixture::new();

    // 0 — success
    fx.cairn().arg("version").assert().code(0);

    // 1 — unhandled / not-yet-implemented subsystem
    fx.cairn()
        .args(["chat"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("E-IMPL-STAGE"));

    // `run` landed in M1: without a key it fails as a provider error, not a stub.
    fx.cairn()
        .args(["run", "-p", "hi"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("E-PROV-AUTH"));

    // 2 — bad usage / bad config
    fx.cairn()
        .args(["run", "-p", "hi", "--prompt-file", "x.txt"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CLI-USAGE"));
    fx.cairn()
        .args(["config", "get", "no.such.key"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-UNKNOWN"));

    // 3 — provider fatal (offline, before any network attempt)
    fx.cairn()
        .args(["run", "-p", "hi", "--offline"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("E-PROV-OFFLINE"));

    // 9 — resource not found
    fx.cairn()
        .args(["resume", "ses_missing"])
        .assert()
        .code(9)
        .stderr(predicate::str::contains("E-SESS-NOTFOUND"));
    fx.cairn()
        .args(["export", "ses_missing"])
        .assert()
        .code(9)
        .stderr(predicate::str::contains("E-SESS-NOTFOUND"));
}

/// REQ-CLI-002: code line always, hint only outside `--quiet`.
#[test]
fn t_cli_002_code_line_in_normal_and_quiet() {
    let fx = Fixture::new();
    let cases: [(&[&str], &str); 5] = [
        (&["chat"], "E-IMPL-STAGE"),
        (
            &["run", "-p", "hi", "--prompt-file", "x.txt"],
            "E-CLI-USAGE",
        ),
        (&["run", "-p", "hi", "--offline"], "E-PROV-OFFLINE"),
        (&["resume", "ses_missing"], "E-SESS-NOTFOUND"),
        (&["config", "get", "no.such.key"], "E-CFG-UNKNOWN"),
    ];

    for (args, code) in cases {
        // Normal: code line + hint.
        fx.cairn()
            .args(args)
            .assert()
            .code(predicate::function(|c: &i32| *c != 0))
            .stderr(
                predicate::str::contains(format!("error: {code}"))
                    .and(predicate::str::contains("hint:")),
            );

        // --quiet: code line only, no hint line.
        let mut quiet = vec!["-q"];
        quiet.extend_from_slice(args);
        let out = fx.cairn().args(&quiet).output().expect("runs");
        assert!(!out.status.success(), "{args:?} must stay non-zero");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("error: {code}")),
            "--quiet must keep the code line for {args:?}: {stderr}"
        );
        assert!(
            !stderr.contains("hint:"),
            "--quiet must drop the hint line for {args:?}: {stderr}"
        );
    }
}

// ---------------------------------------------------------------- T-CLI-020

/// `-p` and `--prompt-file` are mutually exclusive (exit 2).
#[test]
fn t_cli_020_prompt_sources_conflict() {
    let fx = Fixture::new();
    for args in [
        vec!["run", "-p", "hi", "--prompt-file", "x.txt"],
        vec!["run", "-p", "hi", "--stdin"],
        vec!["run", "--prompt-file", "x.txt", "--stdin"],
    ] {
        fx.cairn()
            .args(&args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("E-CLI-USAGE"));
    }
    // None of them is also a usage error.
    fx.cairn()
        .args(["run"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CLI-USAGE"));
}

// ---------------------------------------------------------------- T-CLI-021

/// `--help` on every subcommand exits 0 and lists its own flags.
#[test]
fn t_cli_021_help_on_every_subcommand() {
    let fx = Fixture::new();

    let out = fx.cairn().arg("--help").output().expect("help runs");
    assert_eq!(out.status.code(), Some(0), "--help must exit 0");
    let text = String::from_utf8_lossy(&out.stdout);

    let mut names: Vec<String> = Vec::new();
    let mut in_commands = false;
    for line in text.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands && line.starts_with("Options:") {
            break;
        }
        if in_commands && line.starts_with("  ") {
            let trimmed = line.trim();
            // `wrap_help` folds a long description onto the next line, and that
            // continuation is indented just like a real row — so accept only
            // tokens that can be clap subcommand names (all of §11.1's are).
            if let Some(name) = trimmed
                .split_whitespace()
                .next()
                .filter(|tok| tok.chars().all(|c| c.is_ascii_lowercase() || c == '-'))
            {
                names.push(name.to_string());
            }
        }
    }
    assert!(names.len() >= 14, "command tree looks truncated: {names:?}");

    for name in &names {
        // clap's built-in `help` subcommand takes a command name, not a flag.
        let mut probe = fx.cairn();
        let out = if name == "help" {
            probe.arg("help")
        } else {
            probe.args([name.as_str(), "--help"])
        };
        let out = out.output().expect("subcommand help runs");
        assert_eq!(
            out.status.code(),
            Some(0),
            "`cairn {name} --help` must exit 0: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("Usage: cairn"),
            "`cairn {name} --help` must show usage: {text}"
        );
        if name != "help" {
            // `Usage: cairn <name>` is checked as a line, not a substring:
            // Windows runs `cairn.exe`, and clap prints the binary it was
            // invoked as, so the literal spelling only holds on POSIX.
            let own_usage = text.lines().any(|line| {
                let line = line.trim();
                line.starts_with("Usage: cairn") && line.contains(&format!(" {name}"))
            });
            assert!(
                own_usage,
                "`cairn {name} --help` must show its own usage: {text}"
            );
        }
        assert!(
            text.lines().any(|l| l.trim_start().starts_with('-')),
            "`cairn {name} --help` must list flags: {text}"
        );
    }
}

// ---------------------------------------------------------------- T-CLI-022

/// `--offline` fails fast with `E-PROV-OFFLINE` (exit 3) in well under 100 ms.
#[test]
fn t_cli_022_offline_fails_fast() {
    let fx = Fixture::new();
    let started = Instant::now();
    let out = fx
        .cairn()
        .args(["run", "-p", "hi", "--offline"])
        .output()
        .expect("runs");
    let elapsed = started.elapsed();
    assert_eq!(out.status.code(), Some(3));
    assert!(
        elapsed.as_millis() < 100,
        "--offline must short-circuit in < 100 ms, took {elapsed:?}"
    );
}

// ---------------------------------------------------------------- T-CLI-023

/// Completions generate cleanly for the four shells of SPEC §11.1.
#[test]
fn t_cli_023_completions_for_four_shells() {
    let fx = Fixture::new();
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let out = fx
            .cairn()
            .args(["completions", shell])
            .output()
            .expect("runs");
        assert_eq!(out.status.code(), Some(0), "{shell}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("cairn"),
            "{shell} script never mentions cairn"
        );
        assert!(!text.trim().is_empty(), "{shell} script is empty");
    }
}

// ---------------------------------------------------------------- T-CFG-003

/// An error confined to `[[mcp.servers]]` never blocks startup (REQ-CLI-004).
///
/// The spec's row says `run` exits 0; in M0 `run` is an `E-IMPL-STAGE` stub
/// (exit 1), so the invariant under test is "the bad key did not make startup
/// fail with exit 2". From M1 the exit code is 0 — logged in PROGRESS.md.
#[test]
fn t_cfg_003_partial_config_still_starts() {
    let fx = Fixture::new();
    std::fs::write(
        fx.project_config(),
        "[[mcp.servers]]\ntransport = \"carrier-pigeon\"\n",
    )
    .unwrap();

    // The config surface itself is fine and says why.
    let out = fx.cairn().args(["config", "list"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "config list must not fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("W-CFG-PARTIAL"),
        "expected the partial-startup warning: {stderr}"
    );

    // `run` reaches its own stage, not a config failure.
    let out = fx.cairn().args(["run", "-p", "hi"]).output().unwrap();
    let code = out.status.code().expect("runs");
    assert_ne!(code, 2, "a non-fatal section must not become exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("W-CFG-PARTIAL"), "{stderr}");
}

// ---------------------------------------------------------------- T-CFG-004

/// Invalid project value, valid user value → user value + `W-CFG-FALLBACK`
/// (REQ-CLI-005).
#[test]
fn t_cfg_004_invalid_layer_falls_back() {
    let fx = Fixture::new();
    std::fs::write(fx.user_config(), "temperature = 0.5\n").unwrap();
    std::fs::write(fx.project_config(), "temperature = 9.0\n").unwrap();

    let out = fx
        .cairn()
        .args(["config", "get", "temperature"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0.5");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("W-CFG-FALLBACK"),
        "expected the fallback warning: {stderr}"
    );
}

// ---------------------------------------------------------------- T-CFG-010

/// The §11.8 worked example, byte for byte.
#[test]
fn t_cfg_010_worked_example() {
    let fx = Fixture::new();
    std::fs::write(
        fx.user_config(),
        "mode = \"auto\"\n\n[ui]\ntheme = \"cairn-light\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.project_config(),
        "mode = \"plan\"\n\n[ui]\nanimation = \"off\"\n",
    )
    .unwrap();

    let get = |key: &str| -> String {
        let out = fx
            .cairn()
            .args(["config", "get", key])
            .output()
            .expect("runs");
        assert_eq!(out.status.code(), Some(0), "config get {key}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // flag --mode build wins over project/env/user
    let out = fx
        .cairn()
        .args(["--mode", "build", "config", "get", "mode"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "build");

    // env CAIRN_MODEL wins over the default
    let mut cmd = fx.cairn();
    cmd.env("CAIRN_MODEL", "openai/o4-mini");
    let out = cmd.args(["config", "get", "model"]).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "openai/o4-mini"
    );

    // user layer theme, project layer animation
    assert_eq!(get("ui.theme"), "cairn-light");
    assert_eq!(get("ui.animation"), "off");
    // project beats user for mode when no flag is given
    assert_eq!(get("mode"), "plan");
}

/// REQ-CLI-006: `--effective` annotates every key with its winning layer.
#[test]
fn t_cfg_005_effective_annotates_sources() {
    let fx = Fixture::new();
    std::fs::write(fx.user_config(), "temperature = 0.5\n").unwrap();

    let out = fx
        .cairn()
        .args(["config", "get", "temperature", "--effective"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().next().unwrap().trim(), "0.5");
    assert!(
        stdout.contains("# source: user"),
        "expected a source annotation: {stdout}"
    );

    // The full list carries annotations too.
    let out = fx
        .cairn()
        .args(["config", "list", "--effective"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let annotated = stdout.lines().filter(|l| l.contains(" # ")).count();
    assert!(annotated > 100, "every key must be annotated: {annotated}");
    assert!(stdout.contains("temperature = 0.5  # user"), "{stdout}");
}

// ---------------------------------------------------------------- T-CFG-012

/// Array-of-tables in the project layer replaces the user layer's (§11.5).
#[test]
fn t_cfg_012_array_of_tables_replaces() {
    let fx = Fixture::new();
    std::fs::write(
        fx.user_config(),
        "[[mcp.servers]]\nname = \"user-server\"\ntransport = \"stdio\"\ncommand = \"true\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.project_config(),
        "[[mcp.servers]]\nname = \"project-server\"\ntransport = \"stdio\"\ncommand = \"true\"\n",
    )
    .unwrap();

    let out = fx.cairn().args(["mcp", "list", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("json");
    let servers = parsed.as_array().expect("array");
    assert_eq!(servers.len(), 1, "project replaces, never merges: {text}");
    assert_eq!(servers[0]["name"], "project-server");
}

// ---------------------------------------------------------------- T-CFG-013

/// Env lists split on `,` (and `:` for path lists), empty string → empty list
/// (§11.5, §11.3).
#[test]
fn t_cfg_013_env_list_split() {
    let fx = Fixture::new();
    let mut cmd = fx.cairn();
    cmd.env("CAIRN_VERIFY_COMMANDS", "cargo test,cargo clippy");
    let out = cmd
        .args(["config", "get", "verify.commands"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("list is JSON");
    assert_eq!(
        parsed.as_array().map(Vec::len),
        Some(2),
        "comma split: {text}"
    );

    // Empty string yields an empty list, not a one-element list.
    let mut cmd = fx.cairn();
    cmd.env("CAIRN_VERIFY_COMMANDS", "");
    let out = cmd
        .args(["config", "get", "verify.commands"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.as_array().map(Vec::len)),
        Some(0),
        "empty env string → empty list: {text}"
    );
}

// ------------------------------------------------------------ code coverage

/// `cairn --version` / `version` must agree and carry the schema versions
/// (REQ-ARCH-011 context, §11.1).
#[test]
fn version_and_json_agree() {
    let fx = Fixture::new();
    let plain = fx.cairn().arg("version").output().unwrap();
    let text = String::from_utf8_lossy(&plain.stdout);
    let as_json = fx.cairn().args(["version", "--json"]).output().unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&as_json.stdout)).expect("version --json");
    assert!(text.contains(json["version"].as_str().unwrap()), "{text}");
    assert_eq!(json["name"], "cairn");
    assert_eq!(json["config_schema_version"], cairn_config::SCHEMA_VERSION);
    assert_eq!(json["session_schema_version"], 1);
    assert!(json["target"].as_str().unwrap().contains('-'));
    assert_ne!(json["registry_updated"].as_str().unwrap(), "");
}

/// `config path` names all three layers of SPEC §11.6.
#[test]
fn config_path_lists_three_layers() {
    let fx = Fixture::new();
    let out = fx.cairn().args(["config", "path"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("user: "), "{text}");
    assert!(text.contains("project: "), "{text}");
    assert!(text.contains("system: "), "{text}");
    assert!(text.contains(&fx.user_config().display().to_string()));

    let out = fx
        .cairn()
        .args(["config", "path", "--user"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("user:"), "{text}");
    assert!(!text.contains("project:"), "{text}");
}

/// `config set` / `unset` edit one layer and refuse invalid values (§11.1).
#[test]
fn config_set_refuses_invalid_values() {
    let fx = Fixture::new();

    fx.cairn()
        .args(["config", "set", "ui.frame_rate", "120"])
        .assert()
        .code(0);
    assert!(fx.user_config().exists());
    let out = fx
        .cairn()
        .args(["config", "get", "ui.frame_rate"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "120");

    // Out of range → refuse, nothing written.
    fx.cairn()
        .args(["config", "set", "temperature", "9.9"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-RANGE"));
    let out = fx
        .cairn()
        .args(["config", "get", "temperature"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "0.2",
        "unchanged"
    );

    // Unknown key → `E-CFG-UNKNOWN`, nothing written.
    fx.cairn()
        .args(["config", "set", "bogus.key", "1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-UNKNOWN"));

    // Unset removes it, then reports honestly that it is gone.
    fx.cairn()
        .args(["config", "unset", "ui.frame_rate"])
        .assert()
        .code(0);
    let out = fx
        .cairn()
        .args(["config", "get", "ui.frame_rate"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "60");
    fx.cairn()
        .args(["config", "unset", "ui.frame_rate"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-UNKNOWN"));
}

/// `config validate` reports every §11.4.2 violation kind at once (REQ-CLI-003).
#[test]
fn config_validate_reports_all_errors() {
    let fx = Fixture::new();
    // Range, bad enum and range again — three independent violations in one file.
    std::fs::write(
        fx.user_config(),
        "temperature = 9.0\nmode = \"sideways\"\n\n[ui]\nframe_rate = 600\n",
    )
    .unwrap();

    let out = fx.cairn().args(["config", "validate"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "validate fails on any E-");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("E-CFG-RANGE"), "{stderr}");
    assert!(stderr.contains("E-CFG-BADVALUE"), "{stderr}");
    assert!(
        stderr.contains("error(s), ") && stderr.contains("3 error(s)"),
        "all three must be reported at once: {stderr}"
    );
    assert!(
        stderr.contains("config.toml:1:1"),
        "file:line:col: {stderr}"
    );
    assert!(
        stderr.contains("config.toml:2:1"),
        "file:line:col: {stderr}"
    );
    assert!(
        stderr.contains("config.toml:5:1"),
        "file:line:col: {stderr}"
    );

    // An unknown key is fatal with its own code.
    let fx = Fixture::new();
    std::fs::write(fx.user_config(), "bogus_key = 1\n").unwrap();
    let out = fx.cairn().args(["config", "validate"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("E-CFG-UNKNOWN"));

    // A clean config validates.
    let fx = Fixture::new();
    fx.cairn().args(["config", "validate"]).assert().code(0);
}

/// `config list --json-schema` emits the annotated schema (REQ-ARCH-011).
#[test]
fn config_json_schema_has_flag_annotations() {
    let fx = Fixture::new();
    let out = fx
        .cairn()
        .args(["config", "list", "--json-schema"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let schema: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON Schema");
    assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
    let defs = schema["definitions"].as_object().expect("definitions");
    assert!(defs.len() > 10, "schema must describe the whole config");
    // Every flag of §11.1 carries its annotations (REQ-ARCH-011, T-CFG-001).
    let flags = schema["x-cli-flags"].as_array().expect("x-cli-flags");
    assert!(
        flags.len() >= 20,
        "flag annotations missing: {}",
        flags.len()
    );
    for row in flags {
        assert!(row["x-flag"].as_str().unwrap().starts_with('-'));
        assert!(
            row["x-scope"].as_str().unwrap() == "global"
                || row["x-scope"].as_str().unwrap() == "run"
        );
    }
    assert!(schema.get("x-env-var-map").is_some(), "env map missing");
}

/// `auth list` never prints key material beyond the last four characters
/// (REQ-PROV-018).
#[test]
fn auth_list_redacts_keys() {
    let fx = Fixture::new();
    std::fs::write(
        fx.user_config(),
        "[providers.anthropic]\napi_key = \"sk-supersecret-9876\"\n",
    )
    .unwrap();

    let out = fx.cairn().args(["auth", "list"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("supersecret"), "key leaked: {text}");
    assert!(text.contains("…9876"), "{text}");

    let out = fx
        .cairn()
        .args(["auth", "list", "--json"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("supersecret"), "key leaked: {text}");

    let out = fx.cairn().args(["auth", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("supersecret"), "key leaked: {text}");
    assert!(text.contains("anthropic: ok"), "{text}");
}

/// Unknown provider name is a usage error, not a crash (SPEC §11.1).
#[test]
fn auth_unknown_provider_is_usage_error() {
    let fx = Fixture::new();
    fx.cairn()
        .args(["auth", "logout", "skynet"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CLI-USAGE"));
}

/// `doctor` covers §12.3 and exits 1 only when a row failed (SPEC §12.3).
#[test]
fn doctor_reports_every_row() {
    let fx = Fixture::new();
    let out = fx.cairn().arg("doctor").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("pass"), "{text}");
    assert!(text.contains("skip"), "{text}");
    // Row 19 must expose the telemetry setting (REQ-OPS-004).
    assert!(text.contains("telemetry.enabled=false"), "{text}");

    let code = out.status.code().unwrap();
    assert!(
        code == 0 || code == 1,
        "doctor exits 0 (clean) or 1 (a row failed): {code}"
    );
    if code == 1 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("error: E-"),
            "a failing doctor must name a code: {stderr}"
        );
    }

    let out = fx.cairn().args(["doctor", "--json"]).output().unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("json");
    assert_eq!(json["checks"].as_array().map(Vec::len), Some(25));
    assert!(json["summary"].get("fail").is_some());
}

/// Sessions list is empty and honest before M3 (SPEC §11.1).
#[test]
fn sessions_empty_state() {
    let fx = Fixture::new();
    let out = fx.cairn().arg("sessions").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("no sessions"), "{text}");

    let out = fx.cairn().args(["sessions", "--json"]).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "[]",
        "machine output must be an array"
    );
}

/// A stored session is found by id and its restored state is reported
/// (T-SESS-013 through the CLI).
#[test]
fn resume_of_a_real_session_reports_its_state() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    run_json(&fx, &["-p", "hi"]);
    let (id, _) = only_session(&fx);

    let out = fx.cairn().args(["resume", &id, "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let state: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(state["session_id"], id.as_str());
    assert_eq!(state["messages"], 2);
    assert_eq!(state["next_turn_id"], 2);
    assert_eq!(state["mode"], "build");
    assert_eq!(state["dangling"], false);

    let out = fx.cairn().args(["resume", &id]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!("cairn run --session {id}")),
        "{text}"
    );
}

// ------------------------------------------------- session fixtures (M1)

/// One stored session, one line per record (SPEC §11.7).
fn put_session(fx: &Fixture, id: &str, records: &[&str]) -> PathBuf {
    let dir = fx.home.join("data/sessions/ws16");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let mut text = String::new();
    for record in records {
        text.push_str(record);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn header_record(id: &str) -> String {
    serde_json::json!({
        "v": 1, "type": "header", "schema_version": 1, "session_id": id,
        "created_at": "2026-10-03T00:00:00.000Z", "workspace": "/ws", "mode": "build",
        "model": "anthropic/claude-sonnet-4-5", "cairn_version": "0.1.0",
        "ruleset_version": null, "parent_session": null,
    })
    .to_string()
}

/// A `message` record: the §4.1 `Message` nested under `message` (SPEC §11.7).
fn message_record(seq: u64, role: &str, text: &str) -> String {
    serde_json::json!({
        "v": 1, "type": "message", "seq": seq, "ts": "2026-10-03T00:00:01.000Z",
        "turn_id": 1,
        "message": {
            "id": format!("m{seq}"), "role": role,
            "blocks": [{"kind": "text", "text": text}],
            "created_at": "2026-10-03T00:00:01.000Z",
            "usage": null, "turn_id": 1,
        },
    })
    .to_string()
}

fn tool_result_record(seq: u64) -> String {
    serde_json::json!({
        "v": 1, "type": "tool_result", "seq": seq, "ts": "2026-10-03T00:00:02.000Z",
        "turn_id": 1, "call_id": "c1", "name": "bash", "ok": true,
        "output": "done", "duration_ms": 5, "truncated": false,
    })
    .to_string()
}

/// A session that holds one secret, for the export redaction cases.
fn session_with_a_secret(fx: &Fixture) {
    put_session(
        fx,
        "ses_sec",
        &[
            &header_record("ses_sec"),
            &message_record(1, "user", "my token is token = sk-abcdefghijklmnop123456"),
            &message_record(2, "assistant", "hello"),
            &tool_result_record(3),
        ],
    );
}

/// T-CLI-015: a session id that is not stored is a resource-not-found, not a
/// usage error (SPEC §11.2 exit 9).
#[test]
fn t_cli_015_unknown_session_is_exit_9() {
    let fx = Fixture::new();
    for args in [vec!["export", "ses_missing"], vec!["resume", "ses_missing"]] {
        let out = fx.cairn().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(9), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("E-SESS-NOTFOUND"), "{args:?}: {stderr}");
    }
}

/// T-SESS-023: a record that is not JSON makes `export`/`resume` fail with
/// `E-SESS-CORRUPT` (exit 9) and leaves the file byte-identical.
#[test]
fn t_see_023_a_corrupt_record_is_reported_and_never_rewritten() {
    let fx = Fixture::new();
    let path = put_session(
        &fx,
        "ses_bad",
        &[
            &header_record("ses_bad"),
            &message_record(1, "user", "first"),
            "not json",
            &message_record(2, "assistant", "third"),
        ],
    );
    let before = std::fs::read(&path).unwrap();

    for args in [vec!["export", "ses_bad"], vec!["resume", "ses_bad"]] {
        let out = fx.cairn().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(9), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("E-SESS-CORRUPT"), "{args:?}: {stderr}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{args:?} must not touch the file"
        );
    }
}

/// T-SESS-031: md/json/html all render, and `--output PATH` writes a file
/// (stdout stays the default for every format).
#[test]
fn t_see_031_export_renders_every_format_and_can_write_a_file() {
    let fx = Fixture::new();
    session_with_a_secret(&fx);

    for (format, needle) in [
        ("md", "## User"),
        ("json", "\"messages\""),
        ("html", "<!doctype html>"),
    ] {
        let out = fx
            .cairn()
            .args(["export", "ses_sec", "--format", format])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{format}: {out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(needle), "{format}: {stdout}");
    }

    let dest = fx.home.join("session.html");
    let out = fx
        .cairn()
        .args([
            "export",
            "ses_sec",
            "--format",
            "html",
            "--output",
            &dest.display().to_string(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty(), "stdout is not the destination here");
    let written = std::fs::read_to_string(&dest).unwrap();
    assert!(written.starts_with("<!doctype html>"), "{}", &written[..40]);

    // A destination that cannot be created is reported, not silently dropped.
    let out = fx
        .cairn()
        .args([
            "export",
            "ses_sec",
            "--output",
            &fx.home.join("nope/x.html").display().to_string(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(9), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("E-FS-NOPARENT"),
        "{out:?}"
    );
}

/// T-SEC-012 / REQ-CLI-010: redaction is on unless it was deliberately, and
/// verifiably, turned off.
#[test]
fn t_sec_012_export_redacts_by_default_and_refuses_to_stop() {
    let fx = Fixture::new();
    session_with_a_secret(&fx);

    // Default: the secret never reaches stdout.
    let out = fx
        .cairn()
        .args(["export", "ses_sec", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("sk-abcdefghijklmnop123456"),
        "redacted by default: {stdout}"
    );
    assert!(stdout.contains("REDACTED"), "{stdout}");

    // Off, with no terminal to ask and no env escape hatch: refuse (exit 2).
    let out = fx
        .cairn()
        .args(["export", "ses_sec", "--no-redact", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("E-CLI-USAGE"),
        "{out:?}"
    );

    // The documented script escape hatch works.
    let out = fx
        .cairn()
        .env("CAIRN_ALLOW_UNREDACTED_EXPORT", "1")
        .args(["export", "ses_sec", "--no-redact", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("sk-abcdefghijklmnop123456"),
        "{out:?}"
    );
}

/// `cairn migrate` (SPEC §11.7.1, REQ-CLI-009): a v0 session and a v0
/// `config.toml` both move to the schema this build writes, each keeping a
/// backup first — and it must run *on* a config `validate` would reject.
#[test]
fn migrate_upgrades_sessions_and_config_with_backups() {
    let fx = Fixture::new();

    let dir = fx.home.join("data/sessions/ws16");
    std::fs::create_dir_all(&dir).unwrap();
    let session = dir.join("ses_old.jsonl");
    let v0 = serde_json::json!({
        "v": 1, "type": "header", "schema_version": 0, "session_id": "ses_old",
        "created_at": "2026-10-03T00:00:00.000Z", "workspace": "/ws", "mode": "build",
        "model": "m", "cairn_version": "0.0.1",
    })
    .to_string();
    std::fs::write(&session, format!("{v0}\n")).unwrap();

    std::fs::write(
        fx.user_config(),
        "schema_version = 0\n# keep this comment\n[providers.openai]\napi_key_env = \"OPENAI_API_KEY\"\n",
    )
    .unwrap();

    let out = fx.cairn().arg("migrate").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ses_old"), "{stdout}");
    assert!(stdout.contains("schema_version 0 → 1"), "{stdout}");

    let text = std::fs::read_to_string(&session).unwrap();
    assert!(text.contains("\"schema_version\":1"), "{text}");
    assert!(dir.join("ses_old.jsonl.bak-v0").exists(), "session backup");

    let cfg = std::fs::read_to_string(fx.user_config()).unwrap();
    assert!(cfg.contains("schema_version = 1"), "{cfg}");
    assert!(cfg.contains("# keep this comment"), "comments survive");
    assert!(cfg.contains("api_key_env"), "content survives");
    assert!(
        fx.home.join("config/config.toml.bak-v0").exists(),
        "config backup: {cfg}"
    );

    // A second run changes nothing.
    let out = fx.cairn().arg("migrate").output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nothing to migrate"),
        "{out:?}"
    );
}

/// A config written by a *newer* Cairn is never downgraded (SPEC §0: no
/// downgrades) — `E-CFG-VERSION`, exit 2, bytes untouched.
#[test]
fn migrate_refuses_to_downgrade_a_newer_config() {
    let fx = Fixture::new();
    let newer = "schema_version = 2\n# from the future\n";
    std::fs::write(fx.user_config(), newer).unwrap();

    let out = fx.cairn().arg("migrate").output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("E-CFG-VERSION"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(fx.user_config()).unwrap(),
        newer,
        "a refusal must not rewrite the file"
    );
}

/// `--quiet` never swallows the code line (REQ-CLI-002, global flag).
#[test]
fn quiet_is_a_global_flag() {
    let fx = Fixture::new();
    let out = fx
        .cairn()
        .args(["config", "get", "no.such.key", "--quiet"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("E-CFG-UNKNOWN"), "{stderr}");
    assert!(!stderr.contains("hint:"), "{stderr}");
}

/// `mcp add` / `remove` manage `[[mcp.servers]]` in one layer (SPEC §11.1).
#[test]
fn mcp_add_and_remove_roundtrip() {
    let fx = Fixture::new();
    fx.cairn()
        .args([
            "mcp",
            "add",
            "docs",
            "--transport",
            "stdio",
            "--command",
            "npx",
            "--arg",
            "-y",
        ])
        .assert()
        .code(0);

    let out = fx.cairn().args(["mcp", "list"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("docs"), "{text}");
    assert!(text.contains("not_connected"), "{text}");

    // Duplicates are refused.
    fx.cairn()
        .args([
            "mcp",
            "add",
            "docs",
            "--transport",
            "stdio",
            "--command",
            "npx",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-DUPNAME"));

    fx.cairn().args(["mcp", "remove", "docs"]).assert().code(0);
    let out = fx.cairn().args(["mcp", "list", "--json"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.trim(), "[]", "{text}");
}

/// Stub subcommands name the milestone that delivers them (SPEC §15.4).
#[test]
fn stubs_name_their_milestone() {
    let fx = Fixture::new();
    let cases: [(&[&str], &str); 6] = [
        (&["chat"], "M3"),
        (&["init"], "M3"),
        (&["update"], "M5"),
        (&["mcp", "inspect", "x"], "M4"),
        (&["mcp", "refresh"], "M4"),
        (&["auth", "login", "anthropic"], "M1"),
    ];
    for (args, milestone) in cases {
        let out = fx.cairn().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("E-IMPL-STAGE"), "{args:?}: {stderr}");
        assert!(stderr.contains(milestone), "{args:?}: {stderr}");
    }
}

/// Never a bare `unimplemented!()` — stubs are data, not panics (SPEC §15).
#[test]
fn no_bare_unimplemented_paths() {
    let fx = Fixture::new();
    // A panic would exit 101 with a Rust backtrace instead of a stable code.
    for args in [
        vec!["chat"],
        vec!["init"],
        vec!["update"],
        vec!["migrate"],
        vec!["export", "ses_x"],
    ] {
        let out = fx.cairn().args(&args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("panicked"), "{args:?} panicked: {stderr}");
        assert_ne!(out.status.code(), Some(101), "{args:?}");
    }
}

/// The binary never touches the network in M0 (offline by construction).
#[test]
fn version_needs_no_config() {
    let fx = Fixture::new();
    // Even a broken config cannot stop `--version` (it never loads config).
    std::fs::write(fx.user_config(), "this is not toml [[[[").unwrap();
    let out = fx.cairn().arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("cairn"));
}

// ------------------------------------------------- T-CLI-010 `run -p` live

/// A loopback `OpenAI` server: serves `response` to the next `takes`
/// connections, capturing each request body for assertions.
fn serve_loopback(
    response: Vec<u8>,
    takes: usize,
    bodies: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback");
    let address = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for _ in 0..takes {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read_exact(&mut byte).is_err() || head.len() > 1 << 20 {
                    return;
                }
                head.push(byte[0]);
            }
            let length = String::from_utf8_lossy(&head)
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length.min(1 << 20)];
            if !body.is_empty() && socket.read_exact(&mut body).is_err() {
                return;
            }
            bodies.lock().expect("bodies").push(body);
            if socket.write_all(&response).is_err() {
                return;
            }
        }
    });
    format!("http://{address}")
}

fn sse_ok(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn success_body() -> String {
    concat!(
        "data: {\"id\":\"c\",\"model\":\"gpt-5.1-codex\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2},\"choices\":[]}\n\n",
        "data: [DONE]\n\n",
    )
    .to_string()
}

/// Point the fixture's config at `base_url` for the `OpenAI` provider.
fn point_at_loopback(fx: &Fixture, base_url: &str) {
    std::fs::write(
        fx.user_config(),
        format!(
            "model = \"openai/gpt-5.1-codex\"\n\n[providers.openai]\nbase_url = \"{base_url}\"\n"
        ),
    )
    .expect("config");
}

/// T-CLI-010's success third and T-CLI-017: all three formats against one
/// loopback turn — text prints the answer, `json` prints one object, and
/// every `stream-json` line parses with no ANSI anywhere.
#[test]
fn run_succeeds_in_all_three_formats() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 3, Arc::clone(&bodies));

    for format in ["text", "json", "stream-json"] {
        let fx = Fixture::new();
        point_at_loopback(&fx, &url);
        let out = fx
            .cairn()
            .env("CAIRN_OPENAI_API_KEY", "test")
            .args(["run", "-p", "hi", "--output", format])
            .output()
            .expect("runs");
        assert_eq!(out.status.code(), Some(0), "{format}: {out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            !text.contains('\u{1b}'),
            "{format}: no ANSI escapes on stdout"
        );
        match format {
            "text" => assert_eq!(text, "Hi\n"),
            "json" => {
                let value: serde_json::Value =
                    serde_json::from_str(&text).expect("one JSON object");
                assert_eq!(value["schema_version"], 1);
                assert_eq!(value["status"], "ok");
                assert_eq!(value["exit_code"], 0);
                assert_eq!(value["turn_id"], 1);
                assert_eq!(value["messages"][0]["role"], "assistant");
                assert_eq!(value["messages"][0]["blocks"][0]["text"], "Hi");
                assert_eq!(value["tool_calls"], serde_json::json!([]));
                assert_eq!(value["usage"]["input"], 10);
                assert_eq!(
                    value["usage"]["estimated"], false,
                    "provider numbers are real"
                );
                // The registry carries no price for this model: null, never 0 (REQ-PROV-012).
                assert!(value["cost_usd"].is_null(), "{value}");
                assert_eq!(value["stop"], "end_turn");
                assert!(value["plan"].is_null());
                assert!(value["error"].is_null());
            }
            _ => {
                let kinds = envelope_kinds(&text);
                assert_eq!(
                    kinds,
                    [
                        "session.created",
                        "turn.started",
                        "model.request",
                        "model.delta",
                        "model.usage",
                        "message.appended",
                        "turn.ended"
                    ],
                    "{text}"
                );
            }
        }
    }

    // The prompt reached the server as the shaped request body.
    let bodies = bodies.lock().expect("bodies");
    assert_eq!(bodies.len(), 3);
    for body in bodies.iter() {
        let request: serde_json::Value = serde_json::from_slice(body).expect("shaped JSON");
        assert_eq!(request["model"], "gpt-5.1-codex");
        assert_eq!(request["messages"][0]["content"], "hi");
    }
}

/// Parse every stdout line as a §3.5 envelope and return the `type`s. Every
/// line must be JSON with the five envelope keys and `v: 1` (T-CLI-017).
fn envelope_kinds(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("every stdout line is JSON");
            for key in ["v", "seq", "ts", "session", "type"] {
                assert!(value.get(key).is_some(), "envelope lacks `{key}`: {line}");
            }
            assert_eq!(value["v"], 1);
            value["type"].as_str().expect("type").to_string()
        })
        .collect()
}

/// The one session file `cairn sessions --json` lists, as (id, path).
fn only_session(fx: &Fixture) -> (String, PathBuf) {
    let out = fx
        .cairn()
        .args(["sessions", "--json"])
        .output()
        .expect("lists");
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON listing");
    let list = value.as_array().expect("array");
    assert_eq!(list.len(), 1, "{value}");
    (
        list[0]["session_id"].as_str().expect("id").to_string(),
        PathBuf::from(list[0]["path"].as_str().expect("path")),
    )
}

fn records(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("session file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("record"))
        .collect()
}

fn run_json(fx: &Fixture, extra: &[&str]) -> (std::process::Output, serde_json::Value) {
    let mut args = vec!["run", "--output", "json"];
    args.extend_from_slice(extra);
    let out = fx
        .cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(args)
        .output()
        .expect("runs");
    let doc = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (out, doc)
}

/// §11.7: one run writes `header`, `turn_started`, user message, assistant
/// message, `turn_ended` — in that order, with usage and cost on the end.
#[test]
fn a_run_persists_its_turn_to_the_session_store() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    let (out, doc) = run_json(&fx, &["-p", "hi"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let (id, path) = only_session(&fx);
    assert_eq!(doc["session_id"], id.as_str());
    let kinds: Vec<String> = records(&path)
        .iter()
        .map(|r| r["type"].as_str().expect("type").to_string())
        .collect();
    assert_eq!(
        kinds,
        ["header", "turn_started", "message", "message", "turn_ended"]
    );
    let all = records(&path);
    let ended = all.last().expect("ended");
    assert_eq!(ended["status"], "ok");
    assert_eq!(ended["usage"]["input"], 10);
    assert_eq!(all[2]["message"]["role"], "user");
    assert_eq!(all[3]["message"]["role"], "assistant");
}

/// `--session ID` continues: the second request carries the first exchange,
/// the file gains a second turn, and nothing is duplicated.
#[test]
fn run_session_continues_with_the_earlier_history() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 2, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    run_json(&fx, &["-p", "first"]);
    let (id, path) = only_session(&fx);
    let (out, doc) = run_json(&fx, &["-p", "second", "--session", &id]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(doc["turn_id"], 2);
    assert_eq!(doc["session_id"], id.as_str());

    let bodies = bodies.lock().expect("bodies");
    let second: serde_json::Value = serde_json::from_slice(&bodies[1]).expect("body");
    let sent: Vec<String> = second["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| {
            format!(
                "{}:{}",
                m["role"].as_str().unwrap_or(""),
                m["content"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(sent, ["user:first", "assistant:Hi", "user:second"]);
    let turns = records(&path)
        .iter()
        .filter(|r| r["type"] == "turn_ended")
        .count();
    assert_eq!(turns, 2);
}

/// T-CLI-015: an unknown session is exit 9 with `E-SESS-NOTFOUND`, before
/// any request is made.
#[test]
fn t_cli_015_run_with_an_unknown_session_is_exit_9() {
    let fx = Fixture::new();
    let (out, _) = run_json(&fx, &["-p", "hi", "--session", "01NOPE"]);
    assert_eq!(out.status.code(), Some(9));
    assert!(String::from_utf8_lossy(&out.stderr).contains("E-SESS-NOTFOUND"));
}

/// `--input` (text and json) reaches the model ahead of the prompt.
#[test]
fn run_input_preloads_context() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 2, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);

    let text_input = fx.ws.join("context.txt");
    std::fs::write(&text_input, "background notes\n").expect("input");
    let (out, _) = run_json(
        &fx,
        &["-p", "go", "--input", text_input.to_str().expect("utf8")],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let json_input = fx.ws.join("context.jsonl");
    std::fs::write(
        &json_input,
        "{\"role\":\"user\",\"content\":\"q1\"}\n{\"role\":\"assistant\",\"content\":\"a1\"}\n",
    )
    .expect("input");
    let (out, _) = run_json(
        &fx,
        &[
            "-p",
            "go",
            "--input",
            json_input.to_str().expect("utf8"),
            "--input-fmt",
            "json",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let bodies = bodies.lock().expect("bodies");
    let contents = |body: &[u8]| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_slice(body).expect("body");
        v["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|m| m["content"].as_str().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(contents(&bodies[0]), ["background notes", "go"]);
    assert_eq!(contents(&bodies[1]), ["q1", "a1", "go"]);
}

/// A malformed `--input` line is a usage error naming the line, and no
/// session is created for it.
#[test]
fn run_input_with_a_bad_line_is_a_usage_error() {
    let fx = Fixture::new();
    let input = fx.ws.join("bad.jsonl");
    std::fs::write(&input, "{\"role\":\"user\",\"content\":\"ok\"}\nnot json\n").expect("input");
    let (out, _) = run_json(
        &fx,
        &[
            "-p",
            "go",
            "--input",
            input.to_str().expect("utf8"),
            "--input-fmt",
            "json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("line 2"), "{stderr}");
}

/// §7.7: a failed turn still prints exactly one JSON document on stdout —
/// status `error`, the fault's code, `exit_code` 3 — and exits 3.
#[test]
fn a_failed_turn_prints_an_error_document_and_exits_3() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let denied = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    let url = serve_loopback(denied.as_bytes().to_vec(), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    let (out, doc) = run_json(&fx, &["-p", "hi"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(doc["status"], "error");
    assert_eq!(doc["exit_code"], 3);
    assert_eq!(doc["error"]["code"], "E-PROV-AUTH");
    assert_eq!(doc["messages"], serde_json::json!([]));

    // The failure is on disk too: an `error` record and an `error` turn end.
    let (_, path) = only_session(&fx);
    let all = records(&path);
    assert!(all
        .iter()
        .any(|r| r["type"] == "error" && r["code"] == "E-PROV-AUTH"));
    assert_eq!(all.last().expect("last")["status"], "error");
}

/// A stream-json failure ends with `model.error` then `turn.ended{error}`,
/// and still nothing but envelopes on stdout.
#[test]
fn stream_json_failure_ends_with_model_error_and_turn_ended() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let denied = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    let url = serve_loopback(denied.as_bytes().to_vec(), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    let out = fx
        .cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(["run", "-p", "hi", "--output", "stream-json"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    let kinds = envelope_kinds(&String::from_utf8_lossy(&out.stdout));
    assert_eq!(
        kinds,
        [
            "session.created",
            "turn.started",
            "model.request",
            "model.error",
            "turn.ended"
        ]
    );
}

/// §8.7 headless: a session whose last turn never ended resumes with the
/// banner, the committed prompt stays in context, and a second run neither
/// repeats the banner nor duplicates anything (REQ-LOOP-007).
#[test]
fn run_session_recovers_a_dangling_turn_once() {
    use std::io::Write;
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 2, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    run_json(&fx, &["-p", "first"]);
    let (id, path) = only_session(&fx);

    // Simulate a crash mid-turn 2: started, prompt durable, no end.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open");
    let user = cairn_core::Message::user("interrupted", 2);
    let seq = records(&path).len() as u64 + 1;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&cairn_session::Record::turn_started(2, seq)).expect("json")
    )
    .expect("write");
    writeln!(
        file,
        "{}",
        serde_json::to_string(&cairn_session::Record::message(&user, seq + 1)).expect("json")
    )
    .expect("write");
    drop(file);

    let (out, _) = run_json(&fx, &["-p", "after", "--session", &id]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Recovered interrupted turn 2"), "{stderr}");

    let sent: serde_json::Value =
        serde_json::from_slice(&bodies.lock().expect("bodies")[1]).expect("body");
    let texts: Vec<&str> = sent["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["content"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(texts, ["first", "Hi", "interrupted", "after"]);

    // Turn 2 was closed, turn 3 ran; nothing is dangling any more.
    let all = records(&path);
    let closed: Vec<(u64, String)> = all
        .iter()
        .filter(|r| r["type"] == "turn_ended")
        .map(|r| {
            (
                r["turn_id"].as_u64().unwrap_or(0),
                r["status"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    assert_eq!(
        closed,
        [
            (1, "ok".to_string()),
            (2, "recovered".to_string()),
            (3, "ok".to_string())
        ]
    );
}

/// Append a started turn with its prompt durable and no end: what a crash
/// mid-turn leaves behind (§8.7).
fn make_dangling(path: &std::path::Path, turn: u64, prompt: &str) {
    use std::io::Write;
    let seq = records(path).len() as u64 + 1;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open");
    let started = cairn_session::Record::turn_started(turn, seq);
    let user = cairn_core::Message::user(prompt, turn);
    let message = cairn_session::Record::message(&user, seq + 1);
    for record in [started, message] {
        writeln!(file, "{}", serde_json::to_string(&record).expect("json")).expect("write");
    }
}

/// T-SESS-022 (`r`): resuming with `auto_recover` re-issues the model call
/// over the committed messages exactly once; running resume again finds
/// nothing dangling, sends nothing, and duplicates nothing (T-SESS-021).
#[test]
fn t_sess_022_rebuild_reissues_the_model_call_once() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 2, Arc::clone(&bodies));
    let fx = Fixture::new();
    std::fs::write(
        fx.user_config(),
        format!(
            "model = \"openai/gpt-5.1-codex\"\n\n[session]\nauto_recover = true\n\n\
             [providers.openai]\nbase_url = \"{url}\"\n"
        ),
    )
    .expect("config");
    run_json(&fx, &["-p", "first"]);
    let (id, path) = only_session(&fx);
    make_dangling(&path, 2, "interrupted");

    let out = fx
        .cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(["resume", &id])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Recovered interrupted turn 2"));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Hi\n");
    {
        let sent = bodies.lock().expect("bodies");
        assert_eq!(sent.len(), 2, "the original run plus one re-issue");
        let body: serde_json::Value = serde_json::from_slice(&sent[1]).expect("body");
        let texts: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|m| m["content"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(texts, ["first", "Hi", "interrupted"]);
    }

    let closed = |records: &[serde_json::Value]| -> Vec<String> {
        records
            .iter()
            .filter(|r| r["type"] == "turn_ended")
            .map(|r| r["status"].as_str().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(closed(&records(&path)), ["ok", "interrupted", "ok"]);

    // Second resume: no banner, no model call, no new records.
    let before = records(&path).len();
    let out = fx
        .cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(["resume", &id])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("Recovered"));
    assert_eq!(bodies.lock().expect("bodies").len(), 2);
    assert_eq!(records(&path).len(), before);
}

/// Without a terminal and without `auto_recover`, resume keeps the partial
/// turn as context (`k`) and makes no model call.
#[test]
fn resume_without_a_terminal_keeps_the_partial_turn() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    run_json(&fx, &["-p", "first"]);
    let (id, path) = only_session(&fx);
    make_dangling(&path, 2, "interrupted");

    let out = fx.cairn().args(["resume", &id, "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let state: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(
        state["messages"], 3,
        "the interrupted prompt stays in context"
    );
    assert_eq!(state["dangling"], false);
    assert_eq!(bodies.lock().expect("bodies").len(), 1, "no model call");
    let last = records(&path).pop().expect("last");
    assert_eq!(
        (last["type"].as_str(), last["status"].as_str()),
        (Some("turn_ended"), Some("recovered"))
    );
}

/// T-CLI-010's failure third: a 401 is exit 3 with the stable code and the
/// login hint — no retries, no success output.
#[test]
fn run_model_failure_is_exit_3_with_code() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let denied = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    let url = serve_loopback(denied.as_bytes().to_vec(), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    let out = fx
        .cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(["run", "-p", "hi"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("E-PROV-AUTH"), "{stderr}");
    assert!(stderr.contains("cairn auth login"), "{stderr}");
    assert!(String::from_utf8_lossy(&out.stdout).is_empty());
}

/// An id the registry does not know — via REQ-PROV-013's escape hatch, so
/// validation passes it and the failure surfaces at build time — fails
/// before any socket, as usage error 2 with the validator's code.
#[test]
fn run_unknown_model_is_exit_2() {
    let fx = Fixture::new();
    std::fs::write(
        fx.user_config(),
        "model = \"my-custom-model\"\n\n[models.\"my-custom-model\"]\ncontext_window = 8000\n",
    )
    .expect("config");
    fx.cairn()
        .args(["run", "-p", "hi"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-NOMODEL"));
}

/// A `base_url` that is not a URL never reaches the adapter: validation
/// substitutes the registry default (`W-CFG-FALLBACK`), and the run proceeds
/// against it — here, into the no-key failure.
#[test]
fn run_bad_base_url_falls_back_to_default() {
    let fx = Fixture::new();
    point_at_loopback(&fx, "not a url");
    let out = fx
        .cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(["run", "-p", "hi"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("W-CFG-FALLBACK"), "{stderr}");
    assert!(stderr.contains("E-PROV-AUTH"), "{stderr}");
}

/// Anything but the three run formats is exit 2 — caught by startup
/// validation before `run` itself ever sees the value.
#[test]
fn run_rejects_unknown_output_format() {
    let fx = Fixture::new();
    fx.cairn()
        .args(["run", "-p", "hi", "--output", "bogus"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("E-CFG-BADVALUE"));
}

/// `--prompt-file` feeds the prompt: the file's text is what the server
/// receives.
#[test]
fn run_prompt_file_feeds_the_prompt() {
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = serve_loopback(sse_ok(&success_body()), 1, Arc::clone(&bodies));
    let fx = Fixture::new();
    point_at_loopback(&fx, &url);
    let prompt = fx.ws.join("prompt.txt");
    std::fs::write(&prompt, "from a file").expect("prompt");
    fx.cairn()
        .env("CAIRN_OPENAI_API_KEY", "test")
        .args(["run", "--prompt-file", prompt.to_str().expect("utf8")])
        .assert()
        .code(0);
    let bodies = bodies.lock().expect("bodies");
    assert_eq!(bodies.len(), 1);
    let request: serde_json::Value = serde_json::from_slice(&bodies[0]).expect("shaped JSON");
    assert_eq!(request["messages"][0]["content"], "from a file");
}
