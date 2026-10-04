//! Custom keybindings — `~/.config/cairn/keybindings.toml` (SPEC §10.4).
//!
//! The file is parsed line by line so every binding keeps its line number
//! (TOML itself would collapse duplicates into a single opaque error), which is
//! what `E-CFG-KEYCONFLICT` needs in order to list *both* lines.
//!
//! Conflict rules (SPEC §10.4):
//! 1. a binding may be defined once per context;
//! 2. user bindings override built-ins;
//! 3. two user bindings for the same key in the same context →
//!    `E-CFG-KEYCONFLICT`, both lines listed, that context's file bindings are
//!    dropped and the built-ins are restored;
//! 4. chords are at most 2 keys (1000 ms window);
//! 5. reserved OS combos (`alt+f4`, `cmd+q`, `ctrl+alt+tab`) →
//!    `E-CFG-KEYRESERVED`.

use crate::issue::Issue;
use crate::paths::Paths;
use cairn_core::error::codes;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The context names bound by `[bindings]` (no explicit context).
pub const GLOBAL_CONTEXT: &str = "global";

/// Combos the OS owns and Cairn must never steal (SPEC §10.4 rule 5).
pub const RESERVED: &[&str] = &["alt+f4", "cmd+q", "ctrl+alt+tab"];

/// One binding line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub context: String,
    pub key: String,
    pub action: String,
    pub line: usize,
}

/// `keybindings.toml` for a path set (SPEC §11.6: same dir as `config.toml`).
#[must_use]
pub fn file_for(paths: &Paths) -> PathBuf {
    paths.config_home.join("keybindings.toml")
}

/// Parse bindings out of the raw file text.
#[must_use]
pub fn parse(text: &str) -> Vec<Binding> {
    let mut out = Vec::new();
    let mut context = None::<String>;
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let line_text = raw.trim();
        if line_text.is_empty() || line_text.starts_with('#') {
            continue;
        }
        if let Some(header) = line_text
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
        {
            context = match header.trim() {
                "bindings" => Some(GLOBAL_CONTEXT.to_string()),
                other => other
                    .trim()
                    .strip_prefix("bindings.context.")
                    .map(|c| c.trim().to_ascii_lowercase()),
            };
            continue;
        }
        let Some(context) = context.clone() else {
            continue;
        };
        let Some((key_part, action_part)) = line_text.split_once('=') else {
            continue;
        };
        let key = unquote(key_part.trim());
        let action = unquote(strip_comment(action_part).trim());
        if key.is_empty() || action.is_empty() {
            continue;
        }
        out.push(Binding {
            context,
            key: key.to_ascii_lowercase(),
            action,
            line,
        });
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn strip_comment(s: &str) -> &str {
    match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Issues for `cairn config validate` (REQ-TUI-006, T-CFG-021).
#[must_use]
pub fn issues(paths: &Paths) -> Vec<Issue> {
    check(&file_for(paths))
}

/// Validate one `keybindings.toml`.
#[must_use]
pub fn check(path: &Path) -> Vec<Issue> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let bindings = parse(&text);
    let mut out = Vec::new();
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    for b in &bindings {
        let id = (b.context.clone(), b.key.clone());
        if let Some(first_line) = seen.get(&id) {
            out.push(
                Issue::error(
                    codes::CFG_KEYCONFLICT,
                    format!(
                        "context \"{}\" binds \"{}\" at line {first_line} and again at line {}; \
                         this context's file bindings are ignored and built-ins are restored",
                        b.context, b.key, b.line
                    ),
                    Some("keybindings"),
                    "keybindings",
                )
                .at(path, b.line, 1),
            );
        } else {
            seen.insert(id, b.line);
        }
        if chord_len(&b.key) > 2 {
            out.push(
                Issue::error(
                    codes::CFG_BADVALUE,
                    format!(
                        "\"{}\" is a {}-key chord; chords are at most 2 keys",
                        b.key,
                        chord_len(&b.key)
                    ),
                    Some("keybindings"),
                    "keybindings",
                )
                .at(path, b.line, 1),
            );
        }
        if RESERVED.contains(&b.key.as_str()) {
            out.push(
                Issue::error(
                    codes::CFG_KEYRESERVED,
                    format!(
                        "\"{}\" is reserved by the operating system (reserved: {})",
                        b.key,
                        RESERVED.join(", ")
                    ),
                    Some("keybindings"),
                    "keybindings",
                )
                .at(path, b.line, 1),
            );
        }
    }
    out
}

fn chord_len(key: &str) -> usize {
    key.split_whitespace().count()
}

/// Effective user bindings: contexts that failed rule 3 disappear entirely
/// (SPEC §10.4 rule 3 — built-ins take that context back).
#[must_use]
pub fn resolve(path: &Path) -> BTreeMap<(String, String), String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let bindings = parse(&text);
    let mut count: BTreeMap<(String, String), usize> = BTreeMap::new();
    for b in &bindings {
        *count.entry((b.context.clone(), b.key.clone())).or_insert(0) += 1;
    }
    let broken: BTreeSet<String> = bindings
        .iter()
        .filter(|b| count[&(b.context.clone(), b.key.clone())] > 1)
        .map(|b| b.context.clone())
        .collect();
    bindings
        .into_iter()
        .filter(|b| !broken.contains(&b.context))
        .map(|b| ((b.context, b.key), b.action))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LoadOptions;

    fn paths_in(dir: &Path) -> Paths {
        let mut p = Paths::resolve(&|_| None);
        p.config_home = dir.to_path_buf();
        p
    }

    fn write(dir: &Path, text: &str) -> PathBuf {
        let file = dir.join("keybindings.toml");
        std::fs::write(&file, text).unwrap();
        file
    }

    /// T-CFG-021: conflicting keybindings → `E-CFG-KEYCONFLICT` and the
    /// conflicting context falls back to built-ins.
    #[test]
    fn conflict_reports_both_lines_and_restores_builtins() {
        let tmp = tempfile::tempdir().unwrap();
        let file = write(
            tmp.path(),
            "[bindings]\n\"ctrl+j\" = \"submit\"\n\n[bindings.context.global]\n\"ctrl+j\" = \"newline\"\n",
        );
        let issues = check(&file);
        let conflicts: Vec<&Issue> = issues
            .iter()
            .filter(|i| i.code == codes::CFG_KEYCONFLICT)
            .collect();
        assert_eq!(conflicts.len(), 1, "{issues:#?}");
        let msg = &conflicts[0].message;
        assert!(msg.contains("line 2") && msg.contains("line 5"), "{msg}");
        assert!(msg.contains("built-ins"), "{msg}");
        assert_eq!(conflicts[0].line, Some(5));
        // The whole context is dropped → built-ins restored.
        let resolved = resolve(&file);
        assert!(
            !resolved.keys().any(|(c, _)| c == GLOBAL_CONTEXT),
            "{resolved:?}"
        );
    }

    /// A clean file resolves and keeps everything.
    #[test]
    fn clean_file_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let file = write(
            tmp.path(),
            "[bindings]\n\"shift+tab\" = \"cycle_mode\"\n[bindings.context.input]\n\"ctrl+j\" = \"newline\"\n",
        );
        assert!(check(&file).is_empty());
        let resolved = resolve(&file);
        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved
                .get(&(GLOBAL_CONTEXT.to_string(), "shift+tab".to_string()))
                .map(String::as_str),
            Some("cycle_mode")
        );
        assert_eq!(
            resolved
                .get(&(input(), "ctrl+j".to_string()))
                .map(String::as_str),
            Some("newline")
        );
    }

    fn input() -> String {
        "input".to_string()
    }

    /// Rule 5: reserved OS combos are rejected.
    #[test]
    fn reserved_combos_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let file = write(tmp.path(), "[bindings]\n\"alt+f4\" = \"quit\"\n");
        let issues = check(&file);
        assert!(
            issues.iter().any(|i| i.code == codes::CFG_KEYRESERVED),
            "{issues:#?}"
        );
    }

    /// Rule 4: chords are at most two keys.
    #[test]
    fn long_chords_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let file = write(
            tmp.path(),
            "[bindings]\n\"ctrl+x ctrl+y ctrl+z\" = \"submit\"\n",
        );
        let issues = check(&file);
        assert!(
            issues
                .iter()
                .any(|i| i.code == codes::CFG_BADVALUE && i.message.contains("3-key")),
            "{issues:#?}"
        );
    }

    /// Missing file or a file without bindings is not an error.
    #[test]
    fn missing_file_is_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(check(&tmp.path().join("nope.toml")).is_empty());
        let paths = paths_in(tmp.path());
        assert!(issues(&paths).is_empty());
        assert!(resolve(&file_for(&paths)).is_empty());
        assert_eq!(file_for(&paths).file_name().unwrap(), "keybindings.toml");
        let _ = LoadOptions::default();
    }

    /// Comments and non-binding lines never produce bindings.
    #[test]
    fn comments_and_foreign_sections_are_ignored() {
        let text = "# comment\n[other]\n\"ctrl+a\" = \"nope\"\n[bindings]\n# c\n\"ctrl+b\" = \"x\"  # trailing\n";
        let bindings = parse(text);
        assert_eq!(bindings.len(), 1, "{bindings:?}");
        assert_eq!(bindings[0].key, "ctrl+b");
        assert_eq!(bindings[0].action, "x");
        assert_eq!(bindings[0].line, 6);
    }
}
