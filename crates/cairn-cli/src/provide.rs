//! Build a live provider from the loaded config (§4.9, §4.10, §11.4).
//!
//! The bridging rules, in one place so `run` (and later `chat`) cannot drift
//! apart:
//!
//! * The model id resolves against the effective registry — `models_path`
//!   when it loads, the bundle otherwise (the `W-REG-FALLBACK` warning, if
//!   any, was already emitted at load; it is not re-emitted here).
//! * `providers.<id>.base_url` overrides the registry URL (`""` keeps the
//!   default); `api_key` is the §4.10 step-4 fallback after both env steps;
//!   `ca_bundle` reaches the TLS client.
//! * `models.<id>.max_output`/`temperature` override the request fields; the
//!   retry budget is `max_total_ms` capped at §4.5's 180 s ceiling.
//! * A model id the registry does not know fails here with `E-CFG-NOMODEL`,
//!   not at the socket — unless `models.<id>.context_window` is set
//!   (REQ-PROV-013's escape hatch). That entry is then synthesised: the
//!   provider is the id's `<provider>/` prefix and must be one the registry
//!   knows; `max_output` defaults to 4096 (capped at the window), tool calling
//!   is native only for the hosted kinds (the §4.6 prompt fallback works
//!   anywhere, so the unknown stays safe), and pricing is `null`.

use std::sync::Arc;

use cairn_config::{expand_tilde, Config};
use cairn_core::error::codes;
use cairn_core::registry::Registry;
use cairn_provider::{env_key, RetryBudget, MAX_TOTAL};

use crate::output::Fail;

/// A provider ready to stream, with the call parameters the config decided.
pub struct LiveProvider {
    pub provider: Arc<dyn cairn_provider::Provider>,
    /// Canonical registry id, for `ModelRequest` and display.
    pub model_id: String,
    pub max_tokens: u32,
    pub temperature: Option<f64>,
    pub budget: RetryBudget,
    /// §4.9's price row, `null` figures and all (REQ-PROV-012).
    pub pricing: cairn_core::registry::Pricing,
}

/// The effective registry for this run: `models_path` when it loads, the
/// bundle otherwise. A broken override was already reported as
/// `W-REG-FALLBACK` at load, so the fallback here is silent. (This mirrors
/// `cairn-config`'s loader on the core document type the adapters resolve
/// against; the config crate's own `Registry` is a summary view.)
fn effective_registry(config: &Config) -> Registry {
    if config.models_path.trim().is_empty() {
        return cairn_core::registry::bundled().clone();
    }
    let get = |key: &str| std::env::var(key).ok();
    let path = expand_tilde(config.models_path.trim(), &get);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| Registry::parse(&text).ok())
        .unwrap_or_else(|| cairn_core::registry::bundled().clone())
}

/// REQ-PROV-013's escape hatch: a registry row for a model the registry does
/// not list, from `models.<id>` plus the provider named by the id's prefix.
/// `None` when the user did not define a context window, or the prefix names
/// no registry provider (the caller then reports `E-CFG-NOMODEL`).
fn user_model_entry(
    config: &Config,
    registry: &Registry,
    id: &str,
) -> Option<cairn_core::registry::ModelEntry> {
    use cairn_core::registry::{CapabilityFlags, ModelEntry, Pricing, ProviderKind, Tokenizer};

    let over = config.models.get(id)?;
    let context_window = over.context_window?;
    let provider = id.split_once('/')?.0;
    let kind = registry.providers.get(provider)?.kind;
    let native_tools = matches!(
        kind,
        ProviderKind::Anthropic | ProviderKind::Openai | ProviderKind::OpenaiCompatible
    );
    Some(ModelEntry {
        provider: provider.to_string(),
        display_name: None,
        context_window,
        max_output: over.max_output.unwrap_or_else(|| context_window.min(4096)),
        capabilities: CapabilityFlags {
            tool_calling: native_tools,
            streaming: true,
            ..CapabilityFlags::default()
        },
        pricing: Pricing::default(),
        tokenizer: Tokenizer::Unknown,
        aliases: Vec::new(),
    })
}

/// Resolve `config.model` to a live provider. Fails with `E-CFG-NOMODEL`
/// (exit 2) when the id names nothing runnable — the call never starts.
pub fn build(config: &Config) -> Result<LiveProvider, Fail> {
    use cairn_core::error::ExitStatus;
    use cairn_provider::{
        AnthropicAdapter, OllamaAdapter, OpenaiAdapter, OpenaiCompatibleAdapter, VllmAdapter,
    };

    let mut registry = effective_registry(config);
    let model_id = config.model.trim();
    if registry.resolve_with_provider(model_id).is_none() {
        if let Some(entry) = user_model_entry(config, &registry, model_id) {
            registry.models.insert(model_id.to_string(), entry);
        }
    }
    let found = registry.resolve_with_provider(model_id).ok_or_else(|| {
        Fail::new(
            codes::CFG_NOMODEL,
            ExitStatus::Usage,
            format!("unknown model `{model_id}`"),
            Some(
                "pick a registry id or alias (see `models.json`), or define one under `models.<id>`"
                    .to_string(),
            ),
        )
    })?;
    let provider_key = found.provider_id.to_string();
    let kind = found.provider.kind;
    let provider_config = config.providers.get(&provider_key);
    let base_url = provider_config
        .map(|provider| provider.base_url.clone())
        .filter(|url| !url.is_empty());
    let key = env_key(&provider_key, found.provider.env_key.as_deref()).or_else(|| {
        provider_config
            .map(|provider| provider.api_key.clone())
            .filter(|key| !key.is_empty())
    });
    let ca_bundle = provider_config
        .map(|provider| provider.ca_bundle.clone())
        .filter(|bundle| !bundle.is_empty());
    let ca_bundle_path;
    let ca_bundle = match &ca_bundle {
        Some(bundle) => {
            let get = |key: &str| std::env::var(key).ok();
            ca_bundle_path = expand_tilde(bundle, &get);
            Some(ca_bundle_path.as_path())
        }
        None => None,
    };
    let model_entry = found.model.clone();
    let canonical = found.id.to_string();
    let provider: Arc<dyn cairn_provider::Provider> = match kind {
        cairn_core::registry::ProviderKind::Anthropic => Arc::new(AnthropicAdapter::new(
            &canonical, &registry, key, ca_bundle, base_url,
        )),
        cairn_core::registry::ProviderKind::Openai => Arc::new(OpenaiAdapter::new(
            &canonical, &registry, key, ca_bundle, base_url,
        )),
        cairn_core::registry::ProviderKind::OpenaiCompatible => {
            // No registry entry to resolve a proxy from — but reaching this
            // arm means the model *did* resolve, so its provider entry named
            // this kind explicitly. The URL still comes from config.
            let url = base_url.ok_or_else(|| {
                Fail::new(
                    codes::CFG_NOMODEL,
                    ExitStatus::Usage,
                    format!("provider `{provider_key}` needs `providers.{provider_key}.base_url`"),
                    Some("a proxy has no registry default to fall back to".to_string()),
                )
            })?;
            Arc::new(OpenaiCompatibleAdapter::new(
                &canonical,
                url,
                key,
                cairn_provider::Capabilities::from_entry(&model_entry),
                ca_bundle,
            ))
        }
        cairn_core::registry::ProviderKind::Ollama => Arc::new(OllamaAdapter::new(
            &canonical, &registry, key, ca_bundle, base_url,
        )),
        cairn_core::registry::ProviderKind::Vllm => Arc::new(VllmAdapter::new(
            &canonical, &registry, key, ca_bundle, base_url,
        )),
    };
    let over = config.models.get(canonical.as_str());
    let max_tokens = over
        .and_then(|model| model.max_output)
        .unwrap_or(model_entry.max_output);
    let temperature = over
        .and_then(|model| model.temperature)
        .or(Some(config.temperature));
    // §4.5's ceiling binds the knob: a configured 600 s must not outlive
    // the 180 s REQ-PROV-005 budget the loop enforces.
    let ceiling_ms = u64::try_from(MAX_TOTAL.as_millis()).unwrap_or(u64::MAX);
    let total_ms =
        provider_config.map_or(ceiling_ms, |provider| provider.max_total_ms.min(ceiling_ms));
    Ok(LiveProvider {
        provider,
        model_id: canonical,
        max_tokens,
        temperature,
        budget: RetryBudget::after(std::time::Duration::from_millis(total_ms)),
        pricing: model_entry.pricing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(model: &str) -> Config {
        Config {
            model: model.to_string(),
            ..Config::default()
        }
    }

    /// An id the registry does not know fails before any socket, as
    /// `E-CFG-NOMODEL` — the same code `config validate` reports
    /// (T-PROV-013), so one fix clears both surfaces.
    #[test]
    fn unknown_model_fails_with_cfg_nomodel() {
        let config = config_with("no/such-model");
        let Err(err) = build(&config) else {
            panic!("unknown model fails")
        };
        assert_eq!(err.code, codes::CFG_NOMODEL);
        assert_eq!(err.exit, 2);
    }

    /// `max_total_ms` above §4.5's ceiling binds to the ceiling: a configured
    /// 600 s must not outlive REQ-PROV-005's 180 s budget.
    #[test]
    fn the_retry_budget_never_outlives_180_seconds() {
        let mut config = config_with("openai/gpt-5.1-codex");
        let provider = config.providers.get_mut("openai").expect("defaulted");
        provider.max_total_ms = 600_000;
        let live = build(&config).expect("builds");
        assert!(live.budget.remaining() <= std::time::Duration::from_secs(180));
        assert_eq!(live.model_id, "openai/gpt-5.1-codex");
        assert_eq!(live.max_tokens, 128_000);
    }

    /// Per-model overrides reach the request fields: `max_output` replaces
    /// the registry ceiling, `temperature` replaces the global default.
    #[test]
    fn model_overrides_reach_the_request() {
        let mut config = config_with("openai/gpt-5.1-codex");
        config.models.insert(
            "openai/gpt-5.1-codex".to_string(),
            cairn_config::ModelOverride {
                context_window: None,
                max_output: Some(512),
                temperature: Some(0.9),
            },
        );
        let live = build(&config).expect("builds");
        assert_eq!(live.max_tokens, 512);
        assert_eq!(live.temperature, Some(0.9));
    }

    fn user_model(id: &str, window: Option<u32>) -> Config {
        let mut config = config_with(id);
        config.models.insert(
            id.to_string(),
            cairn_config::ModelOverride {
                context_window: window,
                max_output: None,
                temperature: None,
            },
        );
        config
    }

    /// REQ-PROV-013: a model the registry lacks runs when the user gives it a
    /// context window and a known provider prefix.
    #[test]
    fn a_user_defined_model_builds_from_its_provider_prefix() {
        let live = build(&user_model("ollama/my-finetune", Some(32_768))).expect("builds");
        assert_eq!(live.model_id, "ollama/my-finetune");
        assert_eq!(live.max_tokens, 4096, "defaults to 4096 under the window");
        let caps = live.provider.capabilities();
        assert_eq!(caps.max_context, 32_768);
        assert!(!caps.tool_calling, "local kinds use the §4.6 fallback");
    }

    #[test]
    fn a_small_window_caps_the_default_output() {
        let live = build(&user_model("ollama/tiny", Some(2048))).expect("builds");
        assert_eq!(live.max_tokens, 2048);
    }

    #[test]
    fn hosted_kinds_get_native_tool_calling() {
        let live = build(&user_model("openai/ft:gpt-private", Some(128_000))).expect("builds");
        assert!(live.provider.capabilities().tool_calling);
    }

    /// Without a window, or with a prefix no provider answers to, it is still
    /// `E-CFG-NOMODEL`.
    #[test]
    fn the_escape_hatch_needs_a_window_and_a_known_provider() {
        for config in [
            user_model("ollama/my-finetune", None),
            user_model("nowhere/my-finetune", Some(8192)),
            user_model("no-slash", Some(8192)),
        ] {
            let Err(err) = build(&config) else {
                panic!("must not build")
            };
            assert_eq!(err.code, codes::CFG_NOMODEL);
        }
    }
}
