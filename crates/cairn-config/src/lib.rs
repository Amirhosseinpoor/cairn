//! `cairn-config` — layered configuration (SPEC §11).
//!
//! * [`load()`] merges system → user → profile → project → `--config` → env → flags
//!   (SPEC §11.5) and returns every problem it found (REQ-CLI-003).
//! * [`validate()`] applies the rules of SPEC §11.4.2.
//! * [`schema::json_schema`] generates the annotated JSON Schema (REQ-ARCH-011).

pub mod issue;
pub mod keybindings;
pub mod load;
pub mod model;
pub mod paths;
pub mod schema;
pub mod validate;

pub use issue::{fatal_section, Issue, Source};
pub use load::{
    apply_kv, load, parse_bool_env, FlagOverrides, Kv, LayerInfo, LoadOptions, Loaded, ENV_KEYS,
};
pub use model::{Config, SCHEMA_VERSION};
pub use paths::{discover_workspace, expand_tilde, is_abs_or_tilde, EnvLookup, Paths};
pub use schema::{effective, json_schema, json_schema_pretty, EffectiveEntry, FlagSpec, FLAGS};
pub use validate::{bundled_registry, registry_updated_at, validate, Ctx, Registry};

#[cfg(test)]
mod tests {
    use super::*;

    /// T-CFG-005: `cairn config validate` (here: `load`) reports everything at once.
    #[test]
    fn loads_with_many_issues_but_returns_all() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("CAIRN_TRACE".to_string(), "nope".to_string());
        env.insert("CAIRN_MODE".to_string(), "sideways".to_string());
        let loaded = load(&LoadOptions {
            env: Some(env),
            ..Default::default()
        });
        let codes: Vec<&str> = loaded.issues.iter().map(|i| i.code).collect();
        assert!(
            codes.len() >= 2,
            "both bad values must be reported: {codes:?}"
        );
        assert!(
            codes.contains(&cairn_core::error::codes::CFG_BADENV),
            "{codes:?}"
        );
        assert!(loaded.fatal_issues().len() >= 2, "{codes:?}");
    }

    /// REQ-CLI-006 / REQ-ARCH-011: schema and effective view are available.
    #[test]
    fn public_api_is_complete() {
        let s = json_schema_pretty();
        assert!(s.contains("x-cli-flags"));
        let loaded = load(&LoadOptions::default());
        assert!(!effective(&loaded).is_empty());
        assert!(bundled_registry().model_ids.len() >= 5);
    }
}
