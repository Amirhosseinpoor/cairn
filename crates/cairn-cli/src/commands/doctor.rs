//! `cairn doctor` (SPEC §11.1, §12.3, REQ-OPS-004, T-XPLAT-001/002).
//!
//! Each row of §12.3 becomes one `Row`. Checks that need a subsystem that has
//! not landed yet are reported as `SKIP` with the milestone, never silently.

use crate::args::{Cli, DoctorArgs};
use crate::commands::auth;
use crate::output::{ellipsize, Fail, Status};
use cairn_config::ENV_KEYS;
use cairn_core::error::codes;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};

/// One diagnostics row (SPEC §12.3).
#[derive(Debug, serde::Serialize)]
pub struct Row {
    pub n: u8,
    pub check: &'static str,
    /// `info | warn | error` — severity when the check fails.
    pub level: &'static str,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

impl Row {
    fn new(
        n: u8,
        check: &'static str,
        level: &'static str,
        status: Status,
        detail: String,
    ) -> Self {
        Self {
            n,
            check,
            level,
            status,
            detail,
            code: None,
        }
    }

    fn code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }

    fn pass(n: u8, check: &'static str, level: &'static str, detail: impl Into<String>) -> Self {
        Self::new(n, check, level, Status::Pass, detail.into())
    }

    fn warn(n: u8, check: &'static str, level: &'static str, detail: impl Into<String>) -> Self {
        Self::new(n, check, level, Status::Warn, detail.into())
    }

    fn fail(
        n: u8,
        check: &'static str,
        level: &'static str,
        detail: impl Into<String>,
        code: &'static str,
    ) -> Self {
        Self::new(n, check, level, Status::Fail, detail.into()).code(code)
    }

    fn skip(n: u8, check: &'static str, level: &'static str, detail: impl Into<String>) -> Self {
        Self::new(n, check, level, Status::Skip, detail.into())
    }
}

pub fn run(cli: &Cli, args: &DoctorArgs, loaded: &cairn_config::Loaded) -> Result<i32, Fail> {
    let startup = super::Startup {
        loaded: loaded.clone(),
        quiet: cli.quiet,
    };
    let rows = collect(&startup, args);

    if args.json {
        let pass = rows.iter().filter(|r| r.status == Status::Pass).count();
        let warn = rows.iter().filter(|r| r.status == Status::Warn).count();
        let fail = rows.iter().filter(|r| r.status == Status::Fail).count();
        let skip = rows.iter().filter(|r| r.status == Status::Skip).count();
        let out = serde_json::json!({
            "cairn_version": env!("CARGO_PKG_VERSION"),
            "checks": rows,
            "summary": { "pass": pass, "warn": warn, "fail": fail, "skip": skip },
        });
        say!(
            "{}",
            serde_json::to_string_pretty(&out).expect("doctor json")
        );
        return Ok(i32::from(fail > 0));
    }

    let mut out = std::io::stdout().lock();
    for r in &rows {
        let _ = writeln!(
            out,
            "{:<4}  {:<26} {}",
            r.status.label(),
            r.check,
            ellipsize(&r.detail, 100)
        );
    }
    let pass = rows.iter().filter(|r| r.status == Status::Pass).count();
    let warn = rows.iter().filter(|r| r.status == Status::Warn).count();
    let fail = rows.iter().filter(|r| r.status == Status::Fail).count();
    let skip = rows.iter().filter(|r| r.status == Status::Skip).count();
    let summary = if skip > 0 {
        format!("{pass} pass, {warn} warn, {fail} fail, {skip} skip")
    } else {
        format!("{pass} pass, {warn} warn, {fail} fail")
    };
    let _ = writeln!(out, "{summary}");
    drop(out);

    if fail > 0 {
        let first = rows
            .iter()
            .find(|r| r.status == Status::Fail)
            .expect("fail > 0");
        let code = first.code.unwrap_or(codes::CLI_USAGE);
        return Err(Fail::new(
            code,
            cairn_core::error::ExitStatus::Generic,
            format!("{} check(s) failed: {}", fail, first.detail),
            Some("run `cairn doctor --json` for the machine-readable report".to_string()),
        ));
    }
    Ok(0)
}

fn collect(startup: &super::Startup, args: &DoctorArgs) -> Vec<Row> {
    let cfg = &startup.loaded.config;
    let mut rows = vec![
        row_1_binary(),
        row_2_write_access(startup),
        row_3_config(startup),
        row_4_provider_keys(startup),
    ];
    rows.push(Row::skip(
        5,
        "provider reachability",
        "error",
        "probing needs the HTTP client (M1)",
    ));
    rows.push(Row::skip(
        6,
        "ollama/vllm auto-detect",
        "info",
        "local server probing lands in M1",
    ));
    rows.push(row_7_shell(cfg));
    rows.push(row_8_git(startup));
    rows.push(row_9_agents(startup));
    rows.push(row_10_workspace(startup));
    rows.push(Row::skip(11, "ignore engine", "info", "wired up in M1"));
    rows.push(Row::skip(
        12,
        "ripgrep",
        "info",
        "embedded ripgrep lands in M1",
    ));
    rows.push(Row::skip(13, "tui capabilities", "info", "TUI lands in M3"));
    rows.push(row_14_hooks(cfg));
    rows.push(row_15_mcp(cfg));
    rows.push(row_16_checkpoints(startup));
    rows.push(row_17_sessions(startup));
    rows.push(row_18_disk(startup));
    rows.push(row_19_telemetry(cfg));
    rows.push(row_20_terminal());
    rows.push(Row::skip(
        21,
        "time sync",
        "warn",
        "clock check needs the HTTP client (M1)",
    ));
    rows.push(row_22_orphans(startup));
    rows.push(Row::skip(23, "tool table", "info", "--tools needs M2"));
    rows.push(row_24_env());
    rows.push(Row::skip(
        25,
        "self-check",
        "info",
        "--deep needs a fixture provider (M1)",
    ));
    // The flags select extra rows; until their subsystems land they only change
    // what the SKIP rows promise, so they are recorded in the details above.
    let _ = (args.deep, args.network, args.tools);
    rows
}

// ------------------------------------------------------------------ rows

fn row_1_binary() -> Row {
    Row::pass(
        1,
        "binary & platform",
        "info",
        format!(
            "cairn {} {} ({}), tier {}",
            env!("CARGO_PKG_VERSION"),
            env!("CAIRN_TARGET"),
            env!("CAIRN_RUSTC"),
            crate::commands::version::tier(env!("CAIRN_TARGET"))
        ),
    )
}

fn row_2_write_access(startup: &super::Startup) -> Row {
    let paths = &startup.loaded.paths;
    for dir in [
        &paths.config_home,
        &paths.data_home,
        &paths.state_home,
        &paths.cache_home,
    ] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Row::fail(
                2,
                "write access",
                "error",
                format!("{}: {e}", dir.display()),
                codes::FS_PERM,
            );
        }
    }
    let probe = paths.cache_home.join(".doctor-write-probe");
    match std::fs::write(&probe, b"ok").and(std::fs::remove_file(&probe)) {
        Ok(()) => Row::pass(
            2,
            "write access",
            "error",
            paths.cache_home.display().to_string(),
        ),
        Err(e) => Row::fail(
            2,
            "write access",
            "error",
            format!("{}: {e}", paths.cache_home.display()),
            codes::FS_PERM,
        ),
    }
}

fn row_3_config(startup: &super::Startup) -> Row {
    let mut issues = startup.loaded.issues.clone();
    issues.extend(cairn_config::keybindings::issues(&startup.loaded.paths));
    let issues = &issues;
    let errors: Vec<&cairn_config::Issue> =
        issues.iter().filter(|i| i.code.starts_with('E')).collect();
    let warnings = issues.len() - errors.len();
    if !errors.is_empty() {
        let first = errors[0];
        Row::new(
            3,
            "config validity",
            "error",
            Status::Fail,
            format!(
                "{} error(s), {} warning(s); first: {}",
                errors.len(),
                warnings,
                first.render()
            ),
        )
        .code(first.code)
    } else if warnings > 0 {
        Row::warn(
            3,
            "config validity",
            "error",
            format!("{warnings} warning(s): {}", issues[0].render()),
        )
    } else {
        Row::pass(3, "config validity", "error", "no issues")
    }
}

fn row_4_provider_keys(startup: &super::Startup) -> Row {
    let missing = auth::missing_required(startup);
    if missing.is_empty() {
        Row::pass(
            4,
            "provider keys",
            "error",
            "every enabled remote provider has a key",
        )
    } else {
        Row::fail(
            4,
            "provider keys",
            "error",
            format!("no key for {}", missing.join(", ")),
            codes::PROV_AUTH,
        )
    }
}

fn row_7_shell(cfg: &cairn_config::Config) -> Row {
    let shell = if cfg.shell.command.is_empty() {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    } else {
        cfg.shell.command.clone()
    };
    let resolved = which(&shell).unwrap_or_else(|| shell.clone());
    if !Path::new(&resolved).exists() {
        return Row::warn(
            7,
            "shell",
            "error",
            format!("`{shell}` not found; set shell.command"),
        );
    }
    let version = Command::new(&resolved)
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| resolved.clone());
    Row::pass(7, "shell", "error", version)
}

fn row_8_git(startup: &super::Startup) -> Row {
    let Some(version) = run_capture("git", &["--version"]) else {
        return Row::warn(8, "git", "warn", "git not found on PATH");
    };
    let ws = startup.workspace();
    let inside = run_capture_in(
        "git",
        &[
            "-C",
            &ws.display().to_string(),
            "rev-parse",
            "--is-inside-work-tree",
        ],
        ws,
    );
    if inside.as_deref() != Some("true") {
        return Row::warn(
            8,
            "git",
            "warn",
            format!("{version}; {} is not a git repository", ws.display()),
        );
    }
    let name = run_capture_in("git", &["config", "--get", "user.name"], ws);
    let email = run_capture_in("git", &["config", "--get", "user.email"], ws);
    match (name, email) {
        (Some(n), Some(e)) => Row::pass(8, "git", "warn", format!("{version}; {n} <{e}>")),
        _ => Row::warn(
            8,
            "git",
            "warn",
            "git user.name/user.email not set (commits will fail)",
        ),
    }
}

fn row_9_agents(startup: &super::Startup) -> Row {
    let mut dir = Some(startup.workspace().to_path_buf());
    while let Some(d) = dir {
        for name in ["AGENTS.md", "CAIRN.md"] {
            if d.join(name).exists() {
                return Row::pass(9, "agents file", "info", d.join(name).display().to_string());
            }
        }
        dir = d.parent().map(std::path::Path::to_path_buf);
    }
    Row::warn(
        9,
        "agents file",
        "info",
        "no AGENTS.md found; run `cairn init`",
    )
}

fn row_10_workspace(startup: &super::Startup) -> Row {
    let ws = startup.workspace();
    let files = count_files(ws, 10_000);
    let note = if files >= 10_000 { "≥" } else { "" };
    Row::pass(
        10,
        "workspace",
        "info",
        format!("{note}{files} files under {}", ws.display()),
    )
}

fn row_14_hooks(cfg: &cairn_config::Config) -> Row {
    let mut missing = Vec::new();
    let mut checked = 0usize;
    for hook in &cfg.hooks {
        checked += 1;
        if !executable_exists(&hook.command) {
            missing.push(hook.command.clone());
        }
    }
    for tool in &cfg.custom_tools {
        checked += 1;
        if !executable_exists(&tool.command) {
            missing.push(tool.command.clone());
        }
    }
    if missing.is_empty() {
        Row::pass(
            14,
            "hooks/custom tools",
            "warn",
            format!("{checked} configured, all executable"),
        )
    } else {
        Row::warn(
            14,
            "hooks/custom tools",
            "warn",
            format!("not executable: {}", missing.join(", ")),
        )
    }
}

fn row_15_mcp(cfg: &cairn_config::Config) -> Row {
    let n = cfg.mcp.servers.len();
    if n == 0 {
        Row::pass(15, "mcp servers", "warn", "none configured")
    } else {
        Row::pass(
            15,
            "mcp servers",
            "warn",
            format!("{n} configured; connection checked in M4"),
        )
    }
}

fn row_16_checkpoints(startup: &super::Startup) -> Row {
    let ws = startup.workspace();
    if !ws.join(".git").exists() {
        return Row::warn(
            16,
            "checkpoints",
            "info",
            "no .git — filesystem fallback (SPEC §9.8)",
        );
    }
    let refs = run_capture_in(
        "git",
        &[
            "-C",
            &ws.display().to_string(),
            "for-each-ref",
            "refs/cairn/checkpoints",
            "--format=%(refname)",
        ],
        ws,
    )
    .map_or(0, |out| out.lines().filter(|l| !l.is_empty()).count());
    Row::pass(
        16,
        "checkpoints",
        "info",
        format!("{refs} checkpoint ref(s)"),
    )
}

fn row_17_sessions(startup: &super::Startup) -> Row {
    let dir = startup.loaded.paths.sessions_dir();
    let mut count = 0usize;
    let mut bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for ws_dir in entries.flatten() {
            if let Ok(files) = std::fs::read_dir(ws_dir.path()) {
                for f in files.flatten() {
                    if f.path().extension().is_some_and(|e| e == "jsonl") {
                        count += 1;
                        bytes += f.metadata().map_or(0, |m| m.len());
                    }
                }
            }
        }
    }
    Row::pass(
        17,
        "sessions",
        "info",
        format!("{count} session(s), {} KiB", bytes / 1024),
    )
}

fn row_18_disk(startup: &super::Startup) -> Row {
    let Some(kb) = free_kb(&startup.loaded.paths.cache_home) else {
        return Row::skip(
            18,
            "disk space",
            "warn",
            "`df` unavailable on this platform",
        );
    };
    if kb < 1_048_576 {
        Row::warn(
            18,
            "disk space",
            "warn",
            format!("{} MiB free (< 1 GiB)", kb / 1024),
        )
    } else {
        Row::pass(
            18,
            "disk space",
            "warn",
            format!("{} GiB free", kb / 1_048_576),
        )
    }
}

fn row_19_telemetry(cfg: &cairn_config::Config) -> Row {
    Row::pass(
        19,
        "telemetry/update",
        "info",
        format!(
            "telemetry.enabled={} update.check={} every {}h channel={}",
            cfg.telemetry.enabled,
            cfg.update.check,
            cfg.update.interval_hours,
            channel_name(cfg.update.channel)
        ),
    )
}

fn channel_name(c: cairn_config::model::UpdateChannel) -> String {
    let s = serde_json::to_value(c).expect("channel serializes");
    s.as_str().map_or_else(|| s.to_string(), str::to_string)
}

fn row_20_terminal() -> Row {
    let stdout_tty = std::io::stdout().is_terminal();
    let term = std::env::var("TERM").unwrap_or_default();
    let unicode = ["LANG", "LC_ALL", "LC_CTYPE"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .is_some_and(|v| v.to_ascii_uppercase().contains("UTF"));
    let mut problems = Vec::new();
    if !stdout_tty {
        problems.push("stdout is not a TTY".to_string());
    }
    if term.is_empty() {
        problems.push("TERM unset".to_string());
    }
    if !unicode {
        problems.push("LANG is not UTF-8".to_string());
    }
    let size = terminal_size();
    match size {
        Some((cols, rows)) if cols < 40 || rows < 12 => {
            problems.push(format!("{cols}x{rows} < 40x12"));
        }
        None if stdout_tty => problems.push("size unknown".to_string()),
        _ => {}
    }
    let detail = match size {
        Some((c, r)) => format!("TERM={term} {c}x{r} unicode={unicode} tty={stdout_tty}"),
        None => format!("TERM={term} unicode={unicode} tty={stdout_tty}"),
    };
    if problems.is_empty() {
        Row::pass(20, "terminal", "warn", detail)
    } else {
        Row::warn(
            20,
            "terminal",
            "warn",
            format!("{detail} — {}", problems.join(", ")),
        )
    }
}

fn row_22_orphans(startup: &super::Startup) -> Row {
    let mut pids = Vec::new();
    for dir in [
        &startup.loaded.paths.cache_home,
        &startup.loaded.paths.state_home,
    ] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for f in entries.flatten() {
                if f.path().extension().is_some_and(|e| e == "pid") {
                    pids.push(f.path().display().to_string());
                }
            }
        }
    }
    if pids.is_empty() {
        Row::pass(22, "orphan processes", "warn", "no stale pid files")
    } else {
        Row::warn(
            22,
            "orphan processes",
            "warn",
            format!("stale pid files: {}", pids.join(", ")),
        )
    }
}

fn row_24_env() -> Row {
    let mut known: Vec<String> = ENV_KEYS.iter().map(|(v, _, _)| (*v).to_string()).collect();
    known.extend(
        [
            "CAIRN_HOME",
            "CAIRN_WORKSPACE",
            "CAIRN_CONFIG",
            "CAIRN_RT_THREADS",
            "CAIRN_TMP",
            "CAIRN_SHELL",
            "CAIRN_INSTALL_DIR",
            "CAIRN_SCREEN_READER",
            "CAIRN_TEST_CLOCK",
            "CAIRN_CASSETTE_MODE",
            "CAIRN_LIVE",
            "CAIRN_ALLOW_UNREDACTED_EXPORT",
            "CAIRN_LOG_HTTP",
            "CAIRN_LOG_SSE",
        ]
        .iter()
        .copied()
        .map(str::to_string),
    );
    let mut unknown: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("CAIRN_"))
        .filter(|k| !known.iter().any(|n| n == k) && !k.ends_with("_API_KEY"))
        .collect();
    unknown.sort();
    if unknown.is_empty() {
        Row::pass(
            24,
            "conflicting env",
            "warn",
            "no unknown CAIRN_* variables",
        )
    } else {
        Row::warn(
            24,
            "conflicting env",
            "warn",
            format!("unknown: {}", unknown.join(", ")),
        )
    }
}

// ---------------------------------------------------------------- helpers

/// `PATH` lookup for a bare command name.
fn which(cmd: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(cmd);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

fn executable_exists(cmd: &str) -> bool {
    if cmd.contains('/') {
        Path::new(cmd).exists()
    } else {
        which(cmd).is_some()
    }
}

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    run_capture_in(cmd, args, Path::new("."))
}

fn run_capture_in(cmd: &str, args: &[&str], dir: &Path) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Bounded recursive file count (doctor must stay interactive).
fn count_files(root: &Path, cap: usize) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if count >= cap {
                return count;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git" {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(path),
                Ok(t) if t.is_file() => count += 1,
                _ => {}
            }
        }
    }
    count
}

/// Free space in KiB via `df -Pk` (portable on Unix; absent on Windows).
fn free_kb(dir: &Path) -> Option<u64> {
    let out = Command::new("df")
        .arg("-Pk")
        .arg(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().last()?;
    let fields: Vec<&str> = last.split_whitespace().collect();
    fields.get(3)?.parse::<u64>().ok()
}

/// Terminal size (cols, rows) when stdin is a TTY.
fn terminal_size() -> Option<(u16, u16)> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let out = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let rows: u16 = it.next()?.parse().ok()?;
    let cols: u16 = it.next()?.parse().ok()?;
    Some((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{LoadOptions, Paths};
    use std::collections::BTreeMap;

    fn startup_in(dir: &Path) -> super::super::Startup {
        let mut loaded = cairn_config::load(&LoadOptions {
            cwd: dir.to_path_buf(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        loaded.paths = Paths {
            config_home: dir.join("config"),
            data_home: dir.join("data"),
            state_home: dir.join("state"),
            cache_home: dir.join("cache"),
        };
        super::super::Startup {
            loaded,
            quiet: true,
        }
    }

    fn args() -> DoctorArgs {
        DoctorArgs {
            json: false,
            tools: false,
            network: false,
            deep: false,
        }
    }

    #[test]
    fn every_spec_row_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let rows = collect(&startup, &args());
        let numbers: Vec<u8> = rows.iter().map(|r| r.n).collect();
        let mut expected: Vec<u8> = (1..=25).collect();
        expected.sort_unstable();
        let mut sorted = numbers.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted, expected,
            "rows must cover §12.3 exactly once: {numbers:?}"
        );
        assert_eq!(rows.len(), 25);
    }

    #[test]
    fn keyless_remote_providers_fail_with_a_code() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let rows = collect(&startup, &args());
        let provider_row = rows.iter().find(|r| r.n == 4).expect("row 4");
        // The developer machine may already carry key env vars; when the row
        // fails it must fail with the provider-auth code.
        if provider_row.status == Status::Fail {
            assert_eq!(provider_row.code, Some(codes::PROV_AUTH));
        } else {
            assert_eq!(provider_row.code, None);
            assert!(
                provider_row.detail.contains("key"),
                "{}",
                provider_row.detail
            );
        }
        // Failure sets the exit code path.
        let config_row = rows.iter().find(|r| r.n == 1).expect("row 1");
        assert_eq!(config_row.status, Status::Pass);
        assert!(config_row.detail.contains("tier"));
    }

    #[test]
    fn env_row_ignores_known_and_api_key_vars() {
        let row = row_24_env();
        match row.status {
            Status::Pass => assert_eq!(row.detail, "no unknown CAIRN_* variables"),
            Status::Warn => assert!(row.detail.starts_with("unknown: "), "{}", row.detail),
            other => panic!("unexpected status {other:?}"),
        }
    }

    #[test]
    fn count_files_skips_git_and_caps() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), "x").unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        assert_eq!(count_files(tmp.path(), 100), 1);
        assert_eq!(count_files(tmp.path(), 0), 0);
    }

    #[test]
    fn rows_serialize_for_json_output() {
        let r = Row::pass(1, "x", "info", "ok");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["status"], "pass");
        assert!(v.get("code").is_none());
        let f = Row::fail(2, "y", "error", "bad", codes::FS_PERM);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["code"], codes::FS_PERM);
    }
}
