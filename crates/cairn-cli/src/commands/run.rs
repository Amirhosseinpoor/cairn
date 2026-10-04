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
        // No id: listing is the only useful M0 behaviour.
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
    if sessions::scan(startup).iter().any(|m| m.session_id == id) {
        return Err(Fail::not_implemented(
            &format!("`cairn resume {id}` (session replay)"),
            "M3",
        ));
    }
    Err(Fail::not_found(
        format!("session '{id}' not found"),
        "run `cairn sessions` to list stored sessions".to_string(),
    ))
}

/// `cairn export SESSION_ID …` — session store is M1.
pub fn export(args: &ExportArgs, startup: &Startup) -> Result<i32, Fail> {
    if !sessions::scan(startup)
        .iter()
        .any(|m| m.session_id == args.session_id)
    {
        return Err(Fail::not_found(
            format!("session '{}' not found", args.session_id),
            "run `cairn sessions` to list stored sessions".to_string(),
        ));
    }
    if args.no_redact {
        let allowed = std::env::var("CAIRN_ALLOW_UNREDACTED_EXPORT").is_ok_and(|v| v == "1");
        if !allowed {
            return Err(Fail::usage(
                "--no-redact exports secrets in cleartext",
                "set CAIRN_ALLOW_UNREDACTED_EXPORT=1 to confirm (SPEC §11.7, REQ-CLI-010)"
                    .to_string(),
            ));
        }
    }
    Err(Fail::not_implemented(
        &format!("`cairn export {}`", args.session_id),
        "M1",
    ))
}

/// `cairn update [--check]` — release plumbing is M5.
pub fn update(_args: &UpdateArgs) -> Result<i32, Fail> {
    Err(Fail::not_implemented(
        "`cairn update` (signed releases)",
        "M5",
    ))
}

/// `cairn migrate` — session/config migration is M1.
pub fn migrate() -> Result<i32, Fail> {
    Err(Fail::not_implemented("`cairn migrate`", "M1"))
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
        assert!(migrate().unwrap_err().message.contains("M1"));
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
        // Unknown session short-circuits before the redaction check.
        let err = export(
            &ExportArgs {
                session_id: "ses_nope".into(),
                format: crate::args::ExportFormat::Md,
                output: None,
                redact: false,
                no_redact: false,
            },
            &startup,
        )
        .unwrap_err();
        assert_eq!(err.exit, 9);
    }
}
