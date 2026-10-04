//! The synchronous half of SPEC §4.3: framing only.

use std::borrow::Cow;

use crate::error::SseError;

/// §4.3 rule 4 — a single SSE event may not exceed **1 MiB**.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// One fully-assembled SSE event: `event:` and `data:` consumed, `id:` and
/// `retry:` recorded but unused in v1 (§4.3 rule 2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    /// The `event:` field, if the server sent one.
    pub event: Option<String>,
    /// Every `data:` line of the event, joined with `\n` (§4.3 rule 1).
    pub data: String,
    /// The `id:` field — recorded, never used: `Last-Event-ID` resume is
    /// unsupported across all five adapters (REQ-PROV-010).
    pub id: Option<String>,
    /// The `retry:` field — recorded, never used (same rationale).
    pub retry: Option<u64>,
}

impl SseEvent {
    /// §4.3 rule 3 — the `OpenAI` family terminates on `data: [DONE]`. The
    /// adapter decides whether this stream is one of them; the parser only
    /// recognises the token.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// A streaming SSE parser over raw bytes.
///
/// Bytes may arrive in arbitrarily small chunks (T-PROV-020 feeds one byte at a
/// time): a partial line is retained in the buffer until its terminator shows
/// up (§4.3 rule 5). Delimiters are matched at the byte level, so a UTF-8
/// sequence split across reads is never mis-decoded — the lossy substitution
/// only runs over a *complete* event (§4.3, non-UTF-8 handling).
#[derive(Debug, Clone)]
pub struct SseParser {
    /// Undelimited tail; always shorter than the cap once `feed` returns.
    buf: Vec<u8>,
    /// §4.3 rule 4 cap.
    max_event_bytes: usize,
    /// How many events needed lossy UTF-8 substitution. `cairn-provider` logs
    /// the §4.3 `warn` when this moves; the parser itself has no logger.
    lossy: usize,
    /// Resume offset for the delimiter scan, so re-feeding a large incomplete
    /// event stays linear instead of re-scanning the buffer each time.
    scan_from: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    /// A parser with the §4.3 default cap of 1 MiB per event.
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_event_bytes(MAX_EVENT_BYTES)
    }

    /// A parser with a custom per-event cap (tests use a small one).
    #[must_use]
    pub fn with_max_event_bytes(max_event_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_event_bytes,
            lossy: 0,
            scan_from: 0,
        }
    }

    /// The configured per-event cap in bytes.
    #[must_use]
    pub const fn max_event_bytes(&self) -> usize {
        self.max_event_bytes
    }

    /// Bytes held for the event currently being assembled.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// §4.3: how many events required lossy U+FFFD substitution. The consumer
    /// logs the `warn` — this crate deliberately carries no logger.
    #[must_use]
    pub const fn lossy_count(&self) -> usize {
        self.lossy
    }

    /// Feed bytes and take back every event that became complete.
    ///
    /// `Err` means the stream is over (an oversized event): the caller aborts
    /// and does not retry (§4.3 rule 4).
    pub fn feed(&mut self, input: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        self.buf.extend_from_slice(input);
        let mut out = Vec::new();

        loop {
            if let Some((start, delim_len)) = find_boundary(&self.buf, self.scan_from) {
                if start > self.max_event_bytes {
                    return Err(SseError::EventTooBig {
                        limit: self.max_event_bytes,
                        seen: start,
                    });
                }
                let chunk: Vec<u8> = self.buf.drain(..start + delim_len).collect();
                self.scan_from = 0;
                if let Some(event) = self.decode_event(&chunk) {
                    out.push(event);
                }
                continue;
            }

            if self.buf.len() > self.max_event_bytes {
                return Err(SseError::EventTooBig {
                    limit: self.max_event_bytes,
                    seen: self.buf.len(),
                });
            }
            // Re-test the last byte: it may be the first half of a boundary
            // once the next chunk lands.
            self.scan_from = self.buf.len().saturating_sub(1);
            break;
        }

        Ok(out)
    }

    /// Dispatch whatever is buffered when the byte source ends.
    ///
    /// §4.3 rule 6: providers that omit the final blank line would otherwise
    /// lose `[DONE]` or `message_stop`, so a pending event that carries at
    /// least one `data:` line (or an `event:` name) is still emitted. A
    /// half-written *line* is emitted too — at EOF there is nothing left to
    /// complete it with, and dropping it would silently truncate a final delta.
    pub fn finish(&mut self) -> Result<Option<SseEvent>, SseError> {
        if self.buf.len() > self.max_event_bytes {
            return Err(SseError::EventTooBig {
                limit: self.max_event_bytes,
                seen: self.buf.len(),
            });
        }
        let chunk = std::mem::take(&mut self.buf);
        self.scan_from = 0;
        Ok(self.decode_event(&chunk))
    }

    /// Decode one complete event's bytes and split it into fields.
    fn decode_event(&mut self, chunk: &[u8]) -> Option<SseEvent> {
        // A whole event is decoded at once, so a UTF-8 sequence that straddled
        // two reads is already reassembled here and never turns into a
        // spurious U+FFFD.
        let text = if let Ok(text) = std::str::from_utf8(chunk) {
            Cow::Borrowed(text)
        } else {
            self.lossy += 1;
            String::from_utf8_lossy(chunk)
        };

        let mut event: Option<String> = None;
        let mut id: Option<String> = None;
        let mut retry: Option<u64> = None;
        let mut data: Vec<&str> = Vec::new();

        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                // A blank line inside the chunk can only come from an
                // unsupported line ending; nothing to record.
                continue;
            }
            if line.starts_with(':') {
                // §4.3: comments are ignored — `: ping` is a heartbeat, which
                // the idle timer has already seen at the byte level.
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                // `field` with no colon: an empty value (WHATWG), which for a
                // field we do not consume means "ignore this line".
                None => (line, ""),
            };
            match field {
                "event" => event = Some(value.to_string()),
                "data" => data.push(value),
                "id" => id = Some(value.to_string()),
                "retry" => retry = value.parse::<u64>().ok(),
                // §4.3: only `event` and `data` are consumed; anything else —
                // including a stray JSON blob written with no field name — is
                // ignored and the stream continues.
                _ => {}
            }
        }

        // Dispatch an event that says something: a payload, or a name without
        // one (so the consumer can apply §4.3's "unknown `event:` → ignore").
        if data.is_empty() && event.is_none() {
            return None;
        }

        Some(SseEvent {
            event,
            data: data.join("\n"),
            id,
            retry,
        })
    }
}

/// Find the first pair of adjacent line endings — the blank line that closes an
/// event — starting the scan at `from`.
///
/// Returns `(offset_of_the_event, total_delimiter_len)`. Line endings are `\n`
/// with an optional preceding `\r`, so `\n\n`, `\r\n\r\n`, `\n\r\n` and
/// `\r\n\n` all delimit. A lone `\r` is data: §4.3 only mandates the first two.
fn find_boundary(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    if from >= buf.len() {
        return None;
    }
    let mut i = from;
    while i < buf.len() {
        let first = line_ending_len(buf, i);
        if let Some(first_len) = first {
            if let Some(second_len) = line_ending_len(buf, i + first_len) {
                return Some((i, first_len + second_len));
            }
            i += first_len;
            continue;
        }
        i += 1;
    }
    None
}

/// Length of the line ending starting at `i`, if there is one.
fn line_ending_len(buf: &[u8], i: usize) -> Option<usize> {
    match buf.get(i) {
        Some(b'\n') => Some(1),
        Some(b'\r') if buf.get(i + 1) == Some(&b'\n') => Some(2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_bytes(chunks: &[&[u8]]) -> Result<Vec<SseEvent>, SseError> {
        let mut parser = SseParser::new();
        let mut all = Vec::new();
        for chunk in chunks {
            all.extend(parser.feed(chunk)?);
        }
        Ok(all)
    }

    /// T-PROV-020 — the boundary lands mid-`data:` across reads: feed the
    /// whole stream one byte at a time and the event must be identical.
    #[test]
    fn t_prov_020_chunk_boundaries_split_a_data_line() {
        let stream = b"data: {\"delta\":\"hi\"}\n\nevent: message_stop\ndata: {}\n\n";
        let one_chunk = feed_bytes(&[stream]).expect("complete stream");
        let bytewise: Vec<SseEvent> = {
            let mut parser = SseParser::new();
            let mut out = Vec::new();
            for byte in stream {
                out.extend(parser.feed(&[*byte]).expect("byte"));
            }
            out
        };
        assert_eq!(one_chunk, bytewise);
        assert_eq!(one_chunk.len(), 2);
        assert_eq!(one_chunk[0].data, "{\"delta\":\"hi\"}");
        assert_eq!(one_chunk[1].event.as_deref(), Some("message_stop"));
    }

    /// T-PROV-021 — CRLF is a line ending, not payload, and it may straddle a
    /// read (`…\r` in one chunk, `\n…` in the next).
    #[test]
    fn t_prov_021_crlf_line_endings_delimit_events() {
        let events = feed_bytes(&[b"data: a\r\n\r\ndata: b\r\n", b"\r\ndata: c\r\n\r\n"])
            .expect("crlf stream");
        let texts: Vec<&str> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    /// T-PROV-022 — several `data:` lines of one event join with `\n`.
    #[test]
    fn t_prov_022_multiple_data_lines_join_with_newline() {
        let events = feed_bytes(&[b"data: one\ndata: two\ndata: three\n\n"]).expect("multi-data");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "one\ntwo\nthree");
    }

    /// §4.3 rule 1 — comments are not events.
    #[test]
    fn comment_lines_are_ignored() {
        let events = feed_bytes(&[
            b": ping\n\ndata: real\n\n",
            b": keep-alive\ndata: after\n\n",
        ])
        .expect("comments");
        let texts: Vec<&str> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(texts, ["real", "after"]);
    }

    /// §4.3 rule 1 — a line that is not `field: value` is ignored and the
    /// stream continues rather than failing.
    #[test]
    fn a_line_that_is_not_a_field_is_ignored() {
        let events = feed_bytes(&[b"this is not a field\ndata: still here\n\n"]).expect("garbage");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "still here");
        assert!(events[0].event.is_none());
    }

    /// T-PROV-024 — one event over 1 MiB aborts with `E-PROV-EVENTBIG`, and it
    /// is caught while the event is still growing, not after it completes.
    #[test]
    fn t_prov_024_an_event_over_the_1_mib_cap_aborts() {
        let mut parser = SseParser::new();
        assert_eq!(parser.max_event_bytes(), MAX_EVENT_BYTES);
        let block = vec![b'x'; 64 * 1024];
        let mut err = None;
        for _ in 0..17 {
            // 17 x 64 KiB = 1088 KiB > 1 MiB.
            match parser.feed(&block) {
                Ok(_) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let err = err.expect("the cap must trip before the event completes");
        assert_eq!(
            err,
            SseError::EventTooBig {
                limit: MAX_EVENT_BYTES,
                seen: 1_114_112
            }
        );
        assert_eq!(err.code(), Some("E-PROV-EVENTBIG"));
        assert!(err.fatal(), "an oversized event is never retried (§4.5)");
    }

    /// The cap counts one event, not the whole stream: a burst of complete
    /// small events in a single read passes even though the chunk is huge.
    #[test]
    fn the_cap_counts_one_event_not_the_buffer() {
        let mut parser = SseParser::with_max_event_bytes(16);
        let mut events = Vec::new();
        for i in 0..8 {
            let line = format!("data: {i}\n\n");
            events.extend(parser.feed(line.as_bytes()).expect("small event"));
        }
        assert_eq!(events.len(), 8);
        assert_eq!(parser.buffered(), 0);
    }

    /// §4.3 non-UTF-8 handling — lossy substitution, and the parser reports it
    /// so the consumer can log the `warn` (this crate carries no logger).
    #[test]
    fn t_prov_027_non_utf8_bytes_become_replacement_characters() {
        let mut parser = SseParser::new();
        let events = parser
            .feed(b"data: ok \xff\xfe tail\n\n")
            .expect("lossy decode must not fail");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "ok \u{FFFD}\u{FFFD} tail");
        assert_eq!(parser.lossy_count(), 1);

        // A clean event afterwards must not be charged for the earlier damage.
        parser.feed(b"data: clean\n\n").expect("clean");
        assert_eq!(parser.lossy_count(), 1);
    }

    /// §4.3 rule 5 — a UTF-8 sequence split across reads decodes cleanly; only
    /// genuinely invalid bytes cost a replacement character.
    #[test]
    fn a_utf8_sequence_split_across_reads_is_not_mangled() {
        let payload = "data: こんにちは\n\n".as_bytes();
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for byte in payload {
            events.extend(parser.feed(&[*byte]).expect("byte"));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "こんにちは");
        assert_eq!(
            parser.lossy_count(),
            0,
            "no replacement for a split sequence"
        );
    }

    /// §4.3 rule 2 — `id:` and `retry:` are recorded, never used.
    #[test]
    fn id_and_retry_are_recorded_but_unused() {
        let events = feed_bytes(&[b"id: 7\nretry: 250\ndata: payload\n\n"]).expect("id and retry");
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[0].retry, Some(250));
        assert_eq!(events[0].data, "payload");

        // A non-numeric `retry:` is ignored rather than failing the event.
        let events = feed_bytes(&[b"retry: soon\ndata: ok\n\n"]).expect("bad retry");
        assert_eq!(events[0].retry, None);
        assert_eq!(events[0].data, "ok");
    }

    /// §4.3 rule 3 — the terminator token is recognised, but only the adapter
    /// decides that this stream is one of the `[DONE]` families.
    #[test]
    fn done_is_recognised_not_interpreted() {
        let events = feed_bytes(&[b"data: [DONE]\n\n"]).expect("done");
        assert!(events[0].is_done());

        let events = feed_bytes(&[b"data: [DONE] \n\n"]).expect("done with padding");
        assert!(events[0].is_done());

        let events = feed_bytes(&[b"data: {}\n\n"]).expect("not done");
        assert!(!events[0].is_done());
    }

    /// §4.3 rule 6 — a server that omits the final blank line still gets its
    /// last event, which is how `[DONE]` and `message_stop` usually arrive.
    #[test]
    fn finish_dispatches_an_event_without_a_trailing_blank_line() {
        // A complete line that no blank line ever closed.
        let mut parser = SseParser::new();
        assert!(parser
            .feed(b"data: {\"a\":1}\n")
            .expect("complete line")
            .is_empty());
        let event = parser.finish().expect("finish").expect("pending event");
        assert_eq!(event.data, "{\"a\":1}");
        assert_eq!(parser.finish().expect("drained"), None);

        // Not even a line terminator — still dispatched rather than dropped.
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: [DONE]").expect("partial").is_empty());
        let event = parser.finish().expect("finish").expect("pending event");
        assert!(event.is_done());
        assert_eq!(parser.finish().expect("drained"), None);

        // An event already closed by a blank line is not dispatched twice.
        let mut parser = SseParser::new();
        assert_eq!(parser.feed(b"data: once\n\n").expect("closed").len(), 1);
        assert_eq!(parser.finish().expect("finish"), None);

        // Nothing at all buffered means nothing to dispatch.
        let mut parser = SseParser::new();
        assert_eq!(parser.finish().expect("empty"), None);
    }

    /// §4.3 rule 5 — an unterminated line stays buffered across reads and is
    /// only dispatched by the blank line that closes it.
    #[test]
    fn an_unterminated_line_is_retained_across_reads() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"data: half").expect("half").is_empty());
        assert_eq!(parser.buffered(), 10);
        assert!(parser.feed(b"way\n").expect("rest").is_empty());
        assert_eq!(parser.buffered(), 14);
        let events = parser.feed(b"\n").expect("boundary");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "halfway");
        assert_eq!(parser.buffered(), 0);
    }

    /// An event that names but never payloads still reaches the consumer, who
    /// applies §4.3's "unknown `event:` name → ignore, keep stream".
    #[test]
    fn a_named_event_with_no_data_is_still_dispatched() {
        let events = feed_bytes(&[b"event: ping\n\n"]).expect("named");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "");
    }

    /// A wholly blank chunk dispatches nothing (WHATWG: empty event buffers
    /// are discarded).
    #[test]
    fn a_blank_event_is_not_dispatched() {
        let events = feed_bytes(&[b"\n\n\n\n"]).expect("blank");
        assert!(events.is_empty());
    }
}
