//! The repository map as the model reads it (SPEC §5.2): the most relevant
//! files, each with the symbols that matter, within the repo-map budget.

use std::fmt::Write as _;

use cairn_index::{Index, Query, Ranked, SymbolRow};

use crate::budget::{fit_repo_map, MapEntry};

/// The most symbols shown for one file.
const SYMBOLS_PER_FILE: usize = 12;
/// A signature longer than this is cut.
const SIGNATURE_CHARS: usize = 110;

/// A map ready for the system prompt.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Map {
    pub text: String,
    /// Files in it.
    pub files: usize,
    /// The index was not ready, so there is no map (REQ-CTX-009).
    pub degraded: bool,
}

fn describe(symbol: &SymbolRow) -> String {
    let callable = matches!(symbol.kind.as_str(), "func" | "method");
    if callable && !symbol.signature.is_empty() {
        let sig: String = symbol.signature.chars().take(SIGNATURE_CHARS).collect();
        format!("{}: {sig}", symbol.line)
    } else {
        format!("{}: {} {}", symbol.line, symbol.kind, symbol.name)
    }
}

/// One file's entry: its path, then its symbols in line order. Symbols that
/// matched the query come first when there are too many to show.
#[must_use]
pub fn render_file(ranked: &Ranked) -> String {
    let mut chosen: Vec<&SymbolRow> = ranked
        .symbols
        .iter()
        .filter(|s| s.kind != "var" || ranked.lines_of_interest.contains(&s.line))
        .collect();
    chosen.sort_by_key(|s| (!ranked.lines_of_interest.contains(&s.line), s.line));
    let hidden = chosen.len().saturating_sub(SYMBOLS_PER_FILE);
    chosen.truncate(SYMBOLS_PER_FILE);
    chosen.sort_by_key(|s| s.line);
    let mut out = format!("{}\n", ranked.path);
    for symbol in chosen {
        out.push_str("  ");
        out.push_str(&describe(symbol));
        out.push('\n');
    }
    if hidden > 0 {
        let _ = writeln!(out, "  … and {hidden} more");
    }
    out
}

/// Rank the index for `query` and fit the result to `budget_tokens`.
#[must_use]
pub fn build(index: &Index, query: &Query<'_>, budget_tokens: u32) -> Map {
    if index.degraded() {
        return Map {
            degraded: true,
            ..Map::default()
        };
    }
    let Ok(ranked) = index.rank(query) else {
        return Map {
            degraded: true,
            ..Map::default()
        };
    };
    // A file with nothing to show (a README, a lockfile) adds only noise.
    let entries: Vec<MapEntry> = ranked
        .iter()
        .filter(|r| !r.symbols.is_empty())
        .map(|r| MapEntry {
            path: r.path.clone(),
            score: r.score,
            text: render_file(r),
        })
        .collect();
    let kept = fit_repo_map(entries, budget_tokens);
    Map {
        files: kept.len(),
        text: kept
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join(""),
        degraded: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, kind: &str, line: u32, signature: &str) -> SymbolRow {
        SymbolRow {
            name: name.into(),
            simple_name: name.rsplit("::").next().unwrap_or(name).into(),
            kind: kind.into(),
            line,
            end_line: line + 1,
            container: None,
            signature: signature.into(),
            doc: String::new(),
        }
    }

    fn ranked(symbols: Vec<SymbolRow>, interest: Vec<u32>) -> Ranked {
        Ranked {
            path: "src/parser.rs".into(),
            score: 0.9,
            lines_of_interest: interest,
            symbols,
        }
    }

    #[test]
    fn a_file_lists_its_symbols_with_signatures_for_callables() {
        let r = ranked(
            vec![
                symbol("Parser", "struct", 2, "pub struct Parser"),
                symbol(
                    "Parser::parse",
                    "method",
                    4,
                    "pub fn parse(&mut self) -> Token",
                ),
            ],
            vec![],
        );
        assert_eq!(
            render_file(&r),
            "src/parser.rs\n  2: struct Parser\n  4: pub fn parse(&mut self) -> Token\n"
        );
    }

    #[test]
    fn variables_appear_only_when_they_matched() {
        let r = ranked(
            vec![
                symbol("LIMIT", "var", 3, ""),
                symbol("go", "func", 5, "fn go()"),
            ],
            vec![],
        );
        assert!(!render_file(&r).contains("LIMIT"));
        let r = ranked(
            vec![
                symbol("LIMIT", "var", 3, ""),
                symbol("go", "func", 5, "fn go()"),
            ],
            vec![3],
        );
        assert!(render_file(&r).contains("3: var LIMIT"));
    }

    #[test]
    fn too_many_symbols_keep_the_matches_and_say_how_many_are_hidden() {
        let symbols: Vec<SymbolRow> = (1..=20)
            .map(|i| symbol(&format!("f{i}"), "func", i * 10, &format!("fn f{i}()")))
            .collect();
        let r = ranked(symbols, vec![200]);
        let text = render_file(&r);
        assert!(text.contains("200: fn f20()"), "the match survives: {text}");
        assert!(text.contains("… and 8 more"));
        assert_eq!(text.lines().count(), 1 + SYMBOLS_PER_FILE + 1);
        // Shown in line order.
        let lines: Vec<u32> = text
            .lines()
            .skip(1)
            .filter_map(|l| l.trim().split(':').next()?.parse().ok())
            .collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted);
    }

    #[test]
    fn a_long_signature_is_cut() {
        let long = format!("fn big({})", "argument: u32, ".repeat(30));
        let r = ranked(vec![symbol("big", "func", 1, &long)], vec![]);
        let text = render_file(&r);
        assert!(text.lines().nth(1).unwrap().len() < 130, "{text}");
    }
}
