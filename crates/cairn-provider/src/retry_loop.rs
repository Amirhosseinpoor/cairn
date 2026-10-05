//! §4.5 retry loop: drive [`Provider::stream`] until it stays up.
//!
//! The *policy* is [`crate::retry`] (which fault, how many backoffs, how
//! long); this module is the `while` around it. One call to
//! [`stream_with_retry`] yields one continuous [`StreamEvent`] stream across
//! attempts: setup failures and mid-stream failures both flow through §4.5's
//! matrix, attempts concatenate, and the turn ends with whatever the last
//! attempt produced.
//!
//! Three rules keep the concatenation honest:
//!
//! * **Attempts never overlap.** The next `stream()` starts only after the
//!   previous stream ended — there is no hedging, so the provider never sees
//!   two live calls for one turn.
//! * **A failed turn still ends with `Finish`.** Mid-stream failures already
//!   carry `Finish { stop: Error }` (the transport guarantees it); when every
//!   attempt fails before the first byte, the loop synthesises one — the
//!   agent must see the turn abort, not an empty stream it could mistake for
//!   silence.
//! * **Cancellation wins everywhere.** No retry starts, sleeps, or continues
//!   after the token fires (REQ-PROV-006): backoff sleeps in 250 ms steps so
//!   a mid-backoff cancel lands inside §4.3's budget, and a cancelled stream
//!   ends silently, exactly like the transport's.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt;
use rand::SeedableRng;

use cairn_core::cancel::CancellationToken;
use cairn_core::message::StopReason;

use crate::error::{ProviderError, ProviderFault};
use crate::retry::{delay_bounds, sample_delay, RetryBudget};
use crate::types::{ModelRequest, StreamEvent};
use crate::Provider;

/// T-PROV-037's conditional as a callback: takes the failed request, returns
/// the compacted one to resend exactly once. `None` means no compactor —
/// the matrix still allows `ContextLength` its single retry.
pub type Compact = Box<dyn FnOnce(&ModelRequest) -> ModelRequest + Send>;

/// The backoff sleep wakes this often to check the token, so a cancel lands
/// inside §4.3's 250 ms budget no matter how long the backoff is.
const BACKOFF_POLL: Duration = Duration::from_millis(250);

struct Loop {
    provider: Arc<dyn Provider>,
    req: ModelRequest,
    cancel: CancellationToken,
    budget: RetryBudget,
    compact: Option<Compact>,
    compacted: bool,
    retries_spent: u8,
    rng: rand::rngs::StdRng,
    inner: Option<BoxStream<'static, StreamEvent>>,
    pending_fault: Option<ProviderError>,
    error_finish_yielded: bool,
    done: bool,
}

impl Loop {
    /// Decide a fault: `Ok(())` starts another attempt, `Err` hands the fault
    /// back for the caller — the loop re-publishes it for the end consumer
    /// before the turn ends. Sleeps the backoff inside (cancellably);
    /// compacts immediately for the first `ContextLength` instead.
    async fn handle_fault(&mut self, fault: ProviderError) -> Result<(), ProviderError> {
        // REQ-PROV-006: nothing starts, sleeps, or continues after cancel.
        if self.cancel.is_cancelled() {
            return Err(fault);
        }
        if fault.fault == ProviderFault::ContextLength && !self.compacted {
            self.compacted = true;
            if let Some(compact) = self.compact.take() {
                // T-PROV-037: one compaction, one immediate resend. It sits
                // outside the backoff budget, which governs *waiting* —
                // fixing the request is not waiting.
                self.req = compact(&self.req);
                return Ok(());
            }
            // No compactor: fall through to the matrix, which still allows
            // `ContextLength` its single retry.
        } else if fault.fault == ProviderFault::ContextLength {
            // The compacted resend failed the same way: fatal (T-PROV-037's
            // "second 400").
            return Err(fault);
        }
        if !fault.fault.retryable() {
            return Err(fault);
        }
        let attempt = self.retries_spent + 1;
        let Some(bounds) = delay_bounds(attempt, fault.fault, fault.retry_after) else {
            return Err(fault);
        };
        let delay = sample_delay(&bounds, &mut self.rng);
        // REQ-PROV-005: never sleep past the deadline — surface the fault
        // instead of breaching the total budget.
        if self.budget.afford(delay).is_none() {
            return Err(fault);
        }
        if sleep_backoff(delay, &self.cancel).await {
            return Err(fault);
        }
        self.retries_spent = attempt;
        Ok(())
    }
}

/// Sleep `delay`, waking every [`BACKOFF_POLL`] to check the token.
/// Returns whether the sleep was cancelled.
async fn sleep_backoff(delay: Duration, cancel: &CancellationToken) -> bool {
    let mut left = delay;
    while !left.is_zero() {
        let step = left.min(BACKOFF_POLL);
        tokio::time::sleep(step).await;
        if cancel.is_cancelled() {
            return true;
        }
        left -= step;
    }
    false
}

/// Run one call under §4.5's matrix and yield its events across attempts.
///
/// `budget` bounds the total backoff sleep (REQ-PROV-005); `compact` handles
/// T-PROV-037's conditional. Attempts concatenate — a `MessageStart`
/// following `Finish { stop: Error }` starts a new turn and the assembler
/// discards the partial one (REQ-PROV-009) — and a call that never gets its
/// first byte still ends with one terminal `Finish { stop: Error }`.
pub fn stream_with_retry(
    provider: Arc<dyn Provider>,
    req: ModelRequest,
    cancel: CancellationToken,
    budget: RetryBudget,
    compact: Option<Compact>,
) -> BoxStream<'static, StreamEvent> {
    futures::stream::unfold(
        Some(Loop {
            provider,
            req,
            cancel,
            budget,
            compact,
            compacted: false,
            retries_spent: 0,
            rng: rand::rngs::StdRng::from_entropy(),
            inner: None,
            pending_fault: None,
            error_finish_yielded: false,
            done: false,
        }),
        move |this| async move {
            let mut this = this?;
            loop {
                if this.done {
                    return None;
                }
                // A fault carried from the previous step gets its retry
                // decision now — the event it ended with was already yielded,
                // in order.
                if let Some(fault) = this.pending_fault.take() {
                    let Err(fault) = this.handle_fault(fault).await else {
                        continue;
                    };
                    // Cancellation ends silently, exactly like the transport:
                    // the turn is dead, so there is nothing to record.
                    if this.cancel.is_cancelled() {
                        return None;
                    }
                    // Re-publish the terminal fault for the end consumer:
                    // taking consumed the adapter's copy when the attempt
                    // failed, and the caller after the loop needs the code
                    // and message for its report.
                    this.provider.record_last_error(fault);
                    if this.error_finish_yielded {
                        return None;
                    }
                    return Some((
                        StreamEvent::Finish {
                            stop: StopReason::Error,
                        },
                        None,
                    ));
                }
                if this.inner.is_none() {
                    if this.cancel.is_cancelled() {
                        return None;
                    }
                    match this
                        .provider
                        .stream(this.req.clone(), this.cancel.clone())
                        .await
                    {
                        Err(fault) => {
                            this.pending_fault = Some(fault);
                            continue;
                        }
                        Ok(stream) => this.inner = Some(stream),
                    }
                }
                let event = this
                    .inner
                    .as_mut()
                    .expect("stream just started")
                    .next()
                    .await;
                match event {
                    Some(StreamEvent::Finish {
                        stop: StopReason::Error,
                    }) => {
                        this.pending_fault = this.provider.take_last_error();
                        this.error_finish_yielded = true;
                        this.inner = None;
                        // No recorded fault behind an error finish means the
                        // provider broke its contract — retrying blindly
                        // could loop forever, so the turn ends here.
                        if this.pending_fault.is_none() {
                            this.done = true;
                        }
                        return Some((
                            StreamEvent::Finish {
                                stop: StopReason::Error,
                            },
                            Some(this),
                        ));
                    }
                    Some(event) => return Some((event, Some(this))),
                    None => {
                        this.inner = None;
                        if this.cancel.is_cancelled() {
                            return None;
                        }
                        match this.provider.take_last_error() {
                            // Ended with nothing but recorded a fault: the
                            // pump always yields `Finish` first, so this is
                            // defensive — handled like any other fault, with
                            // the synthesised terminal below if it is fatal.
                            Some(fault) => this.pending_fault = Some(fault),
                            None => return None,
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use futures::FutureExt;

    use crate::types::{Capabilities, ProviderHealth, ProviderId, TokenCount};

    /// Scripted outcomes: setup failures, or event sequences that end with a
    /// recorded fault exactly like the transport's pump does.
    enum Outcome {
        SetupError(ProviderError),
        Stream {
            events: Vec<StreamEvent>,
            fault: Option<ProviderFault>,
        },
    }

    struct FakeProvider {
        outcomes: Mutex<VecDeque<Outcome>>,
        calls: AtomicUsize,
        last: Mutex<Option<ProviderError>>,
    }

    impl FakeProvider {
        fn new(outcomes: Vec<Outcome>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes.into()),
                calls: AtomicUsize::new(0),
                last: Mutex::new(None),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Provider for FakeProvider {
        fn id(&self) -> &ProviderId {
            static ID: std::sync::OnceLock<ProviderId> = std::sync::OnceLock::new();
            ID.get_or_init(|| ProviderId::new("fake"))
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::baseline()
        }

        fn stream(
            &self,
            _req: ModelRequest,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
            async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match self.outcomes.lock().expect("script").pop_front() {
                    Some(Outcome::SetupError(error)) => Err(error),
                    Some(Outcome::Stream { mut events, fault }) => {
                        if let Some(fault) = fault {
                            events.push(StreamEvent::Finish {
                                stop: StopReason::Error,
                            });
                            *self.last.lock().expect("last") =
                                Some(ProviderError::new(fault, "scripted"));
                        }
                        Ok(futures::stream::iter(events).boxed())
                    }
                    None => panic!("no scripted outcome left"),
                }
            }
            .boxed()
        }

        fn count_tokens<'a>(
            &'a self,
            _req: &'a ModelRequest,
        ) -> BoxFuture<'a, Result<TokenCount, ProviderError>> {
            async move { Ok(TokenCount::estimate(0)) }.boxed()
        }

        fn health(&self) -> ProviderHealth {
            ProviderHealth::Ready
        }

        fn take_last_error(&self) -> Option<ProviderError> {
            self.last.lock().expect("last").take()
        }

        fn record_last_error(&self, error: ProviderError) {
            *self.last.lock().expect("last") = Some(error);
        }
    }

    fn failed(fault: ProviderFault) -> Outcome {
        Outcome::SetupError(ProviderError::new(fault, "scripted"))
    }

    fn request() -> ModelRequest {
        ModelRequest::new("fake/m", Vec::new(), 10)
    }

    async fn collect(
        provider: Arc<FakeProvider>,
        req: ModelRequest,
        budget: RetryBudget,
        compact: Option<Compact>,
    ) -> (Vec<StreamEvent>, usize) {
        let events: Vec<StreamEvent> = tokio::time::timeout(
            Duration::from_secs(30),
            stream_with_retry(
                provider.clone() as Arc<dyn Provider>,
                req,
                CancellationToken::new(),
                budget,
                compact,
            )
            .collect::<Vec<_>>(),
        )
        .await
        .expect("the loop always ends");
        let calls = provider.calls();
        (events, calls)
    }

    fn message_start() -> StreamEvent {
        StreamEvent::MessageStart {
            model: "m".to_string(),
            id: String::new(),
        }
    }

    /// A clean first attempt streams through untouched: one call, no added
    /// events, no synthesis.
    #[tokio::test]
    async fn a_clean_stream_passes_through_untouched() {
        let provider = Arc::new(FakeProvider::new(vec![Outcome::Stream {
            events: vec![
                message_start(),
                StreamEvent::Finish {
                    stop: StopReason::EndTurn,
                },
            ],
            fault: None,
        }]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 1);
        assert_eq!(
            events,
            vec![
                message_start(),
                StreamEvent::Finish {
                    stop: StopReason::EndTurn,
                },
            ]
        );
    }

    /// T-PROV-033's shape: two 500s, then success. The consumer sees the
    /// winning attempt's events; the two failures cost backoffs, not turns.
    #[tokio::test(start_paused = true)]
    async fn retryable_setup_errors_retry_then_succeed() {
        let provider = Arc::new(FakeProvider::new(vec![
            failed(ProviderFault::Server),
            failed(ProviderFault::Server),
            Outcome::Stream {
                events: vec![message_start()],
                fault: None,
            },
        ]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 3);
        assert_eq!(events, vec![message_start()]);
    }

    /// A fatal setup error never retries — but the turn still ends with one
    /// terminal `Finish`, synthesised, so the agent sees the abort rather
    /// than an empty stream it could mistake for silence.
    #[tokio::test]
    async fn a_fatal_setup_error_never_retries_but_still_ends_the_turn() {
        let provider = Arc::new(FakeProvider::new(vec![failed(ProviderFault::Auth)]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 1);
        assert_eq!(
            events,
            vec![StreamEvent::Finish {
                stop: StopReason::Error,
            }]
        );
    }

    /// T-FAULT-004's shape: 500s until the row's budget is spent — one call
    /// plus five retries — then the turn ends as an error.
    #[tokio::test(start_paused = true)]
    async fn server_errors_stop_after_five_retries() {
        let provider = Arc::new(FakeProvider::new(vec![
            failed(ProviderFault::Server),
            failed(ProviderFault::Server),
            failed(ProviderFault::Server),
            failed(ProviderFault::Server),
            failed(ProviderFault::Server),
            failed(ProviderFault::Server),
        ]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 6, "one call plus five retries");
        assert_eq!(
            events,
            vec![StreamEvent::Finish {
                stop: StopReason::Error,
            }]
        );
    }

    /// T-PROV-006: cancelling mid-backoff aborts the sleep — the assertion is
    /// on wall time, because a 30 s floor that slept through cancel would
    /// take this test with it.
    #[tokio::test(start_paused = true)]
    async fn cancel_during_backoff_aborts_without_retry() {
        // A floor the loop could never sleep out: cancelling must win.
        let mut error = ProviderError::new(ProviderFault::RateLimited, "scripted");
        error.retry_after = Some(Duration::from_secs(30));
        let provider = Arc::new(FakeProvider::new(vec![
            Outcome::SetupError(error),
            Outcome::Stream {
                events: vec![message_start()],
                fault: None,
            },
        ]));
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stopper.cancel();
        });
        let started = std::time::Instant::now();
        let events: Vec<StreamEvent> = tokio::time::timeout(
            Duration::from_secs(10),
            stream_with_retry(
                provider.clone() as Arc<dyn Provider>,
                request(),
                cancel,
                RetryBudget::new(),
                None,
            )
            .collect::<Vec<_>>(),
        )
        .await
        .expect("cancel ends the stream");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a 30 s floor must not be slept through: {:?}",
            started.elapsed()
        );
        assert_eq!(provider.calls(), 1, "no retries after cancel");
        assert!(events.is_empty(), "cancellation ends silently");
    }

    /// T-PROV-037's conditional: the first 400 compacts and resends
    /// immediately; the second 400 is fatal. The hook runs exactly once.
    #[tokio::test]
    async fn context_length_compacts_once_then_fails() {
        let provider = Arc::new(FakeProvider::new(vec![
            failed(ProviderFault::ContextLength),
            failed(ProviderFault::ContextLength),
        ]));
        let compactions = Arc::new(AtomicUsize::new(0));
        let marker = Arc::clone(&compactions);
        let compact: Compact = Box::new(move |req: &ModelRequest| {
            marker.fetch_add(1, Ordering::SeqCst);
            let mut shortened = req.clone();
            shortened.max_tokens /= 2;
            shortened
        });
        let (events, calls) = collect(provider, request(), RetryBudget::new(), Some(compact)).await;
        assert_eq!(calls, 2, "one compaction, one resend");
        assert_eq!(compactions.load(Ordering::SeqCst), 1);
        assert_eq!(
            events,
            vec![StreamEvent::Finish {
                stop: StopReason::Error,
            }]
        );
    }

    /// T-FAULT-006: a stream that ends with no terminator and no stop is a
    /// disconnect with one retry. Twice in a row means the server is broken,
    /// and the turn ends — as `E-PROV-NET`, the code `Truncated` shares.
    #[tokio::test]
    async fn a_truncated_stream_retries_once() {
        let truncated = || Outcome::Stream {
            events: vec![message_start()],
            fault: Some(ProviderFault::Truncated),
        };
        let provider = Arc::new(FakeProvider::new(vec![truncated(), truncated()]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 2, "one retry, not five");
        assert_eq!(
            events,
            vec![
                message_start(),
                StreamEvent::Finish {
                    stop: StopReason::Error,
                },
                message_start(),
                StreamEvent::Finish {
                    stop: StopReason::Error,
                },
            ]
        );
    }

    /// T-FAULT-001: a disconnect mid-turn replays the whole turn — attempts
    /// concatenate, and the assembler starts over at the second
    /// `MessageStart` (REQ-PROV-009). Nothing partial survives as a message.
    #[tokio::test]
    async fn a_mid_stream_failure_replays_the_whole_turn() {
        let provider = Arc::new(FakeProvider::new(vec![
            Outcome::Stream {
                events: vec![
                    message_start(),
                    StreamEvent::TextDelta {
                        text: "partial".to_string(),
                    },
                ],
                fault: Some(ProviderFault::Unreachable),
            },
            Outcome::Stream {
                events: vec![
                    message_start(),
                    StreamEvent::TextDelta {
                        text: "whole".to_string(),
                    },
                    StreamEvent::Finish {
                        stop: StopReason::EndTurn,
                    },
                ],
                fault: None,
            },
        ]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 2);
        let starts = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::MessageStart { .. }))
            .count();
        assert_eq!(starts, 2, "the replay starts a second turn: {events:?}");
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::Error,
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::EndTurn,
        }));
    }

    /// REQ-PROV-005: a backoff the budget cannot afford is never slept — the
    /// fault surfaces instead of breaching the deadline.
    #[tokio::test]
    async fn an_unaffordable_backoff_is_never_slept() {
        let mut error = ProviderError::new(ProviderFault::RateLimited, "scripted");
        error.retry_after = Some(Duration::from_secs(3_600));
        let provider = Arc::new(FakeProvider::new(vec![Outcome::SetupError(error)]));
        let (events, calls) = collect(
            provider,
            request(),
            RetryBudget::after(Duration::from_millis(300)),
            None,
        )
        .await;
        assert_eq!(calls, 1);
        assert_eq!(
            events,
            vec![StreamEvent::Finish {
                stop: StopReason::Error,
            }]
        );
    }

    /// `MalformedStream` gets its single retry, then the turn ends — "after 1
    /// retry → fatal", exactly as the matrix budgets it.
    #[tokio::test(start_paused = true)]
    async fn malformed_aborts_after_one_retry() {
        let provider = Arc::new(FakeProvider::new(vec![
            failed(ProviderFault::MalformedStream),
            failed(ProviderFault::MalformedStream),
        ]));
        let (events, calls) = collect(provider, request(), RetryBudget::new(), None).await;
        assert_eq!(calls, 2);
        assert_eq!(
            events,
            vec![StreamEvent::Finish {
                stop: StopReason::Error,
            }]
        );
    }
    /// Serve `responses` in connection order on loopback, counting accepts.
    /// Each response is written whole after draining the request, then the
    /// connection closes — except responses flagged to truncate, which stop
    /// mid-body.
    fn serve_scripted(responses: Vec<(Vec<u8>, bool)>, connections: Arc<AtomicUsize>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().expect("loopback addr");
        std::thread::spawn(move || {
            for (response, truncate) in responses {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                connections.fetch_add(1, Ordering::SeqCst);
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
                if truncate {
                    // Cut the body, never the head: a truncated status line
                    // is a setup failure, not a mid-stream disconnect.
                    let split = response
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map_or(response.len(), |at| at + 4);
                    let cut = split + (response.len() - split) / 2;
                    socket.write_all(&response[..cut]).ok();
                } else {
                    socket.write_all(&response).ok();
                }
            }
        });
        format!("http://{address}")
    }

    fn sse_200(body: &str) -> (Vec<u8>, bool) {
        (
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
            false,
        )
    }

    /// T-PROV-009: the first connection dies after three deltas; the retry
    /// replays the whole turn against the second, and the session never sees
    /// a partial assistant message — at event level, two complete turns with
    /// the failed one explicitly closed.
    #[tokio::test]
    async fn disconnect_after_three_deltas_replays_the_turn() {
        use crate::adapters::OpenaiAdapter;

        let partial = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"c\"},\"finish_reason\":null}]}\n\n",
        );
        let whole = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"abc\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let connections = Arc::new(AtomicUsize::new(0));
        let url = serve_scripted(
            vec![(sse_200(partial).0, true), sse_200(whole)],
            Arc::clone(&connections),
        );
        let registry = cairn_core::registry::bundled();
        let adapter = OpenaiAdapter::new(
            "openai/gpt-5.1-codex",
            registry,
            Some("key".to_string()),
            None,
            Some(url),
        );
        let events: Vec<StreamEvent> = tokio::time::timeout(
            Duration::from_secs(30),
            stream_with_retry(
                Arc::new(adapter),
                ModelRequest::new("openai/gpt-5.1-codex", Vec::new(), 10),
                CancellationToken::new(),
                RetryBudget::new(),
                None,
            )
            .collect::<Vec<_>>(),
        )
        .await
        .expect("the retry wins");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        let starts = events
            .iter()
            .filter(|event| matches!(event, StreamEvent::MessageStart { .. }))
            .count();
        assert_eq!(starts, 2, "the replay starts a second turn: {events:?}");
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::Error,
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::EndTurn,
        }));
    }

    /// T-PROV-033's live shape: 500, 503, then the winning turn — three
    /// connections, real backoffs, attempts visible on the wire.
    #[tokio::test]
    async fn status_errors_then_success_across_connections() {
        use crate::adapters::OpenaiAdapter;

        let failure = |status: &str| {
            (
                format!("HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}")
                    .into_bytes(),
                false,
            )
        };
        let whole = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Back\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let connections = Arc::new(AtomicUsize::new(0));
        let url = serve_scripted(
            vec![
                failure("500 Internal Server Error"),
                failure("503 Service Unavailable"),
                sse_200(whole),
            ],
            Arc::clone(&connections),
        );
        let registry = cairn_core::registry::bundled();
        let adapter = OpenaiAdapter::new(
            "openai/gpt-5.1-codex",
            registry,
            Some("key".to_string()),
            None,
            Some(url),
        );
        let events: Vec<StreamEvent> = tokio::time::timeout(
            Duration::from_secs(30),
            stream_with_retry(
                Arc::new(adapter),
                ModelRequest::new("openai/gpt-5.1-codex", Vec::new(), 10),
                CancellationToken::new(),
                RetryBudget::new(),
                None,
            )
            .collect::<Vec<_>>(),
        )
        .await
        .expect("the third attempt wins");
        assert_eq!(connections.load(Ordering::SeqCst), 3);
        assert!(events.contains(&StreamEvent::TextDelta {
            text: "Back".to_string()
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::EndTurn,
        }));
    }
}
