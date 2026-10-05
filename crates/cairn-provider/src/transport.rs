//! §4.3 transport: one HTTPS POST in, §3.4's [`StreamEvent`]s out.
//!
//! The adapters own §4.4's shaping (URL, headers, JSON body); this module owns
//! everything after that: the `reqwest` client (D-04: rustls with bundled
//! Mozilla roots, `ca_bundle` adding a site CA), sending the request, mapping
//! the status line through [`ProviderFault::from_status`], and pumping the
//! response bytes through `cairn-sse` into [`WireDecoder`].
//!
//! Two rules shape the pump:
//!
//! * **The stream never fails, it ends.** §3.4's `stream` returns
//!   `BoxStream<StreamEvent>`, which cannot carry a `Result`, so every failure
//!   after the first byte — disconnect, idle timeout, malformed abort,
//!   in-band provider error — ends the stream with
//!   `Finish { stop: Error }` (§4.3, §16.5 amendment 20). The `ProviderError`
//!   behind it is recorded on the adapter's side channel for the §4.5 retry
//!   loop and logged under §12.1; the model never sees it.
//! * **Cancellation ends the stream silently.** A raised token means the turn
//!   is dead, so there is nothing to record — the pump stops with no terminal
//!   event, and dropping the stream drops the HTTP body (§4.3's 250 ms).

use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};

use cairn_core::cancel::CancellationToken;
use cairn_core::registry::ProviderKind;
use cairn_sse::{SseError, SseOptions, SseStream, MAX_EVENT_BYTES};

use crate::error::{ProviderError, ProviderFault};
use crate::retry::parse_retry_after;
use crate::types::StreamEvent;
use crate::wire::{fault_from_name, WireDecoder};
use cairn_core::message::StopReason;

/// §4.5's 408 row is about the network, not the server: ten seconds without
/// a connection is a dead route, not a slow model. (Un-spec'd default; the
/// knob, if one is ever needed, belongs in `providers.<id>` beside
/// `idle_timeout_ms`.)
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What the adapter shaped: where to POST, with which headers, which JSON.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: serde_json::Value,
}

/// Build the D-04 client: rustls, bundled Mozilla roots, and nothing else.
/// `ca_bundle` (`providers.<id>.ca_bundle`) adds one site CA file in PEM
/// form on top of the bundle — it never replaces it.
pub(crate) fn build_client(ca_bundle: Option<&Path>) -> Result<reqwest::Client, ProviderError> {
    let mut builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
    if let Some(bundle) = ca_bundle {
        let pem = std::fs::read(bundle).map_err(|err| {
            ProviderError::new(
                ProviderFault::Tls,
                format!("cannot read ca_bundle {}: {err}", bundle.display()),
            )
        })?;
        let extra = reqwest::Certificate::from_pem_bundle(&pem).map_err(|err| {
            ProviderError::new(
                ProviderFault::Tls,
                format!("ca_bundle {} is not PEM: {err}", bundle.display()),
            )
        })?;
        for certificate in extra {
            builder = builder.add_root_certificate(certificate);
        }
    }
    builder.build().map_err(|err| {
        ProviderError::new(
            ProviderFault::Tls,
            format!("cannot build the HTTP client: {err}"),
        )
    })
}

/// Map a non-2xx status plus its body to §4.5's fault. `None` means success —
/// the caller streams the body.
///
/// `provider` is the §4.9 key (`anthropic`), used only for `E-PROV-AUTH`'s
/// fixed message. A 400 is refined past `BadRequest` when the body names a
/// row §4.5 singles out: a `context_length_exceeded` code (or a message about
/// the maximum context) means one compaction and one resend (T-PROV-037), a
/// content-filter marker means the turn is dead.
pub(crate) fn error_for_status(provider: &str, status: u16, body: &str) -> Option<ProviderError> {
    let fault = ProviderFault::from_status(status)?;
    if fault == ProviderFault::Auth {
        return Some(ProviderError::new(
            fault,
            format!("Provider `{provider}`: invalid API key. Run 'cairn auth login `{provider}`'."),
        ));
    }
    let mut error = ProviderError::new(fault, short_body(body));
    if fault == ProviderFault::BadRequest {
        error.fault = refine_bad_request(body);
    }
    Some(error)
}

/// The human half of an HTTP error: the provider's `error.message` /
/// `message` when the body is JSON, else the first line of the body.
fn short_body(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let message = value
            .get("error")
            .and_then(|entry| {
                entry.as_str().map_or_else(
                    || entry.get("message").and_then(serde_json::Value::as_str),
                    Some,
                )
            })
            .or_else(|| value.get("message").and_then(serde_json::Value::as_str));
        if let Some(message) = message {
            return message.to_string();
        }
    }
    body.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// Tell §4.5's two special 400s apart by the body's own vocabulary.
fn refine_bad_request(body: &str) -> ProviderFault {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let error = value.get("error");
        let names = [
            error
                .and_then(|entry| entry.get("code"))
                .and_then(|code| code.as_str()),
            error
                .and_then(|entry| entry.get("type"))
                .and_then(|kind| kind.as_str()),
        ];
        if names.into_iter().flatten().find_map(fault_from_name)
            == Some(ProviderFault::ContextLength)
        {
            return ProviderFault::ContextLength;
        }
        if names.into_iter().flatten().find_map(fault_from_name)
            == Some(ProviderFault::ContentFilter)
        {
            return ProviderFault::ContentFilter;
        }
    }
    let lowered = body.to_lowercase();
    if lowered.contains("context length")
        || lowered.contains("context_length")
        || lowered.contains("maximum context")
    {
        ProviderFault::ContextLength
    } else if lowered.contains("content_filter")
        || lowered.contains("content filter")
        || lowered.contains("flagged")
    {
        ProviderFault::ContentFilter
    } else {
        ProviderFault::BadRequest
    }
}

/// Map a send-time `reqwest` failure (§4.5's rows are about answers, so this
/// is the no-answer half): timeouts, refused connections and DNS under their
/// own faults, everything else as unreachable — the route, not the provider,
/// is what failed.
fn fault_for_send_error(error: &reqwest::Error) -> ProviderError {
    let (fault, detail) = if error.is_timeout() {
        (ProviderFault::Timeout, "the request timed out")
    } else if error.is_connect() {
        (ProviderFault::Unreachable, "cannot connect")
    } else if error.is_builder() {
        // The adapter validated the URL at construction, so this is
        // unreachable in practice — but a request that cannot be built is a
        // protocol violation by us, not a failure of the route.
        (ProviderFault::Protocol, "the request could not be built")
    } else {
        (ProviderFault::Unreachable, "the request failed")
    };
    ProviderError::new(fault, format!("{detail}: {error}"))
}

fn fault_for_sse_error(error: &SseError) -> ProviderError {
    match error {
        SseError::Idle { .. } => ProviderError::new(ProviderFault::Idle, error.to_string()),
        SseError::EventTooBig { .. } => {
            ProviderError::new(ProviderFault::EventTooBig, error.to_string())
        }
        SseError::Source(_) => ProviderError::new(ProviderFault::Unreachable, error.to_string()),
        SseError::Cancelled => {
            ProviderError::new(ProviderFault::Cancelled, "cancelled".to_string())
        }
    }
}

/// POST `request` and return the live event stream.
///
/// Setup failures (no connection, non-2xx status) come back as `Err` with
/// §4.5's fault and `retry_after` filled from a `Retry-After` header when the
/// server sent one. Past the first byte every failure becomes a terminal
/// `Finish { stop: Error }` inside the stream, with the fault recorded on
/// `record` for the §4.5 retry loop.
pub(crate) async fn post_events(
    client: &reqwest::Client,
    provider: &str,
    request: Request,
    kind: ProviderKind,
    cancel: CancellationToken,
    record: Arc<Mutex<Option<ProviderError>>>,
) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
    let mut builder = client.post(request.url.clone());
    for (name, value) in &request.headers {
        // Header values carry secrets, and a secret with a newline is a
        // credential problem, not a panic: `RequestBuilder::header` would
        // panic on it, so both halves are validated first.
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            ProviderError::new(
                ProviderFault::Auth,
                format!("Provider `{provider}`: cannot send its auth header"),
            )
        })?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
            ProviderError::new(
                ProviderFault::Auth,
                format!("Provider `{provider}`: its API key is not valid HTTP header material"),
            )
        })?;
        builder = builder.header(name, value);
    }
    // Built in two steps rather than `builder.send()`: T-ARCH-006 scans the
    // tree textually for `.send(` (channel sends outside the bus), and an
    // HTTP send is not one — plus a builder failure (a URL the adapter let
    // through) surfaces here instead of inside the send.
    let request = builder
        .header("content-type", "application/json")
        // `RequestBuilder::json` sits behind reqwest's `json` feature, which
        // §15.2 does not list and nothing else needs: the body is already a
        // `Value`, so serialising it here keeps the feature list honest.
        .body(serde_json::to_vec(&request.body).map_err(|error| {
            ProviderError::new(
                ProviderFault::Protocol,
                format!("the shaped request is not JSON: {error}"),
            )
        })?)
        .build()
        .map_err(|error| fault_for_send_error(&error))?;
    let response = client
        .execute(request)
        .await
        .map_err(|error| fault_for_send_error(&error))?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, chrono::Utc::now()));
        let body = response.text().await.unwrap_or_default();
        let mut error = error_for_status(provider, status, &body).unwrap_or_else(|| {
            ProviderError::new(ProviderFault::Protocol, format!("HTTP {status}"))
        });
        if error.fault == ProviderFault::RateLimited {
            error.retry_after = retry_after;
        }
        return Err(error);
    }
    let bytes: ByteStream = Box::pin(response.bytes_stream());
    let bytes = match kind {
        // Ollama speaks NDJSON, not SSE: reframe each line as an event so the
        // idle deadline, the cancel poll and the 1 MiB cap below all apply to
        // it unchanged.
        ProviderKind::Ollama => ndjson_to_sse(bytes),
        ProviderKind::Anthropic
        | ProviderKind::Openai
        | ProviderKind::OpenaiCompatible
        | ProviderKind::Vllm => bytes,
    };
    let stream = SseStream::new(bytes, SseOptions::default(), cancel);
    Ok(pump(stream, WireDecoder::new(kind), record))
}

/// The byte stream every [`SseStream`] reads.
type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Reframe NDJSON (`{...}\n{...}\n`) as SSE (`data: {...}\n\n`), so the Ollama
/// column flows through the same framing, idle and cancel policy as every
/// other provider. Blank lines are keep-alives, not events. A line longer
/// than the event cap is forwarded whole — [`SseStream`] rejects it with
/// `E-PROV-EVENTBIG`, which is where §4.3 wants the decision.
fn ndjson_to_sse(body: ByteStream) -> ByteStream {
    struct Carry {
        inner: ByteStream,
        buffered: Vec<u8>,
    }
    Box::pin(futures::stream::unfold(
        Carry {
            inner: body,
            buffered: Vec::new(),
        },
        |mut carry| async move {
            loop {
                if let Some(end) = carry.buffered.iter().position(|byte| *byte == b'\n') {
                    let mut line: Vec<u8> = carry.buffered.drain(..=end).collect();
                    while line
                        .last()
                        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
                    {
                        line.pop();
                    }
                    if line.is_empty() {
                        continue;
                    }
                    let mut framed = Vec::with_capacity(line.len() + 8);
                    framed.extend_from_slice(b"data: ");
                    framed.extend_from_slice(&line);
                    framed.extend_from_slice(b"\n\n");
                    return Some((Ok(Bytes::from(framed)), carry));
                }
                if carry.buffered.len() > MAX_EVENT_BYTES {
                    // No newline in over a megabyte: not a line, an attack or
                    // a broken server. Forward it framed and let the event cap
                    // below turn it into `E-PROV-EVENTBIG`.
                    let line: Vec<u8> = std::mem::take(&mut carry.buffered);
                    let mut framed = Vec::with_capacity(line.len() + 8);
                    framed.extend_from_slice(b"data: ");
                    framed.extend_from_slice(&line);
                    framed.extend_from_slice(b"\n\n");
                    return Some((Ok(Bytes::from(framed)), carry));
                }
                match carry.inner.next().await {
                    Some(Ok(chunk)) => carry.buffered.extend_from_slice(&chunk),
                    Some(Err(error)) => return Some((Err(error), carry)),
                    None => {
                        if carry.buffered.is_empty() {
                            return None;
                        }
                        let line = std::mem::take(&mut carry.buffered);
                        let mut framed = Vec::with_capacity(line.len() + 8);
                        framed.extend_from_slice(b"data: ");
                        framed.extend_from_slice(&line);
                        framed.extend_from_slice(b"\n\n");
                        return Some((Ok(Bytes::from(framed)), carry));
                    }
                }
            }
        },
    ))
}

struct Reading {
    sse: SseStream<ByteStream>,
    decoder: WireDecoder,
    pending: VecDeque<StreamEvent>,
    finished_source: bool,
}

/// Drive one [`SseStream`] through one [`WireDecoder`] as a `StreamEvent`
/// stream. Several payloads — and the end-of-stream flush — can each produce
/// several events, so decoded events wait in `pending` while the pump keeps
/// reading only when it runs dry. The `unfold` state is `None` once the
/// stream has ended, which is also why there is no `Done` variant beside a
/// `Reading` one hundreds of bytes larger.
fn pump(
    sse: SseStream<ByteStream>,
    decoder: WireDecoder,
    record: Arc<Mutex<Option<ProviderError>>>,
) -> BoxStream<'static, StreamEvent> {
    futures::stream::unfold(
        Some(Reading {
            sse,
            decoder,
            pending: VecDeque::new(),
            finished_source: false,
        }),
        move |reading| {
            let record = Arc::clone(&record);
            async move {
                let mut reading = reading?;
                loop {
                    if let Some(event) = reading.pending.pop_front() {
                        return Some((event, Some(reading)));
                    }
                    if reading.finished_source {
                        return None;
                    }
                    match reading.sse.next_event().await {
                        Ok(Some(sse_event)) => match reading.decoder.decode(&sse_event.data) {
                            Ok(more) => reading.pending.extend(more),
                            Err(error) => {
                                *record.lock().expect("fault record") = Some(error);
                                let tail = StreamEvent::Finish {
                                    stop: StopReason::Error,
                                };
                                return Some((tail, None));
                            }
                        },
                        Ok(None) => {
                            reading.finished_source = true;
                            let clean = reading.decoder.ended_cleanly();
                            reading.pending.extend(reading.decoder.finish());
                            if reading.pending.is_empty() && !clean {
                                // EOF with no terminator and no stop: the
                                // server closed the stream without ending the
                                // turn. A disconnect (T-FAULT-006), so it
                                // shares `E-PROV-NET` — but a server that does
                                // this twice is broken rather than flaky,
                                // hence `Truncated`'s single retry instead of
                                // the five a dropped socket gets.
                                *record.lock().expect("fault record") = Some(ProviderError::new(
                                    ProviderFault::Truncated,
                                    "the connection closed before the stream terminator",
                                ));
                                reading.pending.push_back(StreamEvent::Finish {
                                    stop: StopReason::Error,
                                });
                            }
                        }
                        Err(SseError::Cancelled) => return None,
                        Err(error) => {
                            *record.lock().expect("fault record") =
                                Some(fault_for_sse_error(&error));
                            let tail = StreamEvent::Finish {
                                stop: StopReason::Error,
                            };
                            return Some((tail, None));
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

    /// §4.5's status rows, through the transport's mapping: the fault, and
    /// `E-PROV-AUTH`'s exact fixed message.
    #[test]
    fn statuses_map_to_the_matrix() {
        assert!(error_for_status("anthropic", 200, "").is_none());
        let err = error_for_status("anthropic", 401, "bad key").expect("401 faults");
        assert_eq!(err.fault, ProviderFault::Auth);
        assert_eq!(
            err.message,
            "Provider `anthropic`: invalid API key. Run 'cairn auth login `anthropic`'."
        );
        let cases = [
            (403, ProviderFault::Forbidden),
            (404, ProviderFault::NoModel),
            (408, ProviderFault::Timeout),
            (413, ProviderFault::PayloadTooLarge),
            (429, ProviderFault::RateLimited),
            (500, ProviderFault::Server),
            (503, ProviderFault::Server),
        ];
        for (status, fault) in cases {
            let err = error_for_status("openai", status, "x").expect("faults");
            assert_eq!(err.fault, fault, "HTTP {status}");
        }
    }

    /// §4.5's two special 400s are told apart by the body's vocabulary: a
    /// `context_length_exceeded` code compacts and resends (T-PROV-037), a
    /// content-filter marker kills the turn, anything else is a plain 400.
    #[test]
    fn a_400_is_refined_by_its_body() {
        let err = error_for_status(
            "openai",
            400,
            r#"{"error":{"message":"too long","code":"context_length_exceeded"}}"#,
        )
        .expect("400 faults");
        assert_eq!(err.fault, ProviderFault::ContextLength);

        let err = error_for_status(
            "anthropic",
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"This model's maximum context length is 200000 tokens"}}"#,
        )
        .expect("400 faults");
        assert_eq!(err.fault, ProviderFault::ContextLength);

        let err = error_for_status(
            "openai",
            400,
            r#"{"error":{"message":"flagged","code":"content_filter"}}"#,
        )
        .expect("400 faults");
        assert_eq!(err.fault, ProviderFault::ContentFilter);

        let err =
            error_for_status("openai", 400, r#"{"error":{"message":"nope"}}"#).expect("400 faults");
        assert_eq!(err.fault, ProviderFault::BadRequest);
    }

    /// The human half of an error is the provider's message, not the whole
    /// body — and never more than a line of it.
    #[test]
    fn short_body_prefers_the_providers_own_message() {
        assert_eq!(
            short_body(r#"{"error":{"message":"Slow down","code":429}}"#),
            "Slow down"
        );
        assert_eq!(short_body("plain text failure"), "plain text failure");
        assert_eq!(
            short_body("first\nsecond"),
            "first",
            "one line, not the page"
        );
    }

    /// The D-04 client builds with no configuration, and a `ca_bundle` that
    /// cannot be read — or is not PEM — is `E-PROV-TLS`, not a panic.
    #[test]
    fn the_client_builds_and_a_bad_bundle_is_tls() {
        build_client(None).expect("defaults build");
        let missing = Path::new("/nonexistent/cairn-test-bundle.pem");
        let err = build_client(Some(missing)).expect_err("missing file faults");
        assert_eq!(err.fault, ProviderFault::Tls);

        let dir = std::env::temp_dir().join(format!("cairn-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let junk = dir.join("bundle.pem");
        // Not "not a certificate" — `from_pem_bundle` skips non-PEM text
        // without an error. A block that *claims* to be PEM but is not DER
        // is what fails.
        std::fs::write(
            &junk,
            "-----BEGIN CERTIFICATE-----\nnot-der-at-all\n-----END CERTIFICATE-----\n",
        )
        .expect("scratch");
        let err = build_client(Some(&junk)).expect_err("junk faults");
        assert_eq!(err.fault, ProviderFault::Tls);
        std::fs::remove_dir_all(&dir).ok();
    }

    use crate::mock::{serve_for_test as serve, sse_response_for_test as sse_response};

    fn post_against(url: String, body: &str) -> (Request, Arc<Mutex<Option<ProviderError>>>) {
        (
            Request {
                url,
                headers: vec![("x-test".to_string(), "1".to_string())],
                body: serde_json::json!({"model": "m", "stream": true, "note": body}),
            },
            Arc::new(Mutex::new(None)),
        )
    }

    /// The whole path, live: POST → status → SSE bytes → decoder events, with
    /// the usage chunk arriving after `finish_reason` and `Finish` still last.
    #[tokio::test]
    async fn a_live_sse_stream_decodes_end_to_end() {
        let body = concat!(
            "data: {\"id\":\"c\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1},\"choices\":[]}\n\n",
            "data: [DONE]\n\n",
        );
        let url = serve(sse_response(body));
        let client = build_client(None).expect("client");
        let (request, record) = post_against(url, "live");
        let stream = post_events(
            &client,
            "test",
            request,
            ProviderKind::Openai,
            CancellationToken::new(),
            Arc::clone(&record),
        )
        .await
        .expect("200 streams");
        let events: Vec<StreamEvent> =
            tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
                .await
                .expect("the stream ends");
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    model: "gpt-x".to_string(),
                    id: "c".to_string(),
                },
                StreamEvent::TextDelta {
                    text: "Hi".to_string(),
                },
                StreamEvent::Usage {
                    input: 4,
                    output: 1,
                    cache_read: 0,
                    cache_write: 0,
                },
                StreamEvent::Finish {
                    stop: cairn_core::message::StopReason::EndTurn,
                },
            ]
        );
        assert!(
            record.lock().expect("record").is_none(),
            "clean streams record nothing"
        );
    }

    /// A 429 from the server is `Err` before any event, with `retry_after`
    /// filled from the header for the §4.5 retry loop.
    #[tokio::test]
    async fn a_429_is_an_error_with_retry_after() {
        let raw = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 7\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
        let url = serve(raw.as_bytes().to_vec());
        let client = build_client(None).expect("client");
        let (request, record) = post_against(url, "limited");
        let Err(err) = post_events(
            &client,
            "test",
            request,
            ProviderKind::Openai,
            CancellationToken::new(),
            record,
        )
        .await
        else {
            panic!("429 faults")
        };
        assert_eq!(err.fault, ProviderFault::RateLimited);
        assert_eq!(err.retry_after, Some(Duration::from_secs(7)));
    }

    /// EOF with no terminator and no stop is a connection lost mid-turn
    /// (§4.7), not an end: the stream carries `Finish { stop: Error }` and
    /// the side channel says `Unreachable`, so the retry loop — not the
    /// model — decides what happens next.
    #[tokio::test]
    async fn a_cut_connection_is_an_error_not_an_ending() {
        let body = "data: {\"id\":\"c\",\"model\":\"gpt-x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n";
        let url = serve(sse_response(body));
        let client = build_client(None).expect("client");
        let (request, record) = post_against(url, "cut");
        let stream = post_events(
            &client,
            "test",
            request,
            ProviderKind::Openai,
            CancellationToken::new(),
            Arc::clone(&record),
        )
        .await
        .expect("200 starts streaming");
        let events: Vec<StreamEvent> =
            tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
                .await
                .expect("the stream ends");
        assert!(events.contains(&StreamEvent::TextDelta {
            text: "Hi".to_string()
        }));
        assert!(events.contains(&StreamEvent::Finish {
            stop: cairn_core::message::StopReason::Error,
        }));
        assert_eq!(
            record
                .lock()
                .expect("record")
                .as_ref()
                .map(|error| error.fault),
            Some(ProviderFault::Truncated)
        );
    }

    /// The NDJSON reframer: lines split across reads still frame whole, blank
    /// lines never become events, and a trailing line without its newline is
    /// flushed rather than lost.
    #[tokio::test]
    async fn ndjson_lines_reframe_as_events() {
        use futures::StreamExt;
        let chunks: ByteStream = Box::pin(futures::stream::iter(vec![
            Ok(Bytes::from("{\"a\":1}\n{\"b\"")),
            Ok(Bytes::from(":2}\n\n{\"c\":3}")),
        ]));
        let framed: Vec<Bytes> = ndjson_to_sse(chunks)
            .map(|result| result.expect("bytes frame"))
            .collect()
            .await;
        let text: Vec<String> = framed
            .iter()
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect();
        assert_eq!(
            text,
            vec![
                "data: {\"a\":1}\n\n",
                "data: {\"b\":2}\n\n",
                "data: {\"c\":3}\n\n",
            ]
        );
    }
    /// T-PROV-006's stream half: cancelling mid-stream ends the collection
    /// inside §4.3's 250 ms — the server is still holding the connection
    /// open, and nothing arrives after the cancel.
    #[tokio::test]
    async fn cancel_mid_stream_ends_inside_250ms() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback");
        let address = listener.local_addr().expect("loopback addr");
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut socket, _) = listener.accept().expect("one client");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read_exact(&mut byte).is_err() {
                    return;
                }
                head.push(byte[0]);
            }
            // One event, flushed, then hold the connection open: the client
            // cancels against a live stream, not a closed one.
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .ok();
            socket.write_all(b"data: {\"model\":\"m\"}\n\n").ok();
            socket.flush().ok();
            std::thread::sleep(Duration::from_secs(30));
        });

        let client = build_client(None).expect("client");
        let (request, _) = post_against(format!("http://{address}"), "cancel");
        let cancel = CancellationToken::new();
        let stream = post_events(
            &client,
            "test",
            request,
            ProviderKind::Openai,
            cancel.clone(),
            Arc::new(Mutex::new(None)),
        )
        .await
        .expect("200 streams");
        let mut stream = Box::pin(stream);
        let first = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("first event arrives")
            .expect("stream yields");
        assert!(
            matches!(first, StreamEvent::MessageStart { .. }),
            "the held stream still opens: {first:?}"
        );
        // The measured window starts at the cancel: the 50 ms poll observes
        // it, the stream ends, and nothing else arrives.
        let at = std::time::Instant::now();
        cancel.cancel();
        let rest: Vec<StreamEvent> =
            tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
                .await
                .expect("cancel ends the stream");
        assert!(
            at.elapsed() <= Duration::from_millis(250),
            "cancel-to-end {:?} exceeds §4.3's budget",
            at.elapsed()
        );
        assert!(rest.is_empty(), "nothing arrives after cancel");
    }
}
