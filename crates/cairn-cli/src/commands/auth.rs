//! `cairn auth <sub>` (SPEC §4.10, §11.1, REQ-PROV-015/017/018).
//!
//! M0 resolves keys from the parts of the lookup order that exist without a
//! keychain: `CAIRN_<PROVIDER>_API_KEY`, the provider-standard env var,
//! `providers.<id>.api_key` and the workspace-local `.cairn/credentials.toml`.
//! Keychain storage lands with `login`/`logout` in M1.

use crate::args::AuthCmd;
use crate::commands::Startup;
use crate::output::Fail;
use cairn_config::bundled_registry;
use cairn_core::error::codes;
use std::path::Path;

/// Where a key came from (`env|keychain|file`, REQ-PROV-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Env,
    /// Built in M1 together with `auth login` (SPEC §4.10 step 3).
    #[allow(dead_code)]
    Keychain,
    File,
}

impl KeySource {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Keychain => "keychain",
            Self::File => "file",
        }
    }
}

/// One provider's credential status.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthRow {
    pub provider: String,
    pub enabled: bool,
    /// `None` when no key is available.
    pub source: Option<&'static str>,
    pub env_var: Option<String>,
    /// Last four characters only (REQ-PROV-018).
    pub last4: Option<String>,
    pub base_url: String,
}

/// Provider-standard env var names (SPEC §4.10 step 2).
fn standard_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "ollama" => Some("OLLAMA_API_KEY"),
        "vllm" => Some("VLLM_API_KEY"),
        _ => None,
    }
}

fn cairn_env_var(provider: &str) -> String {
    format!("CAIRN_{}_API_KEY", provider.to_ascii_uppercase())
}

/// Resolve one provider's key following SPEC §4.10 (steps 1, 2, 4, 5).
fn lookup(provider: &str, startup: &Startup) -> Option<(KeySource, String)> {
    let cairn_var = cairn_env_var(provider);
    if let Ok(v) = std::env::var(&cairn_var) {
        if !v.is_empty() {
            return Some((KeySource::Env, v));
        }
    }
    if let Some(var) = standard_env(provider) {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some((KeySource::Env, v));
            }
        }
    }
    let cfg = &startup.loaded.config;
    if let Some(p) = cfg.providers.get(provider) {
        if !p.api_key.is_empty() {
            harden(&startup.loaded.paths.user_config_file());
            return Some((KeySource::File, p.api_key.clone()));
        }
    }
    let local = startup.workspace().join(".cairn").join("credentials.toml");
    if let Some(key) = read_credentials(&local, provider) {
        harden(&local);
        return Some((KeySource::File, key));
    }
    None
}

/// `0600` on a file that holds key material (REQ-PROV-015).
#[cfg(unix)]
fn harden(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            if std::fs::set_permissions(path, perms).is_ok() {
                yell!(
                    "warning: {}: file mode {mode:o} tightened to 600 (REQ-PROV-015)",
                    codes::CRED_PERM
                );
            }
        }
    }
}

#[cfg(not(unix))]
fn harden(_path: &Path) {}

/// Workspace-local credentials: `[providers.<id>]` or `[<id>]` with `api_key`.
fn read_credentials(file: &Path, provider: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let doc: toml::Value = toml::from_str(&text).ok()?;
    let key_in = |table: Option<&toml::Value>| {
        table
            .and_then(|t| t.get("api_key"))
            .and_then(toml::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    key_in(doc.get("providers").and_then(|p| p.get(provider))).or_else(|| key_in(doc.get(provider)))
}

fn rows(startup: &Startup) -> Vec<AuthRow> {
    let cfg = &startup.loaded.config;
    let mut names: Vec<String> = cfg.providers.keys().cloned().collect();
    for p in &bundled_registry().providers {
        if !names.iter().any(|n| n == p) {
            names.push(p.clone());
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|provider| {
            let (enabled, base_url) = cfg
                .providers
                .get(&provider)
                .map_or((false, String::new()), |p| (p.enabled, p.base_url.clone()));
            let hit = lookup(&provider, startup);
            let env_var = {
                let cairn_var = cairn_env_var(&provider);
                if std::env::var(&cairn_var).is_ok() {
                    Some(cairn_var)
                } else {
                    standard_env(&provider)
                        .filter(|v| std::env::var(v).is_ok())
                        .map(str::to_string)
                }
            };
            AuthRow {
                provider,
                enabled,
                source: hit.as_ref().map(|(s, _)| s.name()),
                env_var,
                last4: hit.as_ref().map(|(_, k)| last4(k)),
                base_url,
            }
        })
        .collect()
}

fn last4(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let start = chars.len().saturating_sub(4);
    format!("…{}", chars[start..].iter().collect::<String>())
}

pub fn run(cmd: &AuthCmd, startup: &Startup) -> Result<i32, Fail> {
    match cmd {
        AuthCmd::List { json } => list(*json, startup),
        AuthCmd::Status => status(startup),
        AuthCmd::Login { provider, .. } => {
            let provider = resolve_provider(provider.as_deref(), startup)?;
            Err(Fail::not_implemented(
                &format!("`cairn auth login {provider}` (keychain storage)"),
                "M1",
            ))
        }
        AuthCmd::Logout { provider } => {
            let provider = resolve_provider(Some(provider), startup)?;
            Err(Fail::not_implemented(
                &format!("`cairn auth logout {provider}` (keychain removal)"),
                "M1",
            ))
        }
    }
}

/// Explicit name, else the active model's provider, else a usage error.
fn resolve_provider(provider: Option<&str>, startup: &Startup) -> Result<String, Fail> {
    let known = &bundled_registry().providers;
    let chosen = match provider {
        Some(p) => p.to_string(),
        None => startup
            .loaded
            .config
            .model
            .split_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default(),
    };
    if chosen.is_empty() || !known.contains(&chosen) {
        return Err(Fail::usage(
            format!("unknown provider `{chosen}`"),
            format!("known providers: {}", known.join(", ")),
        ));
    }
    Ok(chosen)
}

fn list(json: bool, startup: &Startup) -> Result<i32, Fail> {
    let rows = rows(startup);
    if json {
        say!(
            "{}",
            serde_json::to_string_pretty(&rows).expect("auth rows")
        );
        return Ok(0);
    }
    for r in &rows {
        match (&r.source, &r.last4) {
            (Some(src), Some(last4)) => say!(
                "{:<10} {} {last4}{}",
                r.provider,
                src,
                if r.enabled { "" } else { "  (disabled)" }
            ),
            _ => say!(
                "{:<10} missing  ({})",
                r.provider,
                missing_hint(&r.provider)
            ),
        }
    }
    Ok(0)
}

fn status(startup: &Startup) -> Result<i32, Fail> {
    for r in rows(startup) {
        match (&r.source, &r.last4) {
            (Some(src), Some(last4)) => say!("{}: ok ({src}, {last4})", r.provider),
            _ => say!(
                "{}: MISSING — run `cairn auth login {}`",
                r.provider,
                r.provider
            ),
        }
    }
    Ok(0)
}

fn missing_hint(provider: &str) -> String {
    format!(
        "set ${} or run `cairn auth login {provider}`",
        cairn_env_var(provider)
    )
}

/// For `doctor` (SPEC §12.3 row 4): providers that need a key but have none.
pub fn missing_required(startup: &Startup) -> Vec<String> {
    // Local servers (ollama/vllm) never require a key.
    let local = ["ollama", "vllm"];
    rows(startup)
        .into_iter()
        .filter(|r| r.enabled && r.source.is_none() && !local.contains(&r.provider.as_str()))
        .map(|r| r.provider)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{LoadOptions, Paths};
    use std::collections::BTreeMap;

    fn startup_in(dir: &Path) -> Startup {
        let mut loaded = cairn_config::load(&LoadOptions {
            cwd: dir.to_path_buf(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        loaded.paths = Paths {
            config_home: dir.join("config"),
            data_home: dir.join("data"),
            state_home: dir.join("state"),
            cache_home: dir.join("cache"),
        };
        Startup {
            loaded,
            quiet: true,
        }
    }

    #[test]
    fn last_four_only() {
        assert_eq!(last4("sk-1234567890"), "…7890");
        assert_eq!(last4("ab"), "…ab");
    }

    #[test]
    fn standard_envs_match_the_spec() {
        assert_eq!(standard_env("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(standard_env("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(standard_env("ollama"), Some("OLLAMA_API_KEY"));
        assert_eq!(cairn_env_var("openai"), "CAIRN_OPENAI_API_KEY");
    }

    #[test]
    fn credentials_file_is_read_both_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join(".cairn")).unwrap();
        let file = dir.join(".cairn/credentials.toml");
        std::fs::write(&file, "[providers.openai]\napi_key = \"sk-local\"\n").unwrap();
        assert_eq!(
            read_credentials(&file, "openai").as_deref(),
            Some("sk-local")
        );
        std::fs::write(&file, "[anthropic]\napi_key = \"sk-alt\"\n").unwrap();
        assert_eq!(
            read_credentials(&file, "anthropic").as_deref(),
            Some("sk-alt")
        );
        assert_eq!(read_credentials(&file, "openai"), None);
    }

    #[test]
    fn list_covers_every_registry_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let rows = rows(&startup);
        let names: Vec<&str> = rows.iter().map(|r| r.provider.as_str()).collect();
        for p in &bundled_registry().providers {
            assert!(names.contains(&p.as_str()), "{p} missing from {names:?}");
        }
        // No key material may be printed anywhere but last4.
        for r in &rows {
            assert!(r.last4.as_deref().is_none_or(|l| l.starts_with('…')));
        }
    }

    #[test]
    fn unknown_provider_is_a_usage_error() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let err = resolve_provider(Some("skynet"), &startup).unwrap_err();
        assert_eq!(err.exit, 2);
        assert_eq!(err.code, codes::CLI_USAGE);
    }
}
