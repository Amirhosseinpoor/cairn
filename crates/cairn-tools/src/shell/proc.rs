//! Running a command line in a child shell (SPEC §6.4): which shell, which
//! environment, output caps, timeouts, and killing the whole process group.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_sandbox::process::{signal_group, Signal};

use crate::types::ToolError;

/// §6.4.3: the only variables a child inherits.
const ENV_ALLOWLIST: [&str; 24] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "TZ",
    "PWD",
    "OLDPWD",
    "TMPDIR",
    "COLORTERM",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
    "SSH_AUTH_SOCK",
    "GOPATH",
    "CARGO_HOME",
    "JAVA_HOME",
    "NODE_PATH",
    "PYTHONPATH",
    "VIRTUAL_ENV",
    "npm_config_registry",
    "CI",
];

/// §6.4.3's name denylist, applied to inherited variables.
fn secret_name(name: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(^|_)(SECRET|TOKEN|PASSWORD|APIKEY|API_KEY|CREDENTIAL|PRIVATE)")
            .expect("static regex")
    })
    .is_match(name)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The environment a child gets: the allowlist from this process, then the
/// model's `env` on top of it.
#[must_use]
pub fn child_env(extra: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for name in ENV_ALLOWLIST {
        if secret_name(name) {
            continue;
        }
        if let Ok(value) = std::env::var(name) {
            out.insert(name.to_string(), value);
        }
    }
    out.entry("TERM".to_string())
        .or_insert_with(|| "dumb".to_string());
    for (name, value) in extra {
        if valid_name(name) {
            out.insert(name.clone(), value.clone());
        }
    }
    out.into_iter().collect()
}

/// A shell and how to hand it a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: String,
    pub args: Vec<String>,
}

impl Shell {
    /// What the result reports as `shell`.
    #[must_use]
    pub fn display(&self) -> &str {
        &self.program
    }
}

/// §6.4.1.
///
/// # Errors
/// `E-SHELL-NOEXEC` when no shell can be found.
pub fn pick_shell() -> Result<Shell, ToolError> {
    if let Some(chosen) = std::env::var("CAIRN_SHELL").ok().filter(|s| !s.is_empty()) {
        return Ok(shell_named(&chosen));
    }
    #[cfg(windows)]
    {
        let git_bash = r"C:\Program Files\Git\bin\bash.exe";
        if std::path::Path::new(git_bash).exists() {
            return Ok(shell_named(git_bash));
        }
        return Ok(shell_named("pwsh.exe"));
    }
    #[cfg(not(windows))]
    {
        for candidate in [
            "/bin/bash",
            "/usr/bin/bash",
            "/bin/zsh",
            "/usr/bin/fish",
            "/bin/sh",
        ] {
            if std::path::Path::new(candidate).exists() {
                return Ok(shell_named(candidate));
            }
        }
        Err(
            ToolError::new(codes::SHELL_NOEXEC, "no shell was found on this machine")
                .recovery("Set CAIRN_SHELL to a shell binary."),
        )
    }
}

fn shell_named(program: &str) -> Shell {
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let args = if base.starts_with("pwsh") || base.starts_with("powershell") {
        vec!["-NoLogo".to_string(), "-Command".to_string()]
    } else {
        vec!["-c".to_string()]
    };
    Shell {
        program: program.to_string(),
        args,
    }
}

/// What to run.
#[derive(Debug, Clone)]
pub struct Spec {
    pub command: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// Bytes for the command's stdin; `None` closes it.
    pub stdin: Option<String>,
}

/// Start the shell with `spec`: a process-group leader with piped output.
///
/// # Errors
/// `E-SHELL-NOEXEC` when the shell cannot be started.
pub fn spawn(shell: &Shell, spec: &Spec) -> Result<Child, ToolError> {
    let mut command = Command::new(&shell.program);
    // REQ-TOOL-016: the command line is one argument, never re-quoted.
    command
        .args(&shell.args)
        .arg(&spec.command)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0200);
    }
    let mut child = command.spawn().map_err(|e| {
        ToolError::new(
            codes::SHELL_NOEXEC,
            format!("could not start `{}`: {e}", shell.program),
        )
        .recovery("Check that the shell exists and the working directory is readable.")
    })?;
    if let (Some(input), Some(mut pipe)) = (spec.stdin.clone(), child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(input.as_bytes());
        });
    }
    Ok(child)
}

/// How a foreground run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Exited,
    TimedOut,
    Cancelled,
}

/// What a run produced.
#[derive(Debug, Clone)]
pub struct Captured {
    pub ending: Ending,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Output beyond the cap (or the line cap) was dropped.
    pub truncated: bool,
}

struct Budget {
    used: usize,
    cap: usize,
    dropped: bool,
}

struct Reader {
    buf: Arc<Mutex<Vec<u8>>>,
    handle: JoinHandle<()>,
}

fn read_stream<R: Read + Send + 'static>(mut stream: R, budget: Arc<Mutex<Budget>>) -> Reader {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buf);
    let handle = std::thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            let n = match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            // Keep reading after the cap so the child never blocks on a full
            // pipe; the surplus is simply dropped (§6.4.4).
            let take = {
                let mut b = budget.lock().unwrap_or_else(PoisonError::into_inner);
                let room = b.cap.saturating_sub(b.used);
                let take = room.min(n);
                b.used += take;
                if take < n {
                    b.dropped = true;
                }
                take
            };
            if take > 0 {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&chunk[..take]);
            }
        }
    });
    Reader { buf, handle }
}

/// Stop the process group: the polite signal, a grace period, then the sure
/// one (§6.4.6).
pub fn terminate(child: &mut Child, grace: Duration) {
    let pid = child.id();
    if signal_group(pid, Signal::Term).is_err() {
        let _ = child.kill();
    }
    let until = Instant::now() + grace;
    while Instant::now() < until {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if signal_group(pid, Signal::Kill).is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

const LINE_CAP: usize = 5_000;
/// After the shell exits, how long to wait for the pipes to close. A
/// background grandchild can hold them open indefinitely.
const PIPE_GRACE: Duration = Duration::from_millis(300);

fn finish(reader: &Reader) -> (String, bool) {
    let until = Instant::now() + PIPE_GRACE;
    while !reader.handle.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(5));
    }
    let bytes = reader
        .buf
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let lines = text.lines().count();
    if lines > LINE_CAP {
        let kept: Vec<&str> = text.lines().take(LINE_CAP).collect();
        return (kept.join("\n"), true);
    }
    (text, false)
}

#[cfg(unix)]
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

/// Run `spec` to completion, to `timeout`, or to cancellation. Blocking: call
/// it from `spawn_blocking`.
///
/// # Errors
/// `E-SHELL-NOEXEC` when the shell cannot be started.
pub fn run(
    shell: &Shell,
    spec: &Spec,
    timeout: Duration,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<Captured, ToolError> {
    let mut child = spawn(shell, spec)?;
    let budget = Arc::new(Mutex::new(Budget {
        used: 0,
        cap,
        dropped: false,
    }));
    let out = child
        .stdout
        .take()
        .map(|s| read_stream(s, Arc::clone(&budget)));
    let err = child
        .stderr
        .take()
        .map(|s| read_stream(s, Arc::clone(&budget)));
    let deadline = Instant::now() + timeout;
    let (ending, status) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Ending::Exited, Some(status)),
            Ok(None) => {}
            Err(_) => break (Ending::Exited, None),
        }
        if cancel.is_cancelled() {
            terminate(&mut child, Duration::from_millis(1000));
            break (Ending::Cancelled, child.try_wait().ok().flatten());
        }
        if Instant::now() >= deadline {
            terminate(&mut child, Duration::from_millis(2000));
            break (Ending::TimedOut, child.try_wait().ok().flatten());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (stdout, cut_out) = out.map_or((String::new(), false), |r| finish(&r));
    let (stderr, cut_err) = err.map_or((String::new(), false), |r| finish(&r));
    let dropped = budget
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .dropped;
    Ok(Captured {
        ending,
        exit_code: status.as_ref().and_then(std::process::ExitStatus::code),
        signal: status.and_then(signal_of),
        stdout,
        stderr,
        truncated: dropped || cut_out || cut_err,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn spec(command: &str) -> Spec {
        Spec {
            command: command.to_string(),
            cwd: std::env::temp_dir(),
            env: child_env(&BTreeMap::new()),
            stdin: None,
        }
    }

    fn run_it(command: &str, timeout_ms: u64, cap: usize) -> Captured {
        run(
            &pick_shell().expect("shell"),
            &spec(command),
            Duration::from_millis(timeout_ms),
            cap,
            &CancellationToken::new(),
        )
        .expect("runs")
    }

    #[test]
    fn output_and_exit_code_are_captured_separately() {
        let c = run_it("echo out; echo err >&2; exit 3", 5000, 65536);
        assert_eq!(c.ending, Ending::Exited);
        assert_eq!(c.exit_code, Some(3));
        assert_eq!(c.stdout, "out\n");
        assert_eq!(c.stderr, "err\n");
        assert!(!c.truncated);
    }

    #[test]
    fn a_timeout_kills_the_whole_group_and_keeps_partial_output() {
        let started = Instant::now();
        let c = run_it("echo started; sleep 30 & sleep 30; echo never", 300, 65536);
        assert_eq!(c.ending, Ending::TimedOut);
        assert!(c.stdout.contains("started"));
        assert!(!c.stdout.contains("never"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn cancelling_stops_the_command() {
        let token = CancellationToken::new();
        let flip = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flip.cancel();
        });
        let started = Instant::now();
        let c = run(
            &pick_shell().expect("shell"),
            &spec("sleep 30"),
            Duration::from_secs(60),
            65536,
            &token,
        )
        .expect("runs");
        assert_eq!(c.ending, Ending::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_past_the_cap_is_dropped_without_killing_the_command() {
        let c = run_it("yes x | head -c 200000; echo done >&2", 10_000, 1000);
        assert!(c.truncated);
        assert_eq!(c.stdout.len() + c.stderr.len(), 1000);
        assert_eq!(c.exit_code, Some(0));
    }

    #[test]
    fn the_environment_is_an_allowlist() {
        std::env::set_var("CAIRN_TEST_SECRET_TOKEN", "hunter2");
        let mut extra = BTreeMap::new();
        extra.insert("MINE".to_string(), "yes".to_string());
        let env = child_env(&extra);
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert!(env.iter().any(|(k, v)| k == "MINE" && v == "yes"));
        assert!(!env.iter().any(|(k, _)| k == "CAIRN_TEST_SECRET_TOKEN"));
        let c = run(
            &pick_shell().expect("shell"),
            &Spec {
                command: "echo \"[$CAIRN_TEST_SECRET_TOKEN][$MINE]\"".to_string(),
                cwd: std::env::temp_dir(),
                env,
                stdin: None,
            },
            Duration::from_secs(5),
            1024,
            &CancellationToken::new(),
        )
        .expect("runs");
        assert_eq!(c.stdout, "[][yes]\n");
    }

    #[test]
    fn stdin_is_delivered_then_closed() {
        let mut s = spec("cat");
        s.stdin = Some("hello\n".to_string());
        let c = run(
            &pick_shell().expect("shell"),
            &s,
            Duration::from_secs(5),
            1024,
            &CancellationToken::new(),
        )
        .expect("runs");
        assert_eq!(c.stdout, "hello\n");
    }

    #[test]
    fn a_background_grandchild_holding_the_pipe_does_not_hang_the_call() {
        let started = Instant::now();
        let c = run_it("sleep 20 & echo hi", 10_000, 1024);
        assert_eq!(c.stdout, "hi\n");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
