//! §6.4.5: does this command need a terminal?

/// Programs that are interactive by nature.
const EDITORS_AND_PAGERS: [&str; 11] = [
    "vim", "nvim", "vi", "nano", "emacs", "less", "more", "top", "htop", "pico", "watch",
];

/// Programs that run until stopped or wait on a person.
const LONG_RUNNING: [&str; 7] = [
    "webpack",
    "vite",
    "nc",
    "ncat",
    "ssh",
    "cargo watch",
    "npm run dev",
];

const SHORT_LIVED: [&str; 8] = [
    "ls",
    "cat",
    "grep",
    "rg",
    "make test",
    "pytest",
    "npm test",
    "cargo build",
];

/// The score §6.4.5 defines; 60 or more is interactive.
#[must_use]
pub fn score(command: &str) -> i32 {
    let trimmed = command.trim_start();
    // The program is the first word after any `VAR=value` prefixes.
    let mut words = trimmed
        .split_whitespace()
        .skip_while(|w| w.contains('=') && !w.starts_with('-'));
    let program = words.next().unwrap_or("");
    let rest: Vec<&str> = words.collect();
    let program = program.rsplit('/').next().unwrap_or(program);
    let two = format!("{program} {}", rest.first().copied().unwrap_or(""));
    let three = format!("{two} {}", rest.get(1).copied().unwrap_or(""));
    let mut points = 0;
    if EDITORS_AND_PAGERS.contains(&program)
        || (program == "git"
            && ((rest.first() == Some(&"rebase") && rest.contains(&"-i"))
                || (rest.first() == Some(&"add") && rest.contains(&"-p"))))
    {
        points += 100;
    }
    if trimmed
        .split_whitespace()
        .any(|w| matches!(w, "-i" | "--interactive" | "--edit" | "--wait"))
    {
        points += 40;
    }
    let long = LONG_RUNNING.contains(&program) && !(program == "ssh" && rest.contains(&"-N"))
        || LONG_RUNNING.contains(&two.trim())
        || LONG_RUNNING.contains(&three.trim())
        || trimmed.contains("python -m http.server")
        || trimmed.contains("python3 -m http.server")
        || (program == "tail" && rest.contains(&"-f"));
    if long {
        points += 80;
    }
    if SHORT_LIVED.contains(&program)
        || SHORT_LIVED.contains(&two.trim())
        || SHORT_LIVED.contains(&three.trim())
    {
        points -= 50;
    }
    points
}

/// `score >= 60`.
#[must_use]
pub fn is_interactive(command: &str) -> bool {
    score(command) >= 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editors_pagers_and_watchers_are_interactive() {
        for command in [
            "vim src/main.rs",
            "less log.txt",
            "git rebase -i HEAD~3",
            "git add -p",
            "tail -f app.log",
            "cargo watch -x test",
            "npm run dev",
            "python -m http.server 8000",
            "ssh host",
            "FOO=1 htop",
        ] {
            assert!(is_interactive(command), "{command}: {}", score(command));
        }
    }

    #[test]
    fn ordinary_commands_are_not() {
        for command in [
            "ls -la",
            "cat Cargo.toml",
            "cargo build --release",
            "cargo test",
            "pytest -q",
            "npm test",
            "ssh -N -L 8080:localhost:80 host",
            "git status",
            "rg todo",
        ] {
            assert!(!is_interactive(command), "{command}: {}", score(command));
        }
    }

    #[test]
    fn a_flag_alone_is_not_enough() {
        // +40 for `--interactive`, below the 60 threshold.
        assert!(!is_interactive("some-tool --interactive"));
        assert_eq!(score("some-tool --interactive"), 40);
    }
}
