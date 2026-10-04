//! Commands whose subsystems land after M0 (SPEC §11.1, §15.4).
//!
//! Everything here either does real M0 work (argument checks, session lookup,
//! offline gating) or fails with `E-IMPL-STAGE` naming the milestone that
//! delivers it — never a silent success.

use crate::args::{ExportArgs, ResumeArgs, RunArgs, SessionsArgs, UpdateArgs};
use crate::commands::{sessions, Startup};
use crate::output::Fail;
use cairn_core::error::{codes, ExitStatus};

/// `cairn run` — M0 validates input and the offline gate; the model loop is M1.
pub fn run(cli: &crate::args::Cli, args: &RunArgs, startup: &Startup) -> Result<i32, Fail> {
    use std::io::IsTerminal;

    // Offline fails fast, before any prompt or store work (T-CLI-022).
    if cli.offline || startup.loaded.config.network.offline {
        return Err(Fail::new(
            codes::PROV_OFFLINE,
            ExitStatus::Provider,
            "offline: providers are unreachable (`network.offline` / `--offline`)",
            Some(
                "drop --offline to reach a provider, or use `cairn config list` to inspect the key"
                    .to_string(),
            ),
        ));
    }

    // The prompt must be readable before anything else starts (M1 consumes it).
    if let Some(file) = &args.prompt_file {
        if file.as_os_str() != "-" && !file.exists() {
            return Err(Fail::new(
                codes::FS_NOTFOUND,
                ExitStatus::NotFound,
                format!("prompt file {} does not exist", file.display()),
                Some("check the path, or pass the prompt with -p '<text>'".to_string()),
            ));
        }
    }
    if args.stdin && std::io::stdin().is_terminal() {
        return Err(Fail::usage(
            "--stdin was given but stdin is a terminal",
            "pipe the prompt in, or use -p '<text>'".to_string(),
        ));
    }

    Err(Fail::not_implemented(
        "`cairn run` (provider + tool loop)",
        "M1",
    ))
}

/// `cairn chat` (and bare `cairn`) — the TUI is M3.
pub fn chat(_cli: &crate::args::Cli, _startup: &Startup) -> Result<i32, Fail> {
    Err(Fail::not_implemented(
        "`cairn chat` (interactive session)",
        "M3",
    ))
}

/// `cairn resume [SESSION_ID] [--list] [--json]`.
pub fn resume(_cli: &crate::args::Cli, args: &ResumeArgs, startup: &Startup) -> Result<i32, Fail> {
    let Some(id) = args.session_id.as_deref() else {
        // No id: listing is the only useful behaviour before M3.
        return sessions::list(
            &SessionsArgs {
                json: args.json,
                limit: 20,
                workspace: None,
                grep: None,
            },
            startup,
        );
    };
    let store = sessions::store(startup);
    let path = store.find(id).ok_or_else(|| {
        Fail::not_found(
            format!("session '{id}' not found"),
            "run `cairn sessions` to list stored sessions".to_string(),
        )
    })?;
    // Replay lands in M3, but *knowing the session loads* does not: a file that
    // exists and cannot be read must say `E-SESS-CORRUPT` (exit 9) here rather
    // than "not implemented" (SPEC §11.7 read contract, T-SESS-023).
    store.load(&path).map_err(Fail::from_cairn)?;
    Err(Fail::not_implemented(
        &format!("`cairn resume {id}` (session replay)"),
        "M3",
    ))
}

/// `cairn export SESSION_ID --format md|json|html [--output PATH] [--no-redact]`.
///
/// `--output` is the global flag re-read as a destination (see `Cli::output`).
pub fn export(cli: &crate::args::Cli, args: &ExportArgs, startup: &Startup) -> Result<i32, Fail> {
    let dest = export_destination(cli)?;
    let store = sessions::store(startup);
    let path = store.find(&args.session_id).ok_or_else(|| {
        Fail::not_found(
            format!("session '{}' not found", args.session_id),
            "run `cairn sessions` to list stored sessions".to_string(),
        )
    })?;
    let file = store.load(&path).map_err(Fail::from_cairn)?;

    // Redaction is on unless it was explicitly turned off — and turning it off
    // requires a confirmation (REQ-CLI-010, T-SEC-012, T-SESS-031).
    let redactor = if args.no_redact {
        if !confirm_unredacted_export() {
            return Err(Fail::usage(
                "--no-redact would write secrets to the export in cleartext",
                "answer `y` at the prompt in a terminal, or set CAIRN_ALLOW_UNREDACTED_EXPORT=1 \
                 for scripts (SPEC §11.7, REQ-CLI-010)"
                    .to_string(),
            ));
        }
        None
    } else {
        Some(cairn_core::redact::Redactor::default())
    };

    let format = match args.format {
        crate::args::ExportFormat::Md => cairn_session::export::Format::Md,
        crate::args::ExportFormat::Json => cairn_session::export::Format::Json,
        crate::args::ExportFormat::Html => cairn_session::export::Format::Html,
    };
    let rendered = cairn_session::export::render(&file, format, redactor.as_ref())
        .map_err(Fail::from_cairn)?;

    match &dest {
        Some(dest) => {
            std::fs::write(dest, rendered.as_bytes()).map_err(|e| write_fail(dest, &e))?;
            if !startup.quiet {
                yell!("wrote {}", dest.display());
            }
        }
        // `say!` appends one newline; the renderers already terminate theirs.
        None => say!("{}", rendered.trim_end()),
    }
    Ok(0)
}

/// The file `--output` names for `cairn export`, or `None` for stdout.
///
/// §11.1 gives `--output` two meanings — the global format flag and the export
/// destination — and clap lets a command define only one. Export therefore
/// reads the global value back as a path, and refuses the format spellings
/// rather than creating a file literally called `json`.
fn export_destination(cli: &crate::args::Cli) -> Result<Option<std::path::PathBuf>, Fail> {
    match cli.output.as_deref() {
        None => Ok(None),
        Some(value) if matches!(value, "text" | "json" | "stream-json" | "tui") => {
            Err(Fail::usage(
                format!("`export --output` is a destination path, not an output format ({value})"),
                "use `--format md|json|html` to choose the export's format; `--output` names the \
                 file to write, and defaults to stdout (SPEC §11.1)",
            ))
        }
        Some(value) => Ok(Some(std::path::PathBuf::from(value))),
    }
}

/// Consent for `--no-redact`: an explicit env confirmation, or a person at a
/// terminal answering `y`. Anything else refuses (exit 2).
fn confirm_unredacted_export() -> bool {
    use std::io::IsTerminal;

    if std::env::var("CAIRN_ALLOW_UNREDACTED_EXPORT").is_ok_and(|v| v == "1") {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    yell!(
        "warning: exporting WITHOUT redaction — every secret in this session will be written \
         in cleartext"
    );
    yell!("answer in the terminal to continue: [y/N]");
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().chars().next(), Some('y' | 'Y'))
}

/// `cairn migrate` — bring stored sessions and `config.toml` up to the schema
/// this build writes (SPEC §11.7.1, REQ-CLI-009).
///
/// Handled *before* the normal startup checks (see `dispatch`): a config whose
/// `schema_version` is wrong is exactly the thing `validate` rejects, and a
/// repair command that could not run on a broken config would be useless.
pub fn migrate(startup: &Startup) -> Result<i32, Fail> {
    let mut failures: Vec<Fail> = Vec::new();
    let mut changed = 0usize;

    for path in config_files(startup) {
        match migrate_config_file(&path) {
            Ok(ConfigAction::Missing | ConfigAction::Current) => {}
            Ok(ConfigAction::Upgraded(from)) => {
                changed += 1;
                if !startup.quiet {
                    say!(
                        "{}: schema_version {from} → {}",
                        path.display(),
                        cairn_config::SCHEMA_VERSION
                    );
                }
            }
            Err(fail) => failures.push(fail),
        }
    }

    let store = sessions::store(startup);
    let (done, failed) =
        cairn_session::migrate::migrate_all(&store.paths(), cairn_session::CURRENT_SCHEMA_VERSION);
    for report in done {
        match report.action {
            cairn_session::migrate::Action::Migrated => {
                changed += 1;
                if !startup.quiet {
                    say!(
                        "session {}: schema_version {} → {} ({})",
                        report.session_id,
                        report.from,
                        report.to,
                        report.notes.join("; ")
                    );
                }
            }
            cairn_session::migrate::Action::NewerThanTarget => {
                if !startup.quiet {
                    yell!(
                        "warning: {}: session schema_version {} is newer than this build; \
                         left byte-identical",
                        report.session_id,
                        report.from
                    );
                }
            }
            cairn_session::migrate::Action::UpToDate => {}
        }
    }
    for (_path, err) in failed {
        let fail = Fail::from_cairn(err);
        if failures.is_empty() {
            // Reported by `dispatch` as the headline failure.
            failures.push(fail);
        } else {
            yell!("warning: {}: {}", fail.code, fail.message);
            failures.push(fail);
        }
    }

    if let Some(first) = failures.into_iter().next() {
        return Err(first);
    }
    if changed == 0 && !startup.quiet {
        say!("nothing to migrate");
    }
    Ok(0)
}

/// Every `config.toml` Cairn could be reading: system, user, and one `.cairn/`
/// per directory from the workspace up to its root (SPEC §11.5 layer order).
fn config_files(startup: &Startup) -> Vec<std::path::PathBuf> {
    let mut out = vec![
        cairn_config::Paths::system_config_file(),
        startup.loaded.paths.user_config_file(),
    ];
    let mut dir = Some(startup.workspace().to_path_buf());
    while let Some(current) = dir {
        out.push(current.join(".cairn").join("config.toml"));
        dir = current.parent().map(std::path::Path::to_path_buf);
    }
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigAction {
    /// No such file — nothing to do.
    Missing,
    /// Present and already at `SCHEMA_VERSION` (or declaring no version at all,
    /// which means the default).
    Current,
    /// Rewritten from this version to `SCHEMA_VERSION`.
    Upgraded(i64),
}

/// Move one `config.toml`'s `schema_version` forward.
fn migrate_config_file(path: &std::path::Path) -> Result<ConfigAction, Fail> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigAction::Missing);
        }
        Err(e) => return Err(write_fail(path, &e)),
    };
    let Some((index, current)) = declared_schema_version(&text) else {
        return Ok(ConfigAction::Current);
    };
    if current == i64::from(cairn_config::SCHEMA_VERSION) {
        return Ok(ConfigAction::Current);
    }
    if current < 0 || current > i64::from(cairn_config::SCHEMA_VERSION) {
        return Err(Fail::new(
            codes::CFG_VERSION,
            ExitStatus::Usage,
            format!(
                "{} declares schema_version {current}, but this build writes {}",
                path.display(),
                cairn_config::SCHEMA_VERSION
            ),
            "downgrades are not supported (SPEC §0); run `cairn migrate` with the newer Cairn"
                .to_string(),
        ));
    }

    // Backup first, then rewrite — the same order REQ-CLI-009 requires of a
    // session migration, for the same reason.
    let backup = path.with_extension(format!("toml.bak-v{current}"));
    std::fs::write(&backup, text.as_bytes()).map_err(|e| write_fail(&backup, &e))?;

    // Only the one line carrying the version moves, so comments, key order and
    // the author's formatting survive untouched — and the rewrite still works
    // on a file that uses `key += [...]` (SPEC §11.5), which no strict TOML
    // parser would accept.
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let original = lines[index].clone();
    let indent = original.len() - original.trim_start().len();
    lines[index] = format!(
        "{}schema_version = {}",
        &original[..indent],
        cairn_config::SCHEMA_VERSION
    );
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut rewritten = lines.join(newline);
    if text.ends_with('\n') {
        rewritten.push_str(newline);
    }
    write_atomic(path, rewritten.as_bytes()).map_err(|e| write_fail(path, &e))?;
    Ok(ConfigAction::Upgraded(current))
}

/// The root-level `schema_version = <integer>` line, as `(index, value)`.
fn declared_schema_version(text: &str) -> Option<(usize, i64)> {
    let mut inside_table = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // Everything after the first table header belongs to that table.
            inside_table = true;
            continue;
        }
        if inside_table {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("schema_version") else {
            continue;
        };
        let rest = rest.trim_start();
        match rest.chars().next() {
            // `schema_version_extra = 1` is a different key.
            Some(c) if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') => continue,
            Some('=') => {}
            // Declared but not an assignment: leave it to `cairn config
            // validate`, which reports it with a line and a column.
            _ => return None,
        }
        let rhs = rest[1..].split('#').next().unwrap_or_default().trim();
        return rhs.parse::<i64>().ok().map(|value| (index, value));
    }
    None
}

/// Write `bytes` to `path` via a temp file and a rename.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Map a failed write onto the §6.5 filesystem codes that fit it.
fn write_fail(path: &std::path::Path, e: &std::io::Error) -> Fail {
    let (code, status, hint) = match e.kind() {
        std::io::ErrorKind::NotFound => (
            codes::FS_NOPARENT,
            ExitStatus::NotFound,
            "create the parent directory first".to_string(),
        ),
        std::io::ErrorKind::PermissionDenied => (
            codes::FS_PERM,
            ExitStatus::Permission,
            "check who owns the path, and its directory's permissions".to_string(),
        ),
        _ => (
            codes::FS_READONLY,
            ExitStatus::Permission,
            "the destination is not writable (read-only mount, or the disk is full)".to_string(),
        ),
    };
    Fail::new(code, status, format!("{}: {e}", path.display()), Some(hint))
}

/// `cairn update [--check]` — release plumbing is M5.
pub fn update(_args: &UpdateArgs) -> Result<i32, Fail> {
    Err(Fail::not_implemented(
        "`cairn update` (signed releases)",
        "M5",
    ))
}

/// `cairn init [--global]` — scaffolding is M3, alongside §10.3's `/init`.
///
/// The command has to exist before then: `cairn doctor` tells people with no
/// `AGENTS.md` to run it (§12.3), and REQ-SAFE-003 / §7.3 name it as the thing
/// that appends the `.cairn` entries to `.gitignore`.
pub fn init(args: &crate::args::InitArgs) -> Result<i32, Fail> {
    let what = if args.global {
        "`cairn init --global` (user-level scaffolding)"
    } else {
        "`cairn init` (workspace scaffolding)"
    };
    Err(Fail::not_implemented(what, "M3"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{LoadOptions, Paths};
    use clap::Parser;
    use std::collections::BTreeMap;

    fn startup_in(dir: &std::path::Path) -> Startup {
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

    fn args() -> RunArgs {
        RunArgs {
            prompt: Some("hi".to_string()),
            prompt_file: None,
            stdin: false,
            input: None,
            session: None,
            approve_plan: false,
            max_iterations: None,
            allow_ask: false,
            input_fmt: "text".to_string(),
            tee: false,
        }
    }

    #[test]
    fn offline_fails_fast_with_provider_code() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let mut a = args();
        a.prompt = None;
        a.prompt_file = Some("missing.txt".into());
        // Simulate `--offline` on the command line via the config layer.
        let mut loaded = startup.loaded.clone();
        loaded.config.network.offline = true;
        let offline = Startup {
            loaded,
            quiet: true,
        };
        let err = run(&crate::args::Cli::parse_from(["cairn"]), &a, &offline).unwrap_err();
        assert_eq!(err.code, codes::PROV_OFFLINE);
        assert_eq!(err.exit, 3);
    }

    #[test]
    fn missing_prompt_file_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let mut a = args();
        a.prompt = None;
        a.prompt_file = Some("nope.txt".into());
        let err = run(&crate::args::Cli::parse_from(["cairn"]), &a, &startup).unwrap_err();
        assert_eq!(err.exit, 9);
        assert_eq!(err.code, codes::FS_NOTFOUND);
    }

    #[test]
    fn stubs_name_their_milestone() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let cli = crate::args::Cli::parse_from(["cairn"]);
        assert!(chat(&cli, &startup).unwrap_err().message.contains("M3"));
        assert!(run(&cli, &args(), &startup)
            .unwrap_err()
            .message
            .contains("M1"));
        assert!(update(&UpdateArgs {
            check: false,
            version: None,
            yes: false
        })
        .unwrap_err()
        .message
        .contains("M5"));
    }

    #[test]
    fn resume_unknown_session_is_exit_9() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let cli = crate::args::Cli::parse_from(["cairn"]);
        let err = resume(
            &cli,
            &ResumeArgs {
                session_id: Some("ses_missing".into()),
                list: false,
                json: false,
            },
            &startup,
        )
        .unwrap_err();
        assert_eq!(err.exit, 9);
        assert_eq!(err.code, codes::SESS_NOTFOUND);
        assert!(err.message.contains("ses_missing"));
    }

    #[test]
    fn export_without_a_redaction_confirmation_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let startup = startup_in(tmp.path());
        let cli = crate::args::Cli::parse_from(["cairn", "export", "ses_nope"]);
        // Unknown session short-circuits before the redaction check.
        let err = export(
            &cli,
            &ExportArgs {
                session_id: "ses_nope".into(),
                format: crate::args::ExportFormat::Md,
                redact: false,
                no_redact: false,
            },
            &startup,
        )
        .unwrap_err();
        assert_eq!(err.exit, 9);
    }

    /// §11.1 gives `--output` two meanings; a format spelling at `export` would
    /// otherwise create a file called `json`.
    #[test]
    fn export_output_flag_refuses_a_format_name() {
        let cli = crate::args::Cli::parse_from(["cairn", "export", "ses_01", "--output", "json"]);
        let err = export_destination(&cli).unwrap_err();
        assert_eq!(err.code, codes::CLI_USAGE);
        assert_eq!(err.exit, 2);

        let cli = crate::args::Cli::parse_from(["cairn", "export", "ses_01"]);
        assert_eq!(export_destination(&cli).unwrap(), None);

        let cli =
            crate::args::Cli::parse_from(["cairn", "export", "ses_01", "--output", "out.json"]);
        assert_eq!(
            export_destination(&cli).unwrap(),
            Some(std::path::PathBuf::from("out.json"))
        );
    }
}
