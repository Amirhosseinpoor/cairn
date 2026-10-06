//! `cairn init [--global]` — scaffold the files Cairn reads (SPEC §11.1,
//! §10.3 `/init`, §5.7, REQ-SAFE-003, §7.3).
//!
//! Idempotent and non-destructive: a file that exists is never rewritten, and
//! `.gitignore` only ever gains lines it lacks. Running it twice changes
//! nothing the second time.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::args::InitArgs;
use crate::commands::Startup;
use crate::output::Fail;

use cairn_core::error::{codes, ExitStatus};

/// What happened to one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Created,
    Kept,
    Updated,
    Skipped,
}

impl Outcome {
    const fn word(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Kept => "kept   ",
            Self::Updated => "updated",
            Self::Skipped => "skipped",
        }
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub path: PathBuf,
    pub outcome: Outcome,
    pub note: Option<String>,
}

/// `.gitignore` lines `init` guarantees (REQ-SAFE-003, §7.3, §4.10 step 5).
/// Each is `(line, key that makes it shareable)`: a user who opts in to
/// sharing permissions or plans keeps those files tracked.
fn gitignore_lines(startup: &Startup) -> Vec<&'static str> {
    let config = &startup.loaded.config;
    let mut lines = vec!["/.cairn/credentials.toml"];
    if !config.security.share_permissions {
        lines.push("/.cairn/permissions.json");
    }
    if !config.plans.shareable {
        lines.push("/.cairn/plans/");
    }
    lines
}

const IGNORE_BANNER: &str = "# Cairn (added by `cairn init`)";

fn write_new(path: &Path, text: &str) -> Result<Outcome, Fail> {
    if path.exists() {
        return Ok(Outcome::Kept);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| write_fail(path, &e))?;
    }
    std::fs::write(path, text).map_err(|e| write_fail(path, &e))?;
    Ok(Outcome::Created)
}

fn write_fail(path: &Path, error: &std::io::Error) -> Fail {
    Fail::new(
        codes::FS_PERM,
        ExitStatus::Generic,
        format!("could not write {}: {error}", path.display()),
        Some("check permissions on the directory".to_string()),
    )
}

/// The test/build commands a repository declares, for `AGENTS.md`.
fn detect_commands(root: &Path) -> Vec<(&'static str, &'static str)> {
    let mut found = Vec::new();
    let has = |name: &str| root.join(name).exists();
    if has("Cargo.toml") {
        found.push(("Rust", "cargo test"));
    }
    if has("package.json") {
        found.push(("Node", "npm test"));
    }
    if has("pyproject.toml") || has("setup.py") {
        found.push(("Python", "pytest"));
    }
    if has("go.mod") {
        found.push(("Go", "go test ./..."));
    }
    if has("pom.xml") {
        found.push(("Java (Maven)", "mvn test"));
    }
    if has("Makefile") {
        found.push(("Make", "make test"));
    }
    found
}

fn agents_skeleton(root: &Path) -> String {
    let name = root.file_name().map_or_else(
        || "this project".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let commands = detect_commands(root);
    let mut text = format!(
        "# {name}\n\n\
         Instructions for Cairn (and other coding agents) working in this repository.\n\n\
         ## Overview\n\n\
         <!-- What this project is, in two or three sentences. -->\n\n\
         ## Build and test\n\n"
    );
    if commands.is_empty() {
        text.push_str("<!-- The commands to build and run the tests. -->\n\n");
    } else {
        for (what, command) in commands {
            let _ = writeln!(text, "- {what}: `{command}`");
        }
        text.push('\n');
    }
    text.push_str(
        "## Conventions\n\n\
         <!-- Code style, naming, review expectations, things never to touch. -->\n",
    );
    text
}

const CAIRNIGNORE: &str = "\
# Paths Cairn should not read or index, in .gitignore syntax.
# Build output and dependencies are already skipped by default.
# Prefix a pattern with ! to bring a hidden file back in.
*.log
*.tmp
";

const PROJECT_CONFIG: &str = "\
# Project settings for Cairn (checked in; user settings live in
# ~/.config/cairn/config.toml). Every key is optional.
schema_version = 1
";

/// Add `lines` to `<root>/.gitignore` when it exists or the workspace is a
/// git repository; report what changed. Never removes or reorders anything.
fn update_gitignore(root: &Path, lines: &[&str]) -> Result<Step, Fail> {
    let path = root.join(".gitignore");
    let existing = std::fs::read_to_string(&path);
    if existing.is_err() && !root.join(".git").exists() {
        return Ok(Step {
            path,
            outcome: Outcome::Skipped,
            note: Some("not a git repository".to_string()),
        });
    }
    let existing = existing.unwrap_or_default();
    let present: Vec<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&&str> = lines
        .iter()
        .filter(|line| !present.contains(*line))
        .collect();
    if missing.is_empty() {
        return Ok(Step {
            path,
            outcome: Outcome::Kept,
            note: None,
        });
    }
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    if !present.contains(&IGNORE_BANNER) {
        text.push_str(IGNORE_BANNER);
        text.push('\n');
    }
    for line in &missing {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(|e| write_fail(&path, &e))?;
    Ok(Step {
        path,
        outcome: if existing.is_empty() {
            Outcome::Created
        } else {
            Outcome::Updated
        },
        note: Some(format!("{} line(s) added", missing.len())),
    })
}

/// Scaffold the workspace (or, with `global`, the user-level directory).
///
/// # Errors
/// `E-FS-PERM` when a file cannot be written.
pub fn scaffold(
    root: &Path,
    global_dir: &Path,
    global: bool,
    startup: &Startup,
) -> Result<Vec<Step>, Fail> {
    let mut steps = Vec::new();
    let mut push = |path: PathBuf, outcome: Outcome, note: Option<String>| {
        steps.push(Step {
            path,
            outcome,
            note,
        });
    };
    if global {
        let agents = global_dir.join("AGENTS.md");
        let skeleton = "# Global instructions\n\n\
            Instructions Cairn applies in every project (SPEC §5.7 level 1).\n\n\
            ## Conventions\n\n<!-- Personal preferences that hold everywhere. -->\n";
        let outcome = write_new(&agents, skeleton)?;
        push(agents, outcome, None);
        return Ok(steps);
    }

    // `CAIRN.md` is the alias for `AGENTS.md` (§5.7): either one satisfies.
    let agents = root.join("AGENTS.md");
    let alias = root.join("CAIRN.md");
    if alias.exists() && !agents.exists() {
        push(
            alias,
            Outcome::Kept,
            Some("alias for AGENTS.md".to_string()),
        );
    } else {
        let outcome = write_new(&agents, &agents_skeleton(root))?;
        push(agents, outcome, None);
    }
    let ignore = root.join(".cairnignore");
    let outcome = write_new(&ignore, CAIRNIGNORE)?;
    push(ignore, outcome, None);
    let config = root.join(".cairn").join("config.toml");
    let outcome = write_new(&config, PROJECT_CONFIG)?;
    push(config, outcome, None);

    let step = update_gitignore(root, &gitignore_lines(startup))?;
    push(step.path, step.outcome, step.note);
    Ok(steps)
}

/// `cairn init [--global]`.
///
/// # Errors
/// `E-FS-PERM` (exit 1) when a file cannot be written.
pub fn run(args: &InitArgs, startup: &Startup) -> Result<i32, Fail> {
    let root = startup.workspace().to_path_buf();
    let global_dir = startup.loaded.paths.config_home.clone();
    let steps = scaffold(&root, &global_dir, args.global, startup)?;
    for step in &steps {
        let shown = step
            .path
            .strip_prefix(&root)
            .unwrap_or(&step.path)
            .display();
        match &step.note {
            Some(note) => say!("{} {shown} ({note})", step.outcome.word()),
            None => say!("{} {shown}", step.outcome.word()),
        }
    }
    if steps
        .iter()
        .all(|s| matches!(s.outcome, Outcome::Kept | Outcome::Skipped))
    {
        say!("Nothing to do: everything `cairn init` writes is already in place.");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_config::{LoadOptions, Paths};
    use std::collections::BTreeMap;

    fn startup_in(dir: &Path) -> Startup {
        let mut loaded = cairn_config::load(&LoadOptions {
            cwd: dir.to_path_buf(),
            env: Some(BTreeMap::new()),
            ..Default::default()
        });
        loaded.paths = Paths {
            config_home: dir.join("home-config"),
            data_home: dir.join("data"),
            state_home: dir.join("state"),
            cache_home: dir.join("cache"),
        };
        Startup {
            loaded,
            quiet: true,
        }
    }

    fn outcomes(steps: &[Step]) -> Vec<(String, Outcome)> {
        steps
            .iter()
            .map(|s| {
                (
                    s.path
                        .file_name()
                        .expect("name")
                        .to_string_lossy()
                        .into_owned(),
                    s.outcome,
                )
            })
            .collect()
    }

    #[test]
    fn a_git_workspace_gets_all_four_files() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir(dir.path().join(".git")).expect("git");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("manifest");
        let startup = startup_in(dir.path());
        let steps = scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("scaffolds");
        assert_eq!(
            outcomes(&steps),
            [
                ("AGENTS.md".to_string(), Outcome::Created),
                (".cairnignore".to_string(), Outcome::Created),
                ("config.toml".to_string(), Outcome::Created),
                (".gitignore".to_string(), Outcome::Created),
            ]
        );
        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).expect("agents");
        assert!(agents.contains("`cargo test`"), "{agents}");
        let ignore = std::fs::read_to_string(dir.path().join(".gitignore")).expect("ignore");
        // T-SEC-014: permissions.json is listed; REQ-SAFE-003 / §7.3 defaults.
        for line in [
            "/.cairn/permissions.json",
            "/.cairn/plans/",
            "/.cairn/credentials.toml",
        ] {
            assert!(
                ignore.lines().any(|l| l == line),
                "{line} missing: {ignore}"
            );
        }
    }

    /// The config it writes is one `cairn` accepts.
    #[test]
    fn the_project_config_validates() {
        let cfg: cairn_config::Config = toml::from_str(PROJECT_CONFIG).expect("valid config");
        assert_eq!(cfg.schema_version, 1);
    }

    /// Idempotence: a second run changes no byte and reports nothing created.
    #[test]
    fn running_twice_changes_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir(dir.path().join(".git")).expect("git");
        let startup = startup_in(dir.path());
        scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("first");
        let snapshot = |name: &str| std::fs::read(dir.path().join(name)).expect(name);
        let before = (
            snapshot("AGENTS.md"),
            snapshot(".gitignore"),
            snapshot(".cairnignore"),
        );
        let steps = scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("second");
        assert!(
            steps.iter().all(|s| s.outcome == Outcome::Kept),
            "{steps:?}"
        );
        assert_eq!(
            before,
            (
                snapshot("AGENTS.md"),
                snapshot(".gitignore"),
                snapshot(".cairnignore")
            )
        );
    }

    /// Nothing a person wrote is ever overwritten.
    #[test]
    fn existing_files_are_never_rewritten() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("AGENTS.md"), "mine").expect("agents");
        std::fs::write(dir.path().join(".cairnignore"), "mine").expect("ignore");
        let startup = startup_in(dir.path());
        scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("runs");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).expect("r"),
            "mine"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".cairnignore")).expect("r"),
            "mine"
        );
    }

    #[test]
    fn a_cairn_md_alias_satisfies_agents_md() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("CAIRN.md"), "alias").expect("alias");
        let startup = startup_in(dir.path());
        scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("runs");
        assert!(
            !dir.path().join("AGENTS.md").exists(),
            "no second instructions file"
        );
    }

    /// An existing `.gitignore` gains only the missing lines, after a banner,
    /// with its own content untouched.
    #[test]
    fn gitignore_is_appended_to_not_rewritten() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join(".gitignore"), "target/\n/.cairn/plans/\n").expect("ignore");
        let startup = startup_in(dir.path());
        scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("runs");
        let text = std::fs::read_to_string(dir.path().join(".gitignore")).expect("ignore");
        assert!(text.starts_with("target/\n/.cairn/plans/\n"), "{text}");
        assert_eq!(
            text.matches("/.cairn/plans/").count(),
            1,
            "no duplicate: {text}"
        );
        assert!(text.contains(IGNORE_BANNER));
        assert!(text.contains("/.cairn/permissions.json"));
    }

    #[test]
    fn a_workspace_that_is_not_a_repository_gets_no_gitignore() {
        let dir = tempfile::tempdir().expect("tmp");
        let startup = startup_in(dir.path());
        let steps = scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("runs");
        assert!(!dir.path().join(".gitignore").exists());
        let last = steps.last().expect("a step");
        assert_eq!(last.outcome, Outcome::Skipped);
    }

    /// Sharing is opt-in: with `plans.shareable` the plans stay tracked.
    #[test]
    fn opting_in_to_sharing_keeps_those_files_tracked() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::create_dir(dir.path().join(".git")).expect("git");
        let mut startup = startup_in(dir.path());
        startup.loaded.config.plans.shareable = true;
        startup.loaded.config.security.share_permissions = true;
        scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            false,
            &startup,
        )
        .expect("runs");
        let text = std::fs::read_to_string(dir.path().join(".gitignore")).expect("ignore");
        assert!(!text.contains("plans"), "{text}");
        assert!(!text.contains("permissions.json"), "{text}");
        assert!(
            text.contains("credentials.toml"),
            "keys are never shareable: {text}"
        );
    }

    #[test]
    fn global_scaffolds_only_the_user_agents_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let startup = startup_in(dir.path());
        let steps = scaffold(
            dir.path(),
            &startup.loaded.paths.config_home,
            true,
            &startup,
        )
        .expect("runs");
        assert_eq!(steps.len(), 1);
        assert!(dir.path().join("home-config").join("AGENTS.md").exists());
        assert!(
            !dir.path().join("AGENTS.md").exists(),
            "the workspace is untouched"
        );
    }
}
