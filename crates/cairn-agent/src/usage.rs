//! What a finished turn spent and what it cost (REQ-PROV-011/012).

use cairn_core::message::Usage;
use cairn_core::registry::Pricing;
use cairn_provider::{cost_usd, estimate_tokens};

/// What a finished turn spent, and what that cost.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageReport {
    pub usage: Usage,
    pub cost_usd: Option<f64>,
}

/// REQ-PROV-011: the provider's numbers override Cairn's estimate. A report
/// that is missing, or says nothing was spent (T-FAULT-005: a proxy that
/// answers `usage: 0`), is not believed — the §4.8 estimator stands in and
/// the result is flagged `estimated`. `cost_usd` is `None`, never `0.0`, when
/// the model has no price (REQ-PROV-012, T-PROV-012).
#[must_use]
pub fn usage_report(
    reported: Option<Usage>,
    prompt_tokens: u32,
    output_text: &str,
    pricing: &Pricing,
) -> UsageReport {
    let usage = match reported {
        Some(real) if real.input > 0 || real.output > 0 => real,
        _ => Usage {
            output: estimate_tokens(output_text),
            ..Usage::estimate(prompt_tokens)
        },
    };
    let cost_usd = cost_usd(&usage, pricing);
    UsageReport { usage, cost_usd }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced() -> Pricing {
        Pricing {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(15.0),
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        }
    }

    /// REQ-PROV-011: real numbers win and are not flagged.
    #[test]
    fn reported_usage_overrides_the_estimate() {
        let report = usage_report(
            Some(Usage::reported(1_000_000, 100_000, 0, 0)),
            7,
            "ignored",
            &priced(),
        );
        assert!(!report.usage.estimated);
        assert_eq!(report.usage.input, 1_000_000);
        assert!((report.cost_usd.expect("priced") - 4.5).abs() < 1e-9);
    }

    /// T-FAULT-005: a provider that answers `usage: 0` (or nothing) is not
    /// believed — the estimator runs and the result says so.
    #[test]
    fn t_fault_005_zero_or_missing_usage_falls_back_to_the_estimator() {
        for reported in [Some(Usage::reported(0, 0, 0, 0)), None] {
            let report = usage_report(reported, 12, "abcdefgh", &priced());
            assert!(report.usage.estimated, "{reported:?}");
            assert_eq!(report.usage.input, 12);
            assert_eq!(report.usage.output, 2, "ceil(8 chars / 4)");
            assert!(report.cost_usd.is_some(), "an estimate still has a cost");
        }
    }

    /// T-PROV-012: no price means `null`, never `0.0`.
    #[test]
    fn t_prov_012_an_unpriced_model_reports_null_cost() {
        let report = usage_report(
            Some(Usage::reported(10, 10, 0, 0)),
            0,
            "",
            &Pricing::default(),
        );
        assert_eq!(report.cost_usd, None);
    }
}
