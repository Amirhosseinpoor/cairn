//! Command dispatch and the startup sequence (SPEC §11.1, REQ-CLI-004).

// Every handler returns `Result<_, Fail>` even when its current body cannot
// fail: `dispatch` turns a `Fail` into an §11.2 exit code in one place, and a
// handler that later grows a fallible step must not have to change shape.
#![allow(clippy::unnecessary_wraps)]

pub mod auth;
pub mod completions;
pub mod config;
pub mod doctor;
pub mod mcp;
pub mod run;
pub mod sessions;
pub mod version;

use crate::args::{Cli, Command, ConfigCmd};
use crate::output::{warn_line, Fail};
use cairn_config::{load, FlagOverrides, LoadOptions, Loaded};
use cairn_core::error::ExitStatus;
use std::io::IsTerminal;

/// Config + workspace resolution shared by every command that needs it.
pub struct Startup {
    pub loaded: Loaded,
    pub quiet: bool,
}

impl Startup {
    pub fn workspace(&self) -> &std::path::Path {
        &self.loaded.workspace
    }
}

/// Runs one invocation end to end and returns the process exit code.
pub fn dispatch(cli: &Cli) -> Result<i32, Fail> {
    validate_static(cli)?;

    // Commands that must work even when the config is broken.
    match &cli.command {
        Some(Command::Version(args)) => return version::run(cli, args),
        Some(Command::Completions(args)) => return completions::run(args),
        _ => {}
    }

    let loaded = load(&load_options(cli));

    // These three never sit behind the startup checks. `validate` and `doctor`
    // *are* the diagnosis; `migrate` exists precisely to repair a config whose
    // `schema_version` is the thing `validate` rejects as fatal — blocking it
    // behind that rejection would make it unrunnable on the only files it can
    // fix (SPEC §11.7.1).
    match &cli.command {
        Some(Command::Config(ConfigCmd::Validate)) => {
            return config::validate_cmd(&loaded, cli.quiet)
        }
        Some(Command::Doctor(args)) => return doctor::run(cli, args, &loaded),
        Some(Command::Migrate) => {
            return run::migrate(&Startup {
                loaded,
                quiet: cli.quiet,
            });
        }
        _ => {}
    }

    emit_startup_issues(&loaded, cli.quiet)?;

    let startup = Startup {
        loaded,
        quiet: cli.quiet,
    };

    match &cli.command {
        None | Some(Command::Chat(_)) => run::chat(cli, &startup),
        Some(Command::Run(args)) => run::run(cli, args, &startup),
        Some(Command::Resume(args)) => run::resume(cli, args, &startup),
        Some(Command::Sessions(args)) => sessions::list(args, &startup),
        Some(Command::Config(cmd)) => config::run(cmd, &startup),
        Some(Command::Auth(cmd)) => auth::run(cmd, &startup),
        Some(Command::Mcp(cmd)) => mcp::run(cmd, &startup),
        Some(Command::Update(args)) => run::update(args),
        Some(Command::Export(args)) => run::export(cli, args, &startup),
        Some(Command::Init(args)) => run::init(args),
        // `Migrate` is handled above, before the startup checks.
        Some(
            Command::Version(_) | Command::Completions(_) | Command::Doctor(_) | Command::Migrate,
        ) => unreachable!("handled above"),
    }
}

/// Argument rules clap cannot express (SPEC §11.1 `run` signature).
fn validate_static(cli: &Cli) -> Result<(), Fail> {
    if let Some(Command::Run(args)) = &cli.command {
        let sources = [
            args.prompt.is_some(),
            args.prompt_file.is_some(),
            args.stdin,
        ];
        if sources.iter().filter(|x| **x).count() > 1 {
            return Err(Fail::usage(
                "`run` takes exactly one of -p/--prompt, --prompt-file or --stdin",
                Some("pass the prompt in a single place: `cairn run -p '<prompt>'`".to_string()),
            ));
        }
        if !sources.iter().any(|x| *x) {
            return Err(Fail::usage(
                "`run` needs a prompt: one of -p/--prompt, --prompt-file or --stdin",
                Some("example: `cairn run -p 'explain this repository'`".to_string()),
            ));
        }
    }
    Ok(())
}

/// Command line → loader options (SPEC §11.5 precedence: flags layer).
pub fn load_options(cli: &Cli) -> LoadOptions {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // `cairn export … --output PATH` (SPEC §11.1 export signature) shares its
    // spelling with the global `--output` format flag. For that one subcommand
    // the value is a destination, so it must never reach `output.format` —
    // which is an enum, and would reject every path with `E-CFG-BADVALUE`.
    let output = if matches!(cli.command, Some(Command::Export(_))) {
        None
    } else {
        cli.output.clone()
    };
    let flags = FlagOverrides {
        model: cli.model.clone(),
        mode: cli.mode.clone(),
        profile: cli.profile.clone(),
        output,
        log_level: cli.log_level.clone(),
        log_file: cli.log_file.as_ref().map(|p| p.display().to_string()),
        trace: cli.trace.then_some(true),
        offline: cli.offline.then_some(true),
        quiet: cli.quiet.then_some(true),
        no_color: cli.no_color.then_some(true),
        no_update_check: cli.no_update_check.then_some(true),
        allow_unsafe: cli.dangerously_skip_permissions.then_some(true),
        max_iterations: None,
        approve_plan: None,
        verbose: cli.verbose,
    };
    LoadOptions {
        cwd,
        workspace: cli.workspace.clone(),
        explicit_config: cli.config.clone(),
        profile: cli.profile.clone(),
        flags,
        env: None,
        allow_unknown_keys: cli.allow_unknown_keys,
        is_tty: Some(std::io::stdout().is_terminal()),
    }
}

/// Non-fatal issues become warnings; fatal ones abort with exit 2
/// (REQ-CLI-004: errors outside the sections this command needs never block).
fn emit_startup_issues(loaded: &Loaded, quiet: bool) -> Result<(), Fail> {
    if !quiet {
        for issue in &loaded.issues {
            if issue.fatal {
                continue;
            }
            if issue.code.starts_with('E') {
                // Surfaced under the partial-startup warning; the original code
                // stays visible inside the message (REQ-CLI-004).
                warn_line(cairn_core::error::codes::CFG_PARTIAL, &issue.render());
            } else {
                yell!("warning: {}", issue.render());
            }
        }
    }
    if let Some(first) = loaded.fatal_issues().first() {
        return Err(Fail::new(
            first.code,
            ExitStatus::Usage,
            first.render(),
            Some(
                "fix the key above, or run `cairn config validate` for every problem at once"
                    .to_string(),
            ),
        ));
    }
    Ok(())
}
