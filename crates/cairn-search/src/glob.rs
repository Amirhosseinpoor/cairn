//! `glob` (SPEC §6.2.6): globset syntax over the walker, sorted by ascending
//! mtime then path, never following symlinks.

use std::path::Path;
use std::time::{Instant, SystemTime};

use globset::GlobBuilder;

use crate::ignore_rules::IgnoreEngine;
use crate::walk::{walk, Kind, WalkError, WalkOptions};

/// What a glob needs.
#[derive(Debug, Clone, Copy)]
pub struct GlobOptions {
    pub max_results: usize,
    pub respect_ignore: bool,
}

/// What a glob found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobResult {
    /// Workspace-relative paths.
    pub matches: Vec<String>,
    /// Matches found before the cut at `max_results`.
    pub count: usize,
    pub truncated: bool,
    pub duration_ms: u64,
}

/// Why a glob did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobError {
    /// `E-GLOB-SYNTAX`.
    Syntax(String),
    /// `E-FS-NOTFOUND`: the start path does not exist.
    NotFound,
    /// `E-GLOB-CAP`: the scan hit its entry cap before a single match, so
    /// "no results" would be a lie.
    Cap,
    /// `E-FS-PERM`.
    Unreadable(String),
}

/// Find files below `start` whose path *relative to `start`* matches
/// `pattern`.
///
/// # Errors
/// [`GlobError`].
pub fn glob(
    engine: &IgnoreEngine,
    start: &Path,
    pattern: &str,
    options: GlobOptions,
) -> Result<GlobResult, GlobError> {
    let began = Instant::now();
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| GlobError::Syntax(e.to_string()))?
        .compile_matcher();
    let walked = walk(
        engine,
        start,
        &WalkOptions {
            respect_ignore: options.respect_ignore,
            ..WalkOptions::default()
        },
    )
    .map_err(|e| match e {
        WalkError::NotFound => GlobError::NotFound,
        WalkError::Unreadable(why) => GlobError::Unreadable(why),
    })?;

    let mut found: Vec<(Option<SystemTime>, String)> = walked
        .entries
        .iter()
        .filter(|e| e.kind == Kind::File)
        .filter(|e| {
            let relative = e.path.strip_prefix(start).map_or_else(
                |_| e.rel.clone(),
                |p| p.to_string_lossy().replace('\\', "/"),
            );
            // Walking a single file: its own name is what the pattern sees.
            let relative = if relative.is_empty() {
                e.path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            } else {
                relative
            };
            matcher.is_match(&relative)
        })
        .map(|e| (e.mtime, e.rel.clone()))
        .collect();
    if walked.capped && found.is_empty() {
        return Err(GlobError::Cap);
    }
    // Ascending mtime, then path: stable for equal timestamps.
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let count = found.len();
    let truncated = count > options.max_results || walked.capped;
    found.truncate(options.max_results);
    Ok(GlobResult {
        matches: found.into_iter().map(|(_, rel)| rel).collect(),
        count,
        truncated,
        duration_ms: u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore_rules::IgnoreOptions;
    use std::path::PathBuf;

    fn setup(files: &[&str]) -> (tempfile::TempDir, PathBuf, IgnoreEngine) {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().canonicalize().expect("canonical").join("ws");
        std::fs::create_dir_all(&root).expect("root");
        for f in files {
            let full = root.join(f);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            std::fs::write(full, "x").expect("file");
        }
        let engine = IgnoreEngine::new(&root, &IgnoreOptions::default());
        (dir, root, engine)
    }

    const OPTS: GlobOptions = GlobOptions {
        max_results: 500,
        respect_ignore: true,
    };

    #[test]
    fn globset_syntax_works() {
        let (_d, root, engine) = setup(&["a.rs", "b.py", "src/c.rs", "src/deep/d.rs", "src/e.txt"]);
        let run = |p: &str| glob(&engine, &root, p, OPTS).expect("runs").matches;
        assert_eq!(run("*.rs"), ["a.rs"], "`*` does not cross directories");
        let mut deep = run("**/*.rs");
        deep.sort();
        assert_eq!(deep, ["a.rs", "src/c.rs", "src/deep/d.rs"]);
        let mut alt = run("src/*.{rs,txt}");
        alt.sort();
        assert_eq!(alt, ["src/c.rs", "src/e.txt"]);
        assert_eq!(run("?.py"), ["b.py"]);
        assert_eq!(run("[ab].*").len(), 2);
        assert!(run("*.nothing").is_empty());
    }

    #[test]
    fn patterns_are_relative_to_the_start_directory() {
        let (_d, root, engine) = setup(&["src/c.rs", "src/deep/d.rs", "top.rs"]);
        let got = glob(&engine, &root.join("src"), "*.rs", OPTS).expect("runs");
        assert_eq!(got.matches, ["src/c.rs"], "results stay workspace-relative");
    }

    #[test]
    fn ignored_files_are_hidden_unless_asked() {
        let (_d, root, engine) = setup(&["a.rs", "target/b.rs", "node_modules/c.rs"]);
        assert_eq!(
            glob(&engine, &root, "**/*.rs", OPTS).expect("runs").matches,
            ["a.rs"]
        );
        let all = glob(
            &engine,
            &root,
            "**/*.rs",
            GlobOptions {
                respect_ignore: false,
                ..OPTS
            },
        )
        .expect("runs");
        assert_eq!(all.count, 3);
    }

    #[test]
    fn results_sort_by_ascending_mtime_then_path() {
        let (_d, root, engine) = setup(&["b.rs", "a.rs", "c.rs"]);
        let set = |name: &str, secs: u64| {
            let f = std::fs::File::options()
                .write(true)
                .open(root.join(name))
                .expect("open");
            f.set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .expect("mtime");
        };
        set("a.rs", 3000);
        set("b.rs", 1000);
        set("c.rs", 2000);
        assert_eq!(
            glob(&engine, &root, "*.rs", OPTS).expect("runs").matches,
            ["b.rs", "c.rs", "a.rs"]
        );
        // Equal times fall back to the path.
        set("a.rs", 1000);
        assert_eq!(
            glob(&engine, &root, "*.rs", OPTS).expect("runs").matches,
            ["a.rs", "b.rs", "c.rs"]
        );
    }

    #[test]
    fn max_results_truncates_and_says_so() {
        let (_d, root, engine) = setup(&["a.rs", "b.rs", "c.rs"]);
        let got = glob(
            &engine,
            &root,
            "*.rs",
            GlobOptions {
                max_results: 2,
                ..OPTS
            },
        )
        .expect("runs");
        assert_eq!(got.matches.len(), 2);
        assert_eq!(got.count, 3);
        assert!(got.truncated);
    }

    #[test]
    fn errors_are_typed() {
        let (_d, root, engine) = setup(&["a.rs"]);
        assert!(matches!(
            glob(&engine, &root, "[unclosed", OPTS),
            Err(GlobError::Syntax(_))
        ));
        assert_eq!(
            glob(&engine, &root.join("missing"), "*", OPTS),
            Err(GlobError::NotFound)
        );
    }
}
