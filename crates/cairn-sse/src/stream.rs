//! The async half of SPEC §4.3: read a byte source under an idle timeout and
//! a cancellation token, and hand assembled [`SseEvent`]s to the caller.

use std::collections::VecDeque;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use cairn_core::cancel::CancellationToken;

use crate::error::SseError;
use crate::parser::{SseEvent, SseParser, MAX_EVENT_BYTES};
use crate::DEFAULT_IDLE_TIMEOUT;

/// §4.3 cancellation: `cairn-core`'s token is std-only by §3.2, so it has no
/// waker to hook into — the stream re-checks it on this cadence. 50 ms keeps
/// the observed latency at a quarter of §4.3's 250 ms budget (and REQ-ARCH-007
/// only asks executors to poll every 100 ms).
const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How a [`SseStream`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SseOptions {
    /// §4.3 idle timeout: no bytes for this long aborts with
    /// [`SseError::Idle`] (`E-PROV-IDLE`). `providers.<id>.idle_timeout_ms`.
    pub idle_timeout: Duration,
    /// §4.3 rule 4 per-event cap. `providers.<id>.max_event_bytes`.
    pub max_event_bytes: usize,
}

impl Default for SseOptions {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_event_bytes: MAX_EVENT_BYTES,
        }
    }
}

/// A byte source framed into [`SseEvent`]s.
///
/// Dropping the stream drops the underlying body, which is what aborts the HTTP
/// request within §4.3's 250 ms; raising [`CancellationToken`] does the same
/// from inside the loop, before the next read starts.
#[derive(Debug)]
pub struct SseStream<S> {
    inner: Pin<Box<S>>,
    parser: SseParser,
    idle: Duration,
    cancel: CancellationToken,
    ready: VecDeque<SseEvent>,
    ended: bool,
    /// Armed on the first read, re-armed only when bytes actually arrive.
    deadline: Option<tokio::time::Instant>,
}

impl<S, E> SseStream<S>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::fmt::Display,
{
    /// Frame `inner` under `options`, aborting on `cancel`.
    #[must_use]
    pub fn new(inner: S, options: SseOptions, cancel: CancellationToken) -> Self {
        Self {
            inner: Box::pin(inner),
            parser: SseParser::with_max_event_bytes(options.max_event_bytes),
            idle: options.idle_timeout,
            cancel,
            ready: VecDeque::new(),
            ended: false,
            deadline: None,
        }
    }

    /// The parser, so the consumer can read [`SseParser::lossy_count`] and log
    /// §4.3's `warn` for events that needed U+FFFD substitution.
    #[must_use]
    pub const fn parser(&self) -> &SseParser {
        &self.parser
    }

    /// Read until the next assembled event.
    ///
    /// `Ok(None)` means the source ended with nothing pending. Errors are the
    /// four [`SseError`] variants; [`SseError::code`] reports the `E-PROV-*`
    /// code §4.3 fixes for them.
    pub async fn next_event(&mut self) -> Result<Option<SseEvent>, SseError> {
        let mut poll = tokio::time::interval(CANCEL_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if let Some(event) = self.ready.pop_front() {
                return Ok(Some(event));
            }
            if self.ended {
                return Ok(None);
            }
            if self.cancel.is_cancelled() {
                return Err(SseError::Cancelled);
            }

            // Armed on the first read and re-armed only when bytes arrive: a
            // wake-up that delivers nothing must not restart the 45 s window
            // (§4.3 measures "no bytes for 45 s", not "no reads").
            let deadline = *self
                .deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + self.idle);

            tokio::select! {
                biased;
                // REQ-ARCH-007: the token is std-only (§3.2), so this poll is
                // what notices a cancel raised while the stream sits quiet.
                // Falling out of the select re-arms the read above; the
                // deadline is absolute, so a wake-up that delivers nothing
                // does not extend it.
                _ = poll.tick() => {}
                () = tokio::time::sleep_until(deadline) => {
                    return Err(SseError::Idle { timeout: self.idle });
                }
                result = self.inner.next() => match result {
                    None => {
                        self.ended = true;
                        // §4.3 rule 6: a pending event is dispatched at EOF so
                        // a server that omits the final blank line still gets
                        // its `[DONE]` / `message_stop` across.
                        if let Some(event) = self.parser.finish()? {
                            return Ok(Some(event));
                        }
                        return Ok(None);
                    }
                    Some(Err(err)) => return Err(SseError::Source(err.to_string())),
                    Some(Ok(bytes)) => {
                        self.deadline = Some(tokio::time::Instant::now() + self.idle);
                        let events = self.parser.feed(&bytes)?;
                        self.ready.extend(events);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn options(idle_timeout: Duration) -> SseOptions {
        SseOptions {
            idle_timeout,
            ..SseOptions::default()
        }
    }

    fn bytes(source: &'static [u8]) -> Bytes {
        Bytes::from_static(source)
    }

    /// T-PROV-020 through T-PROV-022 already pin the framing rules down in the
    /// parser; this checks the async half actually pipes them through.
    #[tokio::test]
    async fn events_flow_from_the_source_to_the_caller() {
        let source = futures::stream::iter(vec![
            Ok::<Bytes, io::Error>(bytes(b"data: par")),
            Ok(bytes(b"tial\n\ndata: second\n\n")),
        ]);
        let mut stream = SseStream::new(source, SseOptions::default(), CancellationToken::new());

        let first = stream.next_event().await.expect("first").expect("event");
        assert_eq!(first.data, "partial");
        let second = stream.next_event().await.expect("second").expect("event");
        assert_eq!(second.data, "second");
        assert_eq!(stream.next_event().await.expect("end"), None);
        assert_eq!(stream.next_event().await.expect("stays ended"), None);
    }

    /// T-PROV-023 — `: ping` is a comment, not an event, but its bytes reset
    /// the idle timer: the stream survives past 45 s and only gives up 45 s
    /// after the *last* byte, i.e. at t = 85 s rather than t = 45 s.
    #[tokio::test(start_paused = true)]
    async fn t_prov_023_a_comment_ping_resets_the_idle_timer() {
        let start = tokio::time::Instant::now();
        let ping = async {
            tokio::time::sleep(Duration::from_secs(40)).await;
            Ok::<_, io::Error>(Bytes::from_static(b": ping\n\n"))
        };
        let source = futures::stream::once(ping).chain(futures::stream::pending());
        let mut stream = SseStream::new(source, SseOptions::default(), CancellationToken::new());

        let err = stream
            .next_event()
            .await
            .expect_err("silence follows the ping");
        assert_eq!(
            err,
            SseError::Idle {
                timeout: Duration::from_secs(45),
            }
        );
        assert_eq!(err.code(), Some("E-PROV-IDLE"));
        assert!(!err.fatal(), "an idle timeout is retryable (§4.5)");
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(85),
            "the ping must have restarted the 45 s window"
        );
    }

    /// T-FAULT-003: a server that sends a chunk every 2 s keeps the stream
    /// alive indefinitely — the idle window restarts on each chunk, so a
    /// 40 s turn made of 2 s gaps never trips the 45 s timeout.
    #[tokio::test(start_paused = true)]
    async fn t_fault_003_slow_chunks_never_trip_the_idle_timer() {
        let start = tokio::time::Instant::now();
        let slow = futures::stream::unfold(0_u32, |n| async move {
            if n == 20 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            let line = format!("data: chunk {n}\n\n");
            Some((Ok::<_, io::Error>(Bytes::from(line)), n + 1))
        });
        let mut stream = SseStream::new(slow, SseOptions::default(), CancellationToken::new());
        let mut seen = 0;
        while let Some(event) = stream.next_event().await.expect("no idle abort") {
            assert_eq!(event.data, format!("chunk {seen}"));
            seen += 1;
        }
        assert_eq!(seen, 20);
        assert_eq!(start.elapsed(), Duration::from_secs(40));
    }

    /// T-PROV-032 — the deadline is the configured one (5 s ± 200 ms; virtual
    /// time makes the assertion exact).
    #[tokio::test(start_paused = true)]
    async fn t_prov_032_the_idle_deadline_is_the_configured_one() {
        let start = tokio::time::Instant::now();
        let source = futures::stream::pending::<Result<Bytes, io::Error>>();
        let mut stream = SseStream::new(
            source,
            options(Duration::from_secs(5)),
            CancellationToken::new(),
        );

        let err = stream.next_event().await.expect_err("nothing ever arrives");
        assert_eq!(
            err,
            SseError::Idle {
                timeout: Duration::from_secs(5),
            }
        );
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    /// §4.3 cancellation — raising the token ends the read within 250 ms
    /// rather than after the 45 s idle window.
    #[tokio::test(start_paused = true)]
    async fn cancel_ends_the_read_within_250_ms_rather_than_the_idle_window() {
        let start = tokio::time::Instant::now();
        let token = CancellationToken::new();
        let raised = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            raised.cancel();
        });

        let source = futures::stream::pending::<Result<Bytes, io::Error>>();
        let mut stream = SseStream::new(source, SseOptions::default(), token);
        let err = stream.next_event().await.expect_err("cancelled");
        assert_eq!(err, SseError::Cancelled);
        assert_eq!(err.code(), None, "a cancelled turn is not an error code");
        assert!(err.fatal());

        let observed = start.elapsed();
        assert!(
            observed >= Duration::from_millis(10),
            "the token was raised at 10 ms, observed at {observed:?}"
        );
        assert!(
            observed <= Duration::from_millis(250),
            "§4.3 budgets 250 ms, took {observed:?}"
        );
    }

    /// A token raised before the first read is caught by the loop's own check,
    /// with no timer involved at all.
    #[tokio::test]
    async fn a_token_already_raised_short_circuits_the_read() {
        let token = CancellationToken::new();
        token.cancel();
        let source = futures::stream::pending::<Result<Bytes, io::Error>>();
        let mut stream = SseStream::new(source, SseOptions::default(), token);

        assert_eq!(
            stream.next_event().await.expect_err("cancelled"),
            SseError::Cancelled
        );
    }

    /// §4.3 rule 4 — an oversized event aborts through the async half too, and
    /// the parser reports it so `cairn-provider` can stop without retrying.
    #[tokio::test]
    async fn an_oversized_event_aborts_the_stream() {
        let mut chunk = vec![b'x'; 64];
        chunk.extend_from_slice(b"\n\n");
        let source = futures::stream::iter(vec![Ok::<_, io::Error>(Bytes::from(chunk))]);
        let opts = SseOptions {
            max_event_bytes: 32,
            ..SseOptions::default()
        };
        let mut stream = SseStream::new(source, opts, CancellationToken::new());

        let err = stream.next_event().await.expect_err("too big");
        assert_eq!(err.code(), Some("E-PROV-EVENTBIG"));
        assert!(err.fatal());
    }

    /// A failing byte source surfaces as `SseError::Source`; classifying it
    /// (reset vs TLS vs timeout) belongs to `cairn-provider`.
    #[tokio::test]
    async fn a_byte_source_failure_is_reported_not_swallowed() {
        let source = futures::stream::iter(vec![Err::<Bytes, io::Error>(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "broken pipe",
        ))]);
        let mut stream = SseStream::new(source, SseOptions::default(), CancellationToken::new());

        let err = stream.next_event().await.expect_err("source failed");
        assert!(matches!(err, SseError::Source(_) if err.to_string().contains("broken pipe")));
        assert_eq!(err.code(), None);
    }

    /// §4.3 rule 6 through the async half: a stream that just *ends* after its
    /// final line still yields that event.
    #[tokio::test]
    async fn a_source_that_ends_without_a_blank_line_still_yields_its_event() {
        let source = futures::stream::iter(vec![Ok::<Bytes, io::Error>(bytes(b"data: [DONE]"))]);
        let mut stream = SseStream::new(source, SseOptions::default(), CancellationToken::new());

        let event = stream.next_event().await.expect("read").expect("event");
        assert!(event.is_done());
        assert_eq!(stream.next_event().await.expect("end"), None);
    }
}
