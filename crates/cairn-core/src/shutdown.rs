//! Process-exit policy — SPEC §3.3 (REQ-ARCH-008).
//!
//! > Process exit MUST cancel the root token, wait up to 500 ms for flush of
//! > the session file, then `_exit` (no hangs). If flush fails, exit code `13`.
//!
//! The flush itself belongs to `cairn-session` (M1); what belongs here is the
//! *budget*, because it is the part every caller has to obey and the part the
//! test can pin down: a slow or broken flush must never turn into a hang.
//!
//! The call returns an [`ExitStatus`] and never exits the process — `main`
//! decides between a clean return and `std::process::exit`, so the exit code
//! stays testable (T-ARCH-008).

use crate::cancel::CancellationToken;
use crate::error::ExitStatus;
use std::time::{Duration, Instant};

/// REQ-ARCH-008: the flush gets at most 500 ms before the process gives up on
/// it and exits anyway.
pub const FLUSH_BUDGET: Duration = Duration::from_millis(500);

/// Cancel `root`, then give `flush` at most [`FLUSH_BUDGET`] to finish.
///
/// Returns the exit status `main` should use: [`ExitStatus::Ok`] when the flush
/// completed cleanly, [`ExitStatus::Flush`] (`13`) when it failed, panicked, or
/// did not finish inside the budget. In the last case the flush thread is
/// detached and dies with the process — that is the "no hangs" half of
/// REQ-ARCH-008.
pub fn shutdown_with_flush<F>(root: &CancellationToken, flush: F) -> ExitStatus
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    root.cancel();

    let handle = std::thread::Builder::new()
        .name("cairn-flush".into())
        .spawn(flush)
        .expect("flush thread spawns");

    let deadline = Instant::now() + FLUSH_BUDGET;
    loop {
        if handle.is_finished() {
            return match handle.join() {
                Ok(Ok(())) => ExitStatus::Ok,
                // An explicit failure and a panicking flush are the same exit (13).
                Ok(Err(_)) | Err(_) => ExitStatus::Flush,
            };
        }
        if Instant::now() >= deadline {
            return ExitStatus::Flush;
        }
        // Cheap, bounded, and never a blocking wait on the flush itself.
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// The buffer the flush drains, and the sink it writes to.
    type Buffer = Arc<Mutex<VecDeque<String>>>;
    /// Where flushed records land (stands in for the session file).
    type Sink = Arc<Mutex<Vec<String>>>;

    /// A stand-in for the session store: records buffered in memory, drained by
    /// the flush closure.
    fn store(pending: usize) -> (Buffer, Sink) {
        let buffer = Arc::new(Mutex::new(
            (0..pending).map(|i| format!("record {i}\n")).collect(),
        ));
        let sink = Arc::new(Mutex::new(Vec::new()));
        (buffer, sink)
    }

    /// T-ARCH-008 — 200 pending writes drain inside the 500 ms budget and the
    /// root token is cancelled first (REQ-ARCH-008).
    #[test]
    fn t_arch_008_200_pending_writes_flush_within_budget() {
        let root = CancellationToken::new();
        let turn = root.child();
        let (buffer, sink) = store(200);

        let flush_buffer = Arc::clone(&buffer);
        let flush_sink = Arc::clone(&sink);
        let started = Instant::now();
        let status = shutdown_with_flush(&root, move || {
            let drained: Vec<String> = {
                let mut buf = flush_buffer.lock().expect("buffer");
                buf.drain(..).collect()
            };
            assert_eq!(drained.len(), 200, "every buffered record is flushed");
            flush_sink.lock().expect("sink").extend(drained);
            Ok(())
        });
        let elapsed = started.elapsed();

        assert_eq!(status, ExitStatus::Ok);
        assert_eq!(status.code(), 0);
        assert!(root.is_cancelled(), "the root token is cancelled first");
        assert!(turn.is_cancelled(), "and so is every descendant");
        assert!(buffer.lock().expect("buffer").is_empty(), "queue drained");
        assert_eq!(sink.lock().expect("sink").len(), 200);
        assert!(
            elapsed <= FLUSH_BUDGET + Duration::from_millis(250),
            "shutdown took {elapsed:?}, budget is {FLUSH_BUDGET:?}"
        );
    }

    /// T-ARCH-008 — a flush that never finishes is abandoned at the budget,
    /// never a hang (REQ-ARCH-008).
    #[test]
    fn t_arch_008_a_stuck_flush_cannot_hang_the_process() {
        let root = CancellationToken::new();
        let started = Instant::now();
        let status = shutdown_with_flush(&root, || {
            std::thread::sleep(Duration::from_secs(10));
            Ok(())
        });
        let elapsed = started.elapsed();

        assert_eq!(status, ExitStatus::Flush);
        assert_eq!(status.code(), 13, "ERR_FLUSH (REQ-ARCH-008, §11.2)");
        assert!(
            elapsed >= FLUSH_BUDGET && elapsed < FLUSH_BUDGET + Duration::from_millis(250),
            "gave up after {elapsed:?}, budget is {FLUSH_BUDGET:?}"
        );
        assert!(
            root.is_cancelled(),
            "cancellation is not contingent on the flush"
        );
    }

    /// T-ARCH-008 — an explicit flush failure is exit 13 (REQ-ARCH-008).
    #[test]
    fn t_arch_008_a_failed_flush_exits_13() {
        let root = CancellationToken::new();
        let status = shutdown_with_flush(&root, || Err("fsync: read-only filesystem".into()));
        assert_eq!(status, ExitStatus::Flush);
        assert_eq!(status.code(), 13);
        assert!(root.is_cancelled());
    }

    /// A panicking flush is a failed flush, not an unwind through `main`.
    #[test]
    fn a_panicking_flush_is_contained() {
        let root = CancellationToken::new();
        let status = shutdown_with_flush(&root, || panic!("disk went away"));
        assert_eq!(status.code(), 13);
    }

    /// Cancelling first means a slow flush still observes the cancellation.
    #[test]
    fn cancellation_precedes_the_flush() {
        let root = CancellationToken::new();
        let child = root.child();
        let seen_by_flush = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&seen_by_flush);
        let status = shutdown_with_flush(&root, move || {
            *flag.lock().expect("flag") = child.is_cancelled();
            Ok(())
        });
        assert_eq!(status, ExitStatus::Ok);
        assert!(*seen_by_flush.lock().expect("flag"));
    }
}
