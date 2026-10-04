//! SPEC §4.5's retry policy, written as values the loop can be tested against.
//!
//! §4.5 gives three things: a backoff formula, a per-condition attempt budget,
//! and REQ-PROV-005's 180 s wall clock. All three are pure. Keeping them pure
//! means the retry loop can be a thin `while` over a `ProviderFault` rather
//! than a second copy of the table, and means T-PROV-005 can pin a `Retry-After`
//! backoff down to the millisecond without a live server.

use std::ops::RangeInclusive;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;

use crate::error::{Backoff, ProviderFault};

/// REQ-PROV-005 — one model call, retries included, must not run past this
/// (`providers.max_total_ms = 180000`, §11.4.1).
pub const MAX_TOTAL: Duration = Duration::from_secs(180);

/// §4.5's `E-PROV-RATELIMIT` ceiling: `max(delay_n, Retry-After)` may not
/// exceed 120 s, however long the provider asked for.
pub const RATE_LIMIT_CAP: Duration = Duration::from_secs(120);

/// §4.5's base term, `min(30000, 500 * 2^(n-1))` ms, with `n` one-based.
fn base_delay_ms(attempt: u8) -> u64 {
    let shift = u32::from(attempt.saturating_sub(1)).min(16);
    500_u64.saturating_mul(1_u64 << shift).min(30_000)
}

fn as_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The inclusive millisecond range §4.5 allows for this backoff, or `None`
/// when §4.5 says not to retry at all.
///
/// `attempt` is one-based and names the backoff *after* that attempt — so with
/// §4.5's `Retries` column at 5, the valid answers are `1..=5`, matching the
/// formula's "n = 1..5". A point range means §4.5 asked for no jitter for that
/// row (`E-PROV-IDLE`'s plain "exponential", `E-PROV-MALFORMED`'s "1 s fixed"),
/// which is what lets a test assert an exact wait.
#[must_use]
pub fn delay_bounds(
    attempt: u8,
    fault: ProviderFault,
    retry_after: Option<Duration>,
) -> Option<RangeInclusive<u64>> {
    if !fault.retryable() || attempt == 0 || attempt > fault.retries() {
        return None;
    }
    let base = base_delay_ms(attempt);
    Some(match fault.backoff() {
        Backoff::None => 0..=0,
        Backoff::Fixed(fixed) => {
            let ms = as_millis(fixed);
            ms..=ms
        }
        Backoff::Exponential => base..=base,
        Backoff::ExponentialFullJitter => 0..=base,
        Backoff::RateLimited => {
            // `max(delay_n, retry_after_ms)` capped at 120000 ms. With no
            // header this degrades to §4.5's "standard backoff".
            let floor = retry_after.map_or(0, as_millis);
            let floor = floor.min(as_millis(RATE_LIMIT_CAP));
            let ceiling = base.max(floor).min(as_millis(RATE_LIMIT_CAP));
            floor..=ceiling
        }
    })
}

/// Draw one delay from [`delay_bounds`]. Point ranges never reach the RNG, so
/// the rows §4.5 left un-jittered stay deterministic whatever the seed.
#[must_use]
pub fn sample_delay<R: Rng>(bounds: &RangeInclusive<u64>, rng: &mut R) -> Duration {
    let ms = if bounds.start() == bounds.end() {
        *bounds.start()
    } else {
        rng.gen_range(bounds.clone())
    };
    Duration::from_millis(ms)
}

/// §4.5: `Retry-After` is either delta-seconds or an HTTP-date (RFC 5322, by
/// way of RFC 7231 §7.1.3).
///
/// A date already in the past means "retry immediately" and yields zero, not
/// `None` — the difference matters, because `None` falls back to plain backoff
/// while `Some(0)` keeps the floor at zero but still caps at 120 s. Anything
/// unparseable is `None`, which is §4.5's "429 without header" row.
#[must_use]
pub fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let target = DateTime::parse_from_rfc2822(value).ok()?;
    let delta = target.with_timezone(&Utc) - now;
    if delta.num_milliseconds() <= 0 {
        return Some(Duration::ZERO);
    }
    delta.to_std().ok()
}

/// REQ-PROV-005 — the wall clock one call, retries included, is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryBudget {
    deadline: std::time::Instant,
}

impl RetryBudget {
    /// A budget of [`MAX_TOTAL`], armed now.
    #[must_use]
    pub fn new() -> Self {
        Self::after(MAX_TOTAL)
    }

    /// A budget of `total`, armed now. Tests use a shorter one to reach the
    /// boundary without waiting out three minutes.
    #[must_use]
    pub fn after(total: Duration) -> Self {
        Self {
            deadline: std::time::Instant::now() + total,
        }
    }

    /// Time left, floored at zero.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
    }

    /// The delay to actually sleep, or `None` when the budget cannot afford
    /// it: sleeping past the deadline would breach REQ-PROV-005, so the loop
    /// stops and surfaces the fault instead.
    #[must_use]
    pub fn afford(&self, delay: Duration) -> Option<Duration> {
        (delay <= self.remaining()).then_some(delay)
    }
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn rng() -> StdRng {
        StdRng::seed_from_u64(0x00C0_FFEE)
    }

    /// §4.5's formula, `min(30000, 500 * 2^(n-1))`, for the five retries the
    /// `Retries` column allows.
    #[test]
    fn the_base_backoff_doubles_per_retry_and_caps_at_30_s() {
        let expected = [500, 1_000, 2_000, 4_000, 8_000];
        for (i, &want) in expected.iter().enumerate() {
            assert_eq!(base_delay_ms(u8::try_from(i).expect("small") + 1), want);
        }
        // The cap is what the formula names, not the retry count: asking for a
        // base far past n = 5 must not overflow or run away.
        assert_eq!(base_delay_ms(6), 8_000 * 2);
        assert_eq!(base_delay_ms(30), 30_000);
    }

    /// Full jitter means the delay may be *anything* in the range, including
    /// zero — the whole point of the "full" in the name.
    #[test]
    fn full_jitter_sweeps_the_whole_range() {
        let bounds = delay_bounds(1, ProviderFault::Server, None).expect("retryable");
        assert_eq!(bounds, 0..=500);
        let bounds = delay_bounds(5, ProviderFault::Unreachable, None).expect("retryable");
        assert_eq!(bounds, 0..=8_000);

        let mut rng = rng();
        let samples: Vec<u64> = (0..200)
            .map(|_| as_millis(sample_delay(&bounds, &mut rng)))
            .collect();
        assert!(
            samples.iter().any(|&ms| ms < 4_000) && samples.iter().any(|&ms| ms > 4_000),
            "200 draws from 0..=8000 should straddle the midpoint, got {samples:?}"
        );
        assert!(samples.iter().all(|&ms| ms <= 8_000));
    }

    /// §4.5 asks for no jitter on `E-PROV-IDLE` ("exponential") or on
    /// `E-PROV-MALFORMED` ("1 s fixed"), so both are point ranges — which is
    /// what makes their rows assertable exactly.
    #[test]
    fn the_unjittered_rows_are_point_ranges() {
        assert_eq!(
            delay_bounds(3, ProviderFault::Idle, None),
            Some(2_000..=2_000)
        );
        assert_eq!(
            delay_bounds(1, ProviderFault::MalformedStream, None),
            Some(1_000..=1_000)
        );
        let mut rng = rng();
        let bounds = delay_bounds(1, ProviderFault::MalformedStream, None).expect("retryable");
        assert_eq!(sample_delay(&bounds, &mut rng), Duration::from_secs(1));
    }

    /// A fault §4.5 marks non-retryable has no delay at all, and neither does
    /// a retry past the column's budget — the two ways a loop can stop.
    #[test]
    fn non_retryable_faults_and_spent_budgets_have_no_delay() {
        for fault in [
            ProviderFault::Auth,
            ProviderFault::Forbidden,
            ProviderFault::NoModel,
            ProviderFault::PayloadTooLarge,
            ProviderFault::ContentFilter,
            ProviderFault::BadRequest,
            ProviderFault::Tls,
            ProviderFault::Protocol,
            ProviderFault::EventTooBig,
            ProviderFault::FallbackDisabled,
            ProviderFault::Offline,
            ProviderFault::Cancelled,
        ] {
            assert!(!fault.retryable(), "{fault:?}");
            assert_eq!(delay_bounds(1, fault, None), None, "{fault:?}");
        }

        // One retry allowed, so n = 2 is out of budget.
        assert_eq!(
            delay_bounds(1, ProviderFault::MalformedStream, None),
            Some(1_000..=1_000)
        );
        assert_eq!(delay_bounds(2, ProviderFault::MalformedStream, None), None);
        // Five allowed; the sixth backoff is not.
        assert!(delay_bounds(5, ProviderFault::Timeout, None).is_some());
        assert_eq!(delay_bounds(6, ProviderFault::Timeout, None), None);
        // Zero is never a retry index.
        assert_eq!(delay_bounds(0, ProviderFault::Timeout, None), None);
    }

    /// T-PROV-005's first half: `Retry-After: 7` is a *floor*, and at n = 1
    /// the jitter term is at most 500 ms, so the drawn delay is exactly 7.0 s —
    /// inside the row's [7 s, 7.5 s] with nothing to sample.
    #[test]
    fn retry_after_is_a_floor_at_exactly_seven_seconds() {
        let bounds = delay_bounds(1, ProviderFault::RateLimited, Some(Duration::from_secs(7)))
            .expect("429 is retryable");
        assert_eq!(bounds, 7_000..=7_000);
        let mut rng = rng();
        assert_eq!(sample_delay(&bounds, &mut rng), Duration::from_secs(7));
    }

    /// The floor competes with the jitter term rather than replacing it: by
    /// n = 5 the base alone is 8 s, so `max(8 s, 7 s)` may go over.
    #[test]
    fn the_floor_loses_to_a_larger_jitter_ceiling() {
        let bounds = delay_bounds(5, ProviderFault::RateLimited, Some(Duration::from_secs(7)))
            .expect("429 is retryable");
        assert_eq!(bounds, 7_000..=8_000);
    }

    /// §4.5's 120 s cap binds however long the header asks for — a provider
    /// saying "come back in an hour" must not park the call for an hour.
    #[test]
    fn the_rate_limit_cap_binds_whatever_the_header_says() {
        let bounds = delay_bounds(
            1,
            ProviderFault::RateLimited,
            Some(Duration::from_secs(3_600)),
        )
        .expect("429 is retryable");
        assert_eq!(bounds, 120_000..=120_000);

        // Just under the cap keeps its own floor.
        let bounds = delay_bounds(1, ProviderFault::RateLimited, Some(Duration::from_secs(60)))
            .expect("429 is retryable");
        assert_eq!(bounds, 60_000..=60_000);
    }

    /// §4.5's "429 without header" row: no floor, plain backoff.
    #[test]
    fn a_429_without_the_header_is_plain_backoff() {
        assert_eq!(
            delay_bounds(4, ProviderFault::RateLimited, None),
            Some(0..=4_000)
        );
    }

    /// §4.5: the header is seconds *or* an HTTP-date. Both parse; anything
    /// else is the "without header" row.
    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = DateTime::parse_from_rfc2822("Wed, 21 Oct 2015 07:28:00 +0000")
            .expect("rfc2822")
            .with_timezone(&Utc);

        assert_eq!(parse_retry_after("7", now), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("  120 ", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after("0", now), Some(Duration::ZERO));

        // A date 90 s ahead — whole seconds only, because RFC 2822 carries no
        // sub-second field and a fractional `now` would make this wobble.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:29:30 +0000", now),
            Some(Duration::from_secs(90))
        );

        // A date already past means "now", not "fall back to plain backoff".
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 05:28:00 +0000", now),
            Some(Duration::ZERO)
        );

        // An offset other than UTC is still read correctly.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 09:28:00 +0200", now),
            Some(Duration::ZERO)
        );

        assert_eq!(parse_retry_after("tomorrow", now), None);
        assert_eq!(parse_retry_after("", now), None);
        assert_eq!(parse_retry_after("7 seconds", now), None);
    }

    /// REQ-PROV-005: the budget is 180 s and it refuses rather than sleeping
    /// past its own deadline.
    #[test]
    fn the_budget_is_180_seconds_and_refuses_what_it_cannot_afford() {
        assert_eq!(MAX_TOTAL, Duration::from_secs(180));
        assert_eq!(MAX_TOTAL.as_millis(), 180_000, "§11.4.1 max_total_ms");

        let budget = RetryBudget::new();
        assert!(budget.remaining() <= MAX_TOTAL);
        assert!(budget.afford(Duration::from_secs(1)).is_some());
        assert!(budget.afford(Duration::from_secs(179)).is_some());

        // An exhausted budget declines everything, which is what ends the call.
        let exhausted = RetryBudget::after(Duration::ZERO);
        assert!(exhausted.remaining().is_zero());
        assert_eq!(exhausted.afford(Duration::ZERO), Some(Duration::ZERO));
        assert_eq!(exhausted.afford(Duration::from_millis(1)), None);

        // A second-long budget really does decline a two-second sleep.
        let tiny = RetryBudget::after(Duration::from_secs(1));
        assert!(tiny.afford(Duration::from_secs(2)).is_none());
        assert!(tiny.afford(Duration::from_millis(1)).is_some());
    }

    /// Five retries means five backoffs, and the sixth has nowhere to go —
    /// the arithmetic the loop leans on to know it is finished.
    #[test]
    fn five_retries_are_five_backoffs() {
        let fault = ProviderFault::RateLimited;
        assert_eq!(fault.retries(), 5);
        let delays: Vec<_> = (1..=fault.retries())
            .filter_map(|n| delay_bounds(n, fault, Some(Duration::from_secs(7))))
            .collect();
        assert_eq!(delays.len(), 5, "n = 1..5 in §4.5's formula");
        assert!(delay_bounds(6, fault, None).is_none());
    }
}
