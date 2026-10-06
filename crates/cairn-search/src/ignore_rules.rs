//! Ignore rules with the precedence of SPEC §5.1.
//!
//! One engine answers "is this path excluded?" for everything that looks at
//! the tree — `read_file`, `list_dir`, `glob`, `grep`, the walker — so they
//! cannot disagree (REQ-CTX-002). Sources, lowest to highest:
//!
//! 1. built-in defaults (`target/`, `node_modules/`, …)
//! 2. the global gitignore
//! 3. `.gitignore`, root to deepest
//! 4. `.ignore`
//! 5. `.cairnignore`
//! 6. config `include` then `exclude` globs — config beats files
//!
//! A higher source that has an opinion (ignore *or* `!whitelist`) decides; a
//! source with no matching pattern defers to the one below. Within one
//! source a deeper directory's file overrides a shallower one, as in git.
//! `.git` is excluded unconditionally.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;

const DEFAULTS: [&str; 7] = [
    "target/",
    "node_modules/",
    ".venv/",
    "dist/",
    "build/",
    "__pycache__/",
    ".DS_Store",
];

/// What the caller supplies.
#[derive(Debug, Clone, Default)]
pub struct IgnoreOptions {
    /// The global gitignore file, when there is one.
    pub global_file: Option<PathBuf>,
    /// `discovery.include` globs, in `.gitignore` pattern syntax.
    pub include: Vec<String>,
    /// `discovery.exclude` globs.
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    Gitignore,
    Ignore,
    Cairnignore,
}

impl Source {
    const fn file_name(self) -> &'static str {
        match self {
            Self::Gitignore => ".gitignore",
            Self::Ignore => ".ignore",
            Self::Cairnignore => ".cairnignore",
        }
    }
}

/// The answer from one source: `Some(true)` ignored, `Some(false)` whitelisted.
type Verdict = Option<bool>;

fn verdict<T>(m: &Match<T>) -> Verdict {
    match m {
        Match::None => None,
        Match::Ignore(_) => Some(true),
        Match::Whitelist(_) => Some(false),
    }
}

/// One directory's ignore file of one kind, loaded at most once.
type FileCache = HashMap<(PathBuf, Source), Option<Arc<Gitignore>>>;

/// See the module docs.
pub struct IgnoreEngine {
    root: PathBuf,
    defaults: Gitignore,
    global: Gitignore,
    include: Gitignore,
    exclude: Gitignore,
    cache: Mutex<FileCache>,
}

impl std::fmt::Debug for IgnoreEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IgnoreEngine")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn from_lines(root: &Path, lines: &[String]) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    for line in lines {
        // A pattern that does not parse is dropped, never fatal: a typo in a
        // config glob must not stop a repository from being read.
        let _ = builder.add_line(None, line);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

impl IgnoreEngine {
    /// Build the engine for a workspace `root`.
    #[must_use]
    pub fn new(root: &Path, options: &IgnoreOptions) -> Self {
        let defaults = from_lines(
            root,
            &DEFAULTS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );
        let global = options
            .global_file
            .as_deref()
            .filter(|p| p.is_file())
            .map_or_else(Gitignore::empty, |file| {
                let mut builder = GitignoreBuilder::new(root);
                let _ = builder.add(file);
                builder.build().unwrap_or_else(|_| Gitignore::empty())
            });
        Self {
            root: root.to_path_buf(),
            defaults,
            global,
            include: from_lines(root, &options.include),
            exclude: from_lines(root, &options.exclude),
            cache: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.gitignore`-style file `source` in `dir`, loaded once.
    fn file_in(&self, dir: &Path, source: Source) -> Option<Arc<Gitignore>> {
        let key = (dir.to_path_buf(), source);
        if let Some(found) = self.cache.lock().expect("ignore cache").get(&key) {
            return found.clone();
        }
        let file = dir.join(source.file_name());
        let loaded = file.is_file().then(|| {
            let mut builder = GitignoreBuilder::new(dir);
            let _ = builder.add(&file);
            Arc::new(builder.build().unwrap_or_else(|_| Gitignore::empty()))
        });
        self.cache
            .lock()
            .expect("ignore cache")
            .insert(key, loaded.clone());
        loaded
    }

    /// One source's verdict for `path`: the deepest directory's file that has
    /// an opinion wins.
    fn chain(&self, source: Source, path: &Path, is_dir: bool) -> Verdict {
        let mut dir = path.parent();
        while let Some(current) = dir {
            if !current.starts_with(&self.root) {
                break;
            }
            if let Some(file) = self.file_in(current, source) {
                if let Some(v) = verdict(&file.matched(path, is_dir)) {
                    return Some(v);
                }
            }
            if current == self.root {
                break;
            }
            dir = current.parent();
        }
        None
    }

    /// The §5.1 precedence for one path, ignoring its ancestors.
    fn layered(&self, path: &Path, is_dir: bool) -> Verdict {
        // 6. Config. `exclude` is applied after `include`, so it wins a tie.
        if let Some(v) = verdict(&self.exclude.matched(path, is_dir)) {
            if v {
                return Some(true);
            }
        }
        if let Some(v) = verdict(&self.include.matched(path, is_dir)) {
            if v {
                // An include pattern *matching* means "bring this back".
                return Some(false);
            }
        }
        // 5 → 3.
        for source in [Source::Cairnignore, Source::Ignore, Source::Gitignore] {
            if let Some(v) = self.chain(source, path, is_dir) {
                return Some(v);
            }
        }
        // 2, 1.
        verdict(&self.global.matched(path, is_dir))
            .or_else(|| verdict(&self.defaults.matched(path, is_dir)))
    }

    /// Whether `path` (absolute, normally under the root) is excluded.
    /// A path under an excluded directory is excluded whatever its own rules
    /// say, as in git. Paths outside the root are not this engine's business.
    #[must_use]
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        if rel
            .components()
            .any(|c| matches!(c, Component::Normal(n) if n == ".git"))
        {
            return true;
        }
        let mut ancestor = self.root.clone();
        let parts: Vec<_> = rel.components().collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            ancestor.push(part.as_os_str());
            if self.layered(&ancestor, true) == Some(true) {
                return true;
            }
        }
        self.layered(path, is_dir) == Some(true)
    }

    /// Whether a `.cairnignore` rule (and nothing else) excludes `path`: the
    /// question `git_commit` asks, since `.gitignore` already decides what
    /// git tracks (REQ-TOOL-007).
    #[must_use]
    pub fn cairnignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let mut ancestor = self.root.clone();
        let parts: Vec<_> = rel.components().collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            ancestor.push(part.as_os_str());
            if self.chain(Source::Cairnignore, &ancestor, true) == Some(true) {
                return true;
            }
        }
        self.chain(Source::Cairnignore, path, is_dir) == Some(true)
    }

    /// Whether the user asked for this path by name — a config `include`
    /// match or a `.cairnignore` `!` line — which is what lets a hidden file
    /// through a walk (§5.1 "Hidden files").
    #[must_use]
    pub fn explicitly_included(&self, path: &Path, is_dir: bool) -> bool {
        verdict(&self.include.matched(path, is_dir)) == Some(true)
            || self.chain(Source::Cairnignore, path, is_dir) == Some(false)
    }
}

/// The global gitignore file git itself would read: `core.excludesFile` from
/// `~/.gitconfig` if set, else `$XDG_CONFIG_HOME/git/ignore`, else
/// `~/.config/git/ignore`.
#[must_use]
pub fn default_global_ignore(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(home.join(".gitconfig")) {
        let mut in_core = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_core = line.eq_ignore_ascii_case("[core]");
            } else if in_core {
                if let Some((key, value)) = line.split_once('=') {
                    if key.trim().eq_ignore_ascii_case("excludesfile") {
                        let value = value.trim().trim_matches('"');
                        return match value.strip_prefix("~/") {
                            Some(rest) => home.join(rest),
                            None => PathBuf::from(value),
                        };
                    }
                }
            }
        }
    }
    xdg_config_home
        .map_or_else(|| home.join(".config"), Path::to_path_buf)
        .join("git")
        .join("ignore")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    fn tree(files: &[(&str, &str)]) -> Tree {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().canonicalize().expect("canonical").join("repo");
        std::fs::create_dir_all(&root).expect("root");
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            std::fs::write(full, content).expect("file");
        }
        Tree { _dir: dir, root }
    }

    fn engine(t: &Tree) -> IgnoreEngine {
        IgnoreEngine::new(&t.root, &IgnoreOptions::default())
    }

    fn ignored(e: &IgnoreEngine, t: &Tree, rel: &str) -> bool {
        let full = t.root.join(rel);
        e.is_ignored(&full, rel.ends_with('/') || full.is_dir())
    }

    /// REQ-CTX-001: negation, directory-only, anchoring, `**`.
    #[test]
    fn gitignore_semantics_are_honoured() {
        let t = tree(&[
            (
                ".gitignore",
                "*.log\n!keep.log\n/rootonly.txt\nbuild-out/\ndocs/**/draft.md\n",
            ),
            ("a.log", ""),
            ("keep.log", ""),
            ("rootonly.txt", ""),
            ("sub/rootonly.txt", ""),
            ("build-out/x", ""),
            ("docs/a/b/draft.md", ""),
            ("docs/a/final.md", ""),
            ("src/main.rs", ""),
        ]);
        let e = engine(&t);
        assert!(ignored(&e, &t, "a.log"));
        assert!(!ignored(&e, &t, "keep.log"), "negation");
        assert!(ignored(&e, &t, "rootonly.txt"));
        assert!(
            !ignored(&e, &t, "sub/rootonly.txt"),
            "anchored with a leading slash"
        );
        assert!(
            ignored(&e, &t, "build-out/x"),
            "directory-only pattern ignores its contents"
        );
        assert!(
            ignored(&e, &t, "docs/a/b/draft.md"),
            "** crosses directories"
        );
        assert!(!ignored(&e, &t, "docs/a/final.md"));
        assert!(!ignored(&e, &t, "src/main.rs"));
    }

    #[test]
    fn nested_gitignore_files_apply_to_their_subtree_and_override_the_parent() {
        let t = tree(&[
            (".gitignore", "*.tmp\n"),
            ("pkg/.gitignore", "!special.tmp\n*.gen\n"),
            ("pkg/special.tmp", ""),
            ("pkg/other.tmp", ""),
            ("pkg/a.gen", ""),
            ("top.gen", ""),
        ]);
        let e = engine(&t);
        assert!(
            !ignored(&e, &t, "pkg/special.tmp"),
            "the deeper file re-includes it"
        );
        assert!(ignored(&e, &t, "pkg/other.tmp"));
        assert!(ignored(&e, &t, "pkg/a.gen"));
        assert!(
            !ignored(&e, &t, "top.gen"),
            "pkg/.gitignore does not reach outside pkg"
        );
    }

    /// §5.1 order: `.cairnignore` beats `.ignore` beats `.gitignore`.
    #[test]
    fn higher_sources_override_lower_ones() {
        let t = tree(&[
            (".gitignore", "*.dat\n"),
            (".ignore", "!one.dat\n"),
            (".cairnignore", "one.dat\ntwo.dat\n!two.dat\n"),
            ("one.dat", ""),
            ("two.dat", ""),
            ("three.dat", ""),
        ]);
        let e = engine(&t);
        assert!(
            ignored(&e, &t, "one.dat"),
            ".cairnignore ignores what .ignore re-included"
        );
        assert!(!ignored(&e, &t, "two.dat"), "its own later `!` line wins");
        assert!(ignored(&e, &t, "three.dat"), "falls through to .gitignore");
    }

    #[test]
    fn built_in_defaults_apply_and_lock_files_are_not_ignored() {
        let t = tree(&[
            ("target/debug/x", ""),
            ("node_modules/p/i.js", ""),
            ("src/__pycache__/m.pyc", ""),
            (".DS_Store", ""),
            ("Cargo.lock", ""),
            ("package-lock.json", ""),
        ]);
        let e = engine(&t);
        for p in [
            "target/debug/x",
            "node_modules/p/i.js",
            "src/__pycache__/m.pyc",
            ".DS_Store",
        ] {
            assert!(ignored(&e, &t, p), "{p}");
        }
        assert!(!ignored(&e, &t, "Cargo.lock"));
        assert!(!ignored(&e, &t, "package-lock.json"));
    }

    #[test]
    fn a_repository_can_re_include_a_default() {
        let t = tree(&[(".cairnignore", "!build/\n"), ("build/out.txt", "")]);
        assert!(!ignored(&engine(&t), &t, "build/out.txt"));
    }

    /// Config beats files in both directions.
    #[test]
    fn config_include_and_exclude_beat_the_files() {
        let t = tree(&[
            (".gitignore", "*.gen\n"),
            ("a.gen", ""),
            ("b.rs", ""),
            ("vendor/x.rs", ""),
        ]);
        let e = IgnoreEngine::new(
            &t.root,
            &IgnoreOptions {
                include: vec!["a.gen".into()],
                exclude: vec!["vendor/".into(), "b.rs".into()],
                ..IgnoreOptions::default()
            },
        );
        assert!(
            !ignored(&e, &t, "a.gen"),
            "config include overrides .gitignore"
        );
        assert!(
            ignored(&e, &t, "b.rs"),
            "config exclude overrides nothing-ignored"
        );
        assert!(ignored(&e, &t, "vendor/x.rs"));
    }

    #[test]
    fn exclude_wins_over_include_for_the_same_path() {
        let t = tree(&[("x.rs", "")]);
        let e = IgnoreEngine::new(
            &t.root,
            &IgnoreOptions {
                include: vec!["x.rs".into()],
                exclude: vec!["x.rs".into()],
                ..IgnoreOptions::default()
            },
        );
        assert!(ignored(&e, &t, "x.rs"));
    }

    #[test]
    fn git_is_excluded_unconditionally() {
        let t = tree(&[
            (".git/config", ""),
            ("sub/.git/HEAD", ""),
            (".cairnignore", "!.git/\n"),
        ]);
        let e = IgnoreEngine::new(
            &t.root,
            &IgnoreOptions {
                include: vec![".git/".into()],
                ..IgnoreOptions::default()
            },
        );
        assert!(ignored(&e, &t, ".git/config"));
        assert!(ignored(&e, &t, "sub/.git/HEAD"));
    }

    #[test]
    fn contents_of_an_ignored_directory_stay_ignored_even_if_a_file_rule_matches() {
        let t = tree(&[
            (".gitignore", "out/\n"),
            ("out/keep.txt", ""),
            (".cairnignore", "!out/keep.txt\n"),
        ]);
        // git: you cannot re-include a file whose parent directory is excluded.
        // `.cairnignore` is a higher source, so it *can* un-ignore the
        // directory itself, but a bare file rule does not reach into one.
        let e = engine(&t);
        assert!(ignored(&e, &t, "out/keep.txt"));
    }

    #[test]
    fn the_global_gitignore_is_the_second_lowest_source() {
        let t = tree(&[("a.swp", ""), ("b.swp", ""), (".gitignore", "!b.swp\n")]);
        let global = t.root.parent().expect("parent").join("global-ignore");
        std::fs::write(&global, "*.swp\n").expect("global");
        let e = IgnoreEngine::new(
            &t.root,
            &IgnoreOptions {
                global_file: Some(global),
                ..IgnoreOptions::default()
            },
        );
        assert!(ignored(&e, &t, "a.swp"));
        assert!(
            !ignored(&e, &t, "b.swp"),
            "a repository's own file outranks the global one"
        );
    }

    #[test]
    fn explicit_inclusion_is_what_lets_a_hidden_file_through() {
        let t = tree(&[
            (".cairnignore", "!.github/\n"),
            (".github/ci.yml", ""),
            (".hidden", ""),
        ]);
        let e = IgnoreEngine::new(
            &t.root,
            &IgnoreOptions {
                include: vec![".special".into()],
                ..IgnoreOptions::default()
            },
        );
        assert!(e.explicitly_included(&t.root.join(".github"), true));
        assert!(e.explicitly_included(&t.root.join(".special"), false));
        assert!(!e.explicitly_included(&t.root.join(".hidden"), false));
    }

    #[test]
    fn paths_outside_the_root_are_not_the_engines_business() {
        let t = tree(&[(".gitignore", "*\n")]);
        assert!(!engine(&t).is_ignored(Path::new("/etc/passwd"), false));
    }

    #[test]
    fn a_broken_pattern_is_dropped_not_fatal() {
        let t = tree(&[("a.rs", "")]);
        let e = IgnoreEngine::new(
            &t.root,
            &IgnoreOptions {
                exclude: vec!["[unclosed".into(), "ok.txt".into()],
                ..IgnoreOptions::default()
            },
        );
        assert!(!ignored(&e, &t, "a.rs"));
    }

    #[test]
    fn the_global_ignore_location_follows_git() {
        let t = tree(&[(
            ".gitconfig",
            "[user]\n name = x\n[core]\n excludesfile = ~/ignores/global\n",
        )]);
        let home = t.root.clone();
        assert_eq!(
            default_global_ignore(&home, None),
            home.join("ignores/global")
        );
        let plain = tree(&[]);
        assert_eq!(
            default_global_ignore(&plain.root, None),
            plain.root.join(".config/git/ignore")
        );
        assert_eq!(
            default_global_ignore(&plain.root, Some(Path::new("/xdg"))),
            PathBuf::from("/xdg/git/ignore")
        );
    }
}
