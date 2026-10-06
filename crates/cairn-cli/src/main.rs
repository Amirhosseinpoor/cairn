//! `cairn` — entry point: parse, dispatch, map failures to §11.2 exit codes.

/// `println!` that survives `| head` (a broken pipe is not a crash).
macro_rules! say {
    ($($arg:tt)*) => {
        $crate::output::say(std::format_args!($($arg)*))
    };
}

/// `eprintln!` that survives a closed stderr.
macro_rules! yell {
    ($($arg:tt)*) => {
        $crate::output::yell(std::format_args!($($arg)*))
    };
}

mod args;
mod commands;
mod log;
mod output;
mod provide;

use clap::error::ErrorKind;
use clap::Parser;

fn main() {
    let cli = match args::Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => clap_failure(&e),
    };
    let quiet = cli.quiet || quiet_in_argv();
    match commands::dispatch(&cli) {
        Ok(code) => std::process::exit(code),
        Err(fail) => {
            fail.report(quiet);
            std::process::exit(fail.exit);
        }
    }
}

/// clap's own outcomes: help/version exit 0, anything else is `E-CLI-USAGE`
/// with exit 2 (REQ-CLI-002: every non-zero exit names a stable code).
fn clap_failure(e: &clap::Error) -> ! {
    match e.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = e.print();
            std::process::exit(0);
        }
        kind => {
            let quiet = quiet_in_argv();
            let rendered = e.render().to_string();
            let first = rendered
                .lines()
                .next()
                .unwrap_or("invalid command line")
                .trim()
                .to_string();
            eprintln!(
                "error: {}: {first} ({kind:?})",
                cairn_core::error::codes::CLI_USAGE
            );
            if !quiet {
                eprintln!("hint: run `cairn --help` for the command tree");
                eprint!("{rendered}");
            }
            std::process::exit(2);
        }
    }
}

/// `--quiet` may appear anywhere, including after an unparsable argument.
fn quiet_in_argv() -> bool {
    quiet_in_args(std::env::args())
}

fn quiet_in_args<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| match a.as_str() {
        "-q" | "--quiet" => true,
        other => other.starts_with('-') && !other.starts_with("--") && other.contains('q'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_is_found_in_any_position() {
        let yes = |v: &[&str]| quiet_in_args(v.iter().map(ToString::to_string));
        assert!(yes(&["cairn", "-q", "run"]));
        assert!(yes(&["cairn", "run", "--quiet"]));
        assert!(yes(&["cairn", "-vq", "config", "list"]));
        assert!(!yes(&["cairn", "run", "-p", "query"]));
        assert!(!yes(&["cairn", "--quiet-file", "x"]));
    }
}
