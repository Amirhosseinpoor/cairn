//! Configuration model — the Rust source of `config.toml` (SPEC §11.4.1).
//!
//! Every key, default and range here is transcribed from the spec; the JSON
//! Schema (`schema.rs`) is generated from these same structs (REQ-ARCH-011).

use cairn_core::Mode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

macro_rules! doc_str {
    ($name:ident) => {};
}

/// Current `schema_version` of `config.toml` (SPEC §11.7.1 migration gate).
pub const SCHEMA_VERSION: u32 = 1;

/// Root `config.toml` document (SPEC §11.4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Must be 1; anything else triggers migration (SPEC §11.7.1).
    pub schema_version: u32,
    /// Profile selector `^[a-z0-9_-]{1,32}$`.
    pub profile: String,
    /// Operating mode (SPEC §7).
    pub mode: Mode,
    /// Registry model id (`provider/model`, REQ-PROV-013).
    pub model: String,
    /// Sampling temperature, `0.0..=2.0`.
    pub temperature: f64,
    /// Hard output cap, `1..=model.max_output`.
    pub max_output_tokens: u32,
    /// Appended to the system prompt, `≤ 4000` chars.
    pub system_prompt_extra: String,
    /// Per-provider settings keyed by provider id (SPEC §4.5).
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Per-model overrides keyed by registry id (REQ-PROV-014).
    pub models: BTreeMap<String, ModelOverride>,
    /// Path to a custom `models.json` ("" = bundled registry).
    pub models_path: String,
    pub shell: ShellConfig,
    pub discovery: DiscoveryConfig,
    pub repo_map: RepoMapConfig,
    pub index: IndexConfig,
    pub context: ContextConfig,
    pub auto: AutoConfig,
    pub session: SessionConfig,
    pub checkpoint: CheckpointConfig,
    pub security: SecurityConfig,
    pub network: NetworkConfig,
    pub plans: PlansConfig,
    /// `--dangerously-skip-permissions` home (G-M1; see PROGRESS clarification 1).
    pub modes: ModesConfig,
    pub subagent: SubagentConfig,
    pub verify: VerifyConfig,
    /// Reserved section with no keys (SPEC §11.4.1).
    pub todo: TodoConfig,
    pub ui: UiConfig,
    pub input: InputConfig,
    pub output: OutputConfig,
    pub log: LogConfig,
    pub trace: TraceConfig,
    pub telemetry: TelemetryConfig,
    pub update: UpdateConfig,
    pub paths: PathsConfig,
    /// Hook definitions (SPEC §6.7.3).
    pub hooks: Vec<HookConfig>,
    /// Custom tool definitions (SPEC §6.7.2).
    pub custom_tools: Vec<CustomToolConfig>,
    pub mcp: McpConfig,
    /// Permission *policy* (rules live in `.cairn/permissions.json`).
    pub permissions: PermissionsConfig,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        for (id, base) in [
            ("anthropic", "https://api.anthropic.com"),
            ("openai", "https://api.openai.com"),
            ("ollama", "http://127.0.0.1:11434"),
            ("vllm", "http://127.0.0.1:8000"),
        ] {
            providers.insert(id.to_string(), ProviderConfig::with_base_url(base));
        }
        Self {
            schema_version: SCHEMA_VERSION,
            profile: "default".to_string(),
            mode: Mode::Build,
            model: "anthropic/claude-sonnet-4-5".to_string(),
            temperature: 0.2,
            max_output_tokens: 8192,
            system_prompt_extra: String::new(),
            providers,
            models: BTreeMap::new(),
            models_path: String::new(),
            shell: ShellConfig::default(),
            discovery: DiscoveryConfig::default(),
            repo_map: RepoMapConfig::default(),
            index: IndexConfig::default(),
            context: ContextConfig::default(),
            auto: AutoConfig::default(),
            session: SessionConfig::default(),
            checkpoint: CheckpointConfig::default(),
            security: SecurityConfig::default(),
            network: NetworkConfig::default(),
            plans: PlansConfig::default(),
            modes: ModesConfig::default(),
            subagent: SubagentConfig::default(),
            verify: VerifyConfig::default(),
            todo: TodoConfig::default(),
            ui: UiConfig::default(),
            input: InputConfig::default(),
            output: OutputConfig::default(),
            log: LogConfig::default(),
            trace: TraceConfig::default(),
            telemetry: TelemetryConfig::default(),
            update: UpdateConfig::default(),
            paths: PathsConfig::default(),
            hooks: Vec::new(),
            custom_tools: Vec::new(),
            mcp: McpConfig::default(),
            permissions: PermissionsConfig::default(),
        }
    }
}

// ---------------------------------------------------------------- enumerations

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
    Tui,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiffLayout {
    Auto,
    Inline,
    Side,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ShowReasoning {
    Always,
    Collapsed,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Animation {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubmitKey {
    Enter,
    CtrlEnter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EditorMode {
    Emacs,
    Vi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxLevel {
    Auto,
    Full,
    Advisory,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Beta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum EditorKind {
    Internal,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RememberScope {
    Session,
    Project,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FailMode {
    Warn,
    Block,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionClass {
    Read,
    Write,
    Execute,
    Network,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    Stdio,
    Http,
}

/// Hook trigger events (SPEC §6.7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum HookEvent {
    #[serde(rename = "pre_tool")]
    PreTool,
    #[serde(rename = "post_tool")]
    PostTool,
    #[serde(rename = "on_session_start")]
    OnSessionStart,
    #[serde(rename = "on_session_end")]
    OnSessionEnd,
    #[serde(rename = "pre_model")]
    PreModel,
    #[serde(rename = "post_model")]
    PostModel,
    #[serde(rename = "on_permission_request")]
    OnPermissionRequest,
    #[serde(rename = "on_compaction")]
    OnCompaction,
    #[serde(rename = "on_error")]
    OnError,
}

// ---------------------------------------------------------------- sections

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub enabled: bool,
    /// `http`/`https` base URL.
    pub base_url: String,
    /// Inline key; prefer env/keychain (SPEC §4.10).
    pub api_key: String,
    /// `0..=10`.
    pub max_retries: u32,
    /// `5000..=300000`.
    pub idle_timeout_ms: u64,
    /// `10000..=600000`.
    pub max_total_ms: u64,
    /// Optional custom CA bundle path.
    pub ca_bundle: String,
}

impl ProviderConfig {
    fn with_base_url(base: &str) -> Self {
        Self {
            base_url: base.to_string(),
            ..Self::default()
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: String::new(),
            api_key: String::new(),
            max_retries: 5,
            idle_timeout_ms: 45000,
            max_total_ms: 180_000,
            ca_bundle: String::new(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOverride {
    /// `1024..=10_000_000`.
    pub context_window: Option<u32>,
    /// `1..=1_000_000`.
    pub max_output: Option<u32>,
    /// `0.0..=2.0`.
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// "" = auto-detect (SPEC §6.4.1).
    pub command: String,
    pub env_allowlist: Vec<String>,
    pub inherit_cairn_env: bool,
    /// `1000..=600000`.
    pub default_timeout_ms: u64,
    /// `100..=30000`.
    pub kill_grace_ms: u64,
    pub allow_pty: bool,
    pub interactive_detection: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            env_allowlist: [
                "PATH",
                "HOME",
                "USER",
                "LOGNAME",
                "SHELL",
                "TERM",
                "LANG",
                "LC_ALL",
                "TZ",
                "PWD",
                "TMPDIR",
                "COLORTERM",
                "DISPLAY",
                "XDG_RUNTIME_DIR",
                "SSH_AUTH_SOCK",
                "GOPATH",
                "CARGO_HOME",
                "JAVA_HOME",
                "NODE_PATH",
                "PYTHONPATH",
                "VIRTUAL_ENV",
                "CI",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            inherit_cairn_env: false,
            default_timeout_ms: 120_000,
            kill_grace_ms: 2000,
            allow_pty: false,
            interactive_detection: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// `4096..=104857600` (SPEC §5.1, REQ-CTX-003).
    pub max_file_size_bytes: u64,
    /// `1000..=10_000_000`.
    pub max_entries: u64,
    /// `1..=512`.
    pub max_depth: u32,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// MUST stay `false` in v1 (REQ-CTX-003).
    pub follow_symlinks: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            max_file_size_bytes: 8_388_608,
            max_entries: 500_000,
            max_depth: 64,
            include: Vec::new(),
            exclude: Vec::new(),
            follow_symlinks: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub page: f64,
    pub bm25: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            page: 0.55,
            bm25: 0.45,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Personalization {
    pub current_file: f64,
    pub touched: f64,
    pub uniform: f64,
}

impl Default for Personalization {
    fn default() -> Self {
        Self {
            current_file: 0.60,
            touched: 0.30,
            uniform: 0.10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Bm25 {
    pub k1: f64,
    pub b: f64,
    pub name_boost: f64,
    pub path_boost: f64,
    pub signature_boost: f64,
    pub doc_boost: f64,
}

impl Default for Bm25 {
    fn default() -> Self {
        Self {
            k1: 1.2,
            b: 0.75,
            name_boost: 3.0,
            path_boost: 1.5,
            signature_boost: 1.2,
            doc_boost: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RepoMapConfig {
    pub enabled: bool,
    /// `0..=500`.
    pub top_k: u32,
    /// `0..=100000`.
    pub max_tokens: u32,
    /// `0.5..=0.99`.
    pub pagerank_damping: f64,
    /// `5..=100`.
    pub pagerank_iterations: u32,
    /// Sums to `1.0 ± 0.001`.
    pub weights: Weights,
    /// Sums to `1.0 ± 0.001`.
    pub personalization: Personalization,
    /// All values `> 0`.
    pub bm25: Bm25,
}

impl Default for RepoMapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_k: 40,
            max_tokens: 16000,
            pagerank_damping: 0.85,
            pagerank_iterations: 20,
            weights: Weights::default(),
            personalization: Personalization::default(),
            bm25: Bm25::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// `50..=10000`.
    pub debounce_ms: u64,
    pub watch: bool,
    /// `1..=365`.
    pub max_age_days: u32,
    /// "" = derived under the cache dir (SPEC §5.3).
    pub db_path: String,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 300,
            watch: true,
            max_age_days: 30,
            db_path: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// `1..=95`.
    pub history_budget_pct: u32,
    /// `1..=40`.
    pub system_prompt_pct: u32,
    /// `1..=40`.
    pub tools_pct: u32,
    /// `0..=50`.
    pub repo_map_pct: u32,
    /// `0..=50`.
    pub pinned_pct: u32,
    /// `5..=40` (REQ-CTX-010).
    pub output_reserve_pct: u32,
    /// `256..=1048576`.
    pub tool_output_max_bytes: u64,
    /// `0.5..=0.95`.
    pub compaction_threshold: f64,
    /// `1..=10`.
    pub compaction_max_attempts: u32,
    /// `0..=100`.
    pub compaction_keep_messages: u32,
    /// `1..=20` (REQ-CTX-015 `/undo` depth).
    pub history_keep_compactions: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            history_budget_pct: 55,
            system_prompt_pct: 8,
            tools_pct: 6,
            repo_map_pct: 10,
            pinned_pct: 15,
            output_reserve_pct: 12,
            tool_output_max_bytes: 8192,
            compaction_threshold: 0.80,
            compaction_max_attempts: 3,
            compaction_keep_messages: 12,
            history_keep_compactions: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AutoConfig {
    /// `1..=1000`.
    pub max_iterations: u32,
    /// `1..=10000`.
    pub max_tool_calls: u32,
    /// `10000..=3600000`.
    pub max_wall_ms: u64,
    /// `0.01..=1000`.
    pub max_cost_usd: f64,
    /// `1..=10000`.
    pub max_files_changed: u32,
    /// `1..=100`.
    pub max_consecutive_failures: u32,
    pub loop_detection: bool,
    /// Auto-allows `git_commit` in `auto` (SPEC §7.2).
    pub allow_commit: bool,
    pub allow_network: bool,
    pub allow_pty: bool,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            max_iterations: 40,
            max_tool_calls: 120,
            max_wall_ms: 600_000,
            max_cost_usd: 2.0,
            max_files_changed: 40,
            max_consecutive_failures: 5,
            loop_detection: true,
            allow_commit: false,
            allow_network: true,
            allow_pty: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// `0.01..=100000`.
    pub max_cost_usd: f64,
    /// `1..=1000000`.
    pub max_tool_calls: u32,
    /// `1..=3650`.
    pub retention_days: u32,
    /// `10..=100000`.
    pub max_sessions: u32,
    /// Auto-recover an interrupted turn (SPEC §8.7).
    pub auto_recover: bool,
    /// Kill background jobs at exit (SPEC §6.4.7).
    pub kill_jobs_on_exit: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_cost_usd: 20.0,
            max_tool_calls: 3000,
            retention_days: 90,
            max_sessions: 500,
            auto_recover: false,
            kill_jobs_on_exit: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointConfig {
    pub enabled: bool,
    /// Checkpoint before every mutating tool (SPEC §9.8).
    pub per_tool: bool,
    /// `1..=1000`.
    pub keep_per_session: u32,
    /// `1..=10000`.
    pub keep_total: u32,
    /// `67108864..=68719476736` (64 MiB .. 64 GiB).
    pub max_total_bytes: u64,
    /// `10..=100000` (REQ-SAFE-017).
    pub fast_path_max_files: u32,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            per_tool: false,
            keep_per_session: 50,
            keep_total: 200,
            max_total_bytes: 1_073_741_824,
            fast_path_max_files: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub sandbox: SandboxLevel,
    /// SPEC §9.3.1 bypass — gated by `--dangerously-skip-permissions`.
    pub allow_unsafe_shell: bool,
    /// Glob patterns that opt paths out of the protected-path set (SPEC §9.4).
    pub allow_protected_paths: Vec<String>,
    pub additional_dirs: Vec<String>,
    pub additional_dirs_writable: bool,
    /// Extra redaction regexes (REQ-SAFE-011).
    pub redact_patterns: Vec<String>,
    /// Commit `.cairn/permissions.json` (REQ-SAFE-003).
    pub share_permissions: bool,
    /// Load grammar plugins (REQ-TOOL-028).
    pub load_grammar_plugins: bool,
    /// Adds an untrusted-content warning header to `web_fetch` output.
    pub verify_untrusted_html: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            sandbox: SandboxLevel::Auto,
            allow_unsafe_shell: false,
            allow_protected_paths: Vec::new(),
            additional_dirs: Vec::new(),
            additional_dirs_writable: false,
            redact_patterns: Vec::new(),
            share_permissions: false,
            load_grammar_plugins: true,
            verify_untrusted_html: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub offline: bool,
    /// Hosts or `*.suffix`; empty = any host (subject to rules).
    pub allow_hosts: Vec<String>,
    /// Overrides `HTTP(S)_PROXY`.
    pub proxy: String,
    /// `1000..=300000` (`web_fetch` default).
    pub timeout_ms: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            offline: false,
            allow_hosts: Vec::new(),
            proxy: String::new(),
            timeout_ms: 30000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlansConfig {
    /// Write plans to a shareable file (SPEC §7.3).
    pub shareable: bool,
    pub editor: EditorKind,
    /// Auto-approve produced plans in headless runs (REQ-MODE-007).
    pub auto_approve: bool,
}

impl Default for PlansConfig {
    fn default() -> Self {
        Self {
            shareable: false,
            editor: EditorKind::Internal,
            auto_approve: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ModesConfig {
    /// Enables `mode = "auto-unsafe"` (G-M1). Set by
    /// `--dangerously-skip-permissions` only.
    pub allow_unsafe: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SubagentConfig {
    pub enabled: bool,
    /// `1..=16`.
    pub max_parallel: u32,
    /// `1..=3` (REQ-LOOP-009 fixes the effective maximum at 2).
    pub max_depth: u8,
    /// Subagents never inherit `auto-unsafe` unless this is true (SPEC §8.8).
    pub inherit_unsafe: bool,
    /// `10000..=3600000`.
    pub timeout_ms: u64,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_parallel: 4,
            max_depth: 2,
            inherit_unsafe: false,
            timeout_ms: 600_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyConfig {
    pub enabled: bool,
    pub commands: Vec<String>,
    /// `1..=10`.
    pub max_attempts: u32,
    /// `1000..=1800000`.
    pub timeout_ms: u64,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            commands: Vec::new(),
            max_attempts: 3,
            timeout_ms: 300_000,
        }
    }
}

/// Reserved section with no keys (SPEC §11.4.1 `[todo]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TodoConfig {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Built-in: `cairn-dark`, `cairn-light`, `high-contrast` (SPEC §10.4).
    pub theme: String,
    pub diff_layout: DiffLayout,
    /// `40..=400`.
    pub diff_side_by_side_min_width: u16,
    pub show_reasoning: ShowReasoning,
    pub animation: Animation,
    pub screen_reader: bool,
    /// `10..=240`.
    pub frame_rate: u16,
    /// `0` = full width, else wrap column.
    pub message_width: u16,
    pub copy_on_select: bool,
    /// `100..=100000`.
    pub max_transcript_lines: u32,
    /// `None` = auto-detect (color when TTY); set by `--no-color` / `NO_COLOR`.
    pub color: Option<bool>,
    /// Set by `-q` / `CAIRN_QUIET`.
    pub quiet: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "cairn-dark".to_string(),
            diff_layout: DiffLayout::Auto,
            diff_side_by_side_min_width: 100,
            show_reasoning: ShowReasoning::Collapsed,
            animation: Animation::Auto,
            screen_reader: false,
            frame_rate: 60,
            message_width: 0,
            copy_on_select: false,
            max_transcript_lines: 5000,
            color: None,
            quiet: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    pub submit: SubmitKey,
    pub mode: EditorMode,
    /// "" = derived (SPEC §10.2).
    pub history_file: String,
    /// `100..=100000`.
    pub history_limit: u32,
    /// `1..=100`.
    pub mention_max_results: u32,
    /// `0..=1000000`.
    pub paste_confirm_chars: u32,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            submit: SubmitKey::Enter,
            mode: EditorMode::Emacs,
            history_file: String::new(),
            history_limit: 5000,
            mention_max_results: 10,
            paste_confirm_chars: 5000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// `tui` is only usable on a TTY.
    pub format: OutputFormat,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: LogLevel,
    /// "" = XDG state log path (SPEC §11.5).
    pub file: String,
    /// `1048576..=1073741824`.
    pub rotate_bytes: u64,
    /// `1..=100`.
    pub keep_rotated: u32,
    /// Must stay `true` unless `trace.debug_unsafe = true` (SPEC §12.1).
    pub redact: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Warn,
            file: String::new(),
            rotate_bytes: 10_485_760,
            keep_rotated: 3,
            redact: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TraceConfig {
    pub enabled: bool,
    /// "" = `<cache>/trace`.
    pub dir: String,
    pub redact: bool,
    /// `1048576..=1073741824`.
    pub max_file_bytes: u64,
    /// Allows `log.redact = false` (SPEC §12.1 `E-CFG-UNSAFEREDACT`).
    pub debug_unsafe: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: String::new(),
            redact: true,
            max_file_bytes: 52_428_800,
            debug_unsafe: false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Opt-in only (SPEC §12.2).
    pub enabled: bool,
    /// Anonymous install id; `""` regenerates.
    pub install_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub check: bool,
    /// `1..=720`.
    pub interval_hours: u32,
    pub channel: UpdateChannel,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            interval_hours: 24,
            channel: UpdateChannel::Stable,
        }
    }
}

/// XDG path overrides ("" = derived, SPEC §11.6).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PathsConfig {
    pub data_dir: String,
    pub cache_dir: String,
    pub config_dir: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HookConfig {
    pub events: Vec<HookEvent>,
    /// Executable path (SPEC §6.7.3).
    pub command: String,
    /// `1..=600000`.
    pub timeout_ms: u64,
    pub fail_mode: FailMode,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            command: String::new(),
            timeout_ms: 2000,
            fail_mode: FailMode::Warn,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CustomToolConfig {
    /// `^[a-z][a-z0-9_]{1,63}$`, must not collide with built-ins (SPEC §6.7.2).
    pub name: String,
    pub description: String,
    pub command: String,
    /// `1..=3600000`.
    pub timeout_ms: u64,
    pub permission_class: PermissionClass,
    /// Optional JSON Schema for the tool input.
    pub input_schema: Option<serde_json::Value>,
}

impl Default for CustomToolConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            command: String::new(),
            timeout_ms: 300_000,
            permission_class: PermissionClass::Execute,
            input_schema: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    pub transport: McpTransport,
    /// stdio transport: executable.
    pub command: String,
    pub args: Vec<String>,
    /// `${VAR}` expanded from env only (SPEC §6.7.1).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// http transport: endpoint.
    pub url: String,
    /// http transport: request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Working directory for stdio servers.
    pub cwd: String,
    /// `1000..=300000`.
    pub request_timeout_ms: u64,
    pub tools_allow: Vec<String>,
    pub tools_deny: Vec<String>,
}

impl Default for McpServer {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: McpTransport::Stdio,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            cwd: String::new(),
            request_timeout_ms: 15000,
            tools_allow: vec!["*".to_string()],
            tools_deny: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub servers: Vec<McpServer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    pub remember_scope: RememberScope,
    /// `10000..=3600000` (SPEC §8.1).
    pub ask_timeout_ms: u64,
    /// `1..=20` (SPEC §8.3 T-5).
    pub deny_ending_turn_after: u32,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            remember_scope: RememberScope::Project,
            ask_timeout_ms: 600_000,
            deny_ending_turn_after: 3,
        }
    }
}

/// Tool names Cairn ships with (SPEC §6.2) — custom tools may not shadow them.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "multi_edit",
    "list_dir",
    "glob",
    "grep",
    "bash",
    "bash_background",
    "job_output",
    "job_kill",
    "git_status",
    "git_diff",
    "git_commit",
    "web_fetch",
    "todo_write",
    "ask_user",
    "subagent",
    "task",
];

/// Themes shipped with the binary (SPEC §10.4, OQ-04).
pub const BUILTIN_THEMES: &[&str] = &["cairn-dark", "cairn-light", "high-contrast"];

doc_str!(Config);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec_examples() {
        let c = Config::default();
        assert_eq!(c.schema_version, 1);
        assert_eq!(c.profile, "default");
        assert_eq!(c.mode, Mode::Build);
        assert_eq!(c.model, "anthropic/claude-sonnet-4-5");
        assert_eq!(c.temperature, 0.2);
        assert_eq!(c.max_output_tokens, 8192);
        assert_eq!(c.shell.default_timeout_ms, 120_000);
        assert_eq!(c.discovery.max_file_size_bytes, 8_388_608);
        assert!(!c.discovery.follow_symlinks);
        assert_eq!(c.repo_map.top_k, 40);
        assert!((c.repo_map.weights.page + c.repo_map.weights.bm25 - 1.0).abs() < 1e-9);
        assert_eq!(c.context.output_reserve_pct, 12);
        assert_eq!(c.auto.max_iterations, 40);
        assert_eq!(c.session.retention_days, 90);
        assert_eq!(c.checkpoint.max_total_bytes, 1_073_741_824);
        assert_eq!(c.security.sandbox, SandboxLevel::Auto);
        assert_eq!(c.network.timeout_ms, 30000);
        assert_eq!(c.plans.editor, EditorKind::Internal);
        assert!(!c.modes.allow_unsafe);
        assert_eq!(c.subagent.max_parallel, 4);
        assert_eq!(c.verify.max_attempts, 3);
        assert_eq!(c.ui.theme, "cairn-dark");
        assert_eq!(c.log.level, LogLevel::Warn);
        assert_eq!(c.log.rotate_bytes, 10_485_760);
        assert!(!c.trace.enabled);
        assert!(!c.telemetry.enabled);
        assert_eq!(c.update.interval_hours, 24);
        assert_eq!(c.permissions.remember_scope, RememberScope::Project);
        assert_eq!(c.providers.len(), 4);
        assert_eq!(
            c.providers["anthropic"].base_url,
            "https://api.anthropic.com"
        );
        assert_eq!(c.providers["anthropic"].max_retries, 5);
    }

    #[test]
    fn defaults_serialize_to_spec_toml_values() {
        let c = Config::default();
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["context"]["history_budget_pct"], 55);
        assert_eq!(v["ui"]["diff_side_by_side_min_width"], 100);
        assert_eq!(v["shell"]["env_allowlist"].as_array().unwrap().len(), 22);
        assert_eq!(v["output"]["format"], "text");
        assert_eq!(v["log"]["level"], "warn");
        assert_eq!(v["mcp"]["servers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn strict_rejects_unknown_field() {
        let toml_text = "schema_version = 1\nbogus_key = true\n";
        let err = toml::from_str::<Config>(toml_text).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }

    #[test]
    fn partial_documents_deserialize_with_defaults() {
        let c: Config =
            toml::from_str("mode = \"plan\"\n[ui]\ntheme = \"high-contrast\"\n").unwrap();
        assert_eq!(c.mode, Mode::Plan);
        assert_eq!(c.ui.theme, "high-contrast");
        assert_eq!(c.temperature, 0.2, "unset keys keep defaults");
        assert_eq!(c.shell.kill_grace_ms, 2000);
    }

    #[test]
    fn unknown_enum_lists_options() {
        let err = toml::from_str::<Config>("mode = \"turbo\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("turbo"), "{msg}");
        assert!(msg.contains("plan") && msg.contains("auto-unsafe"), "{msg}");
    }

    #[test]
    fn builtin_lists_cover_spec() {
        assert_eq!(BUILTIN_TOOL_NAMES.len(), 19); // 18 tools + `task` alias
        assert_eq!(
            BUILTIN_THEMES,
            &["cairn-dark", "cairn-light", "high-contrast"]
        );
    }

    #[test]
    fn hook_events_match_spec() {
        let text = r#"
[[hooks]]
events = ["pre_tool", "on_error"]
command = "./h"
timeout_ms = 2000
fail_mode = "warn"
"#;
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.hooks.len(), 1);
        assert_eq!(
            c.hooks[0].events,
            vec![HookEvent::PreTool, HookEvent::OnError]
        );
    }

    #[test]
    fn mcp_server_shape() {
        let text = r#"
[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
request_timeout_ms = 15000
tools_allow = ["*"]
tools_deny = ["create_issue"]
"#;
        let c: Config = toml::from_str(text).unwrap();
        let s = &c.mcp.servers[0];
        assert_eq!(s.name, "github");
        assert_eq!(s.transport, McpTransport::Stdio);
        assert_eq!(s.env["GITHUB_TOKEN"], "${GITHUB_TOKEN}");
        assert_eq!(s.request_timeout_ms, 15000);
        assert_eq!(s.tools_deny, vec!["create_issue"]);
    }
}
