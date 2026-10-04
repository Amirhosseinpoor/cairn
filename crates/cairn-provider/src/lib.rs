//! `cairn-provider` — Provider trait, adapters, retry, token accounting (SPEC 4.5, 4.6)
//!
//! The trait itself is §3.4's, unchanged: `BoxFuture` rather than `async fn`
//! so the registry can hold `Box<dyn Provider>`. What lives beside it here is
//! the vocabulary every adapter speaks — [`StreamEvent`] for the wire, the
//! §4.9 [`ProviderId`], and [`ProviderFault`], which turns §4.5's retry matrix
//! into values instead of five copies of the same `match`.
//!
//! Delivered in milestone **M1** (SPEC §15.4): the trait, §4.5's taxonomy and
//! retry policy, and §4.8/§4.9's registry and cost accounting are in place;
//! what is left is the five adapters, the retry loop, and the mock provider
//! with its cassettes.

mod accounting;
mod error;
mod retry;
mod types;

pub use accounting::{cost_usd, estimate_request, estimate_tokens};
pub use error::{Backoff, ProviderError, ProviderFault, ALL_FAULTS};
pub use retry::{
    delay_bounds, parse_retry_after, sample_delay, RetryBudget, MAX_TOTAL, RATE_LIMIT_CAP,
};
pub use types::{
    Capabilities, ModelRequest, ProviderHealth, ProviderId, StreamEvent, TokenCount, ToolSpec,
};

use cairn_core::cancel::CancellationToken;
use futures::future::BoxFuture;
use futures::stream::BoxStream;

/// SPEC §3.4 — the provider interface, object-safe so the model registry can
/// key `provider/…` names to `Box<dyn Provider>`.
///
/// Streaming is the only mode: an adapter that can only do non-streaming
/// buffers internally rather than growing a second method.
pub trait Provider: Send + Sync + 'static {
    /// The §4.9 `providers` key, e.g. `anthropic`.
    fn id(&self) -> &ProviderId;

    /// What this adapter can do, per §4.2's matrix.
    fn capabilities(&self) -> Capabilities;

    /// Run one model call as a stream of [`StreamEvent`]s.
    ///
    /// `cancel` is a child of the turn's token (§3.3); dropping the returned
    /// stream or raising the token must abort the HTTP body within 250 ms
    /// (§4.3).
    fn stream(
        &self,
        req: ModelRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<BoxStream<'static, StreamEvent>, ProviderError>>;

    /// Count the tokens one call would spend (§4.8), without making it.
    ///
    /// Never fails over to a network round trip on its own: a provider whose
    /// counting endpoint is unavailable falls back to §4.8's estimator and
    /// returns a `TokenCount` still flagged `estimated: true`.
    fn count_tokens<'a>(
        &'a self,
        req: &'a ModelRequest,
    ) -> BoxFuture<'a, Result<TokenCount, ProviderError>>;

    /// Cheap, non-network capability probe (§4.3/§4.10): credentials present,
    /// model id known, config usable.
    fn health(&self) -> ProviderHealth;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason §3.4 specifies `BoxFuture`: a trait held behind `dyn`
    /// must not name `Self` in a return type. Forming `dyn Provider` at all is
    /// the check — if anyone "simplifies" these to `async fn`, this test stops
    /// compiling.
    #[test]
    fn the_provider_trait_is_object_safe() {
        let _: fn(&dyn Provider) = |_| {};
        let _: fn(Box<dyn Provider>) = |_| {};
    }

    /// `BoxStream` is `Send`, so a stream built on one worker can be consumed
    /// on another — which is how the renderer drains §3.3's `delta_tx`.
    #[test]
    fn a_provider_stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<BoxStream<'static, StreamEvent>>();
        assert_send::<BoxFuture<'static, Result<BoxStream<'static, StreamEvent>, ProviderError>>>();
    }

    /// §4.2's branching selects a provider by capability, not by name, so the
    /// defaults must be honest rather than optimistic: streaming is the only
    /// mode §3.4 offers, everything else is unset until an adapter or the
    /// registry says otherwise.
    #[test]
    fn capabilities_start_conservative() {
        let capabilities = Capabilities::baseline();
        assert!(capabilities.streaming, "§3.4 has no non-streaming path");
        assert!(
            !capabilities.tool_calling,
            "a claim about a model comes from §4.2 or §4.9, never from a default"
        );
        assert!(!capabilities.reasoning);
        assert!(!capabilities.vision);
        assert_eq!(capabilities.max_context, 0, "comes from the model entry");
        assert_eq!(capabilities.max_output, 0, "comes from the model entry");
    }

    /// A default `ProviderHealth` is not `Ready`: nothing has checked §4.10's
    /// lookup order yet.
    #[test]
    fn health_is_not_ready_by_default() {
        let health: ProviderHealth = serde_json::from_value(serde_json::json!({
            "status": "no_credentials"
        }))
        .expect("deserialize");
        assert_eq!(health, ProviderHealth::NoCredentials);

        let health = ProviderHealth::Misconfigured {
            reason: "base_url is not a valid URI".to_string(),
        };
        let json = serde_json::to_value(&health).expect("serialize");
        assert_eq!(json["status"], "misconfigured");
        assert_eq!(json["reason"], "base_url is not a valid URI");
    }
}
