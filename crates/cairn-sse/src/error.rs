//! Failures the SSE layer surfaces (SPEC §4.3).

use std::time::Duration;

/// What can go wrong while reading an SSE stream.
///
/// The variants whose meaning §4.3 fixes carry their own `E-PROV-*` code
/// ([`SseError::code`]); the rest are classified by `cairn-provider`, which
/// knows whether a failed read was a reset, a TLS failure or a timeout.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    /// §4.3 rule 4: one event over the cap means a malformed server, so the
    /// stream aborts and is never retried.
    #[error("SSE event exceeded the {limit} byte cap after {seen} bytes")]
    EventTooBig {
        /// The cap in bytes (1 MiB by default).
        limit: usize,
        /// Bytes accumulated for the offending event.
        seen: usize,
    },

    /// §4.3 idle timeout — retryable, and it counts as an attempt under §4.5.
    #[error("no SSE bytes for {timeout:?} (idle timeout)")]
    Idle {
        /// The configured deadline that elapsed.
        timeout: Duration,
    },

    /// The byte source itself failed (disconnect, HTTP error, decode error).
    #[error("SSE byte source failed: {0}")]
    Source(String),

    /// §4.3 cancellation: the stream was dropped or `token.cancel()` fired.
    /// Not an error — there is no code, because a cancelled turn is not a
    /// failure (§8.6, exit 7).
    #[error("SSE stream cancelled")]
    Cancelled,
}

impl SseError {
    /// The §4.3/§4.5 code for errors whose classification is fixed by the
    /// parser itself; `None` when the caller must decide (a read failure) or
    /// when the condition is not an error at all (cancellation).
    #[must_use]
    pub const fn code(&self) -> Option<&'static str> {
        match self {
            Self::EventTooBig { .. } => Some(cairn_core::error::codes::PROV_EVENTBIG),
            Self::Idle { .. } => Some(cairn_core::error::codes::PROV_IDLE),
            Self::Source(_) | Self::Cancelled => None,
        }
    }

    /// `true` when the caller should give up rather than retry (§4.5: an
    /// oversized event is a malformed server, and cancellation is terminal).
    #[must_use]
    pub const fn fatal(&self) -> bool {
        matches!(self, Self::EventTooBig { .. } | Self::Cancelled)
    }
}
