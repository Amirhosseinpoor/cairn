//! Mock provider and cassettes: T-PROV-* and T-FAULT-* without a live key.
//!
//! A cassette is a JSON script of one call's wire shapes — SSE `data`
//! payloads for Anthropic/`OpenAI`, NDJSON lines for Ollama — optionally
//! interrupted by faults. [`MockProvider`] plays it through a real
//! [`WireDecoder`], so every row runs against the same mapping production
//! uses; only the socket is fake.
//!
//! The committed cassettes live in `assets/cassettes/`, one scenario per
//! file. [`steps_from_json`] loads them; tests that need a scenario inline
//! build [`Step`]s directly.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, StreamExt};
use serde_json::Value;

use cairn_core::cancel::CancellationToken;
use cairn_core::message::StopReason;
use cairn_core::registry::ProviderKind;

use crate::error::{ProviderError, ProviderFault};
use crate::types::{
    Capabilities, ModelRequest, ProviderHealth, ProviderId, StreamEvent, TokenCount,
};
use crate::wire::WireDecoder;
use crate::Provider;

/// One scripted moment of a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// A wire payload, decoded exactly like production.
    Payload(String),
    /// The setup fails before the first byte: `stream()` returns `Err`.
    SetupError {
        fault: ProviderFault,
        message: String,
    },
    /// The stream dies mid-turn: events so far, then `Finish { stop: Error }`
    /// with the fault recorded — the transport's contract, without a socket.
    StreamError {
        fault: ProviderFault,
        message: String,
    },
}

/// Parse a fault name as written in a cassette (`"Server"`, `"Truncated"`).
fn parse_fault(name: &str) -> Option<ProviderFault> {
    Some(match name {
        "Auth" => ProviderFault::Auth,
        "Forbidden" => ProviderFault::Forbidden,
        "NoModel" => ProviderFault::NoModel,
        "Timeout" => ProviderFault::Timeout,
        "RateLimited" => ProviderFault::RateLimited,
        "PayloadTooLarge" => ProviderFault::PayloadTooLarge,
        "ContextLength" => ProviderFault::ContextLength,
        "ContentFilter" => ProviderFault::ContentFilter,
        "BadRequest" => ProviderFault::BadRequest,
        "Server" => ProviderFault::Server,
        "Tls" => ProviderFault::Tls,
        "Unreachable" => ProviderFault::Unreachable,
        "Truncated" => ProviderFault::Truncated,
        "Idle" => ProviderFault::Idle,
        "MalformedStream" => ProviderFault::MalformedStream,
        "EventTooBig" => ProviderFault::EventTooBig,
        "Protocol" => ProviderFault::Protocol,
        "FallbackDisabled" => ProviderFault::FallbackDisabled,
        "Offline" => ProviderFault::Offline,
        _ => return None,
    })
}

/// Load cassette steps from JSON with a `steps` array of `payload`,
/// `setup_error`, or `stream_error` entries (see `assets/cassettes/` for the
/// shape). `kind` and `model` are documentation — the caller picks the
/// decoder kind — but `steps` must parse exactly, because a cassette that
/// silently drops a step tests nothing.
pub fn steps_from_json(text: &str) -> Result<Vec<Step>, String> {
    let document: Value =
        serde_json::from_str(text).map_err(|error| format!("cassette is not JSON: {error}"))?;
    let steps = document
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| "cassette needs a `steps` array".to_string())?;
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let nth = format!("step {index}");
            if let Some(payload) = step.get("payload").and_then(Value::as_str) {
                return Ok(Step::Payload(payload.to_string()));
            }
            for (key, is_setup) in [("setup_error", true), ("stream_error", false)] {
                if let Some(entry) = step.get(key) {
                    let fault = entry
                        .get("fault")
                        .and_then(Value::as_str)
                        .and_then(parse_fault)
                        .ok_or_else(|| format!("{nth}: unknown fault"))?;
                    let message = entry
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("cassette fault")
                        .to_string();
                    return Ok(if is_setup {
                        Step::SetupError { fault, message }
                    } else {
                        Step::StreamError { fault, message }
                    });
                }
            }
            Err(format!(
                "{nth}: needs `payload`, `setup_error` or `stream_error`"
            ))
        })
        .collect()
}

/// A scripted [`Provider`]: no sockets, no keys, the same decoder.
///
/// Each `stream()` call plays steps until a fault step or the end of the
/// script, buffering that attempt's events — a mock is allowed what the
/// transport is not, because nothing here renders live. `take_last_error`
/// reports the recorded fault, so the §4.5 retry loop treats the mock like
/// any adapter.
#[derive(Debug)]
pub struct MockProvider {
    kind: ProviderKind,
    capabilities: Capabilities,
    steps: Mutex<VecDeque<Step>>,
    calls: AtomicUsize,
    last_error: Mutex<Option<ProviderError>>,
}

impl MockProvider {
    /// A mock playing `steps` as `kind`'s wire format with `capabilities`.
    #[must_use]
    pub fn new(kind: ProviderKind, capabilities: Capabilities, steps: Vec<Step>) -> Self {
        Self {
            kind,
            capabilities,
            steps: Mutex::new(steps.into()),
            calls: AtomicUsize::new(0),
            last_error: Mutex::new(None),
        }
    }

    /// How many `stream()` calls the mock has served — what retry tests
    /// count.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for MockProvider {
    fn id(&self) -> &ProviderId {
        static ID: std::sync::OnceLock<ProviderId> = std::sync::OnceLock::new();
        ID.get_or_init(|| ProviderId::new("mock"))
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn stream(
        &self,
        _req: ModelRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>> {
        async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut decoder = WireDecoder::new(self.kind);
            let mut events = Vec::new();
            loop {
                let step = self.steps.lock().expect("script").pop_front();
                match step {
                    None => {
                        events.extend(decoder.finish());
                        break;
                    }
                    Some(Step::Payload(payload)) => match decoder.decode(&payload) {
                        Ok(more) => events.extend(more),
                        Err(error) => {
                            *self.last_error.lock().expect("last") = Some(error);
                            events.extend(decoder.finish());
                            break;
                        }
                    },
                    Some(Step::SetupError { fault, message }) => {
                        return Err(ProviderError::new(fault, message));
                    }
                    Some(Step::StreamError { fault, message }) => {
                        *self.last_error.lock().expect("last") =
                            Some(ProviderError::new(fault, message));
                        events.push(StreamEvent::Finish {
                            stop: StopReason::Error,
                        });
                        break;
                    }
                }
            }
            Ok(futures::stream::iter(events).boxed())
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
        self.last_error.lock().expect("last").take()
    }

    fn record_last_error(&self, error: ProviderError) {
        *self.last_error.lock().expect("last") = Some(error);
    }
}

/// Serve one canned HTTP response on loopback and return its base URL.
/// Shared by the transport and adapter integration tests: the server reads
/// the whole request (head plus exactly `Content-Length`) before answering,
/// because answering while the client is still sending closes the socket
/// with unread inbound data, which Windows answers with RST.
#[cfg(test)]
pub(crate) fn serve_for_test(response: Vec<u8>) -> String {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
    let address = listener.local_addr().expect("loopback addr");
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("one client");
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
        socket.write_all(&response).ok();
    });
    format!("http://{address}")
}

/// Wrap `body` in a minimal SSE 200 response for [`serve_for_test`].
#[cfg(test)]
pub(crate) fn sse_response_for_test(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::retry::RetryBudget;
    use crate::Provider;

    fn anthropic_cassette() -> &'static str {
        include_str!("../../../assets/cassettes/anthropic-tool-use.json")
    }

    /// Cassettes load strictly: every step parses, faults resolve by name,
    /// and anything else is an error rather than a silently dropped step.
    #[test]
    fn cassettes_load_exactly() {
        let steps = steps_from_json(anthropic_cassette()).expect("loads");
        assert_eq!(steps.len(), 10, "every payload survives");
        assert!(matches!(steps[0], Step::Payload(_)));

        let err = steps_from_json(r#"{"steps": [{"setup_error": {"fault": "Nope"}}]}"#)
            .expect_err("unknown faults fail");
        assert!(err.contains("unknown fault"), "{err}");

        let err = steps_from_json(r#"{"steps": [{"hum": 1}]}"#).expect_err("unknown shapes fail");
        assert!(err.contains("needs `payload`"), "{err}");

        let err = steps_from_json(r#"{"nope": []}"#).expect_err("missing steps fail");
        assert!(err.contains("`steps`"), "{err}");
    }

    /// T-PROV-001's wire half: the committed Anthropic cassette plays
    /// through the real decoder into one start, text, one closed tool call,
    /// merged usage, and a tool-use finish.
    #[tokio::test]
    async fn the_anthropic_cassette_plays_a_tool_turn() {
        let steps = steps_from_json(anthropic_cassette()).expect("loads");
        let mock = MockProvider::new(ProviderKind::Anthropic, Capabilities::baseline(), steps);
        let stream = mock
            .stream(
                ModelRequest::new("mock/m", Vec::new(), 10),
                CancellationToken::new(),
            )
            .await
            .expect("cassette streams");
        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(mock.calls(), 1);
        assert!(mock.take_last_error().is_none(), "clean cassette, no fault");
        assert_eq!(
            events[0],
            StreamEvent::MessageStart {
                model: "claude-x".to_string(),
                id: "msg_1".to_string(),
            }
        );
        assert!(events.contains(&StreamEvent::ToolCallStart {
            index: 1,
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
        }));
        assert!(events.contains(&StreamEvent::ToolCallEnd { index: 1 }));
        assert!(events.contains(&StreamEvent::Usage {
            input: 25,
            output: 15,
            cache_read: 0,
            cache_write: 0,
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::ToolUse,
        }));
    }

    /// T-PROV-011's wire half: the usage chunk arrives after `finish_reason`,
    /// and the cost run uses the reported numbers — with `estimated: false`
    /// — instead of the §4.8 estimate.
    #[tokio::test]
    async fn late_usage_overrides_the_estimate_in_cost() {
        let text = include_str!("../../../assets/cassettes/openai-usage-late.json");
        let steps = steps_from_json(text).expect("loads");
        let mock = MockProvider::new(ProviderKind::Openai, Capabilities::baseline(), steps);
        let stream = mock
            .stream(
                ModelRequest::new("mock/m", Vec::new(), 10),
                CancellationToken::new(),
            )
            .await
            .expect("cassette streams");
        let events: Vec<StreamEvent> = stream.collect().await;
        let reported = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Usage {
                    input,
                    output,
                    cache_read,
                    cache_write,
                } => Some(cairn_core::message::Usage {
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                    estimated: false,
                }),
                _ => None,
            })
            .expect("usage decoded");
        assert!(!reported.estimated);

        let registry = cairn_core::registry::bundled();
        let (_, entry) = registry
            .resolve("anthropic/claude-sonnet-4-5")
            .expect("bundled");
        let actual = crate::accounting::cost_usd(&reported, &entry.pricing);
        let estimated =
            crate::accounting::cost_usd(&cairn_core::message::Usage::estimate(8), &entry.pricing);
        assert!(actual.is_some() && estimated.is_some());
        assert_ne!(actual, estimated, "real tokens cost real money");
    }

    /// T-PROV-035's wire half: the committed Ollama cassette decodes like
    /// the SSE columns — same events, `""` message id, upgraded stop.
    #[tokio::test]
    async fn the_ollama_cassette_decodes_like_sse() {
        let text = include_str!("../../../assets/cassettes/ollama-tools.json");
        let steps = steps_from_json(text).expect("loads");
        let mock = MockProvider::new(ProviderKind::Ollama, Capabilities::baseline(), steps);
        let stream = mock
            .stream(
                ModelRequest::new("mock/m", Vec::new(), 10),
                CancellationToken::new(),
            )
            .await
            .expect("cassette streams");
        let events: Vec<StreamEvent> = stream.collect().await;
        assert!(events.contains(&StreamEvent::MessageStart {
            model: "llama3.1".to_string(),
            id: String::new(),
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::ToolUse,
        }));
    }

    /// Fault steps behave like the transport: setup errors come back as
    /// `Err` before any event, stream errors end the attempt with
    /// `Finish { stop: Error }` and a recorded fault the retry loop reads.
    #[tokio::test]
    async fn fault_steps_mirror_the_transport_contract() {
        let mock = MockProvider::new(
            ProviderKind::Openai,
            Capabilities::baseline(),
            vec![Step::SetupError {
                fault: ProviderFault::Auth,
                message: "bad key".to_string(),
            }],
        );
        let Err(err) = mock
            .stream(
                ModelRequest::new("mock/m", Vec::new(), 10),
                CancellationToken::new(),
            )
            .await
        else {
            panic!("setup errors fail fast")
        };
        assert_eq!(err.fault, ProviderFault::Auth);

        let mock = MockProvider::new(
            ProviderKind::Openai,
            Capabilities::baseline(),
            vec![
                Step::Payload("{\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}".to_string()),
                Step::StreamError {
                    fault: ProviderFault::Unreachable,
                    message: "dropped".to_string(),
                },
            ],
        );
        let stream = mock
            .stream(
                ModelRequest::new("mock/m", Vec::new(), 10),
                CancellationToken::new(),
            )
            .await
            .expect("streams, then fails");
        let events: Vec<StreamEvent> = stream.collect().await;
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::Error,
        }));
        let recorded = mock.take_last_error().expect("recorded");
        assert_eq!(recorded.fault, ProviderFault::Unreachable);
        assert!(mock.take_last_error().is_none(), "reading clears");
    }

    /// A cassette that runs dry mid-turn ends the attempt like a clean EOF:
    /// whatever the decoder flushes, nothing invented.
    #[tokio::test]
    async fn the_truncated_cassette_ends_without_a_finish() {
        let text = include_str!("../../../assets/cassettes/openai-truncated.json");
        let steps = steps_from_json(text).expect("loads");
        let mock = MockProvider::new(ProviderKind::Openai, Capabilities::baseline(), steps);
        let stream = mock
            .stream(
                ModelRequest::new("mock/m", Vec::new(), 10),
                CancellationToken::new(),
            )
            .await
            .expect("streams");
        let events: Vec<StreamEvent> = stream.collect().await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Finish { .. })),
            "no stop, no terminator, no Finish: {events:?}"
        );
        assert!(mock.take_last_error().is_none());
    }

    /// The 500s-then-ok cassette drives the real retry loop to success on
    /// its third call — the T-PROV-033 shape with no sockets and no waiting:
    /// virtual time skips the backoffs.
    #[tokio::test(start_paused = true)]
    async fn mock_failures_drive_the_retry_loop_to_success() {
        use crate::retry_loop::stream_with_retry;

        let text = include_str!("../../../assets/cassettes/openai-500-then-ok.json");
        let steps = steps_from_json(text).expect("loads");
        let mock = Arc::new(MockProvider::new(
            ProviderKind::Openai,
            Capabilities::baseline(),
            steps,
        ));
        let events: Vec<StreamEvent> = stream_with_retry(
            mock.clone(),
            ModelRequest::new("mock/m", Vec::new(), 10),
            CancellationToken::new(),
            RetryBudget::new(),
            None,
        )
        .collect()
        .await;
        // Two setup errors then the winning turn: the loop retried twice and
        // the consumer sees only success-shaped events.
        assert!(events.contains(&StreamEvent::TextDelta {
            text: "Recovered".to_string()
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: StopReason::EndTurn,
        }));
    }
}
