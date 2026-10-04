//! `cairn completions <shell>` (SPEC §11.1, T-CLI-023).

use crate::args::{Cli, CompletionsArgs};
use crate::output::Fail;
use clap::CommandFactory;

pub fn run(args: &CompletionsArgs) -> Result<i32, Fail> {
    use std::io::Write;

    let mut cmd = Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(args.shell, &mut cmd, "cairn", &mut buf);
    let _ = std::io::stdout().write_all(&buf);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// T-CLI-023: every supported shell has a generator and produces output.
    #[test]
    fn every_shell_generates() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
        ] {
            let mut cmd = Cli::command();
            let mut buf: Vec<u8> = Vec::new();
            clap_complete::generate(shell, &mut cmd, "cairn", &mut buf);
            assert!(!buf.is_empty(), "{shell:?} produced no completion script");
            let text = String::from_utf8_lossy(&buf);
            assert!(
                text.contains("cairn"),
                "{shell:?} script never mentions the binary"
            );
        }
    }
}
