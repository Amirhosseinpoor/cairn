//! Validation rules beyond types (SPEC §11.4.2).

use crate::issue::Issue;
use crate::model::{Config, BUILTIN_THEMES, BUILTIN_TOOL_NAMES};
use cairn_core::error::codes;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Bundled model registry (`assets/models.json`, SPEC §4.9).
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub model_ids: Vec<String>,
    pub max_output: BTreeMap<String, u32>,
    pub context_window: BTreeMap<String, u32>,
    pub providers: Vec<String>,
}

// Model limits come from the bundled `assets/models.json` and are bounded by
// the schema, so narrowing them to `u32` cannot lose anything real.
#[expect(
    clippy::cast_possible_truncation,
    reason = "bundled model limits are far below u32::MAX"
)]
fn registry_from_json(text: &str) -> Result<Registry, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut r = Registry::default();
    let models = v
        .get("models")
        .and_then(serde_json::Value::as_object)
        .ok_or("models missing")?;
    for (id, m) in models {
        r.model_ids.push(id.clone());
        if let Some(mo) = m.get("max_output").and_then(serde_json::Value::as_u64) {
            r.max_output.insert(id.clone(), mo as u32);
        }
        if let Some(cw) = m.get("context_window").and_then(serde_json::Value::as_u64) {
            r.context_window.insert(id.clone(), cw as u32);
        }
    }
    if let Some(providers) = v.get("providers").and_then(serde_json::Value::as_object) {
        r.providers = providers.keys().cloned().collect();
    }
    r.model_ids.sort();
    r.providers.sort();
    Ok(r)
}

/// Parse the bundled registry (SPEC §4.9). Falls back to an empty registry if the
/// asset is malformed — startup must not break (REQ-PROV-014).
#[must_use]
pub fn bundled_registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(|| {
        registry_from_json(include_str!("../../../assets/models.json")).unwrap_or_default()
    })
}

/// Registry snapshot date baked into the binary (SPEC §4.9 `updated_at`).
#[must_use]
pub fn registry_updated_at() -> &'static str {
    static DATE: OnceLock<String> = OnceLock::new();
    DATE.get_or_init(|| {
        serde_json::from_str::<serde_json::Value>(include_str!("../../../assets/models.json"))
            .ok()
            .and_then(|v| {
                v.get("updated_at")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    })
}

/// Extra context validation needs (things that depend on the environment).
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    /// `Some(is_tty)` when the output stream's TTY state is known.
    pub is_tty: Option<bool>,
    /// Directory scanned for custom themes (SPEC §10.4).
    pub themes_dir: Option<PathBuf>,
    /// Registry to validate against; `None` = bundled.
    pub registry: Option<Registry>,
}

impl Ctx {
    fn registry(&self) -> &Registry {
        match &self.registry {
            Some(r) => r,
            None => bundled_registry(),
        }
    }
}

macro_rules! range_i {
    ($out:expr, $path:expr, $val:expr, $lo:expr, $hi:expr, $section:expr) => {
        if !($lo..=$hi).contains(&$val) {
            $out.push(Issue::error(
                codes::CFG_RANGE,
                format!(
                    "{} = {} is outside the allowed range {}..={}",
                    $path, $val, $lo, $hi
                ),
                Some($path),
                $section,
            ));
        }
    };
}

/// Upper bound only — used where the lower bound is `0` for an unsigned type
/// (a `< 0` comparison would be a useless type-limit comparison).
macro_rules! max_i {
    ($out:expr, $path:expr, $val:expr, $hi:expr, $section:expr) => {
        if $val > $hi {
            $out.push(Issue::error(
                codes::CFG_RANGE,
                format!("{} = {} exceeds the maximum {}", $path, $val, $hi),
                Some($path),
                $section,
            ));
        }
    };
}

macro_rules! range_f {
    ($out:expr, $path:expr, $val:expr, $lo:expr, $hi:expr, $section:expr) => {
        if !($lo..=$hi).contains(&$val) {
            $out.push(Issue::error(
                codes::CFG_RANGE,
                format!(
                    "{} = {} is outside the allowed range {}..={}",
                    $path, $val, $lo, $hi
                ),
                Some($path),
                $section,
            ));
        }
    };
}

fn check_sum(out: &mut Vec<Issue>, path: &str, values: &[(&str, f64)], section: &'static str) {
    let total: f64 = values.iter().map(|(_, v)| v).sum();
    if (total - 1.0).abs() > 0.001 {
        let detail: Vec<String> = values.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push(Issue::error(
            codes::CFG_SUM,
            format!(
                "{path} must sum to 1.0 ± 0.001, got {total:.4} ({})",
                detail.join(", ")
            ),
            Some(path),
            section,
        ));
    }
}

fn check_globs(out: &mut Vec<Issue>, path: &str, patterns: &[String], section: &'static str) {
    for p in patterns {
        if globset::Glob::new(p).is_err() {
            out.push(Issue::error(
                codes::CFG_BADGLOB,
                format!("{path} contains invalid glob {p:?}"),
                Some(path),
                section,
            ));
        }
    }
}

fn check_regexes(out: &mut Vec<Issue>, path: &str, patterns: &[String], section: &'static str) {
    for p in patterns {
        if regex::Regex::new(p).is_err() {
            out.push(Issue::error(
                codes::CFG_BADREGEX,
                format!("{path} contains invalid regex {p:?}"),
                Some(path),
                section,
            ));
        }
    }
}

fn check_paths(out: &mut Vec<Issue>, path: &str, value: &str, section: &'static str) {
    if !crate::paths::is_abs_or_tilde(value) {
        out.push(Issue::error(
            codes::CFG_BADPATH,
            format!("{path} = {value:?} must be absolute or start with ~/"),
            Some(path),
            section,
        ));
    }
}

/// `""` means "provider default endpoint" (SPEC §11.4.1 documents `base_url`
/// as optional; only a *set* value has to be http/https).
fn check_http_url(out: &mut Vec<Issue>, path: &str, value: &str, section: &'static str) {
    if value.is_empty() {
        return;
    }
    let ok = value.starts_with("http://") || value.starts_with("https://");
    if !ok {
        out.push(Issue::error(
            codes::CFG_BADVALUE,
            format!("{path} = {value:?} must be an http:// or https:// URL"),
            Some(path),
            section,
        ));
    }
}

/// Validate a fully merged configuration. Returns **all** issues (REQ-CLI-003).
///
/// One long, straight-line walk over the §11.4.2 violation table — every check
/// is independent and reports into `out`, so splitting it would only add
/// plumbing without shortening the individual checks.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "enumerates §11.4.2 in order; splitting buys no clarity"
)]
pub fn validate(cfg: &Config, ctx: &Ctx) -> Vec<Issue> {
    let mut out: Vec<Issue> = Vec::new();
    let reg = ctx.registry();

    // ---------------------------------------------------------------- root
    if cfg.schema_version != 1 {
        out.push(Issue::error(
            codes::CFG_VERSION,
            format!(
                "schema_version = {} but only 1 is supported; run `cairn migrate`",
                cfg.schema_version
            ),
            Some("schema_version"),
            "",
        ));
    }
    if !regex::Regex::new(r"^[a-z0-9_-]{1,32}$").is_ok_and(|re| re.is_match(&cfg.profile)) {
        out.push(Issue::error(
            codes::CFG_BADVALUE,
            format!(
                "profile = {:?} must match ^[a-z0-9_-]{{1,32}}$",
                cfg.profile
            ),
            Some("profile"),
            "",
        ));
    }
    range_f!(out, "temperature", cfg.temperature, 0.0, 2.0, "");
    if cfg.system_prompt_extra.chars().count() > 4000 {
        out.push(Issue::error(
            codes::CFG_RANGE,
            format!(
                "system_prompt_extra is {} chars, maximum is 4000",
                cfg.system_prompt_extra.chars().count()
            ),
            Some("system_prompt_extra"),
            "",
        ));
    }

    // model resolves in the registry or has an explicit override (REQ-PROV-013)
    let model_override = cfg.models.get(&cfg.model);
    if model_override.is_none() && !reg.model_ids.contains(&cfg.model) {
        out.push(Issue::error(
            codes::CFG_NOMODEL,
            format!(
                "model = {:?} is not in the registry; add [models.\\\"{}\\\".context_window] or run `cairn config list --json-schema` to see valid ids",
                cfg.model, cfg.model
            ),
            Some("model"),
            "",
        ));
    }
    let model_max_output = model_override
        .and_then(|m| m.max_output)
        .or_else(|| reg.max_output.get(&cfg.model).copied())
        .unwrap_or(u32::MAX);
    if cfg.max_output_tokens == 0 || cfg.max_output_tokens > model_max_output {
        out.push(Issue::error(
            codes::CFG_RANGE,
            format!(
                "max_output_tokens = {} is outside 1..={} for model {}",
                cfg.max_output_tokens, model_max_output, cfg.model
            ),
            Some("max_output_tokens"),
            "",
        ));
    }
    if let Some(mo) = model_override {
        if let Some(cw) = mo.context_window {
            range_i!(
                out,
                "models.context_window",
                cw,
                1024u32,
                10_000_000u32,
                "models"
            );
        }
        if let Some(maxo) = mo.max_output {
            range_i!(out, "models.max_output", maxo, 1u32, 1_000_000u32, "models");
        }
        if let Some(t) = mo.temperature {
            range_f!(out, "models.temperature", t, 0.0, 2.0, "models");
        }
    }

    // auto-unsafe needs the bypass flag (G-M1)
    if cfg.mode == cairn_core::Mode::AutoUnsafe && !cfg.modes.allow_unsafe {
        out.push(Issue::error(
            codes::CFG_UNSAFE_BLOCKED,
            "mode = \"auto-unsafe\" requires --dangerously-skip-permissions (modes.allow_unsafe = true)",
            Some("mode"),
            "",
        ));
    }

    // ------------------------------------------------------------- providers
    for (name, p) in &cfg.providers {
        let path_base = format!("providers.{name}");
        if p.enabled {
            check_http_url(
                &mut out,
                &format!("{path_base}.base_url"),
                &p.base_url,
                "providers",
            );
        }
        max_i!(
            out,
            &format!("{path_base}.max_retries"),
            p.max_retries,
            10u32,
            "providers"
        );
        range_i!(
            out,
            &format!("{path_base}.idle_timeout_ms"),
            p.idle_timeout_ms,
            5000u64,
            300_000u64,
            "providers"
        );
        range_i!(
            out,
            &format!("{path_base}.max_total_ms"),
            p.max_total_ms,
            10_000u64,
            600_000u64,
            "providers"
        );
        if !p.ca_bundle.is_empty() {
            check_paths(
                &mut out,
                &format!("{path_base}.ca_bundle"),
                &p.ca_bundle,
                "providers",
            );
        }
    }
    if !cfg.providers.values().any(|p| p.enabled) && !cfg.model.starts_with("ollama/") {
        // no provider can serve the model
        out.push(Issue::error(
            codes::CFG_BADVALUE,
            "no provider is enabled but model requires one (enable providers.<id>.enabled)"
                .to_string(),
            Some("providers"),
            "providers",
        ));
    }

    // ---------------------------------------------------------------- shell
    if !cfg.shell.command.is_empty() {
        check_paths(&mut out, "shell.command", &cfg.shell.command, "shell");
    }
    range_i!(
        out,
        "shell.default_timeout_ms",
        cfg.shell.default_timeout_ms,
        1000u64,
        600_000u64,
        "shell"
    );
    range_i!(
        out,
        "shell.kill_grace_ms",
        cfg.shell.kill_grace_ms,
        100u64,
        30_000u64,
        "shell"
    );

    // ------------------------------------------------------------ discovery
    range_i!(
        out,
        "discovery.max_file_size_bytes",
        cfg.discovery.max_file_size_bytes,
        4096u64,
        104_857_600u64,
        "discovery"
    );
    range_i!(
        out,
        "discovery.max_entries",
        cfg.discovery.max_entries,
        1000u64,
        10_000_000u64,
        "discovery"
    );
    range_i!(
        out,
        "discovery.max_depth",
        cfg.discovery.max_depth,
        1u32,
        512u32,
        "discovery"
    );
    check_globs(
        &mut out,
        "discovery.include",
        &cfg.discovery.include,
        "discovery",
    );
    check_globs(
        &mut out,
        "discovery.exclude",
        &cfg.discovery.exclude,
        "discovery",
    );
    if cfg.discovery.follow_symlinks {
        out.push(Issue::error(
            codes::CFG_BADVALUE,
            "discovery.follow_symlinks must be false in v1 (REQ-CTX-003)",
            Some("discovery.follow_symlinks"),
            "discovery",
        ));
    }

    // ------------------------------------------------------------- repo_map
    max_i!(
        out,
        "repo_map.top_k",
        cfg.repo_map.top_k,
        500u32,
        "repo_map"
    );
    max_i!(
        out,
        "repo_map.max_tokens",
        cfg.repo_map.max_tokens,
        100_000u32,
        "repo_map"
    );
    range_f!(
        out,
        "repo_map.pagerank_damping",
        cfg.repo_map.pagerank_damping,
        0.5,
        0.99,
        "repo_map"
    );
    range_i!(
        out,
        "repo_map.pagerank_iterations",
        cfg.repo_map.pagerank_iterations,
        5u32,
        100u32,
        "repo_map"
    );
    check_sum(
        &mut out,
        "repo_map.weights",
        &[
            ("page", cfg.repo_map.weights.page),
            ("bm25", cfg.repo_map.weights.bm25),
        ],
        "repo_map",
    );
    check_sum(
        &mut out,
        "repo_map.personalization",
        &[
            ("current_file", cfg.repo_map.personalization.current_file),
            ("touched", cfg.repo_map.personalization.touched),
            ("uniform", cfg.repo_map.personalization.uniform),
        ],
        "repo_map",
    );
    for (k, v) in [
        ("repo_map.bm25.k1", cfg.repo_map.bm25.k1),
        ("repo_map.bm25.b", cfg.repo_map.bm25.b),
        ("repo_map.bm25.name_boost", cfg.repo_map.bm25.name_boost),
        ("repo_map.bm25.path_boost", cfg.repo_map.bm25.path_boost),
        (
            "repo_map.bm25.signature_boost",
            cfg.repo_map.bm25.signature_boost,
        ),
        ("repo_map.bm25.doc_boost", cfg.repo_map.bm25.doc_boost),
    ] {
        if v <= 0.0 {
            out.push(Issue::error(
                codes::CFG_RANGE,
                format!("{k} = {v} must be > 0"),
                Some(k),
                "repo_map",
            ));
        }
    }

    // ---------------------------------------------------------------- index
    range_i!(
        out,
        "index.debounce_ms",
        cfg.index.debounce_ms,
        50u64,
        10_000u64,
        "index"
    );
    range_i!(
        out,
        "index.max_age_days",
        cfg.index.max_age_days,
        1u32,
        365u32,
        "index"
    );
    if !cfg.index.db_path.is_empty() {
        check_paths(&mut out, "index.db_path", &cfg.index.db_path, "index");
    }

    // -------------------------------------------------------------- context
    let ctx_checks: [(&str, u32, u32, u32); 4] = [
        (
            "context.history_budget_pct",
            cfg.context.history_budget_pct,
            1,
            95,
        ),
        (
            "context.system_prompt_pct",
            cfg.context.system_prompt_pct,
            1,
            40,
        ),
        ("context.tools_pct", cfg.context.tools_pct, 1, 40),
        (
            "context.output_reserve_pct",
            cfg.context.output_reserve_pct,
            5,
            40,
        ),
    ];
    for (path, v, lo, hi) in ctx_checks {
        range_i!(out, path, v, lo, hi, "context");
    }
    max_i!(
        out,
        "context.repo_map_pct",
        cfg.context.repo_map_pct,
        50u32,
        "context"
    );
    max_i!(
        out,
        "context.pinned_pct",
        cfg.context.pinned_pct,
        50u32,
        "context"
    );
    range_i!(
        out,
        "context.tool_output_max_bytes",
        cfg.context.tool_output_max_bytes,
        256u64,
        1_048_576u64,
        "context"
    );
    range_f!(
        out,
        "context.compaction_threshold",
        cfg.context.compaction_threshold,
        0.5,
        0.95,
        "context"
    );
    range_i!(
        out,
        "context.compaction_max_attempts",
        cfg.context.compaction_max_attempts,
        1u32,
        10u32,
        "context"
    );
    max_i!(
        out,
        "context.compaction_keep_messages",
        cfg.context.compaction_keep_messages,
        100u32,
        "context"
    );
    range_i!(
        out,
        "context.history_keep_compactions",
        cfg.context.history_keep_compactions,
        1u32,
        20u32,
        "context"
    );

    // ----------------------------------------------------------------- auto
    range_i!(
        out,
        "auto.max_iterations",
        cfg.auto.max_iterations,
        1u32,
        1000u32,
        "auto"
    );
    range_i!(
        out,
        "auto.max_tool_calls",
        cfg.auto.max_tool_calls,
        1u32,
        10_000u32,
        "auto"
    );
    range_i!(
        out,
        "auto.max_wall_ms",
        cfg.auto.max_wall_ms,
        10_000u64,
        3_600_000u64,
        "auto"
    );
    range_f!(
        out,
        "auto.max_cost_usd",
        cfg.auto.max_cost_usd,
        0.01,
        1000.0,
        "auto"
    );
    range_i!(
        out,
        "auto.max_files_changed",
        cfg.auto.max_files_changed,
        1u32,
        10_000u32,
        "auto"
    );
    range_i!(
        out,
        "auto.max_consecutive_failures",
        cfg.auto.max_consecutive_failures,
        1u32,
        100u32,
        "auto"
    );

    // -------------------------------------------------------------- session
    range_f!(
        out,
        "session.max_cost_usd",
        cfg.session.max_cost_usd,
        0.01,
        100_000.0,
        "session"
    );
    range_i!(
        out,
        "session.max_tool_calls",
        cfg.session.max_tool_calls,
        1u32,
        1_000_000u32,
        "session"
    );
    range_i!(
        out,
        "session.retention_days",
        cfg.session.retention_days,
        1u32,
        3650u32,
        "session"
    );
    range_i!(
        out,
        "session.max_sessions",
        cfg.session.max_sessions,
        10u32,
        100_000u32,
        "session"
    );

    // ----------------------------------------------------------- checkpoint
    range_i!(
        out,
        "checkpoint.keep_per_session",
        cfg.checkpoint.keep_per_session,
        1u32,
        1000u32,
        "checkpoint"
    );
    range_i!(
        out,
        "checkpoint.keep_total",
        cfg.checkpoint.keep_total,
        1u32,
        10_000u32,
        "checkpoint"
    );
    range_i!(
        out,
        "checkpoint.max_total_bytes",
        cfg.checkpoint.max_total_bytes,
        67_108_864u64,
        68_719_476_736u64,
        "checkpoint"
    );
    range_i!(
        out,
        "checkpoint.fast_path_max_files",
        cfg.checkpoint.fast_path_max_files,
        10u32,
        100_000u32,
        "checkpoint"
    );

    // ------------------------------------------------------------- security
    check_globs(
        &mut out,
        "security.allow_protected_paths",
        &cfg.security.allow_protected_paths,
        "security",
    );
    check_regexes(
        &mut out,
        "security.redact_patterns",
        &cfg.security.redact_patterns,
        "security",
    );
    for d in &cfg.security.additional_dirs {
        check_paths(&mut out, "security.additional_dirs", d, "security");
    }
    if cfg.security.allow_unsafe_shell && cfg.security.sandbox == crate::model::SandboxLevel::None {
        out.push(Issue::error(
            codes::CFG_UNSAFE_BLOCKED,
            "security.allow_unsafe_shell + sandbox = \"none\" requires --dangerously-skip-permissions",
            Some("security.sandbox"),
            "security",
        ));
    }

    // -------------------------------------------------------------- network
    range_i!(
        out,
        "network.timeout_ms",
        cfg.network.timeout_ms,
        1000u64,
        300_000u64,
        "network"
    );
    if !cfg.network.proxy.is_empty() {
        let ok = cfg.network.proxy.starts_with("http://")
            || cfg.network.proxy.starts_with("https://")
            || cfg.network.proxy.starts_with("socks5://");
        if !ok {
            out.push(Issue::error(
                codes::CFG_BADVALUE,
                format!(
                    "network.proxy = {:?} must be http://, https:// or socks5://",
                    cfg.network.proxy
                ),
                Some("network.proxy"),
                "network",
            ));
        }
    }
    for h in &cfg.network.allow_hosts {
        let plain = h.strip_prefix("*.").unwrap_or(h);
        let ok = !plain.is_empty()
            && plain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && !plain.starts_with('.')
            && !plain.ends_with('.');
        if !ok {
            out.push(Issue::error(
                codes::CFG_BADVALUE,
                format!("network.allow_hosts entry {h:?} must be a host or *.suffix"),
                Some("network.allow_hosts"),
                "network",
            ));
        }
    }

    // ---------------------------------------------------------------- modes
    range_i!(
        out,
        "subagent.max_parallel",
        cfg.subagent.max_parallel,
        1u32,
        16u32,
        "subagent"
    );
    range_i!(
        out,
        "subagent.max_depth",
        cfg.subagent.max_depth,
        1u8,
        3u8,
        "subagent"
    );
    range_i!(
        out,
        "subagent.timeout_ms",
        cfg.subagent.timeout_ms,
        10_000u64,
        3_600_000u64,
        "subagent"
    );

    // --------------------------------------------------------------- verify
    range_i!(
        out,
        "verify.max_attempts",
        cfg.verify.max_attempts,
        1u32,
        10u32,
        "verify"
    );
    range_i!(
        out,
        "verify.timeout_ms",
        cfg.verify.timeout_ms,
        1000u64,
        1_800_000u64,
        "verify"
    );

    // ------------------------------------------------------------------- ui
    if !BUILTIN_THEMES.contains(&cfg.ui.theme.as_str()) {
        let known = ctx
            .themes_dir
            .as_ref()
            .is_some_and(|dir| dir.join(format!("{}.toml", cfg.ui.theme)).is_file());
        if !known {
            out.push(Issue::error(
                codes::CFG_THEME,
                format!(
                    "ui.theme = {:?} is not a built-in theme ({}) and no theme file was found",
                    cfg.ui.theme,
                    BUILTIN_THEMES.join(", ")
                ),
                Some("ui.theme"),
                "ui",
            ));
        }
    }
    range_i!(
        out,
        "ui.diff_side_by_side_min_width",
        cfg.ui.diff_side_by_side_min_width,
        40u16,
        400u16,
        "ui"
    );
    range_i!(out, "ui.frame_rate", cfg.ui.frame_rate, 10u16, 240u16, "ui");
    range_i!(
        out,
        "ui.max_transcript_lines",
        cfg.ui.max_transcript_lines,
        100u32,
        100_000u32,
        "ui"
    );

    // ---------------------------------------------------------------- input
    range_i!(
        out,
        "input.history_limit",
        cfg.input.history_limit,
        100u32,
        100_000u32,
        "input"
    );
    range_i!(
        out,
        "input.mention_max_results",
        cfg.input.mention_max_results,
        1u32,
        100u32,
        "input"
    );
    max_i!(
        out,
        "input.paste_confirm_chars",
        cfg.input.paste_confirm_chars,
        1_000_000u32,
        "input"
    );
    if !cfg.input.history_file.is_empty() {
        check_paths(
            &mut out,
            "input.history_file",
            &cfg.input.history_file,
            "input",
        );
    }

    // ---------------------------------------------------------------- output
    if ctx.is_tty == Some(false) && cfg.output.format == crate::model::OutputFormat::Tui {
        out.push(Issue::error(
            codes::CFG_BADVALUE,
            "output.format = \"tui\" requires a TTY; use --output text|json|stream-json",
            Some("output.format"),
            "output",
        ));
    }

    // ------------------------------------------------------------------ log
    range_i!(
        out,
        "log.rotate_bytes",
        cfg.log.rotate_bytes,
        1_048_576u64,
        1_073_741_824u64,
        "log"
    );
    range_i!(
        out,
        "log.keep_rotated",
        cfg.log.keep_rotated,
        1u32,
        100u32,
        "log"
    );
    if !cfg.log.file.is_empty() {
        check_paths(&mut out, "log.file", &cfg.log.file, "log");
    }
    if !cfg.log.redact && !cfg.trace.debug_unsafe {
        out.push(Issue::error(
            codes::CFG_UNSAFEREDACT,
            "log.redact = false requires trace.debug_unsafe = true (SPEC §12.1)",
            Some("log.redact"),
            "log",
        ));
    }

    // ----------------------------------------------------------------- trace
    if !cfg.trace.dir.is_empty() {
        check_paths(&mut out, "trace.dir", &cfg.trace.dir, "trace");
    }
    range_i!(
        out,
        "trace.max_file_bytes",
        cfg.trace.max_file_bytes,
        1_048_576u64,
        1_073_741_824u64,
        "trace"
    );

    // ---------------------------------------------------------------- update
    range_i!(
        out,
        "update.interval_hours",
        cfg.update.interval_hours,
        1u32,
        720u32,
        "update"
    );

    // ----------------------------------------------------------------- paths
    for (path, v) in [
        ("paths.data_dir", &cfg.paths.data_dir),
        ("paths.cache_dir", &cfg.paths.cache_dir),
        ("paths.config_dir", &cfg.paths.config_dir),
    ] {
        if !v.is_empty() {
            check_paths(&mut out, path, v, "paths");
        }
    }

    // ----------------------------------------------------------------- hooks
    for (i, h) in cfg.hooks.iter().enumerate() {
        let base = format!("hooks[{i}]");
        if h.command.trim().is_empty() {
            out.push(Issue::error(
                codes::CFG_BADVALUE,
                format!("{base}.command must not be empty"),
                Some(&base),
                "hooks",
            ));
        }
        range_i!(
            out,
            &format!("{base}.timeout_ms"),
            h.timeout_ms,
            1u64,
            600_000u64,
            "hooks"
        );
        if h.events.is_empty() {
            out.push(Issue::error(
                codes::CFG_BADVALUE,
                format!("{base}.events must list at least one event"),
                Some(&base),
                "hooks",
            ));
        }
    }

    // ---------------------------------------------------------- custom tools
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    let name_re = regex::Regex::new(r"^[a-z][a-z0-9_]{1,63}$").expect("static pattern");
    for (i, t) in cfg.custom_tools.iter().enumerate() {
        let base = format!("custom_tools[{i}]");
        let name_ok = name_re.is_match(&t.name);
        if !name_ok {
            out.push(Issue::error(
                codes::CFG_DUPNAME,
                format!(
                    "{base}.name = {:?} must match ^[a-z][a-z0-9_]{{1,63}}$",
                    t.name
                ),
                Some(&base),
                "custom_tools",
            ));
        } else if BUILTIN_TOOL_NAMES.contains(&t.name.as_str()) {
            out.push(Issue::error(
                codes::CFG_DUPNAME,
                format!("{base}.name = {:?} collides with a built-in tool", t.name),
                Some(&base),
                "custom_tools",
            ));
        } else if !seen_names.insert(t.name.as_str()) {
            out.push(Issue::error(
                codes::CFG_DUPNAME,
                format!("{base}.name = {:?} is defined more than once", t.name),
                Some(&base),
                "custom_tools",
            ));
        }
        if t.command.trim().is_empty() {
            out.push(Issue::error(
                codes::CFG_BADVALUE,
                format!("{base}.command must not be empty"),
                Some(&base),
                "custom_tools",
            ));
        }
        range_i!(
            out,
            &format!("{base}.timeout_ms"),
            t.timeout_ms,
            1u64,
            3_600_000u64,
            "custom_tools"
        );
    }

    // ------------------------------------------------------------------ mcp
    let mut seen_servers: BTreeSet<&str> = BTreeSet::new();
    for (i, s) in cfg.mcp.servers.iter().enumerate() {
        let base = format!("mcp.servers[{i}]");
        if s.name.trim().is_empty() {
            out.push(Issue::error(
                codes::CFG_DUPNAME,
                format!("{base}.name must not be empty"),
                Some(&base),
                "mcp",
            ));
        } else if !seen_servers.insert(s.name.as_str()) {
            out.push(Issue::error(
                codes::CFG_DUPNAME,
                format!("{base}.name = {:?} is defined more than once", s.name),
                Some(&base),
                "mcp",
            ));
        }
        match s.transport {
            crate::model::McpTransport::Stdio => {
                if s.command.trim().is_empty() {
                    out.push(Issue::error(
                        codes::CFG_BADVALUE,
                        format!("{base}.command is required for transport = \"stdio\""),
                        Some(&base),
                        "mcp",
                    ));
                }
            }
            crate::model::McpTransport::Http => {
                if s.url.trim().is_empty() {
                    out.push(Issue::error(
                        codes::CFG_BADVALUE,
                        format!("{base}.url is required for transport = \"http\""),
                        Some(&base),
                        "mcp",
                    ));
                } else {
                    check_http_url(&mut out, &format!("{base}.url"), &s.url, "mcp");
                }
            }
        }
        range_i!(
            out,
            &format!("{base}.request_timeout_ms"),
            s.request_timeout_ms,
            1000u64,
            300_000u64,
            "mcp"
        );
        check_globs(
            &mut out,
            &format!("{base}.tools_allow"),
            &s.tools_allow,
            "mcp",
        );
        check_globs(
            &mut out,
            &format!("{base}.tools_deny"),
            &s.tools_deny,
            "mcp",
        );
        if !s.cwd.is_empty() {
            check_paths(&mut out, &format!("{base}.cwd"), &s.cwd, "mcp");
        }
    }

    // ---------------------------------------------------------- permissions
    range_i!(
        out,
        "permissions.ask_timeout_ms",
        cfg.permissions.ask_timeout_ms,
        10_000u64,
        3_600_000u64,
        "permissions"
    );
    range_i!(
        out,
        "permissions.deny_ending_turn_after",
        cfg.permissions.deny_ending_turn_after,
        1u32,
        20u32,
        "permissions"
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes_of(issues: &[Issue]) -> Vec<&'static str> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn bundled_registry_parses() {
        let r = bundled_registry();
        assert!(r
            .model_ids
            .contains(&"anthropic/claude-sonnet-4-5".to_string()));
        assert!(r.model_ids.contains(&"openai/gpt-5.1-codex".to_string()));
        assert!(r
            .model_ids
            .contains(&"ollama/qwen2.5-coder:14b".to_string()));
        assert_eq!(r.max_output["anthropic/claude-sonnet-4-5"], 64000);
        assert_eq!(r.context_window["anthropic/claude-sonnet-4-5"], 200_000);
        assert!(r.providers.contains(&"anthropic".to_string()));
    }

    /// T-CFG-006: defaults validate clean.
    #[test]
    fn default_config_has_no_issues() {
        let issues = validate(&Config::default(), &Ctx::default());
        assert!(
            issues.is_empty(),
            "{}",
            issues
                .iter()
                .map(Issue::render)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// T-CFG-020 / REQ-CLI-003: every issue type is produced, all at once.
    #[test]
    fn collect_all_issues_at_once() {
        let mut c = Config {
            schema_version: 2,
            temperature: 5.0,
            model: "nope/missing".to_string(),
            ..Default::default()
        };
        c.repo_map.weights = crate::model::Weights {
            page: 0.2,
            bm25: 0.2,
        };
        c.security.redact_patterns.push("(".to_string());
        c.discovery.exclude.push("[bad".to_string());
        c.paths.data_dir = "relative/dir".to_string();
        c.mode = cairn_core::Mode::AutoUnsafe;
        c.mcp.servers.push(crate::model::McpServer {
            name: "srv".into(),
            transport: crate::model::McpTransport::Stdio,
            command: String::new(),
            ..Default::default()
        });
        c.custom_tools.push(crate::model::CustomToolConfig {
            name: "bash".into(),
            ..Default::default()
        });
        c.log.redact = false;

        let issues = validate(&c, &Ctx::default());
        let got = codes_of(&issues);
        for expected in [
            codes::CFG_VERSION,
            codes::CFG_RANGE,
            codes::CFG_NOMODEL,
            codes::CFG_SUM,
            codes::CFG_BADREGEX,
            codes::CFG_BADGLOB,
            codes::CFG_BADPATH,
            codes::CFG_UNSAFE_BLOCKED,
            codes::CFG_DUPNAME,
            codes::CFG_UNSAFEREDACT,
        ] {
            assert!(got.contains(&expected), "missing {expected}; got {got:?}");
        }
        // REQ-CLI-004: only root/providers/model/security issues are fatal
        let fatal: Vec<&Issue> = issues.iter().filter(|i| i.fatal).collect();
        assert!(fatal.iter().any(|i| i.code == codes::CFG_VERSION));
        assert!(fatal.iter().any(|i| i.code == codes::CFG_NOMODEL));
        assert!(
            !issues.iter().any(|i| i.fatal && i.section == "mcp"),
            "mcp issues are partial"
        );
        assert!(
            issues.len() >= 10,
            "expected many issues, got {}",
            issues.len()
        );
    }

    #[test]
    fn range_bounds_are_inclusive() {
        let mut c = Config {
            temperature: 2.0,
            ..Default::default()
        };
        assert!(validate(&c, &Ctx::default()).is_empty());
        c.temperature = 2.01;
        let i = validate(&c, &Ctx::default());
        assert_eq!(codes_of(&i), vec![codes::CFG_RANGE]);
        assert!(i[0].message.contains("0..=2"), "{}", i[0].message);
    }

    #[test]
    fn auto_unsafe_requires_bypass_flag() {
        let mut c = Config {
            mode: cairn_core::Mode::AutoUnsafe,
            ..Default::default()
        };
        let i = validate(&c, &Ctx::default());
        assert_eq!(codes_of(&i), vec![codes::CFG_UNSAFE_BLOCKED]);
        c.modes.allow_unsafe = true;
        assert!(validate(&c, &Ctx::default()).is_empty());
    }

    #[test]
    fn model_override_unlocks_unknown_id_req_prov_013() {
        let mut c = Config {
            model: "acme/custom-model".to_string(),
            ..Default::default()
        };
        assert!(codes_of(&validate(&c, &Ctx::default())).contains(&codes::CFG_NOMODEL));
        c.models.insert(
            "acme/custom-model".to_string(),
            crate::model::ModelOverride {
                context_window: Some(128_000),
                max_output: Some(8192),
                temperature: None,
            },
        );
        assert!(validate(&c, &Ctx::default()).is_empty());
    }

    #[test]
    fn unknown_theme_rejected_known_theme_ok() {
        let mut c = Config::default();
        c.ui.theme = "solarized-midnight".to_string();
        let i = validate(&c, &Ctx::default());
        assert_eq!(codes_of(&i), vec![codes::CFG_THEME]);
        c.ui.theme = "high-contrast".to_string();
        assert!(validate(&c, &Ctx::default()).is_empty());
    }

    #[test]
    fn tui_output_requires_tty() {
        let mut c = Config::default();
        c.output.format = crate::model::OutputFormat::Tui;
        assert!(validate(
            &c,
            &Ctx {
                is_tty: Some(true),
                ..Default::default()
            }
        )
        .is_empty());
        let i = validate(
            &c,
            &Ctx {
                is_tty: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(codes_of(&i), vec![codes::CFG_BADVALUE]);
    }

    #[test]
    fn provider_base_url_must_be_http() {
        let mut c = Config::default();
        c.providers.get_mut("anthropic").unwrap().base_url = "ftp://api.anthropic.com".to_string();
        let i = validate(&c, &Ctx::default());
        assert_eq!(codes_of(&i), vec![codes::CFG_BADVALUE]);
        assert!(i[0].message.contains("http"));

        // `""` = provider default endpoint (SPEC §11.4.1: the key is optional).
        let mut c = Config::default();
        c.providers.get_mut("anthropic").unwrap().base_url = String::new();
        assert!(
            validate(&c, &Ctx::default()).is_empty(),
            "empty base_url is valid"
        );
    }

    #[test]
    fn duplicate_mcp_names_rejected() {
        let mut c = Config::default();
        for _ in 0..2 {
            c.mcp.servers.push(crate::model::McpServer {
                name: "dup".into(),
                transport: crate::model::McpTransport::Http,
                url: "https://example.com/mcp".into(),
                ..Default::default()
            });
        }
        let i = validate(&c, &Ctx::default());
        assert_eq!(codes_of(&i), vec![codes::CFG_DUPNAME]);
    }

    #[test]
    fn follow_symlinks_must_stay_false() {
        let mut c = Config::default();
        c.discovery.follow_symlinks = true;
        let i = validate(&c, &Ctx::default());
        assert_eq!(codes_of(&i), vec![codes::CFG_BADVALUE]);
    }
}
