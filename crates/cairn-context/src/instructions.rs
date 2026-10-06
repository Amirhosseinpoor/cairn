//! Project instructions (SPEC §5.7): `AGENTS.md` files found around the work,
//! merged into one block for the system prompt.
//!
//! Load order, lowest precedence first:
//!
//! 1. `<config home>/AGENTS.md` — the user's own, for every project
//! 2. `<workspace>/AGENTS.md` (or `CAIRN.md`)
//! 3. `<workspace>/.cairn/AGENTS.md` — the team's override
//! 4. `AGENTS.md` (or `CAIRN.md`) in each directory between the workspace root
//!    and the files being worked on, nearest last
//!
//! Sections with the same `##` heading are joined, each body under an
//! `<!-- source: … -->` comment, separated by `---`. A line of the form
//! `Key: value` that a later file gives a different value from an earlier one
//! is kept, with a note that it overrides — the model is told, nothing is
//! silently dropped (§5.7: conflicts are not resolved automatically).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cairn_core::error::codes;
use cairn_core::redact::Redactor;
use sha2::{Digest, Sha256};

/// Where a piece of instruction text came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    /// How it is named in attribution comments.
    pub label: String,
}

/// Something worth telling the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
}

/// The result of reading every instruction file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loaded {
    /// The merged text, redacted; empty when there are no instructions.
    pub text: String,
    /// The files that contributed, lowest precedence first.
    pub sources: Vec<Source>,
    pub warnings: Vec<Warning>,
}

/// Where to look.
#[derive(Debug, Clone, Copy)]
pub struct Locations<'a> {
    pub workspace: &'a Path,
    /// The user's config directory (`~/.config/cairn`).
    pub config_home: Option<&'a Path>,
    /// Directories of the files in play, for the narrowest-scope files.
    pub active_dirs: &'a [PathBuf],
}

const PRECEDENCE_NOTE: &str = "> PRECEDENCE: this overrides earlier instructions.";

struct File {
    label: String,
    text: String,
}

/// The instruction file of a directory: `AGENTS.md`, else `CAIRN.md`. Both
/// is `W-CTX-ALIAS`, and only `AGENTS.md` counts.
fn in_dir(dir: &Path, warnings: &mut Vec<Warning>) -> Option<PathBuf> {
    let agents = dir.join("AGENTS.md");
    let alias = dir.join("CAIRN.md");
    match (agents.is_file(), alias.is_file()) {
        (true, true) => {
            warnings.push(Warning {
                code: codes::CTX_ALIAS,
                message: format!(
                    "{} has both AGENTS.md and CAIRN.md; only AGENTS.md is used",
                    dir.display()
                ),
            });
            Some(agents)
        }
        (true, false) => Some(agents),
        (false, true) => Some(alias),
        (false, false) => None,
    }
}

fn label_for(path: &Path, loc: &Locations<'_>) -> String {
    if let Ok(rel) = path.strip_prefix(loc.workspace) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let Some(home) = loc.config_home {
        if let Ok(rel) = path.strip_prefix(home) {
            return format!(
                "~/.config/cairn/{}",
                rel.to_string_lossy().replace('\\', "/")
            );
        }
    }
    path.to_string_lossy().replace('\\', "/")
}

/// Every directory from the workspace root (exclusive) down to `dir`.
fn below_root(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let Ok(rel) = dir.strip_prefix(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut at = root.to_path_buf();
    for part in rel.components() {
        at.push(part.as_os_str());
        out.push(at.clone());
    }
    out
}

/// Read and merge the instruction files.
#[must_use]
pub fn load(loc: &Locations<'_>, redactor: &Redactor) -> Loaded {
    let mut warnings = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(config) = loc.config_home {
        let global = config.join("AGENTS.md");
        if global.is_file() {
            paths.push(global);
        }
    }
    if let Some(root) = in_dir(loc.workspace, &mut warnings) {
        paths.push(root);
    }
    let project = loc.workspace.join(".cairn").join("AGENTS.md");
    if project.is_file() {
        paths.push(project);
    }
    let mut nested: Vec<PathBuf> = loc
        .active_dirs
        .iter()
        .flat_map(|d| below_root(loc.workspace, d))
        .collect();
    nested.sort_by_key(|d| d.components().count());
    nested.dedup();
    for dir in nested {
        if let Some(found) = in_dir(&dir, &mut warnings) {
            paths.push(found);
        }
    }

    let mut files = Vec::new();
    let mut sources = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let label = label_for(&path, loc);
        sources.push(Source {
            path: path.clone(),
            label: label.clone(),
        });
        files.push(File { label, text });
    }
    let merged = merge(&files);
    Loaded {
        text: redactor.redact(&merged),
        sources,
        warnings,
    }
}

/// Split a file into its preamble and `##` sections, ignoring headings inside
/// code fences.
fn sections(text: &str) -> (String, Vec<(String, String)>) {
    let mut preamble = String::new();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fenced = !fenced;
        }
        if !fenced {
            if let Some(heading) = line.strip_prefix("## ") {
                out.push((heading.trim().to_string(), String::new()));
                continue;
            }
        }
        let target = out.last_mut().map_or(&mut preamble, |(_, body)| body);
        target.push_str(line);
        target.push('\n');
    }
    (preamble, out)
}

/// `Key: value` with a short, plain key.
fn directive(line: &str) -> Option<(String, String)> {
    let line = line.trim().trim_start_matches(['-', '*']).trim();
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    let plain = !key.is_empty()
        && key.len() <= 40
        && key
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '-'));
    (plain && !value.trim().is_empty())
        .then(|| (key.to_ascii_lowercase(), value.trim().to_string()))
}

fn merge(files: &[File]) -> String {
    if files.is_empty() {
        return String::new();
    }
    // Directives seen so far, by key, with the value they were given.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut preambles: Vec<String> = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut bodies: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in files {
        let (preamble, parts) = sections(&file.text);
        let mark = |body: &str, seen: &mut BTreeMap<String, String>| -> String {
            let mut out = String::new();
            for line in body.trim_end().lines() {
                if let Some((key, value)) = directive(line) {
                    if seen.get(&key).is_some_and(|earlier| *earlier != value) {
                        out.push_str(PRECEDENCE_NOTE);
                        out.push('\n');
                    }
                    seen.insert(key, value);
                }
                out.push_str(line);
                out.push('\n');
            }
            out
        };
        let attributed =
            |marked: String| format!("<!-- source: {} -->\n{}", file.label, marked.trim_end());
        if !preamble.trim().is_empty() {
            preambles.push(attributed(mark(&preamble, &mut seen)));
        }
        for (heading, body) in parts {
            if body.trim().is_empty() {
                continue;
            }
            if !headings.contains(&heading) {
                headings.push(heading.clone());
            }
            bodies
                .entry(heading)
                .or_default()
                .push(attributed(mark(&body, &mut seen)));
        }
    }

    let mut blocks: Vec<String> = Vec::new();
    if !preambles.is_empty() {
        blocks.push(preambles.join("\n\n---\n\n"));
    }
    for heading in headings {
        if let Some(parts) = bodies.get(&heading) {
            blocks.push(format!("## {heading}\n{}", parts.join("\n\n---\n\n")));
        }
    }
    blocks.join("\n\n")
}

/// Notices when instruction files change between turns (REQ-CTX-017).
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    hash: Option<[u8; 32]>,
    rev: u32,
}

impl Tracker {
    /// Record `loaded` as the current text. Returns the revision (starting at
    /// 1) and whether it is new this call.
    ///
    /// The revision goes into the next message as `instructions_rev`.
    pub fn observe(&mut self, loaded: &Loaded) -> (u32, bool) {
        let digest: [u8; 32] = Sha256::digest(loaded.text.as_bytes()).into();
        if self.hash == Some(digest) {
            return (self.rev, false);
        }
        self.hash = Some(digest);
        self.rev += 1;
        (self.rev, true)
    }

    #[must_use]
    pub const fn rev(&self) -> u32 {
        self.rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        config: PathBuf,
    }

    fn tree() -> Tree {
        let tmp = tempfile::tempdir().expect("tmp");
        let base = tmp.path().canonicalize().expect("canonical");
        let ws = base.join("ws");
        let config = base.join("config");
        std::fs::create_dir_all(ws.join("src/deep")).expect("dirs");
        std::fs::create_dir_all(ws.join(".cairn")).expect("dirs");
        std::fs::create_dir_all(&config).expect("dirs");
        Tree {
            _tmp: tmp,
            ws,
            config,
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write");
    }

    fn load_with(t: &Tree, active: &[PathBuf]) -> Loaded {
        load(
            &Locations {
                workspace: &t.ws,
                config_home: Some(&t.config),
                active_dirs: active,
            },
            &Redactor::new(),
        )
    }

    /// T-CTX-024.
    #[test]
    fn the_four_levels_merge_in_precedence_order_with_attribution() {
        let t = tree();
        write(&t.config.join("AGENTS.md"), "## Style\nGlobal style.\n");
        write(
            &t.ws.join("AGENTS.md"),
            "## Style\nRoot style.\n\n## Tests\nRun cargo test.\n",
        );
        write(&t.ws.join(".cairn/AGENTS.md"), "## Style\nTeam style.\n");
        write(&t.ws.join("src/AGENTS.md"), "## Style\nSrc style.\n");
        let loaded = load_with(&t, &[t.ws.join("src/deep")]);
        let labels: Vec<&str> = loaded.sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "~/.config/cairn/AGENTS.md",
                "AGENTS.md",
                ".cairn/AGENTS.md",
                "src/AGENTS.md"
            ]
        );
        let want = "\
## Style
<!-- source: ~/.config/cairn/AGENTS.md -->
Global style.

---

<!-- source: AGENTS.md -->
Root style.

---

<!-- source: .cairn/AGENTS.md -->
Team style.

---

<!-- source: src/AGENTS.md -->
Src style.

## Tests
<!-- source: AGENTS.md -->
Run cargo test.";
        assert_eq!(loaded.text, want);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn nested_directories_load_root_to_leaf() {
        let t = tree();
        write(&t.ws.join("src/AGENTS.md"), "## Rules\nsrc\n");
        write(&t.ws.join("src/deep/AGENTS.md"), "## Rules\ndeep\n");
        let loaded = load_with(&t, &[t.ws.join("src/deep")]);
        let src = loaded.text.find("\nsrc").expect("src");
        let deep = loaded.text.find("\ndeep").expect("deep");
        assert!(src < deep, "{}", loaded.text);
        // Nothing active, nothing nested.
        let none = load_with(&t, &[]);
        assert!(none.text.is_empty());
    }

    /// T-CTX-023 / OQ-01.
    #[test]
    fn both_agents_and_cairn_files_load_only_agents_and_warn() {
        let t = tree();
        write(&t.ws.join("AGENTS.md"), "## A\nfrom agents\n");
        write(&t.ws.join("CAIRN.md"), "## A\nfrom cairn\n");
        let loaded = load_with(&t, &[]);
        assert!(loaded.text.contains("from agents"));
        assert!(!loaded.text.contains("from cairn"));
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(loaded.warnings[0].code, "W-CTX-ALIAS");
        // CAIRN.md alone is used.
        std::fs::remove_file(t.ws.join("AGENTS.md")).expect("rm");
        let loaded = load_with(&t, &[]);
        assert!(loaded.text.contains("from cairn"));
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn missing_and_empty_files_are_fine() {
        let t = tree();
        assert_eq!(load_with(&t, &[]), Loaded::default());
        write(&t.ws.join("AGENTS.md"), "");
        let loaded = load_with(&t, &[]);
        assert!(loaded.text.is_empty());
        assert_eq!(loaded.sources.len(), 1);
    }

    #[test]
    fn text_before_the_first_heading_is_kept_and_attributed() {
        let t = tree();
        write(&t.ws.join("AGENTS.md"), "Be brief.\n\n## Tests\nrun them\n");
        let loaded = load_with(&t, &[]);
        assert!(
            loaded
                .text
                .starts_with("<!-- source: AGENTS.md -->\nBe brief."),
            "{}",
            loaded.text
        );
    }

    #[test]
    fn headings_inside_code_fences_are_not_sections() {
        let t = tree();
        write(
            &t.ws.join("AGENTS.md"),
            "## Example\n```\n## not a heading\n```\nafter\n",
        );
        let loaded = load_with(&t, &[]);
        // One section, one body: the fenced line did not start another.
        assert_eq!(
            loaded.text.matches("<!-- source:").count(),
            1,
            "{}",
            loaded.text
        );
        assert!(loaded.text.contains("## not a heading"));
    }

    #[test]
    fn a_directive_a_later_file_changes_is_marked_as_overriding() {
        let t = tree();
        write(
            &t.config.join("AGENTS.md"),
            "## Style\nIndent: 2 spaces\nQuotes: single\n",
        );
        write(
            &t.ws.join("AGENTS.md"),
            "## Style\nIndent: tabs\nQuotes: single\n",
        );
        let loaded = load_with(&t, &[]);
        assert!(
            loaded.text.contains("Indent: 2 spaces"),
            "the earlier one stays"
        );
        assert!(
            loaded
                .text
                .contains("> PRECEDENCE: this overrides earlier instructions.\nIndent: tabs"),
            "{}",
            loaded.text
        );
        assert_eq!(
            loaded.text.matches("PRECEDENCE").count(),
            1,
            "an unchanged value is not a conflict"
        );
    }

    /// REQ-CTX-016 / T-SEC-013.
    #[test]
    fn secrets_in_instructions_are_redacted() {
        let t = tree();
        write(
            &t.ws.join("AGENTS.md"),
            "## Env\nUse key sk-abcdefghijklmnopqrstuvwxyz0123456789ABCD for tests.\n",
        );
        let loaded = load_with(&t, &[]);
        assert!(!loaded.text.contains("sk-abcdefghijkl"), "{}", loaded.text);
        assert!(loaded.text.contains("***REDACTED***"));
    }

    /// T-CTX-019 / REQ-CTX-017.
    #[test]
    fn an_edited_file_is_a_new_revision_and_an_unchanged_one_is_not() {
        let t = tree();
        write(&t.ws.join("AGENTS.md"), "## A\none\n");
        let mut tracker = Tracker::default();
        assert_eq!(tracker.observe(&load_with(&t, &[])), (1, true));
        assert_eq!(tracker.observe(&load_with(&t, &[])), (1, false));
        write(&t.ws.join("AGENTS.md"), "## A\ntwo\n");
        assert_eq!(tracker.observe(&load_with(&t, &[])), (2, true));
        assert_eq!(tracker.rev(), 2);
    }
}
