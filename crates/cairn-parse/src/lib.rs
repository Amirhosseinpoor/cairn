//! `cairn-parse` — syntax validation (SPEC §6.3.6, D-06).
//!
//! Two kinds of language, two rules:
//!
//! * **Source languages** (Rust, Python, TypeScript/JavaScript, Go, Java,
//!   C++, Bash) are parsed with tree-sitter, which recovers from errors and
//!   hands back a tree with `ERROR` and `MISSING` nodes in it. If the file
//!   parsed cleanly *before* the edit, any error afterwards is the edit's
//!   doing and blocks it, wherever the parser reports it (an unclosed brace
//!   can surface far from the line that opened it). If the file was already
//!   broken, only an error that *starts* within ten lines of the edit blocks
//!   it, so a pre-existing problem elsewhere does not make the file
//!   un-editable (§6.3.6 step 3). A tree-sitter error node can stretch to
//!   the end of the file, which is why the start line, not the whole range,
//!   is what is compared.
//! * **Data formats** (JSON, TOML, YAML) have no useful error recovery, so
//!   the whole document is parsed and any failure blocks, wherever it is.
//!
//! The crate answers one question — "is this text acceptable after the
//! edit?" — and imports nothing of the tool layer; the adapter that plugs it
//! into `edit_file` lives in `cairn-tools`.

pub mod shell;

use std::time::Duration;

use tree_sitter::{Language, Node, Parser, Tree};

/// §6.3.6 step 3: an error this many lines from the edit still blocks it.
pub const NEAR_LINES: usize = 10;

/// §6.3.6 step 4: the snippet is at most this many lines.
const SNIPPET_LINES: usize = 8;

/// A syntax error, located.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
    pub expected: Option<String>,
    pub found: Option<String>,
    pub snippet: String,
}

/// The verdict on an edited buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Valid,
    /// No grammar or validator for this file.
    Unchecked,
    /// The parser gave up at its time limit.
    TimedOut,
    Invalid(Problem),
}

/// The languages this build can parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    Go,
    Java,
    Cpp,
    Bash,
    Json,
    Toml,
    Yaml,
}

impl Lang {
    /// By extension, then by shebang (§6.3.6 step 1). `.c` and `.h` are left
    /// unchecked on purpose: they parse as C++ most of the time and not
    /// always, and a false rejection costs more than a missed error.
    #[must_use]
    pub fn detect(path: &str, first_line: Option<&str>) -> Option<Self> {
        let name = path.rsplit('/').next().unwrap_or(path);
        if let Some((_, ext)) = name.rsplit_once('.') {
            let by_ext = match ext.to_ascii_lowercase().as_str() {
                "rs" => Some(Self::Rust),
                "py" | "pyi" => Some(Self::Python),
                "ts" | "mts" | "cts" => Some(Self::TypeScript),
                // JavaScript goes through the TSX grammar: it accepts JSX
                // and, JavaScript having no type assertions, loses nothing.
                "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Self::Tsx),
                "go" => Some(Self::Go),
                "java" => Some(Self::Java),
                "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
                "sh" | "bash" => Some(Self::Bash),
                "json" => Some(Self::Json),
                "toml" => Some(Self::Toml),
                "yaml" | "yml" => Some(Self::Yaml),
                _ => None,
            };
            if by_ext.is_some() {
                return by_ext;
            }
        }
        let shebang = first_line?.strip_prefix("#!")?;
        let program = shebang
            .split_whitespace()
            .filter_map(|w| w.rsplit('/').next())
            .find(|w| *w != "env")?;
        match program {
            "bash" | "sh" | "dash" => Some(Self::Bash),
            "python" | "python3" => Some(Self::Python),
            "node" | "nodejs" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn grammar(self) -> Option<Language> {
        Some(match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::Json | Self::Toml | Self::Yaml => return None,
        })
    }
}

// ----------------------------------------------------------------- snippets

/// Up to eight numbered lines of `text` around 1-based `line`.
fn snippet(text: &str, line: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let first = line.saturating_sub(SNIPPET_LINES / 2).max(1);
    let last = (first + SNIPPET_LINES - 1).min(lines.len());
    let width = last.to_string().len();
    (first..=last)
        .map(|n| {
            let marker = if n == line { ">" } else { " " };
            format!("{marker}{n:>width$} | {}", lines[n - 1])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Column (1-based, characters) of byte `offset` within its line.
fn column_of(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    // A byte offset inside a multi-byte character rounds down to its start.
    let mut end = offset;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[start..end].chars().count() + 1
}

#[allow(
    clippy::naive_bytecount,
    reason = "edited buffers are small; a counting crate is not worth a dependency"
)]
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

// ------------------------------------------------------------ tree-sitter

/// One error or missing node, in 1-based lines.
struct Found {
    start_line: usize,
    offset: usize,
    missing: bool,
    kind: String,
    text: String,
}

fn collect(node: Node<'_>, source: &str, out: &mut Vec<Found>) {
    if node.is_missing() || node.is_error() {
        let range = node.byte_range();
        out.push(Found {
            start_line: node.start_position().row + 1,
            offset: node.start_byte(),
            missing: node.is_missing(),
            kind: node.kind().to_string(),
            text: source
                .get(range)
                .unwrap_or("")
                .chars()
                .take(24)
                .collect::<String>()
                .replace('\n', "⏎"),
        });
        // An ERROR node's children are the parser's guesses; the node itself
        // is the report.
        if node.is_error() {
            return;
        }
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

fn parse(lang: Lang, text: &str, budget: Duration) -> Result<Option<Tree>, ()> {
    let Some(grammar) = lang.grammar() else {
        return Err(());
    };
    let mut parser = Parser::new();
    parser.set_language(&grammar).map_err(|_| ())?;
    #[allow(
        deprecated,
        reason = "set_timeout_micros is the 0.24 API; the progress-callback form is 0.25"
    )]
    parser.set_timeout_micros(u64::try_from(budget.as_micros()).unwrap_or(u64::MAX));
    Ok(parser.parse(text, None))
}

fn has_error(lang: Lang, text: &str, budget: Duration) -> Option<bool> {
    match parse(lang, text, budget) {
        Ok(Some(tree)) => Some(tree.root_node().has_error()),
        _ => None,
    }
}

fn check_source(
    lang: Lang,
    before: &str,
    after: &str,
    changed: (usize, usize),
    budget: Duration,
) -> Outcome {
    let tree = match parse(lang, after, budget) {
        Ok(Some(tree)) => tree,
        Ok(None) => return Outcome::TimedOut,
        Err(()) => return Outcome::Unchecked,
    };
    if !tree.root_node().has_error() {
        return Outcome::Valid;
    }
    let mut found = Vec::new();
    collect(tree.root_node(), after, &mut found);
    // A file that parsed cleanly before has no errors of its own: whatever
    // the parser reports now, the edit caused. (If the "before" parse itself
    // gave up, assume the worst and use the locality rule.)
    let was_clean = has_error(lang, before, budget) == Some(false);
    let low = changed.0.saturating_sub(NEAR_LINES);
    let high = changed.1 + NEAR_LINES;
    let culprit = found
        .into_iter()
        .filter(|f| was_clean || (f.start_line >= low && f.start_line <= high))
        .min_by_key(|f| (f.start_line, f.offset));
    let Some(error) = culprit else {
        // Errors exist, but not ones the edit can be blamed for.
        return Outcome::Valid;
    };
    let (expected, found) = if error.missing {
        (Some(format!("`{}`", error.kind)), None)
    } else {
        (None, Some(format!("`{}`", error.text)))
    };
    Outcome::Invalid(Problem {
        line: error.start_line,
        column: column_of(after, error.offset),
        expected,
        found,
        snippet: snippet(after, error.start_line),
    })
}

// ------------------------------------------------------------ data formats

fn check_json(after: &str) -> Outcome {
    match serde_json::from_str::<serde::de::IgnoredAny>(after) {
        Ok(_) => Outcome::Valid,
        Err(e) => Outcome::Invalid(Problem {
            line: e.line().max(1),
            column: e.column().max(1),
            expected: None,
            found: Some(e.to_string()),
            snippet: snippet(after, e.line().max(1)),
        }),
    }
}

fn check_toml(after: &str) -> Outcome {
    match toml::from_str::<toml::Table>(after) {
        Ok(_) => Outcome::Valid,
        Err(e) => {
            let offset = e.span().map_or(0, |s| s.start);
            let line = line_of(after, offset);
            Outcome::Invalid(Problem {
                line,
                column: column_of(after, offset),
                expected: None,
                found: Some(e.message().to_string()),
                snippet: snippet(after, line),
            })
        }
    }
}

fn check_yaml(after: &str) -> Outcome {
    match yaml_rust2::YamlLoader::load_from_str(after) {
        Ok(_) => Outcome::Valid,
        Err(e) => {
            let marker = e.marker();
            Outcome::Invalid(Problem {
                line: marker.line().max(1),
                column: marker.col() + 1,
                expected: None,
                found: Some(e.info().to_string()),
                snippet: snippet(after, marker.line().max(1)),
            })
        }
    }
}

/// How long a parse may run before it is abandoned (REQ-TOOL-015's 500 ms,
/// less a margin for the work around it).
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(400);

/// Validate `after`, the text an edit is about to write over `before`.
///
/// `changed` is the 1-based `(first, last)` line range the edit touched.
#[must_use]
pub fn validate(path: &str, before: &str, after: &str, changed: (usize, usize)) -> Outcome {
    validate_with_budget(path, before, after, changed, DEFAULT_BUDGET)
}

/// [`validate`] with an explicit parse budget.
#[must_use]
pub fn validate_with_budget(
    path: &str,
    before: &str,
    after: &str,
    changed: (usize, usize),
    budget: Duration,
) -> Outcome {
    let Some(lang) = Lang::detect(path, after.lines().next()) else {
        return Outcome::Unchecked;
    };
    match lang {
        Lang::Json => check_json(after),
        Lang::Toml => check_toml(after),
        Lang::Yaml => check_yaml(after),
        source => check_source(source, before, after, changed, budget),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check `text` as a whole new file (everything is "the edit").
    fn check(path: &str, text: &str) -> Outcome {
        validate(path, "", text, (1, text.lines().count().max(1)))
    }

    fn invalid(path: &str, text: &str) -> Problem {
        match check(path, text) {
            Outcome::Invalid(p) => p,
            other => panic!("{path}: expected a syntax error, got {other:?}"),
        }
    }

    #[test]
    fn valid_source_in_every_language_is_valid() {
        for (path, text) in [
            ("a.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
            ("a.py", "def f(x):\n    return x + 1\n"),
            ("a.ts", "function f(x: number): number { return x + 1; }\n"),
            ("a.tsx", "const A = () => <div>hi</div>;\n"),
            ("a.js", "const f = (x) => x + 1;\nmodule.exports = f;\n"),
            ("a.jsx", "const A = () => <b>x</b>;\n"),
            ("a.go", "package main\n\nfunc main() {}\n"),
            ("A.java", "class A { void f() {} }\n"),
            ("a.cpp", "int main() { return 0; }\n"),
            ("a.sh", "#!/bin/bash\nif true; then echo ok; fi\n"),
        ] {
            assert_eq!(check(path, text), Outcome::Valid, "{path}");
        }
    }

    /// T-EDIT-021's Rust case: an unclosed brace is caught and located.
    #[test]
    fn a_rust_syntax_error_is_found_with_line_column_and_snippet() {
        let p = invalid("a.rs", "fn a() {}\nfn b() {\n    let x = ;\n}\n");
        assert_eq!(p.line, 3);
        assert!(p.column >= 5, "{p:?}");
        assert!(p.snippet.contains(">3 |"), "{}", p.snippet);
        assert!(p.snippet.lines().count() <= 8);
    }

    #[test]
    fn missing_tokens_are_reported_as_expected() {
        let p = invalid("a.rs", "fn a() {\n    let x = 1\n}\n");
        assert!(
            p.expected.as_deref().is_some_and(|e| e.contains(';')),
            "{p:?}"
        );
    }

    #[test]
    fn every_source_language_rejects_its_own_breakage() {
        for (path, text) in [
            ("a.py", "def f(:\n    pass\n"),
            ("a.ts", "function f( { return 1 }\n"),
            ("a.go", "package main\nfunc main( {\n"),
            ("A.java", "class A { void f( { } }\n"),
            ("a.cpp", "int main( { return 0; }\n"),
            ("a.sh", "if true; then\n  echo hi\n"),
        ] {
            let _ = invalid(path, text);
        }
    }

    /// T-EDIT-023: a pre-existing error far from the edit does not block it.
    #[test]
    fn t_edit_023_an_error_far_from_the_edit_is_ignored() {
        let mut text = String::from("fn broken( {\n");
        for i in 0..60 {
            text.push_str("// filler ");
            text.push_str(&i.to_string());
            text.push('\n');
        }
        text.push_str("fn fine() {}\n");
        // The file was already broken ("before" has the same error); the edit
        // touched line 62 only.
        assert_eq!(validate("a.rs", &text, &text, (62, 62)), Outcome::Valid);
        // The same text with the edit on the broken line is rejected.
        assert!(matches!(
            validate("a.rs", &text, &text, (1, 1)),
            Outcome::Invalid(_)
        ));
    }

    /// An edit to a file that parsed cleanly owns every error that appears,
    /// wherever the parser reports it.
    #[test]
    fn an_error_in_a_previously_clean_file_is_the_edits_even_when_it_surfaces_far_away() {
        let before =
            "fn a() {\n    one();\n}\n\n".to_string() + &"// pad\n".repeat(40) + "fn z() {}\n";
        // Deleting the closing brace of `a` breaks things at line 3, but the
        // parser may only complain much later; either way it blocks.
        let after = before.replacen("}\n\n", "\n\n", 1);
        assert!(matches!(
            validate("a.rs", &before, &after, (3, 3)),
            Outcome::Invalid(_)
        ));
        // And a clean edit of the same file is fine.
        let fine = before.replace("one()", "two()");
        assert_eq!(validate("a.rs", &before, &fine, (2, 2)), Outcome::Valid);
    }

    #[test]
    fn the_ten_line_margin_is_inclusive() {
        let mut text = String::from("fn broken( {\n");
        text.push_str(&"// filler\n".repeat(30));
        // Error on line 1 already in "before": an edit at line 11 is within
        // ten lines; one at line 12 is not.
        assert!(matches!(
            validate("a.rs", &text, &text, (11, 11)),
            Outcome::Invalid(_)
        ));
        assert_eq!(validate("a.rs", &text, &text, (12, 12)), Outcome::Valid);
    }

    /// T-EDIT-022: a broken `package.json`, with the validator's position.
    #[test]
    fn t_edit_022_json_errors_always_block_with_line_and_column() {
        let p = invalid(
            "package.json",
            "{\n  \"name\": \"x\",\n  \"version\": \n}\n",
        );
        assert_eq!(p.line, 4, "{p:?}");
        assert!(
            p.found.as_deref().unwrap_or("").contains("expected"),
            "{p:?}"
        );
        // Wherever the error is, a data format blocks: the range is ignored.
        assert!(matches!(
            validate("a.json", "{}", "{\"a\": }", (999, 999)),
            Outcome::Invalid(_)
        ));
        assert_eq!(check("a.json", "{\"a\": [1, 2, 3]}"), Outcome::Valid);
        assert_eq!(check("a.json", "[]"), Outcome::Valid);
    }

    #[test]
    fn toml_and_yaml_are_validated_as_whole_documents() {
        assert_eq!(check("a.toml", "[a]\nb = 1\n"), Outcome::Valid);
        let p = invalid("a.toml", "[a]\nb = \nc = 2\n");
        assert_eq!(p.line, 2, "{p:?}");
        assert_eq!(check("a.yaml", "a:\n  - 1\n  - 2\n"), Outcome::Valid);
        assert_eq!(check("a.yml", ""), Outcome::Valid);
        let p = invalid("a.yaml", "a: [1, 2\nb: 3\n");
        assert!(p.line >= 1);
    }

    #[test]
    fn languages_without_a_validator_are_unchecked() {
        for path in [
            "README.md",
            "a.c",
            "a.h",
            "notes.txt",
            "Makefile",
            "image.png",
            "a.unknown",
        ] {
            assert_eq!(check(path, "anything { ( ["), Outcome::Unchecked, "{path}");
        }
    }

    #[test]
    fn a_shebang_chooses_the_language_for_an_extensionless_script() {
        assert_eq!(
            Lang::detect("run", Some("#!/usr/bin/env python3")),
            Some(Lang::Python)
        );
        assert_eq!(Lang::detect("run", Some("#!/bin/sh")), Some(Lang::Bash));
        assert_eq!(
            Lang::detect("run", Some("#!/usr/bin/env node")),
            Some(Lang::Tsx)
        );
        assert_eq!(Lang::detect("run", Some("# plain")), None);
        assert!(matches!(
            validate("tool", "", "#!/bin/bash\nif true; then\n", (1, 2)),
            Outcome::Invalid(_)
        ));
    }

    /// REQ-TOOL-015: a parse that runs out of budget says so instead of
    /// pretending.
    #[test]
    fn a_parse_over_its_budget_times_out() {
        let text = "fn f() { let v = vec![1, 2, 3]; }\n".repeat(5000);
        let outcome = validate_with_budget("a.rs", "", &text, (1, 1), Duration::from_micros(1));
        assert_eq!(outcome, Outcome::TimedOut);
        // With a real budget the same text is fine.
        assert_eq!(
            validate_with_budget("a.rs", "", &text, (1, 1), Duration::from_secs(5)),
            Outcome::Valid
        );
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        // `é` is two bytes; the error is after it on the same line.
        let p = invalid("a.py", "x = 'é' +\n");
        assert!(p.column <= "x = 'é' +".chars().count() + 1, "{p:?}");
        assert_eq!(
            column_of("aé b", 4),
            4,
            "byte 4 is the space, the 4th character"
        );
    }

    #[test]
    fn the_snippet_is_at_most_eight_numbered_lines_with_a_marker() {
        let text: String = (1..=30).fold(String::new(), |mut acc, i| {
            acc.push_str("line ");
            acc.push_str(&i.to_string());
            acc.push('\n');
            acc
        });
        let s = snippet(&text, 15);
        assert_eq!(s.lines().count(), 8);
        assert_eq!(s.lines().filter(|l| l.starts_with('>')).count(), 1);
        assert!(s.contains(">15 | line 15"), "{s}");
        let near_start = snippet(&text, 1);
        assert!(near_start.starts_with(">1 | line 1"), "{near_start}");
    }

    #[test]
    fn an_empty_file_is_valid_everywhere_it_can_be_checked() {
        for path in ["a.rs", "a.py", "a.go", "a.sh", "a.ts"] {
            assert_eq!(check(path, ""), Outcome::Valid, "{path}");
        }
    }
}
