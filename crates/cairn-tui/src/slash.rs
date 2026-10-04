//! The slash-command registry — SPEC §10.3, REQ-TUI-005.
//!
//! The table in SPEC §10.3 is normative: every row MUST be registered exactly
//! once, and the test `tests::table_and_registry_match_1_to_1` (T-CLI-003)
//! transcribes that table and fails if the two ever drift apart.
//!
//! Handlers land with M3; until then this is the data half of the contract,
//! which is what the headless path (`--help`-style listings, tab completion and
//! the `Unknown command` suggestion of REQ-TUI-004) already depends on.

/// Bit flags for the mode columns of the SPEC §10.3 table.
pub const MODE_PLAN: u8 = 1 << 0;
pub const MODE_BUILD: u8 = 1 << 1;
pub const MODE_AUTO: u8 = 1 << 2;
pub const MODE_UNSAFE: u8 = 1 << 3;
/// All four operating modes (§7.6).
pub const MODE_ALL: u8 = MODE_PLAN | MODE_BUILD | MODE_AUTO | MODE_UNSAFE;

/// One row of the SPEC §10.3 slash-command table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    /// Name **without** the leading `/`.
    pub name: &'static str,
    /// Argument placeholder exactly as the table prints it (`""` when none).
    pub args: &'static str,
    /// Behavior text exactly as the table prints it.
    pub behavior: &'static str,
    /// Modes in which the command is available (columns Plan/Build/Auto/Unsafe).
    pub modes: u8,
    /// Headless behavior exactly as the table prints it (`""` when none).
    pub headless: &'static str,
}

impl SlashCommand {
    /// The full invocation as the user types it.
    #[must_use]
    pub fn invocation(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.args)
        }
    }

    /// Whether the command is available in `modes`.
    #[must_use]
    pub fn available_in(&self, modes: u8) -> bool {
        self.modes & modes != 0
    }
}

/// The authoritative registry — 1:1 with SPEC §10.3 (REQ-TUI-005).
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        args: "[topic]",
        behavior: "overlay listing commands & keys",
        modes: MODE_ALL,
        headless: "prints to stdout, exit 0",
    },
    SlashCommand {
        name: "mode",
        args: "<plan|build|auto|auto-unsafe>",
        behavior: "switch mode (§7.6)",
        modes: MODE_ALL,
        headless: "--mode only",
    },
    SlashCommand {
        name: "plan",
        args: "[goal text]",
        behavior: "shorthand: set mode plan + submit",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "auto",
        args: "",
        behavior: "shorthand: mode auto (shows guardrail dialog)",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "model",
        args: "[provider/model]",
        behavior: "pick/list models; changing mid-session keeps history",
        modes: MODE_ALL,
        headless: "--model",
    },
    SlashCommand {
        name: "compact",
        args: "[instructions]",
        behavior: "force compaction §5.6 with optional extra focus",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "clear",
        args: "",
        behavior: "clear transcript (session kept on disk, new session fork)",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "undo",
        args: "[n|last|id] [--hard]",
        behavior: "§9.8 restore",
        modes: MODE_ALL,
        headless: "✅ (exit 0)",
    },
    SlashCommand {
        name: "redo",
        args: "[n]",
        behavior: "re-apply undone",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "diff",
        args: "[id] [file]",
        behavior: "open diff viewer",
        modes: MODE_ALL,
        headless: "prints unified diff",
    },
    SlashCommand {
        name: "commit",
        args: "[message]",
        behavior: "runs `git_commit` (permission-gated)",
        modes: MODE_BUILD | MODE_AUTO | MODE_UNSAFE,
        headless: "✅",
    },
    SlashCommand {
        name: "add",
        args: "<glob…>",
        behavior: "add to explicit include set (overcomes `.cairnignore`) for session",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "drop",
        args: "<glob|@file…>",
        behavior: "unpin files / remove from include set",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "init",
        args: "[--global]",
        behavior: "create `AGENTS.md` skeleton, `.cairnignore`, `.cairn/config.toml`, `.gitignore` entries",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "resume",
        args: "[id]",
        behavior: "session picker → resume",
        modes: MODE_ALL,
        headless: "`resume` subcommand",
    },
    SlashCommand {
        name: "sessions",
        args: "[filter]",
        behavior: "session list overlay",
        modes: MODE_ALL,
        headless: "`sessions` subcommand",
    },
    SlashCommand {
        name: "cost",
        args: "[--session|--all]",
        behavior: "tokens/cost breakdown by model and cache",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "config",
        args: "get|set|unset|list|validate <key> [value]",
        behavior: "config ops (§11.4)",
        modes: MODE_ALL,
        headless: "`config` subcommand",
    },
    SlashCommand {
        name: "theme",
        args: "[name]",
        behavior: "switch theme (`cairn-dark`, `cairn-light`, `high-contrast`)",
        modes: MODE_ALL,
        headless: "n/a",
    },
    SlashCommand {
        name: "export",
        args: "[fmt: md|json|html] [path]",
        behavior: "export session",
        modes: MODE_ALL,
        headless: "`export` subcommand",
    },
    SlashCommand {
        name: "quit",
        args: "",
        behavior: "exit (jobs prompt if any running)",
        modes: MODE_ALL,
        headless: "n/a",
    },
    SlashCommand {
        name: "permissions",
        args: "[show|add|rm]",
        behavior: "view/edit rules",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "task",
        args: "<description>",
        behavior: "spawn subagent (same as tool)",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "checkpoint",
        args: "[label]",
        behavior: "create manual checkpoint",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "checkpoints",
        args: "",
        behavior: "list checkpoints (§9.8)",
        modes: MODE_ALL,
        headless: "✅",
    },
    SlashCommand {
        name: "doctor",
        args: "",
        behavior: "run diagnostics overlay",
        modes: MODE_ALL,
        headless: "`doctor` subcommand",
    },
    SlashCommand {
        name: "trace",
        args: "on|off|view",
        behavior: "toggle `--trace` recording (§12.1)",
        modes: MODE_ALL,
        headless: "`--trace`",
    },
];

/// Number of rows in SPEC §10.3.
pub const SLASH_COMMAND_COUNT: usize = 27;

/// Does this input start a slash command?
#[must_use]
pub fn is_slash(input: &str) -> bool {
    input.starts_with('/')
}

/// Look a command up, tolerating (or requiring) the leading `/`.
#[must_use]
pub fn lookup(input: &str) -> Option<&'static SlashCommand> {
    let name = input.strip_prefix('/').unwrap_or(input);
    let name = name.split_whitespace().next().unwrap_or(name);
    SLASH_COMMANDS.iter().find(|c| c.name == name)
}

/// Names in registry order, without the leading `/`.
#[must_use]
pub fn names() -> Vec<&'static str> {
    SLASH_COMMANDS.iter().map(|c| c.name).collect()
}

/// Levenshtein edit distance (REQ-TUI-004).
#[must_use]
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// REQ-TUI-004: the closest known command within 2 edits, ignoring the `/`.
#[must_use]
pub fn suggest(input: &str) -> Option<&'static str> {
    let typed = input.strip_prefix('/').unwrap_or(input);
    let typed = typed.split_whitespace().next().unwrap_or(typed);
    if typed.is_empty() {
        return None;
    }
    SLASH_COMMANDS
        .iter()
        .map(|c| (c.name, levenshtein(typed, c.name)))
        .filter(|&(_, d)| d <= 2)
        .min_by_key(|&(_, d)| d)
        .map(|(name, _)| name)
}

/// REQ-TUI-004: the exact inline error text for an unknown command.
///
/// `Unknown command '/fooo'. Did you mean '/foo'?`
#[must_use]
pub fn unknown_message(input: &str) -> String {
    let shown = if input.starts_with('/') {
        input.to_string()
    } else {
        format!("/{input}")
    };
    match suggest(input) {
        Some(name) => format!("Unknown command '{shown}'. Did you mean '/{name}'?"),
        None => format!("Unknown command '{shown}'."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-CLI-003 — the SPEC §10.3 table and the registry are 1:1 (REQ-TUI-005).
    ///
    /// The array below is a transcription of the spec table; keeping it in the
    /// test rather than in the module is deliberate: it must be written out a
    /// second time for the drift check to mean anything.
    #[test]
    fn table_and_registry_match_1_to_1() {
        // (name, args, behavior, modes, headless)
        let table: &[(&str, &str, &str, u8, &str)] = &[
            ("help", "[topic]", "overlay listing commands & keys", MODE_ALL, "prints to stdout, exit 0"),
            ("mode", "<plan|build|auto|auto-unsafe>", "switch mode (§7.6)", MODE_ALL, "--mode only"),
            ("plan", "[goal text]", "shorthand: set mode plan + submit", MODE_ALL, "✅"),
            ("auto", "", "shorthand: mode auto (shows guardrail dialog)", MODE_ALL, "✅"),
            ("model", "[provider/model]", "pick/list models; changing mid-session keeps history", MODE_ALL, "--model"),
            ("compact", "[instructions]", "force compaction §5.6 with optional extra focus", MODE_ALL, "✅"),
            ("clear", "", "clear transcript (session kept on disk, new session fork)", MODE_ALL, "✅"),
            ("undo", "[n|last|id] [--hard]", "§9.8 restore", MODE_ALL, "✅ (exit 0)"),
            ("redo", "[n]", "re-apply undone", MODE_ALL, "✅"),
            ("diff", "[id] [file]", "open diff viewer", MODE_ALL, "prints unified diff"),
            ("commit", "[message]", "runs `git_commit` (permission-gated)", MODE_BUILD | MODE_AUTO | MODE_UNSAFE, "✅"),
            ("add", "<glob…>", "add to explicit include set (overcomes `.cairnignore`) for session", MODE_ALL, "✅"),
            ("drop", "<glob|@file…>", "unpin files / remove from include set", MODE_ALL, "✅"),
            ("init", "[--global]", "create `AGENTS.md` skeleton, `.cairnignore`, `.cairn/config.toml`, `.gitignore` entries", MODE_ALL, "✅"),
            ("resume", "[id]", "session picker → resume", MODE_ALL, "`resume` subcommand"),
            ("sessions", "[filter]", "session list overlay", MODE_ALL, "`sessions` subcommand"),
            ("cost", "[--session|--all]", "tokens/cost breakdown by model and cache", MODE_ALL, "✅"),
            ("config", "get|set|unset|list|validate <key> [value]", "config ops (§11.4)", MODE_ALL, "`config` subcommand"),
            ("theme", "[name]", "switch theme (`cairn-dark`, `cairn-light`, `high-contrast`)", MODE_ALL, "n/a"),
            ("export", "[fmt: md|json|html] [path]", "export session", MODE_ALL, "`export` subcommand"),
            ("quit", "", "exit (jobs prompt if any running)", MODE_ALL, "n/a"),
            ("permissions", "[show|add|rm]", "view/edit rules", MODE_ALL, "✅"),
            ("task", "<description>", "spawn subagent (same as tool)", MODE_ALL, "✅"),
            ("checkpoint", "[label]", "create manual checkpoint", MODE_ALL, "✅"),
            ("checkpoints", "", "list checkpoints (§9.8)", MODE_ALL, "✅"),
            ("doctor", "", "run diagnostics overlay", MODE_ALL, "`doctor` subcommand"),
            ("trace", "on|off|view", "toggle `--trace` recording (§12.1)", MODE_ALL, "`--trace`"),
        ];

        assert_eq!(
            table.len(),
            SLASH_COMMAND_COUNT,
            "the §10.3 table has {SLASH_COMMAND_COUNT} rows"
        );
        assert_eq!(
            SLASH_COMMANDS.len(),
            table.len(),
            "registry size drifted from the spec table"
        );

        for (i, (name, args, behavior, modes, headless)) in table.iter().enumerate() {
            let got = &SLASH_COMMANDS[i];
            assert_eq!(got.name, *name, "row {i} name");
            assert_eq!(got.args, *args, "row {i} ({name}) args");
            assert_eq!(got.behavior, *behavior, "row {i} ({name}) behavior");
            assert_eq!(got.modes, *modes, "row {i} ({name}) mode columns");
            assert_eq!(got.headless, *headless, "row {i} ({name}) headless column");
        }
    }

    /// Every command is registered exactly once (REQ-TUI-005).
    #[test]
    fn every_command_is_registered_exactly_once() {
        assert_eq!(SLASH_COMMANDS.len(), SLASH_COMMAND_COUNT);
        for (i, cmd) in SLASH_COMMANDS.iter().enumerate() {
            let first = SLASH_COMMANDS
                .iter()
                .position(|c| c.name == cmd.name)
                .unwrap();
            assert_eq!(first, i, "/{} registered more than once", cmd.name);
            assert!(!cmd.name.contains(' '), "names are single tokens");
            assert!(!cmd.name.starts_with('/'), "names carry no leading slash");
        }
        assert_eq!(names().len(), SLASH_COMMAND_COUNT);
    }

    /// `/commit` is the only command unavailable in plan mode (§10.3 row 11).
    #[test]
    fn mode_columns_match_the_table() {
        let unavailable: Vec<&str> = SLASH_COMMANDS
            .iter()
            .filter(|c| !c.available_in(MODE_PLAN))
            .map(|c| c.name)
            .collect();
        assert_eq!(unavailable, ["commit"]);
        for cmd in SLASH_COMMANDS {
            assert!(cmd.available_in(MODE_BUILD), "/{}", cmd.name);
            assert!(cmd.available_in(MODE_AUTO), "/{}", cmd.name);
            assert!(cmd.available_in(MODE_UNSAFE), "/{}", cmd.name);
        }
    }

    /// Lookup tolerates the leading slash and any trailing arguments.
    #[test]
    fn lookup_accepts_typed_forms() {
        assert_eq!(lookup("/help").map(|c| c.name), Some("help"));
        assert_eq!(lookup("help").map(|c| c.name), Some("help"));
        assert_eq!(lookup("/help topic").map(|c| c.name), Some("help"));
        assert_eq!(lookup("/unasked"), None);
        assert!(is_slash("/undo"));
        assert!(!is_slash("undo"));
    }

    /// REQ-TUI-004 — suggestion text, within 2 edits.
    #[test]
    fn unknown_command_message_and_suggestion() {
        assert_eq!(suggest("/und"), Some("undo"));
        assert_eq!(
            unknown_message("/und"),
            "Unknown command '/und'. Did you mean '/undo'?"
        );
        assert_eq!(suggest("/mdoel"), Some("model"));
        assert_eq!(suggest("/chekpoints"), Some("checkpoints"));
        assert_eq!(suggest("/undo"), Some("undo"));
        // Nothing within 2 edits → the message degrades, never invents one.
        assert_eq!(suggest("/fooo"), None);
        assert_eq!(suggest("/zzzzzzz"), None);
        assert_eq!(unknown_message("/fooo"), "Unknown command '/fooo'.");
        assert_eq!(
            unknown_message("fooo"),
            "Unknown command '/fooo'.",
            "a bare word is normalized with the slash"
        );
        // The message never lets the raw text through as a model message.
        assert!(unknown_message("/fooo").starts_with("Unknown command "));
    }

    #[test]
    fn levenshtein_matches_reference_values() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("foo", "foo"), 0);
        assert_eq!(levenshtein("foo", "fooo"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("undo", "redo"), 2);
    }

    #[test]
    fn invocations_render_as_shown_in_the_table() {
        assert_eq!(SLASH_COMMANDS[0].invocation(), "/help [topic]");
        assert_eq!(lookup("/auto").unwrap().invocation(), "/auto");
    }
}
