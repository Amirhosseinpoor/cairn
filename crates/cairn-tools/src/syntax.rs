//! The tree-sitter-backed [`SyntaxCheck`] (SPEC §6.3.6), over `cairn-parse`.

use crate::edit::changed_lines;
use crate::types::{SyntaxCheck, SyntaxProblem, SyntaxVerdict};

/// Past this size a buffer is not parsed at all: REQ-TOOL-015 promises
/// 500 ms only up to 1 MiB, and beyond a few MiB the answer is never worth
/// the wait.
const MAX_CHECKED_BYTES: usize = 4 * 1024 * 1024;

/// Validates edits with the grammars and validators `cairn-parse` carries.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParseCheck;

impl SyntaxCheck for ParseCheck {
    fn check(&self, path: &str, before: &str, after: &str) -> SyntaxVerdict {
        if after.len() > MAX_CHECKED_BYTES {
            return SyntaxVerdict::Unchecked;
        }
        // Nothing changed means nothing to blame: the edit cannot have
        // introduced an error, so do not parse.
        let Some(changed) = changed_lines(before, after) else {
            return SyntaxVerdict::Valid;
        };
        match cairn_parse::validate(path, before, after, changed) {
            cairn_parse::Outcome::Valid => SyntaxVerdict::Valid,
            cairn_parse::Outcome::Unchecked => SyntaxVerdict::Unchecked,
            cairn_parse::Outcome::TimedOut => SyntaxVerdict::TimedOut,
            cairn_parse::Outcome::Invalid(p) => SyntaxVerdict::Invalid(SyntaxProblem {
                line: p.line,
                column: p.column,
                expected: p.expected,
                found: p.found,
                snippet: p.snippet,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unchanged_buffer_is_valid_without_a_parse() {
        assert_eq!(
            ParseCheck.check("a.rs", "fn broken( {", "fn broken( {"),
            SyntaxVerdict::Valid
        );
    }

    #[test]
    fn the_verdict_carries_the_problem_through() {
        let v = ParseCheck.check("a.json", "{}", "{\"a\": }");
        let SyntaxVerdict::Invalid(p) = v else {
            panic!("{v:?}")
        };
        assert_eq!(p.line, 1);
        assert!(p.snippet.contains("{\"a\": }"));
    }

    #[test]
    fn unknown_languages_and_huge_buffers_are_unchecked() {
        assert_eq!(
            ParseCheck.check("notes.txt", "", "x {"),
            SyntaxVerdict::Unchecked
        );
        let huge = "// x\n".repeat(MAX_CHECKED_BYTES / 5 + 10);
        assert_eq!(
            ParseCheck.check("a.rs", "", &huge),
            SyntaxVerdict::Unchecked
        );
    }
}
