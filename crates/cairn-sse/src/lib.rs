//! `cairn-sse` — Hand-written SSE parser: framing, fields, idle timeout (SPEC 4.3, D-04)
//!
//! D-04 chose a bespoke parser over `eventsource-stream` because the contract
//! in SPEC §4.3 is precise about the cases an off-the-shelf parser glosses
//! over: partial lines that straddle reads, CRLF, `id:`/`retry:` recorded but
//! unused, a hard 1 MiB cap per event, and an idle timer that heartbeat
//! comments reset.
//!
//! The crate is split in two halves:
//!
//! * [`SseParser`] — a pure, synchronous byte state machine. It never sleeps,
//!   so every framing rule is testable by feeding one byte at a time.
//! * [`SseStream`] — the async half: it reads a byte source under an idle
//!   timeout and a cancellation token, and hands [`SseEvent`]s to the caller.
//!
//! What this crate deliberately does *not* do: decode provider payloads, decide
//! which `event:` names matter, or count malformed JSON (that is §4.3's
//! "malformed-chunk recovery", which needs to know what a valid payload looks
//! like, so it belongs to `cairn-provider`).

mod error;
mod parser;
mod stream;

pub use error::SseError;
pub use parser::{SseEvent, SseParser, MAX_EVENT_BYTES};
pub use stream::{SseOptions, SseStream};

/// §4.3 idle timeout: no bytes for this long aborts the stream with
/// [`SseError::Idle`] (`E-PROV-IDLE`). Maps to `providers.<id>.idle_timeout_ms`.
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
