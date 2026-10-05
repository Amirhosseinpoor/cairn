//! The §4.5 error taxonomy, as data.
//!
//! §4.5 is a table: a condition, a code, whether it is retryable, how many
//! retries it gets and which backoff applies. Writing that as five `match`
//! arms spread across five adapters would put the policy where nobody can read
//! it — and where no test can hold it against the table. [`ProviderFault`] is
//! one value per row; [`ProviderError`] is a row plus what actually happened.

use std::fmt;
use std::time::Duration;

use cairn_core::error::codes;
use cairn_sse::SseError;

/// The shape of §4.5's backoff column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// No retry at all (§4.5's `—`).
    None,
    /// `E-PROV-MALFORMED`: a fixed 1 s, then fatal.
    Fixed(Duration),
    /// `E-PROV-IDLE`: exponential, no jitter called for.
    Exponential,
    /// The default for 408/5xx/connect: `rand_uniform(0, min(30000, 500 * 2^(n-1)))` ms.
    ExponentialFullJitter,
    /// `E-PROV-RATELIMIT`: `max(delay_n, Retry-After)` capped at 120 s.
    RateLimited,
}

/// One row of §4.5's error taxonomy and retry matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderFault {
    /// 401.
    Auth,
    /// 403 — model not entitled.
    Forbidden,
    /// 404 for the model.
    NoModel,
    /// 408 or a network timeout.
    Timeout,
    /// 429, with or without `Retry-After`.
    RateLimited,
    /// 413.
    PayloadTooLarge,
    /// 400 context length exceeded — conditional: one compaction, then one resend.
    ContextLength,
    /// 400 content filter.
    ContentFilter,
    /// Any other 4xx, including schema violations.
    BadRequest,
    /// 500/502/503/504/529.
    Server,
    /// TLS failure.
    Tls,
    /// DNS failure or connection refused — and, per §4.7, a connection lost
    /// mid-stream.
    Unreachable,
    /// Clean EOF with no terminator and no stop: the server closed the
    /// stream without ending the turn (T-FAULT-006). A disconnect, so it
    /// shares `E-PROV-NET` — but a server that does this twice in a row is
    /// broken rather than flaky, so it gets one retry, not five.
    Truncated,
    /// §4.3 idle timeout: no bytes for 45 s.
    Idle,
    /// §4.3: five or more malformed events in one stream.
    MalformedStream,
    /// §4.3 rule 4: one event over the 1 MiB cap.
    EventTooBig,
    /// 1xx or an invalid status line.
    Protocol,
    /// REQ-PROV-008: prompt fallback disabled after two bad `<tool>` blocks.
    FallbackDisabled,
    /// §11.1 `--offline` / `network.offline` — no socket is ever opened.
    Offline,
    /// Not a failure: §11.2 exit 7, so it carries no `E-*` code.
    Cancelled,
}

/// Every fault, so a test can prove a row was added to one list and not the
/// other (the same shape as `cairn_core::error::ALL_CODES`).
pub const ALL_FAULTS: &[ProviderFault] = &[
    ProviderFault::Auth,
    ProviderFault::Forbidden,
    ProviderFault::NoModel,
    ProviderFault::Timeout,
    ProviderFault::RateLimited,
    ProviderFault::PayloadTooLarge,
    ProviderFault::ContextLength,
    ProviderFault::ContentFilter,
    ProviderFault::BadRequest,
    ProviderFault::Server,
    ProviderFault::Tls,
    ProviderFault::Unreachable,
    ProviderFault::Truncated,
    ProviderFault::Idle,
    ProviderFault::MalformedStream,
    ProviderFault::EventTooBig,
    ProviderFault::Protocol,
    ProviderFault::FallbackDisabled,
    ProviderFault::Offline,
    ProviderFault::Cancelled,
];

impl ProviderFault {
    /// The stable `E-PROV-*` code for this row, or `None` for cancellation,
    /// which is not an error (§11.2 `ERR_CANCELLED`).
    #[must_use]
    pub const fn code(self) -> Option<&'static str> {
        Some(match self {
            Self::Auth => codes::PROV_AUTH,
            Self::Forbidden => codes::PROV_FORBID,
            Self::NoModel => codes::PROV_NOMODEL,
            Self::Timeout => codes::PROV_TIMEOUT,
            Self::RateLimited => codes::PROV_RATELIMIT,
            Self::PayloadTooLarge => codes::PROV_PAYLOAD,
            Self::ContextLength => codes::PROV_CONTEXT,
            Self::ContentFilter => codes::PROV_FILTER,
            Self::BadRequest => codes::PROV_REQ,
            Self::Server => codes::PROV_SERVER,
            Self::Tls => codes::PROV_TLS,
            Self::Unreachable | Self::Truncated => codes::PROV_NET,
            Self::Idle => codes::PROV_IDLE,
            Self::MalformedStream => codes::PROV_MALFORMED,
            Self::EventTooBig => codes::PROV_EVENTBIG,
            Self::Protocol => codes::PROV_PROTO,
            Self::FallbackDisabled => codes::PROV_FALLBACK,
            Self::Offline => codes::PROV_OFFLINE,
            Self::Cancelled => return None,
        })
    }

    /// §4.5's *Retryable* column.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::Timeout
                | Self::RateLimited
                | Self::ContextLength
                | Self::Server
                | Self::Unreachable
                | Self::Truncated
                | Self::Idle
                | Self::MalformedStream
        )
    }

    /// §4.5's *Retries* column: how many backoffs the condition may take.
    #[must_use]
    pub const fn retries(self) -> u8 {
        match self {
            Self::MalformedStream | Self::ContextLength | Self::Truncated => 1,
            Self::Timeout | Self::RateLimited | Self::Server | Self::Unreachable | Self::Idle => 5,
            _ => 0,
        }
    }

    /// §4.5's *Backoff* column.
    #[must_use]
    pub const fn backoff(self) -> Backoff {
        match self {
            Self::MalformedStream => Backoff::Fixed(Duration::from_secs(1)),
            Self::Idle => Backoff::Exponential,
            Self::Timeout | Self::Server | Self::Unreachable | Self::Truncated => {
                Backoff::ExponentialFullJitter
            }
            Self::RateLimited => Backoff::RateLimited,
            _ => Backoff::None,
        }
    }

    /// Whether this fault ends the turn outright. Non-retryable faults do;
    /// `ContextLength` and `FallbackDisabled` do not — the first compacts and
    /// resends, the second lets the turn continue (REQ-PROV-008).
    #[must_use]
    pub const fn fatal(self) -> bool {
        !self.retryable() && !matches!(self, Self::FallbackDisabled)
    }

    /// Classify a §4.3 stream failure. Every `SseError` has a row here:
    /// `Source` is a connection lost mid-stream, which §4.7 resolves by
    /// retrying the whole call, so it is `E-PROV-NET` and retryable.
    #[must_use]
    pub const fn from_sse(err: &SseError) -> Self {
        match err {
            SseError::EventTooBig { .. } => Self::EventTooBig,
            SseError::Idle { .. } => Self::Idle,
            SseError::Source(_) => Self::Unreachable,
            SseError::Cancelled => Self::Cancelled,
        }
    }

    /// §4.5's first column, which is written as statuses before it is written
    /// as anything else — so the mapping sits beside the table rather than
    /// inside each adapter that has to apply it.
    ///
    /// `None` for a 2xx: there is no fault to report. The two 400s the spec
    /// singles out (context length, content filter) are told apart by the
    /// response *message*, never by the status, so every 400 comes back as
    /// [`ProviderFault::BadRequest`] and the caller refines it where it still
    /// has the body to read.
    #[must_use]
    pub const fn from_status(status: u16) -> Option<Self> {
        match status {
            200..=299 => None,
            401 => Some(Self::Auth),
            403 => Some(Self::Forbidden),
            404 => Some(Self::NoModel),
            408 => Some(Self::Timeout),
            413 => Some(Self::PayloadTooLarge),
            429 => Some(Self::RateLimited),
            400..=499 => Some(Self::BadRequest),
            500..=599 => Some(Self::Server),
            // 1xx, 3xx, and anything outside the status-code space: §4.5's
            // "1xx / invalid status line" row.
            _ => Some(Self::Protocol),
        }
    }
}

impl fmt::Display for ProviderFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code() {
            Some(code) => f.write_str(code),
            None => f.write_str("cancelled"),
        }
    }
}

/// A provider failure: which row of §4.5 fired, what the message was, and how
/// many retries have already been spent against it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{fault}: {message}")]
pub struct ProviderError {
    /// Which row of §4.5 applies.
    pub fault: ProviderFault,
    /// Human-readable detail. §4.5 fixes the wording for `Auth` outright:
    /// "Provider `<id>`: invalid API key. Run 'cairn auth login `<id>`'."
    pub message: String,
    /// Parsed `Retry-After` (seconds or HTTP-date) for `RateLimited`.
    pub retry_after: Option<Duration>,
    /// Backoffs already taken, starting at 0. §4.5's column used to be headed
    /// *Attempts* while its `E-PROV-MALFORMED` note called the same number "1
    /// retry" and §11.4.1 called it `max_retries` — it counts retries, and the
    /// field is named for what it counts.
    pub retries_spent: u8,
}

impl ProviderError {
    /// A fresh error with no backoff taken and no `Retry-After`.
    #[must_use]
    pub fn new(fault: ProviderFault, message: impl Into<String>) -> Self {
        Self {
            fault,
            message: message.into(),
            retry_after: None,
            retries_spent: 0,
        }
    }

    /// The stable `E-PROV-*` code (§4.5), or `None` when the "error" is a
    /// cancellation.
    #[must_use]
    pub const fn code(&self) -> Option<&'static str> {
        self.fault.code()
    }

    /// How many retries §4.5 still allows, given what has been spent.
    #[must_use]
    pub fn remaining_retries(&self) -> u8 {
        self.fault.retries().saturating_sub(self.retries_spent)
    }

    /// `true` once §4.5 says to stop: the fault is not retryable, or its
    /// budget is spent (REQ-PROV-007 — this is what aborts the turn).
    #[must_use]
    pub fn exhausted(&self) -> bool {
        !self.fault.retryable() || self.remaining_retries() == 0
    }
}

impl From<SseError> for ProviderError {
    fn from(err: SseError) -> Self {
        Self::new(ProviderFault::from_sse(&err), err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every `E-PROV-*` code in the closed registry (§16.4, enforced by
    /// `lint-docs.sh --check-codes`) has a row here, and no row invents a code
    /// the registry does not know.
    #[test]
    fn the_fault_table_covers_exactly_the_provider_codes() {
        let registered: BTreeSet<&str> = cairn_core::error::ALL_CODES
            .iter()
            .copied()
            .filter(|code| code.starts_with("E-PROV-"))
            .collect();
        let covered: BTreeSet<&str> = ALL_FAULTS
            .iter()
            .copied()
            .filter_map(ProviderFault::code)
            .collect();
        assert_eq!(
            covered, registered,
            "§4.5's taxonomy and the error registry have drifted apart"
        );
        assert_eq!(registered.len(), 18);
    }

    /// §4.5, row by row: code, retryability, attempt budget and backoff. This
    /// is the executable copy of the table — if the spec and this test ever
    /// disagree, one of them is wrong.
    #[test]
    fn the_fault_table_matches_section_4_5() {
        use Backoff::*;
        let expected: &[(ProviderFault, &str, bool, u8, Backoff)] = &[
            (ProviderFault::Auth, "E-PROV-AUTH", false, 0, None),
            (ProviderFault::Forbidden, "E-PROV-FORBID", false, 0, None),
            (ProviderFault::NoModel, "E-PROV-NOMODEL", false, 0, None),
            (
                ProviderFault::Timeout,
                "E-PROV-TIMEOUT",
                true,
                5,
                ExponentialFullJitter,
            ),
            (
                ProviderFault::RateLimited,
                "E-PROV-RATELIMIT",
                true,
                5,
                RateLimited,
            ),
            (
                ProviderFault::PayloadTooLarge,
                "E-PROV-PAYLOAD",
                false,
                0,
                None,
            ),
            (
                ProviderFault::ContextLength,
                "E-PROV-CONTEXT",
                true,
                1,
                None,
            ),
            (
                ProviderFault::ContentFilter,
                "E-PROV-FILTER",
                false,
                0,
                None,
            ),
            (ProviderFault::BadRequest, "E-PROV-REQ", false, 0, None),
            (
                ProviderFault::Server,
                "E-PROV-SERVER",
                true,
                5,
                ExponentialFullJitter,
            ),
            (ProviderFault::Tls, "E-PROV-TLS", false, 0, None),
            (
                ProviderFault::Unreachable,
                "E-PROV-NET",
                true,
                5,
                ExponentialFullJitter,
            ),
            (
                ProviderFault::Truncated,
                "E-PROV-NET",
                true,
                1,
                ExponentialFullJitter,
            ),
            (ProviderFault::Idle, "E-PROV-IDLE", true, 5, Exponential),
            (
                ProviderFault::MalformedStream,
                "E-PROV-MALFORMED",
                true,
                1,
                Fixed(Duration::from_secs(1)),
            ),
            (
                ProviderFault::EventTooBig,
                "E-PROV-EVENTBIG",
                false,
                0,
                None,
            ),
            (ProviderFault::Protocol, "E-PROV-PROTO", false, 0, None),
            (
                ProviderFault::FallbackDisabled,
                "E-PROV-FALLBACK",
                false,
                0,
                None,
            ),
            (ProviderFault::Offline, "E-PROV-OFFLINE", false, 0, None),
        ];
        for &(fault, code, retryable, retries, backoff) in expected {
            assert_eq!(fault.code(), Some(code), "{fault:?}");
            assert_eq!(fault.retryable(), retryable, "{code}");
            assert_eq!(fault.retries(), retries, "{code}");
            assert_eq!(fault.backoff(), backoff, "{code}");
        }
        assert_eq!(expected.len(), ALL_FAULTS.len() - 1, "Cancelled has no row");
        assert!(ProviderFault::Cancelled.code().is_none());
        assert!(!ProviderFault::Cancelled.retryable());
    }

    /// §4.3 and §4.7 through the SSE layer: each stream failure lands on the
    /// §4.5 row the spec assigns it.
    #[test]
    fn sse_failures_land_on_their_section_4_5_row() {
        let too_big = SseError::EventTooBig {
            limit: 1024,
            seen: 1025,
        };
        let idle = SseError::Idle {
            timeout: Duration::from_secs(45),
        };
        let source = SseError::Source("broken pipe".to_string());
        let cancelled = SseError::Cancelled;

        assert_eq!(too_big.code(), Some("E-PROV-EVENTBIG"));
        assert_eq!(idle.code(), Some("E-PROV-IDLE"));

        // §4.3 rule 4: abort, no retry.
        let error = ProviderError::from(too_big);
        assert_eq!(error.code(), Some("E-PROV-EVENTBIG"));
        assert!(error.exhausted(), "an oversized event is never retried");

        // §4.7: a mid-stream disconnect retries the whole call.
        let error = ProviderError::from(source.clone());
        assert_eq!(error.code(), Some("E-PROV-NET"));
        assert_eq!(error.remaining_retries(), 5);
        assert!(!error.exhausted());

        let error = ProviderError::from(idle);
        assert_eq!(error.code(), Some("E-PROV-IDLE"));

        // Cancellation is not an error code (§11.2 exit 7).
        let error = ProviderError::from(cancelled);
        assert_eq!(error.code(), None);
        assert!(error.to_string().starts_with("cancelled: "));
    }

    /// §4.5's retry budget is spent, not guessed: five backoffs are allowed,
    /// and the call stops once the fifth has been taken.
    #[test]
    fn the_retry_budget_is_spent_rather_than_guessed() {
        let mut error = ProviderError::new(ProviderFault::RateLimited, "429");
        assert_eq!(error.remaining_retries(), 5);
        assert!(!error.exhausted());

        for taken in 1..5 {
            error.retries_spent = taken;
            assert!(!error.exhausted(), "{taken} backoffs still has budget");
        }
        error.retries_spent = 5;
        assert!(error.exhausted(), "the fifth backoff was the last one");

        // A non-retryable fault stops before any backoff, whatever the count.
        let mut error = ProviderError::new(ProviderFault::Auth, "401");
        assert!(error.exhausted());
        error.retries_spent = 9;
        assert!(error.exhausted());
    }

    /// `Display` carries the code, because REQ-CLI-002 requires the code to be
    /// printable from whatever surfaces the failure.
    #[test]
    fn display_carries_the_code() {
        let error = ProviderError::new(ProviderFault::Auth, "invalid API key");
        assert_eq!(error.to_string(), "E-PROV-AUTH: invalid API key");
        assert_eq!(
            ProviderError::new(ProviderFault::Offline, "network.offline = true").to_string(),
            "E-PROV-OFFLINE: network.offline = true"
        );
    }

    /// Every fault is `Send + Sync + 'static` so a boxed `Provider` can ship
    /// one across the stream boundary.
    #[test]
    fn faults_are_send_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<ProviderFault>();
        assert_send_sync::<ProviderError>();
    }
}
