//! The complete command tree of SPEC §11.1 — clap derive types.
//!
//! The flag table in `cairn-config::schema::FLAGS` is checked against this file
//! by test `every_spec_flag_is_annotated` (T-CFG-001).

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// `cairn [GLOBAL FLAGS] [SUBCOMMAND] [ARGS]` (SPEC §11.1).
#[derive(Debug, Parser)]
#[command(
    name = "cairn",
    version,
    about = "Cairn — terminal-native AI coding agent",
    long_about = "Cairn — a terminal-native AI coding agent.\n\n\
        Run `cairn` with no subcommand for the interactive session (chat), \
        `cairn run -p '<prompt>'` for a single headless turn, or `cairn help` \
        for the full command tree.",
    propagate_version = true,
    disable_help_subcommand = false
)]
pub struct Cli {
    /// Workspace root (default: nearest `.git`, else CWD) — `CAIRN_WORKSPACE`.
    #[arg(long, global = true, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// Model id (`provider/model`) — `CAIRN_MODEL`.
    #[arg(short = 'm', long, global = true, value_name = "ID")]
    pub model: Option<String>,

    /// Operating mode: plan | build | auto | auto-unsafe — `CAIRN_MODE`.
    #[arg(long, global = true, value_name = "MODE")]
    pub mode: Option<String>,

    /// Extra config file (highest config precedence) — `CAIRN_CONFIG`.
    #[arg(short = 'c', long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Config profile selector — `CAIRN_PROFILE`.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Output format: text | json | stream-json | tui — `CAIRN_OUTPUT`.
    #[arg(long, global = true, value_name = "FORMAT")]
    pub output: Option<String>,

    /// Disable ANSI color — `NO_COLOR`.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Log verbosity — `CAIRN_LOG_LEVEL`.
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,

    /// Log file path — `CAIRN_LOG_FILE`.
    #[arg(long, global = true, value_name = "FILE")]
    pub log_file: Option<PathBuf>,

    /// Record full model I/O to the trace dir — `CAIRN_TRACE`.
    #[arg(long, global = true)]
    pub trace: bool,

    /// Enable `auto-unsafe` mode (G-M1).
    #[arg(long, global = true)]
    pub dangerously_skip_permissions: bool,

    /// Skip the background update check — `CAIRN_NO_UPDATE_CHECK`.
    #[arg(long, global = true)]
    pub no_update_check: bool,

    /// Never touch the network — `CAIRN_OFFLINE`.
    #[arg(long, global = true)]
    pub offline: bool,

    /// Suppress progress output; only the code line is printed on errors.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// `-v` info, `-vv` debug, `-vvv` trace — `CAIRN_VERBOSE`.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Load config keys Cairn does not know about (SPEC §11.4.2).
    #[arg(long, global = true)]
    pub allow_unknown_keys: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Single-turn headless execution (`run -p TEXT | --prompt-file FILE | --stdin`).
    Run(RunArgs),
    /// Interactive TUI session (the default subcommand).
    Chat(ChatArgs),
    /// Continue a previous session.
    Resume(ResumeArgs),
    /// List stored sessions.
    Sessions(SessionsArgs),
    /// Read and write configuration.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Manage provider credentials.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Manage MCP servers.
    #[command(subcommand)]
    Mcp(McpCmd),
    /// Run diagnostics.
    Doctor(DoctorArgs),
    /// Self-update.
    #[command(disable_version_flag = true)]
    Update(UpdateArgs),
    /// Export a session to md/json/html.
    Export(ExportArgs),
    /// Print version and build information.
    Version(VersionArgs),
    /// Generate shell completions.
    Completions(CompletionsArgs),
    /// Force session/config migration.
    Migrate,
}

/// `cairn run` flags (SPEC §11.1 run table).
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Prompt text.
    #[arg(
        short = 'p',
        long,
        value_name = "TEXT",
        conflicts_with_all = ["prompt_file", "stdin"]
    )]
    pub prompt: Option<String>,

    /// Read the prompt from a file (`-` = stdin).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["prompt", "stdin"])]
    pub prompt_file: Option<PathBuf>,

    /// Read the prompt from stdin.
    #[arg(long, conflicts_with_all = ["prompt", "prompt_file"])]
    pub stdin: bool,

    /// JSONL transcript to preload.
    #[arg(long, value_name = "PATH")]
    pub input: Option<PathBuf>,

    /// Continue an existing session.
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,

    /// Auto-approve a produced plan (headless).
    #[arg(long)]
    pub approve_plan: bool,

    /// Override the iteration cap for the active mode.
    #[arg(long, value_name = "N")]
    pub max_iterations: Option<u32>,

    /// Read approval answers from stdin as JSON lines.
    #[arg(long)]
    pub allow_ask: bool,

    /// Format of `--input` (`text` or `jsonl`).
    #[arg(long, value_name = "FMT", default_value = "text")]
    pub input_fmt: String,

    /// Also print progress to stderr.
    #[arg(long)]
    pub tee: bool,
}

#[derive(Debug, Args, Default)]
pub struct ChatArgs {
    /// Continue an existing session.
    #[arg(long, value_name = "SESSION_ID")]
    pub session: Option<String>,

    /// Resume the most recent session for this workspace.
    #[arg(long)]
    pub resume: bool,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Session id; omit with `--list` to list sessions.
    pub session_id: Option<String>,

    /// List resumable sessions instead of resuming.
    #[arg(long)]
    pub list: bool,

    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionsArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,

    /// Maximum number of sessions shown.
    #[arg(long, value_name = "N", default_value_t = 20)]
    pub limit: usize,

    /// Only sessions for this workspace.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// Substring filter over the session header.
    #[arg(long, value_name = "TEXT")]
    pub grep: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print one effective value.
    Get {
        key: String,
        /// Annotate with the winning layer (REQ-CLI-006).
        #[arg(long)]
        effective: bool,
    },
    /// Write one key (default target: user config).
    Set {
        key: String,
        /// TOML value; unquoted text is stored as a string.
        value: String,
        /// Write to `.cairn/config.toml` instead of the user config.
        #[arg(long)]
        project: bool,
    },
    /// Remove one key.
    Unset {
        key: String,
        /// Remove from `.cairn/config.toml` instead of the user config.
        #[arg(long)]
        project: bool,
    },
    /// Print the effective configuration.
    List {
        /// Machine-readable effective view.
        #[arg(long)]
        json: bool,
        /// Print the JSON Schema of `config.toml`.
        #[arg(long)]
        json_schema: bool,
        /// Annotate each key with its winning layer (REQ-CLI-006).
        #[arg(long)]
        effective: bool,
    },
    /// Validate every layer; reports all problems at once (REQ-CLI-003).
    Validate,
    /// Print config file paths.
    Path {
        #[arg(long)]
        user: bool,
        #[arg(long)]
        project: bool,
        #[arg(long)]
        system: bool,
    },
    /// Open the user config in `$VISUAL`/`$EDITOR`.
    Edit,
}

#[derive(Debug, Subcommand)]
pub enum AuthCmd {
    /// Store a provider API key (keychain first, SPEC §4.10).
    Login {
        /// Provider id; defaults to the active model's provider.
        provider: Option<String>,
        /// Read the key from stdin instead of prompting.
        #[arg(long)]
        key_stdin: bool,
    },
    /// Remove a stored key.
    Logout { provider: String },
    /// List providers, key source and last 4 characters (REQ-PROV-018).
    List {
        #[arg(long)]
        json: bool,
    },
    /// One line per enabled provider: ok / missing.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// List configured servers (REQ-TOOL-025).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a server to the config file.
    Add {
        name: String,
        #[arg(long, value_name = "TRANSPORT", default_value = "stdio")]
        transport: String,
        /// stdio transport: executable.
        #[arg(long, value_name = "CMD")]
        command: Option<String>,
        /// stdio transport: repeatable argument (values may start with `-`).
        #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
        args: Vec<String>,
        /// http transport: endpoint.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Working directory for stdio servers.
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,
        /// Write to `.cairn/config.toml` instead of the user config.
        #[arg(long)]
        project: bool,
    },
    /// Remove a server from the config file.
    Remove {
        name: String,
        /// Remove from `.cairn/config.toml` instead of the user config.
        #[arg(long)]
        project: bool,
    },
    /// Show server details and tool list.
    Inspect {
        name: String,
        #[arg(long)]
        tools: bool,
    },
    /// Re-synchronize tool lists.
    Refresh { name: Option<String> },
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Include the live tool table.
    #[arg(long)]
    pub tools: bool,
    /// Include provider reachability probes.
    #[arg(long)]
    pub network: bool,
    /// Include the scripted self-check and deep scans.
    #[arg(long)]
    pub deep: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only report whether an update is available (exit 0 either way).
    #[arg(long)]
    pub check: bool,
    /// Target version.
    #[arg(long, value_name = "X")]
    pub version: Option<String>,
    /// Skip confirmation.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Session id.
    pub session_id: String,

    /// Output format.
    #[arg(long = "format", value_enum, default_value = "md")]
    pub format: ExportFormat,

    /// Destination path (default: stdout for md/json, required for html).
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Force redaction on (the default).
    #[arg(long)]
    pub redact: bool,

    /// Disable redaction (requires confirmation or `CAIRN_ALLOW_UNREDACTED_EXPORT=1`).
    #[arg(long, conflicts_with = "redact")]
    pub no_redact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ExportFormat {
    Md,
    Json,
    Html,
}

#[derive(Debug, Args, Default)]
pub struct VersionArgs {
    /// Machine-readable build info (identical JSON schema on every Tier-1 target).
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Target shell.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

impl Cli {
    /// Effective `output.format` for this invocation (flag/env/config chain is
    /// resolved later by `cairn-config`; this is only the flag layer).
    pub fn wants_json(&self) -> bool {
        self.output.as_deref() == Some("json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// T-CLI-021 groundwork: the command tree builds and every §11.1 subcommand exists.
    #[test]
    fn command_tree_matches_spec() {
        let cmd = Cli::command();
        let names: Vec<String> = cmd
            .get_subcommands()
            .map(std::string::ToString::to_string)
            .collect();
        for expected in [
            "run",
            "chat",
            "resume",
            "sessions",
            "config",
            "auth",
            "mcp",
            "doctor",
            "update",
            "export",
            "version",
            "completions",
            "migrate",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing subcommand {expected}: {names:?}"
            );
        }
        assert_eq!(names.len(), 13, "exactly the §11.1 subcommands: {names:?}");
    }

    /// T-CLI-020: `-p` together with `--prompt-file` is a usage error.
    #[test]
    fn p_and_prompt_file_conflict() {
        let err = Cli::try_parse_from(["cairn", "run", "-p", "hi", "--prompt-file", "f.txt"])
            .expect_err("must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// Global flags parse before or after the subcommand.
    #[test]
    fn global_flags_parse_anywhere() {
        let a = Cli::try_parse_from(["cairn", "--offline", "run", "-p", "x"]).unwrap();
        assert!(a.offline);
        let b = Cli::try_parse_from(["cairn", "run", "-p", "x", "--offline"]).unwrap();
        assert!(b.offline);
        let c = Cli::try_parse_from(["cairn", "-vv", "version"]).unwrap();
        assert_eq!(c.verbose, 2);
        assert!(matches!(c.command, Some(Command::Version(_))));
    }

    /// No subcommand defaults to chat (SPEC §1.6).
    #[test]
    fn no_subcommand_is_chat() {
        let cli = Cli::try_parse_from(["cairn"]).unwrap();
        assert!(cli.command.is_none());
    }

    /// `--mode` is free-form at the clap layer so the config layer can list the
    /// valid options (SPEC §11.4.2 `E-CFG-BADVALUE`).
    #[test]
    fn unknown_mode_is_not_a_clap_error() {
        let cli = Cli::try_parse_from(["cairn", "--mode", "sideways", "run", "-p", "x"]).unwrap();
        assert_eq!(cli.mode.as_deref(), Some("sideways"));
    }
}
