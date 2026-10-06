//! Background jobs (SPEC §6.4.7): processes that outlive the call that
//! started them, with output the model can read later.
//!
//! A job's output is kept as numbered lines in a ring: the first 8 KiB (so the
//! beginning of a build is never lost) and the last 64 KiB / 5,000 lines.
//! `output` reads from a line number and can wait for more, which is how a
//! model follows a long-running job without polling in a loop.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::event::EventData;
use cairn_sandbox::process::{signal_group, Signal};

use super::proc::{spawn, Shell, Spec};
use crate::types::{EventSink, ToolError};

/// §6.4.7: running jobs at once.
pub const MAX_RUNNING: usize = 8;
const HEAD_BYTES: usize = 8 * 1024;
const TAIL_BYTES: usize = 64 * 1024;
const TAIL_LINES: usize = 5_000;
const MAX_LINE: u64 = 8 * 1024;
/// Finished jobs kept around so their output can still be read.
const KEEP_FINISHED: usize = 32;

/// Where a job is in §6.4.7's state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    Failed,
    Killed,
}

impl Status {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
}

/// One line of a job's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub n: u64,
    pub stream: &'static str,
    pub text: String,
}

#[derive(Debug)]
struct State {
    head: Vec<Line>,
    head_bytes: usize,
    tail: VecDeque<Line>,
    tail_bytes: usize,
    next_n: u64,
    bytes_total: u64,
    status: Status,
    exit_code: Option<i32>,
    signal: Option<i32>,
    killed: bool,
}

/// One background process.
#[derive(Debug)]
pub struct Job {
    pub id: String,
    pub pid: u32,
    pub label: String,
    pub command: String,
    pub started_at: String,
    started: Instant,
    state: Mutex<State>,
    changed: Condvar,
}

/// What `output` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub lines: Vec<Line>,
    pub next_since_line: u64,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub bytes_total: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Job {
    fn push(&self, stream: &'static str, text: String) {
        let mut s = lock(&self.state);
        let line = Line {
            n: s.next_n,
            stream,
            text,
        };
        s.next_n += 1;
        s.bytes_total += line.text.len() as u64 + 1;
        let size = line.text.len() + 1;
        if s.tail.is_empty() && s.head_bytes < HEAD_BYTES {
            s.head_bytes += size;
            s.head.push(line);
        } else {
            s.tail_bytes += size;
            s.tail.push_back(line);
            while s.tail_bytes > TAIL_BYTES || s.tail.len() > TAIL_LINES {
                if let Some(old) = s.tail.pop_front() {
                    s.tail_bytes -= old.text.len() + 1;
                }
            }
        }
        drop(s);
        self.changed.notify_all();
    }

    fn finish(&self, code: Option<i32>, signal: Option<i32>) {
        let mut s = lock(&self.state);
        s.exit_code = code;
        s.signal = signal;
        s.status = if s.killed {
            Status::Killed
        } else if code == Some(0) {
            Status::Done
        } else {
            Status::Failed
        };
        drop(s);
        self.changed.notify_all();
    }

    fn snapshot(s: &State, since: u64, max_lines: usize) -> Snapshot {
        let lines: Vec<Line> = s
            .head
            .iter()
            .chain(s.tail.iter())
            .filter(|l| l.n >= since)
            .take(max_lines)
            .cloned()
            .collect();
        let next = lines
            .last()
            .map_or(since.max(first_n(s)).min(s.next_n), |l| l.n + 1);
        Snapshot {
            lines,
            next_since_line: next,
            status: s.status,
            exit_code: s.exit_code,
            bytes_total: s.bytes_total,
        }
    }

    /// Lines from `since` on. With `wait`, blocks until there is something
    /// new, the job ends, or the time is up.
    #[must_use]
    pub fn output(
        &self,
        since: u64,
        max_lines: usize,
        wait: Duration,
        cancel: &CancellationToken,
    ) -> Snapshot {
        let deadline = Instant::now() + wait;
        let mut s = lock(&self.state);
        loop {
            let has_new = s.next_n > since;
            if has_new || s.status != Status::Running || cancel.is_cancelled() {
                return Self::snapshot(&s, since, max_lines);
            }
            let now = Instant::now();
            if now >= deadline {
                return Self::snapshot(&s, since, max_lines);
            }
            let step = (deadline - now).min(Duration::from_millis(50));
            let (guard, _) = self
                .changed
                .wait_timeout(s, step)
                .unwrap_or_else(PoisonError::into_inner);
            s = guard;
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        lock(&self.state).status
    }

    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        lock(&self.state).exit_code
    }

    /// Signal the job's process group.
    ///
    /// # Errors
    /// `E-JOB-NOTFOUND` is not possible here; the OS error text when the
    /// signal cannot be sent to a job that is still running.
    pub fn kill(&self, signal: Signal) -> Result<bool, String> {
        let mut s = lock(&self.state);
        if s.status != Status::Running {
            return Ok(false);
        }
        if signal != Signal::Int {
            s.killed = true;
        }
        drop(s);
        signal_group(self.pid, signal).map_err(|e| e.to_string())?;
        Ok(true)
    }
}

fn first_n(s: &State) -> u64 {
    s.head.first().map_or(0, |l| l.n)
}

/// Every job this process started.
#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    counter: AtomicU64,
}

fn read_lines<R: Read>(stream: R, stream_name: &'static str, job: &Job) {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.by_ref().take(MAX_LINE).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                job.push(stream_name, String::from_utf8_lossy(&buf).into_owned());
            }
        }
    }
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

impl JobTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Jobs still running.
    #[must_use]
    pub fn running(&self) -> usize {
        lock(&self.jobs)
            .values()
            .filter(|j| j.status() == Status::Running)
            .count()
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        lock(&self.jobs).get(id).cloned()
    }

    /// Start `spec` as a job.
    ///
    /// # Errors
    /// `E-JOB-LIMIT` at eight running jobs; `E-SHELL-NOEXEC` when the shell
    /// cannot be started.
    pub fn start(
        &self,
        shell: &Shell,
        spec: &Spec,
        label: &str,
        events: Arc<dyn EventSink>,
    ) -> Result<Arc<Job>, ToolError> {
        // Count and insert under one lock so two starts cannot both pass.
        let mut jobs = lock(&self.jobs);
        let running = jobs
            .values()
            .filter(|j| j.status() == Status::Running)
            .count();
        if running >= MAX_RUNNING {
            return Err(ToolError::new(
                codes::JOB_LIMIT,
                format!("{MAX_RUNNING} background jobs are already running."),
            )
            .recovery("Stop one with job_kill, or wait for one to finish."));
        }
        let mut spec = spec.clone();
        spec.stdin = None;
        let mut child = spawn(shell, &spec)?;
        let id = format!(
            "job_{:04x}",
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let job = Arc::new(Job {
            id: id.clone(),
            pid: child.id(),
            label: label.to_string(),
            command: spec.command.clone(),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            started: Instant::now(),
            state: Mutex::new(State {
                head: Vec::new(),
                head_bytes: 0,
                tail: VecDeque::new(),
                tail_bytes: 0,
                next_n: 0,
                bytes_total: 0,
                status: Status::Running,
                exit_code: None,
                signal: None,
                killed: false,
            }),
            changed: Condvar::new(),
        });
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            let j = Arc::clone(&job);
            readers.push(std::thread::spawn(move || read_lines(out, "stdout", &j)));
        }
        if let Some(err) = child.stderr.take() {
            let j = Arc::clone(&job);
            readers.push(std::thread::spawn(move || read_lines(err, "stderr", &j)));
        }
        let waiter = Arc::clone(&job);
        std::thread::spawn(move || {
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break None,
                }
            };
            // Let the readers drain what is buffered, but not forever: a
            // grandchild can keep the pipes open.
            let until = Instant::now() + Duration::from_millis(300);
            while readers.iter().any(|r| !r.is_finished()) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            let code = status.as_ref().and_then(std::process::ExitStatus::code);
            let signal = status.and_then(signal_of);
            waiter.finish(code, signal);
            events.emit(EventData::JobFinished {
                job_id: waiter.id.clone(),
                exit_code: code,
                duration_ms: u64::try_from(waiter.started.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
            });
        });
        jobs.insert(id, Arc::clone(&job));
        // Old finished jobs go, oldest first, so the table cannot grow
        // without bound.
        let finished: Vec<String> = jobs
            .iter()
            .filter(|(_, j)| j.status() != Status::Running)
            .map(|(k, _)| k.clone())
            .collect();
        for key in finished
            .iter()
            .take(finished.len().saturating_sub(KEEP_FINISHED))
        {
            jobs.remove(key);
        }
        Ok(job)
    }

    /// Stop every running job: the orphan guard of §6.4.6.
    pub fn kill_all(&self) {
        let jobs: Vec<Arc<Job>> = lock(&self.jobs).values().cloned().collect();
        for job in jobs {
            let _ = job.kill(Signal::Term);
        }
        std::thread::sleep(Duration::from_millis(50));
        for job in lock(&self.jobs).values() {
            if job.status() == Status::Running {
                let _ = signal_group(job.pid, Signal::Kill);
            }
        }
    }
}

impl Drop for JobTable {
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::shell::proc::{child_env, pick_shell};
    use crate::types::NullSink;

    fn spec(command: &str) -> Spec {
        Spec {
            command: command.to_string(),
            cwd: std::env::temp_dir(),
            env: child_env(&BTreeMap::new()),
            stdin: None,
        }
    }

    fn start(table: &JobTable, command: &str) -> Arc<Job> {
        table
            .start(
                &pick_shell().expect("shell"),
                &spec(command),
                command,
                Arc::new(NullSink),
            )
            .expect("starts")
    }

    fn wait_done(job: &Job) {
        for _ in 0..200 {
            if job.status() != Status::Running {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("job did not finish");
    }

    #[test]
    fn output_is_numbered_lines_readable_from_any_point() {
        let table = JobTable::new();
        let job = start(&table, "echo one; echo two >&2; echo three");
        wait_done(&job);
        let all = job.output(0, 100, Duration::ZERO, &CancellationToken::new());
        let texts: Vec<&str> = all.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(all.lines.len(), 3);
        assert!(texts.contains(&"one") && texts.contains(&"two") && texts.contains(&"three"));
        assert_eq!(all.status, Status::Done);
        assert_eq!(all.exit_code, Some(0));
        assert_eq!(all.next_since_line, 3);
        let rest = job.output(2, 100, Duration::ZERO, &CancellationToken::new());
        assert_eq!(rest.lines.len(), 1);
        assert_eq!(rest.lines[0].n, 2);
    }

    #[test]
    fn a_failing_job_is_failed_not_done() {
        let table = JobTable::new();
        let job = start(&table, "exit 4");
        wait_done(&job);
        assert_eq!(job.status(), Status::Failed);
        assert_eq!(job.exit_code(), Some(4));
    }

    #[test]
    fn waiting_returns_as_soon_as_output_arrives() {
        let table = JobTable::new();
        let job = start(&table, "sleep 0.3; echo late; sleep 5");
        let started = Instant::now();
        let seen = job.output(0, 10, Duration::from_secs(4), &CancellationToken::new());
        assert_eq!(seen.lines.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(seen.status, Status::Running);
        job.kill(Signal::Kill).expect("kills");
    }

    #[test]
    fn killing_a_job_ends_it_as_killed() {
        let table = JobTable::new();
        let job = start(&table, "sleep 30");
        assert!(job.kill(Signal::Term).expect("signals"));
        wait_done(&job);
        assert_eq!(job.status(), Status::Killed);
        // Killing a finished job is a no-op, not an error.
        assert!(!job.kill(Signal::Term).expect("noop"));
    }

    #[test]
    fn at_most_eight_jobs_run_at_once() {
        let table = JobTable::new();
        let jobs: Vec<_> = (0..MAX_RUNNING)
            .map(|_| start(&table, "sleep 30"))
            .collect();
        let err = table
            .start(
                &pick_shell().expect("shell"),
                &spec("sleep 30"),
                "ninth",
                Arc::new(NullSink),
            )
            .unwrap_err();
        assert_eq!(err.code, codes::JOB_LIMIT);
        assert_eq!(table.running(), MAX_RUNNING);
        for job in &jobs {
            job.kill(Signal::Kill).expect("kills");
        }
    }

    #[test]
    fn the_ring_keeps_the_start_and_the_end() {
        let table = JobTable::new();
        let job = start(
            &table,
            "i=0; while [ $i -lt 8000 ]; do echo line$i; i=$((i+1)); done",
        );
        wait_done(&job);
        let all = job.output(0, 10_000, Duration::ZERO, &CancellationToken::new());
        assert_eq!(all.lines.first().map(|l| l.text.as_str()), Some("line0"));
        assert_eq!(all.lines.last().map(|l| l.text.as_str()), Some("line7999"));
        assert!(all.lines.len() <= TAIL_LINES + 1500, "{}", all.lines.len());
        assert!(all.lines.len() < 8000, "the middle was dropped");
    }

    #[test]
    fn dropping_the_table_stops_what_it_started() {
        let pid = {
            let table = JobTable::new();
            let job = start(&table, "sleep 30");
            job.pid
        };
        std::thread::sleep(Duration::from_millis(300));
        // The group is gone: signalling it fails.
        assert!(signal_group(pid, Signal::Term).is_err());
    }
}
