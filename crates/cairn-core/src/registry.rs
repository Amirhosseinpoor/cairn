//! SPEC §4.9 — the model registry, bundled at `cairn/assets/models.json`.
//!
//! §4.9 describes the file; something has to hold it. It lives in `cairn-core`
//! because two crates need it and neither may import the other: `cairn-config`
//! validates `model = …` against it (REQ-PROV-013) and `cairn-provider` reads
//! `capabilities`, `pricing` and `base_url` out of it for every adapter (§4.2,
//! §4.8). Both MAY import `core` (§3.2), so the file is parsed once rather than
//! twice — the failure mode of two parsers is a model that validates and then
//! cannot be called.
//!
//! The embed is compile time: `include_str!` yields a `&'static str`, so this
//! module still performs no I/O at runtime (REQ-ARCH-002).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The registry that ships in the binary (§4.9).
pub const BUNDLED_JSON: &str = include_str!("../../../assets/models.json");

/// A registry file that is not valid JSON, or not the shape §4.9 describes.
#[derive(Debug, thiserror::Error)]
#[error("model registry is not valid: {0}")]
pub struct RegistryError(#[from] serde_json::Error);

/// `providers.<id>.kind` — §4.9's `kind` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    OpenaiCompatible,
    Ollama,
    Vllm,
}

/// The registry's `tokenizer` enum (§4.9). Renamed by hand: `snake_case` on a
/// name like `O200kBase` is a guess, and these strings are compared verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tokenizer {
    #[serde(rename = "o200k_base")]
    O200kBase,
    #[serde(rename = "cl100k_base")]
    Cl100kBase,
    #[serde(rename = "p50k_base")]
    P50kBase,
    #[serde(rename = "unknown")]
    Unknown,
}

impl Tokenizer {
    /// Whether §4.8's estimator (`ceil(chars/4)`, or `ceil(bytes/3)` for
    /// CJK/code-heavy text) is the fallback for this tokenizer.
    #[must_use]
    pub const fn needs_estimator(self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// A model row's `capabilities` object — the seven booleans §4.9 requires,
/// *without* the two limits §3.4's `Capabilities` carries, which are separate
/// fields in the file (`context_window`, `max_output`) and not capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CapabilityFlags {
    pub tool_calling: bool,
    pub streaming: bool,
    pub reasoning: bool,
    pub prompt_cache: bool,
    pub vision: bool,
    pub parallel_tool_calls: bool,
    pub json_schema_strict: bool,
}

/// §4.9's `pricing`: every figure is per million tokens, `null` meaning the
/// price is unknown — which is what makes REQ-PROV-012's cost `null` rather
/// than a guess.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Pricing {
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
}

impl Pricing {
    /// `true` when every figure §4.9 marks required is present and priced, so
    /// a cost computed from it is a number rather than `null`.
    #[must_use]
    pub const fn is_fully_priced(self) -> bool {
        self.input_per_mtok.is_some() && self.output_per_mtok.is_some()
    }
}

/// One entry of §4.9's `providers` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    pub base_url: String,
    /// `name[: prefix]` — the header to set and what precedes the key.
    /// §4.9 gives the default `"Authorization: Bearer"`; the bundled registry
    /// also stores Anthropic's bare `"x-api-key"`, which has no prefix.
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    #[serde(default)]
    pub env_key: Option<String>,
}

fn default_auth_header() -> String {
    String::from("Authorization: Bearer")
}

impl ProviderEntry {
    /// Split `auth_header` into the header name and the value prefix, so
    /// `"Authorization: Bearer"` sets `Authorization: Bearer <key>` and
    /// `"x-api-key"` sets `x-api-key: <key>`.
    #[must_use]
    pub fn auth_parts(&self) -> (&str, &str) {
        match self.auth_header.split_once(':') {
            Some((name, prefix)) => (name.trim(), prefix.trim()),
            None => (self.auth_header.trim(), ""),
        }
    }

    /// The request header carrying `key`.
    #[must_use]
    pub fn auth_header_value(&self, key: &str) -> String {
        let (_, prefix) = self.auth_parts();
        if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix} {key}")
        }
    }
}

/// One entry of §4.9's `models` object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Key into [`Registry::providers`].
    pub provider: String,
    #[serde(default)]
    pub display_name: Option<String>,
    pub context_window: u32,
    pub max_output: u32,
    pub capabilities: CapabilityFlags,
    pub pricing: Pricing,
    pub tokenizer: Tokenizer,
    /// Short names that resolve to this model (§4.9). A model may list its own
    /// id as an alias; resolving then finds it twice and says so by returning
    /// the same canonical id.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// The whole `models.json` document (§4.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Registry {
    pub schema_version: u32,
    pub updated_at: String,
    pub providers: BTreeMap<String, ProviderEntry>,
    pub models: BTreeMap<String, ModelEntry>,
}

/// A model id and everything needed to call it (§4.9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved<'a> {
    /// The canonical `<provider>/<name>` id, even when the caller used an alias.
    pub id: &'a str,
    pub model: &'a ModelEntry,
    /// The key into [`Registry::providers`] (same string as the id's prefix
    /// for a well-formed id, but not assumed to be).
    pub provider_id: &'a str,
    pub provider: &'a ProviderEntry,
}

impl Registry {
    /// Parse a `models.json` document. Unknown keys are ignored so a registry
    /// shipped by a newer `cairn update` still loads (REQ-PROV-014); a missing
    /// *required* key is an error, which the bundled-copy test turns into a
    /// CI failure rather than a silently empty registry.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the text is not JSON or does not match
    /// §4.9's required shape.
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        Ok(serde_json::from_str(text)?)
    }

    /// Resolve a configured id to a canonical id, matching an exact model id
    /// first and then any model's `aliases` (§4.9).
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<(&str, &ModelEntry)> {
        if let Some((canonical, model)) = self.models.get_key_value(id) {
            return Some((canonical.as_str(), model));
        }
        self.models
            .iter()
            .find(|(_, model)| model.aliases.iter().any(|alias| alias == id))
            .map(|(canonical, model)| (canonical.as_str(), model))
    }

    /// Resolve an id and look up its provider entry in one step — what an
    /// adapter needs before it can build a URL.
    #[must_use]
    pub fn resolve_with_provider(&self, id: &str) -> Option<Resolved<'_>> {
        let (id, model) = self.resolve(id)?;
        let (provider_id, provider) = self.providers.get_key_value(&model.provider)?;
        Some(Resolved {
            id,
            model,
            provider_id: provider_id.as_str(),
            provider,
        })
    }

    /// Canonical ids, sorted (the document is a `BTreeMap`, but say it).
    pub fn model_ids(&self) -> impl Iterator<Item = &str> {
        self.models.keys().map(String::as_str)
    }
}

/// The registry that ships in the binary.
///
/// A malformed bundle yields an empty registry rather than a panic (REQ-PROV-014
/// — a registry problem must not break startup); `the_bundled_registry_parses`
/// is what keeps that fallback from becoming the normal state.
#[must_use]
pub fn bundled() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| Registry::parse(BUNDLED_JSON).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{
      "schema_version": 1,
      "updated_at": "2026-10-03T00:00:00Z",
      "providers": {
        "demo": {"kind": "vllm", "base_url": "http://127.0.0.1:8000"},
        "cli": {"kind": "anthropic", "base_url": "https://api.anthropic.com",
                "auth_header": "x-api-key", "env_key": "ANTHROPIC_API_KEY"}
      },
      "models": {
        "demo/m": {
          "provider": "demo",
          "context_window": 8192,
          "max_output": 1024,
          "capabilities": {"tool_calling": false, "streaming": true,
            "reasoning": false, "prompt_cache": false, "vision": false,
            "parallel_tool_calls": false, "json_schema_strict": false},
          "pricing": {"input_per_mtok": null, "output_per_mtok": null},
          "tokenizer": "unknown",
          "aliases": ["m", "tiny"]
        }
      }
    }"#;

    /// The bundle is the whole point of §4.9 — if this fails, every model id
    /// in the shipped config fails to validate with it.
    #[test]
    fn the_bundled_registry_parses() {
        let reg = bundled();
        assert_eq!(reg.schema_version, 1, "§4.9 pins schema_version to 1");
        assert!(!reg.updated_at.is_empty());
        assert!(
            reg.models.len() >= 5 && reg.providers.len() >= 4,
            "bundled models.json shrank: {} models, {} providers",
            reg.models.len(),
            reg.providers.len()
        );
        assert!(reg.resolve("anthropic/claude-sonnet-4-5").is_some());
        assert!(reg.resolve("openai/gpt-5.1-codex").is_some());
        assert!(reg.resolve("ollama/qwen2.5-coder:14b").is_some());
    }

    /// Every model row carries the keys §4.9 marks required, points at a
    /// provider that exists, and stays inside the bounds the same section
    /// gives the schema.
    #[test]
    fn every_bundled_model_satisfies_the_section_4_9_schema() {
        let doc: serde_json::Value = serde_json::from_str(BUNDLED_JSON).expect("bundle is JSON");
        let models = doc["models"].as_object().expect("models object");
        assert!(!models.is_empty());

        for (id, row) in models {
            for key in [
                "provider",
                "context_window",
                "max_output",
                "capabilities",
                "pricing",
                "tokenizer",
            ] {
                assert!(row.get(key).is_some(), "{id} is missing required `{key}`");
            }
            for key in ["input_per_mtok", "output_per_mtok"] {
                assert!(
                    row["pricing"].get(key).is_some(),
                    "{id}.pricing is missing required `{key}`"
                );
            }
            for key in [
                "tool_calling",
                "streaming",
                "reasoning",
                "prompt_cache",
                "vision",
                "parallel_tool_calls",
                "json_schema_strict",
            ] {
                assert!(
                    row["capabilities"].get(key).is_some(),
                    "{id}.capabilities is missing required `{key}`"
                );
            }
        }

        let reg = bundled();
        for (id, model) in &reg.models {
            assert!(
                reg.providers.contains_key(&model.provider),
                "{id} names provider {:?}, which §4.9 does not define",
                model.provider
            );
            assert!(
                model.context_window >= 1024,
                "{id}: context_window too small"
            );
            assert!(model.max_output >= 1, "{id}: max_output too small");
        }
    }

    /// Aliases exist to be resolved; a dangling one is a dead key in a shipped
    /// file, which no test would notice unless it checked every one.
    #[test]
    fn every_bundled_alias_resolves_to_a_real_model() {
        let reg = bundled();
        for (id, model) in &reg.models {
            for alias in &model.aliases {
                let (canonical, _) = reg
                    .resolve(alias)
                    .unwrap_or_else(|| panic!("alias {alias:?} of {id} resolves to nothing"));
                assert_eq!(canonical, id, "alias {alias:?} belongs to two models");
            }
        }
    }

    #[test]
    fn an_id_resolves_exactly_before_an_alias_does() {
        let reg = Registry::parse(MINIMAL).expect("minimal parses");
        assert_eq!(reg.resolve("demo/m").map(|(id, _)| id), Some("demo/m"));
        assert_eq!(reg.resolve("m").map(|(id, _)| id), Some("demo/m"));
        assert_eq!(reg.resolve("tiny").map(|(id, _)| id), Some("demo/m"));
        assert_eq!(reg.resolve("nothing"), None);

        let resolved = reg.resolve_with_provider("tiny").expect("resolves");
        assert_eq!(resolved.id, "demo/m");
        assert_eq!(resolved.provider_id, "demo");
        assert_eq!(resolved.provider.kind, ProviderKind::Vllm);
        assert_eq!(resolved.model.context_window, 8192);
        assert!(reg.resolve_with_provider("ghost/m").is_none());
        assert!(reg.resolve_with_provider("demo/ghost").is_none());
    }

    /// §4.9's `auth_header` is two formats wearing one field name: Anthropic's
    /// has no prefix, everyone else's does.
    #[test]
    fn auth_header_is_a_name_and_an_optional_prefix() {
        let reg = Registry::parse(MINIMAL).expect("minimal parses");
        let bearer = &reg.providers["demo"];
        assert_eq!(bearer.auth_parts(), ("Authorization", "Bearer"));
        assert_eq!(bearer.auth_header_value("sk-x"), "Bearer sk-x");

        let bare = &reg.providers["cli"];
        assert_eq!(bare.auth_parts(), ("x-api-key", ""));
        assert_eq!(bare.auth_header_value("sk-ant-x"), "sk-ant-x");
        assert_eq!(bare.env_key.as_deref(), Some("ANTHROPIC_API_KEY"));

        // Absent `auth_header` is §4.9's default, not an empty header name.
        let text = r#"{"schema_version":1,"updated_at":"t",
          "providers":{"d":{"kind":"ollama","base_url":"http://x"}},
          "models":{}}"#;
        let reg = Registry::parse(text).expect("parses");
        assert_eq!(
            reg.providers["d"].auth_header, "Authorization: Bearer",
            "§4.9 gives auth_header a default"
        );
    }

    /// REQ-PROV-014 / REQ-ARCH-010: a registry written by a newer Cairn keeps
    /// loading, while a required key going missing is caught.
    #[test]
    fn unknown_keys_are_ignored_and_required_keys_are_not() {
        let with_extras = r#"{"schema_version":1,"updated_at":"t",
          "providers":{},"models":{},
          "something_new":{"a":1}}"#;
        assert!(Registry::parse(with_extras).is_ok(), "additive keys load");

        let missing = r#"{"schema_version":1,"updated_at":"t","providers":{}}"#;
        assert!(Registry::parse(missing).is_err(), "`models` is required");

        let not_json = "{ this is not json";
        assert!(Registry::parse(not_json).is_err());
    }

    /// §4.9's enums are compared verbatim against a shipped file, so every
    /// spelling the schema allows round-trips back to itself.
    #[test]
    fn kinds_and_tokenizers_round_trip() {
        let kinds = ["anthropic", "openai", "openai_compatible", "ollama", "vllm"];
        for kind in kinds {
            let value = serde_json::to_value(kind).expect("serialise");
            let back: ProviderKind = serde_json::from_value(value).expect("deserialise");
            assert_eq!(
                serde_json::to_value(back).expect("serialise"),
                serde_json::Value::from(kind)
            );
        }
        let tokenizers = ["o200k_base", "cl100k_base", "p50k_base", "unknown"];
        for tokenizer in tokenizers {
            let value = serde_json::to_value(tokenizer).expect("serialise");
            let back: Tokenizer = serde_json::from_value(value).expect("deserialise");
            assert_eq!(
                serde_json::to_value(back).expect("serialise"),
                serde_json::Value::from(tokenizer)
            );
        }
        // §4.8: an unknown tokenizer is the one that falls back to estimating.
        assert!(Tokenizer::Unknown.needs_estimator());
        assert!(!Tokenizer::O200kBase.needs_estimator());
    }

    /// §4.9's pricing is nullable on purpose — a `null` price is how
    /// REQ-PROV-012 gets `cost_usd: null` instead of a made-up number.
    #[test]
    fn pricing_knows_when_it_cannot_answer() {
        let priced = Pricing {
            input_per_mtok: Some(3.0),
            output_per_mtok: Some(15.0),
            cache_read_per_mtok: Some(0.3),
            cache_write_per_mtok: Some(3.75),
        };
        assert!(priced.is_fully_priced());

        let free = Pricing::default();
        assert!(!free.is_fully_priced());

        let reg = bundled();
        let codex = &reg.models["openai/gpt-5.1-codex"].pricing;
        assert!(
            !codex.is_fully_priced(),
            "gpt-5.1-codex is unpriced in §4.9"
        );
        let sonnet = &reg.models["anthropic/claude-sonnet-4-5"].pricing;
        assert!(sonnet.is_fully_priced());
    }

    /// `model_ids()` is what `cairn config validate` walks; a registry that
    /// silently returned nothing would let every `model =` through.
    #[test]
    fn model_ids_are_iterable_and_sorted() {
        let reg = bundled();
        let ids: Vec<&str> = reg.model_ids().collect();
        assert!(ids.len() >= 5);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        assert!(ids.contains(&"openai/o4-mini"));
    }

    /// A broken bundle degrades instead of panicking; the tests above are what
    /// would notice if `unwrap_or_default` ever became the normal path.
    #[test]
    fn a_malformed_registry_is_an_error_not_a_panic() {
        assert!(Registry::parse("[]").is_err(), "an array is not a registry");
        assert!(Registry::parse("").is_err());
        let bad = Registry::parse(r#"{"schema_version":"one"}"#);
        assert!(bad.is_err(), "schema_version is an integer");
    }
}
