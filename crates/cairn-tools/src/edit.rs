//! The edit engine (SPEC §6.3): exact and fuzzy matching, line-ending and
//! BOM handling, and all-or-nothing multi-edits.
//!
//! Everything here is a pure function of text. Nothing reads or writes a
//! file, so the matching rules can be pinned by tests without a filesystem,
//! and the tools that wrap them (`edit_file`, `multi_edit`) stay thin.
//!
//! Two properties are worth holding in mind while reading:
//!
//! * **Determinism.** Fuzzy matching breaks every tie by position, never by
//!   hash order or iteration luck (REQ-TOOL-010).
//! * **Replace, never insert.** A match replaces exactly the span it matched;
//!   whatever the fuzzy score, no byte outside that span changes
//!   (REQ-TOOL-011).

use std::collections::BTreeSet;

/// How hard to try when the exact text is not there (§6.3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fuzzy {
    Off,
    Normal,
    Relaxed,
}

impl Fuzzy {
    /// Parse the schema's spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(Self::Off),
            "normal" => Some(Self::Normal),
            "relaxed" => Some(Self::Relaxed),
            _ => None,
        }
    }

    const fn threshold(self) -> f64 {
        match self {
            Self::Off => 2.0,
            Self::Normal => 0.92,
            Self::Relaxed => 0.85,
        }
    }

    /// Below this a *successful* fuzzy match still carries a warning (§6.3.2 step 6).
    const fn confident(self) -> f64 {
        match self {
            Self::Off => 2.0,
            Self::Normal => 0.97,
            Self::Relaxed => 0.90,
        }
    }
}

/// One edit as the tools receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditSpec {
    pub old: String,
    pub new: String,
    pub replace_all: bool,
    /// `None` when the caller did not say; `Some(1)` is the schema default
    /// but an explicit value also disables the fuzzy fallback (§6.3.1's last
    /// table row).
    pub expect_occurrences: Option<usize>,
    pub fuzzy: Fuzzy,
}

impl EditSpec {
    #[must_use]
    pub fn new(old: &str, new: &str) -> Self {
        Self {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
            expect_occurrences: None,
            fuzzy: Fuzzy::Normal,
        }
    }
}

/// A fuzzy match the model should be told about (`W-EDIT-FUZZY`).
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyNote {
    pub score: f64,
    /// What was actually in the file where the model's text was expected.
    pub matched_text: String,
}

/// What an edit did.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    /// Matches found (before replacement).
    pub occurrences: usize,
    pub replaced: usize,
    /// 1-based, in the *new* text.
    pub start_line: usize,
    pub end_line: usize,
    pub fuzzy_used: bool,
    pub fuzzy_score: Option<f64>,
    pub fuzzy_note: Option<FuzzyNote>,
    /// Byte ranges in the new text that now hold the replacement.
    pub new_spans: Vec<(usize, usize)>,
}

/// Why an edit did not apply.
#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    /// `E-EDIT-NOMATCH`. `closest` names the best fuzzy candidate when there was one.
    NoMatch { closest: Option<Closest> },
    /// `E-EDIT-AMBIGUOUS`: line numbers (1-based) of each candidate, with
    /// scores for fuzzy ones.
    Ambiguous {
        lines: Vec<usize>,
        scores: Vec<f64>,
        expected: usize,
    },
    /// `E-EDIT-NOCHANGE`.
    NoChange,
    /// `E-EDIT-NOMATCH` because the pattern is empty.
    EmptyPattern,
}

/// The nearest thing to a match, for the error message.
#[derive(Debug, Clone, PartialEq)]
pub struct Closest {
    pub line: usize,
    pub score: f64,
}

/// A multi-edit failed before anything was written.
#[derive(Debug, Clone, PartialEq)]
pub enum MultiError {
    /// `E-EDIT-PARTIAL`: edit `index` did not apply to the buffer as the
    /// edits before it left it.
    Partial { index: usize, error: EditError },
    /// `E-EDIT-CONFLICT`: edits `a` and `b` target overlapping text.
    Conflict { a: usize, b: usize },
}

// ------------------------------------------------------------ line endings

/// A file's line-ending style, as found (§6.3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

/// Text decoded for editing: LF-normalised, BOM removed, with what is needed
/// to put both back byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub text: String,
    pub had_bom: bool,
    pub eol: Eol,
    /// The file had both styles (the majority style is restored on write).
    pub mixed: bool,
}

/// The file's bytes are not UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotUtf8 {
    pub valid_up_to: usize,
}

impl Document {
    /// Decode `bytes`.
    ///
    /// # Errors
    /// [`NotUtf8`] when the content is not valid UTF-8.
    pub fn decode(bytes: &[u8]) -> Result<Self, NotUtf8> {
        let (body, had_bom) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
            Some(rest) => (rest, true),
            None => (bytes, false),
        };
        let text = std::str::from_utf8(body).map_err(|e| NotUtf8 {
            valid_up_to: e.valid_up_to(),
        })?;
        let crlf = text.matches("\r\n").count();
        let lf = text.matches('\n').count() - crlf;
        // Majority decides; a tie (including no line breaks at all) is LF.
        let eol = if crlf > lf { Eol::Crlf } else { Eol::Lf };
        Ok(Self {
            text: text.replace("\r\n", "\n"),
            had_bom,
            eol,
            mixed: crlf > 0 && lf > 0,
        })
    }

    /// Encode `text` the way this document was found.
    #[must_use]
    pub fn encode(&self, text: &str) -> Vec<u8> {
        encode_like(text, self.had_bom, self.eol)
    }
}

/// `text` (LF-normalised) as bytes with the given BOM and line endings.
#[must_use]
pub fn encode_like(text: &str, bom: bool, eol: Eol) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 3);
    if bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    match eol {
        Eol::Lf => out.extend_from_slice(text.as_bytes()),
        Eol::Crlf => out.extend_from_slice(text.replace('\n', "\r\n").as_bytes()),
    }
    out
}

// ---------------------------------------------------------------- helpers

/// 1-based line number of byte `offset`.
#[allow(
    clippy::naive_bytecount,
    reason = "edited buffers are small; a counting crate is not worth a dependency"
)]
fn line_at(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// Byte offset of the start of each line.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn indent_width(line: &str) -> usize {
    let mut width = 0;
    for c in line.chars() {
        match c {
            ' ' => width += 1,
            '\t' => width += 4 - (width % 4),
            _ => break,
        }
    }
    width
}

/// A line with its indentation removed and trailing whitespace trimmed;
/// `relaxed` also collapses interior runs of blanks.
fn flatten_line(line: &str, relaxed: bool) -> String {
    let trimmed = line.trim();
    if !relaxed {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut in_blank = false;
    for c in trimmed.chars() {
        if c == ' ' || c == '\t' {
            if !in_blank {
                out.push(' ');
            }
            in_blank = true;
        } else {
            out.push(c);
            in_blank = false;
        }
    }
    out
}

/// Levenshtein similarity in `0.0..=1.0` over chars, §6.3.2's `s_text`. Long
/// inputs compare their first and last 2,000 characters.
fn similarity(a: &str, b: &str) -> f64 {
    fn clip(text: &str) -> Vec<char> {
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= 4000 {
            return chars;
        }
        let mut out = chars[..2000].to_vec();
        out.extend_from_slice(&chars[chars.len() - 2000..]);
        out
    }
    let (a, b) = (clip(a), clip(b));
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    #[allow(clippy::cast_precision_loss, reason = "lengths are at most 4,000")]
    {
        1.0 - previous[b.len()] as f64 / longest as f64
    }
}

/// The whitespace runs of a line, tabs expanded to four spaces.
fn whitespace_runs(line: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    // Trailing whitespace is trimmed on both sides (§6.3.2 step 1), so it is
    // not a difference to be scored.
    for c in line.trim_end().chars() {
        if c == ' ' || c == '\t' {
            if c == '\t' {
                current.push_str("    ");
            } else {
                current.push(' ');
            }
        } else if !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

// ---------------------------------------------------------------- matching

/// Every non-overlapping occurrence of `needle`, as byte offsets.
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    haystack.match_indices(needle).map(|(i, _)| i).collect()
}

/// One fuzzy candidate window of `window` lines starting at line `start`.
#[derive(Debug, Clone)]
struct Candidate {
    start: usize,
    score: f64,
    line_exact: f64,
}

/// Score every plausible window of `lines` against the pattern.
fn fuzzy_candidates(lines: &[&str], pattern: &[String], mode: Fuzzy) -> Vec<Candidate> {
    let relaxed = mode == Fuzzy::Relaxed;
    let window = pattern.len();
    if window == 0 || lines.len() < window {
        return Vec::new();
    }
    let flat_pattern: Vec<String> = pattern.iter().map(|l| flatten_line(l, relaxed)).collect();
    let pattern_text = flat_pattern.join("\n");
    let pattern_indent = indent_width(&pattern[0]);
    let pattern_runs: Vec<Vec<String>> = pattern.iter().map(|l| whitespace_runs(l)).collect();
    let mut found = Vec::new();
    for start in 0..=lines.len() - window {
        let candidate = &lines[start..start + window];
        // Anchors (§6.3.2 step 2): the window must begin or end on a line
        // that equals the pattern's first or last line, indentation aside.
        let first_hit = flatten_line(candidate[0], relaxed) == flat_pattern[0];
        let last_hit = flatten_line(candidate[window - 1], relaxed) == flat_pattern[window - 1];
        if !first_hit && !last_hit {
            continue;
        }
        let flat: Vec<String> = candidate.iter().map(|l| flatten_line(l, relaxed)).collect();
        let exact = flat
            .iter()
            .zip(&flat_pattern)
            .filter(|(a, b)| a == b)
            .count();
        #[allow(clippy::cast_precision_loss, reason = "window sizes are tiny")]
        let line_exact = exact as f64 / window as f64;
        let s_text = similarity(&flat.join("\n"), &pattern_text);
        let cand_indent = indent_width(candidate[0]);
        let max_indent = cand_indent.max(pattern_indent).max(1);
        #[allow(clippy::cast_precision_loss, reason = "indents are tiny")]
        let s_indent = 1.0 - cand_indent.abs_diff(pattern_indent) as f64 / max_indent as f64;
        let (mut equal, mut total) = (0_usize, 0_usize);
        for (line, runs) in candidate.iter().zip(&pattern_runs) {
            let mine = whitespace_runs(line);
            total += mine.len().max(runs.len());
            equal += mine.iter().zip(runs).filter(|(a, b)| a == b).count();
        }
        #[allow(clippy::cast_precision_loss, reason = "run counts are tiny")]
        let s_ws = if total == 0 {
            1.0
        } else {
            equal as f64 / total as f64
        };
        let score = if relaxed {
            0.60 * s_text + 0.25 * s_ws + 0.15 * s_indent
        } else {
            0.70 * s_text + 0.20 * s_ws + 0.10 * s_indent
        };
        found.push(Candidate {
            start,
            score,
            line_exact,
        });
    }
    found
}

/// A fuzzy match located in `text`: the byte span it covers and its score.
struct Located {
    span: (usize, usize),
    score: f64,
    line: usize,
}

fn locate_fuzzy(text: &str, old: &str, mode: Fuzzy) -> Result<Located, EditError> {
    let lines: Vec<&str> = text.split('\n').collect();
    let starts = line_starts(text);
    let relaxed = mode == Fuzzy::Relaxed;
    // §6.3.2 step 1: trailing whitespace goes, blank lines at either end go.
    let mut pattern: Vec<String> = old.split('\n').map(|l| l.trim_end().to_string()).collect();
    while pattern.first().is_some_and(|l| l.trim().is_empty()) {
        pattern.remove(0);
    }
    while pattern.last().is_some_and(|l| l.trim().is_empty()) {
        pattern.pop();
    }
    if pattern.is_empty() {
        return Err(EditError::NoMatch { closest: None });
    }
    let window = pattern.len();
    let mut candidates = fuzzy_candidates(&lines, &pattern, mode);
    // REQ-TOOL-012: with more than one line, the window must be at least 80%
    // line-exact; a single line must match outright (it is only whitespace
    // that differs).
    let needed = if window > 1 { 0.8 } else { 1.0 };
    let closest = candidates
        .iter()
        .max_by(|a, b| a.score.total_cmp(&b.score).then(b.start.cmp(&a.start)))
        .map(|c| Closest {
            line: c.start + 1,
            score: c.score,
        });
    candidates.retain(|c| c.line_exact + f64::EPSILON >= needed);
    let Some(best) = candidates
        .iter()
        .max_by(|a, b| a.score.total_cmp(&b.score).then(b.start.cmp(&a.start)))
        .cloned()
    else {
        return Err(EditError::NoMatch { closest });
    };
    if best.score < mode.threshold() {
        return Err(EditError::NoMatch { closest });
    }
    // Step 5: a *different* place scoring within 0.03 is ambiguous. Windows
    // that overlap the best one are the same place, shifted.
    let rival = candidates
        .iter()
        .filter(|c| c.start + window <= best.start || c.start >= best.start + window)
        .max_by(|a, b| a.score.total_cmp(&b.score).then(b.start.cmp(&a.start)));
    if let Some(rival) = rival {
        if rival.score >= best.score - 0.03 {
            let (a, b) = if rival.start < best.start {
                (rival, &best)
            } else {
                (&best, rival)
            };
            return Err(EditError::Ambiguous {
                lines: vec![a.start + 1, b.start + 1],
                scores: vec![a.score, b.score],
                expected: 1,
            });
        }
    }
    let begin = starts[best.start];
    let last = best.start + window - 1;
    let finish = starts[last] + lines[last].len();
    let _ = relaxed;
    Ok(Located {
        span: (begin, finish),
        score: best.score,
        line: best.start + 1,
    })
}

// ------------------------------------------------------------------ apply

fn replace_spans(text: &str, spans: &[(usize, usize)], new: &str) -> (String, Vec<(usize, usize)>) {
    let mut out = String::with_capacity(text.len());
    let mut new_spans = Vec::with_capacity(spans.len());
    let mut cursor = 0;
    for (start, end) in spans {
        out.push_str(&text[cursor..*start]);
        let from = out.len();
        out.push_str(new);
        new_spans.push((from, out.len()));
        cursor = *end;
    }
    out.push_str(&text[cursor..]);
    (out, new_spans)
}

/// Apply one edit to LF-normalised `text` (§6.3.1, §6.3.2).
///
/// # Errors
/// [`EditError`].
pub fn apply(text: &str, spec: &EditSpec) -> Result<(String, Applied), EditError> {
    let old = normalise(&spec.old);
    let new = normalise(&spec.new);
    if old.is_empty() {
        return Err(EditError::EmptyPattern);
    }
    if old == new {
        return Err(EditError::NoChange);
    }
    let expect = spec.expect_occurrences.unwrap_or(1);
    let hits = find_all(text, &old);
    let k = hits.len();

    if k == 0 {
        // §6.3.1: an explicit `expect_occurrences` means the caller is sure
        // of the text; do not guess.
        if spec.fuzzy == Fuzzy::Off || spec.expect_occurrences.is_some() {
            return Err(EditError::NoMatch { closest: None });
        }
        let located = locate_fuzzy(text, &old, spec.fuzzy)?;
        // The span is whole lines; the replacement covers the trailing line
        // break only if the model's own pattern ended with one.
        let (mut start, mut end) = located.span;
        if old.ends_with('\n') && text[end..].starts_with('\n') {
            end += 1;
        }
        let matched_text = text[start..end].to_string();
        // A pattern that began mid-line keeps the original indentation of
        // the first line: replace from the first non-blank character only
        // when the model's pattern itself had no leading indentation.
        if !old.starts_with([' ', '\t']) {
            let leading = text[start..end]
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(char::len_utf8)
                .sum::<usize>();
            if !new.starts_with([' ', '\t']) {
                start += leading;
            }
        }
        let (out, spans) = replace_spans(text, &[(start, end)], &new);
        let line = line_at(&out, spans[0].0);
        let note = (located.score < spec.fuzzy.confident()).then_some(FuzzyNote {
            score: located.score,
            matched_text,
        });
        let _ = located.line;
        return Ok((
            out.clone(),
            Applied {
                occurrences: 1,
                replaced: 1,
                start_line: line,
                end_line: line_at(&out, spans[0].1.saturating_sub(1).max(spans[0].0)),
                fuzzy_used: true,
                fuzzy_score: Some(located.score),
                fuzzy_note: note,
                new_spans: spans,
            },
        ));
    }

    // k >= 1 — the table of §6.3.1.
    let applies = if spec.replace_all {
        k >= expect
    } else {
        k == expect
    };
    if !applies {
        if k < expect {
            return Err(EditError::NoMatch { closest: None });
        }
        return Err(EditError::Ambiguous {
            lines: hits.iter().map(|h| line_at(text, *h)).collect(),
            scores: Vec::new(),
            expected: expect,
        });
    }
    let spans: Vec<(usize, usize)> = hits.iter().map(|h| (*h, *h + old.len())).collect();
    let (out, new_spans) = replace_spans(text, &spans, &new);
    let first = new_spans[0];
    let last_end = new_spans[new_spans.len() - 1].1;
    Ok((
        out.clone(),
        Applied {
            occurrences: k,
            replaced: k,
            start_line: line_at(&out, first.0),
            end_line: line_at(&out, last_end.saturating_sub(1).max(first.0)),
            fuzzy_used: false,
            fuzzy_score: None,
            fuzzy_note: None,
            new_spans,
        },
    ))
}

/// Apply `edits` in order to one buffer, all or nothing (§6.2.4, §6.3.5).
///
/// Overlap is judged against the *original* text, before anything moves:
/// two edits aiming at the same lines are a mistake worth naming, whereas an
/// edit that depends on an earlier edit's output is legitimate and simply
/// has to match the buffer as it then stands.
///
/// # Errors
/// [`MultiError`]; the input text is untouched either way.
pub fn apply_multi(text: &str, edits: &[EditSpec]) -> Result<(String, Vec<Applied>), MultiError> {
    // Conflict detection on the original text.
    let mut ranges: Vec<(usize, usize, usize)> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let old = normalise(&edit.old);
        if old.is_empty() {
            continue;
        }
        let exact = find_all(text, &old);
        let spans: Vec<(usize, usize)> =
            if exact.is_empty() && edit.fuzzy != Fuzzy::Off && edit.expect_occurrences.is_none() {
                locate_fuzzy(text, &old, edit.fuzzy)
                    .map(|l| vec![l.span])
                    .unwrap_or_default()
            } else if edit.replace_all || exact.len() == edit.expect_occurrences.unwrap_or(1) {
                exact.iter().map(|h| (*h, *h + old.len())).collect()
            } else {
                exact
                    .first()
                    .map(|h| vec![(*h, *h + old.len())])
                    .unwrap_or_default()
            };
        for (s, e) in spans {
            ranges.push((s, e, index));
        }
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a.2 != b.2 && b.0 < a.1 {
            return Err(MultiError::Conflict {
                a: a.2.min(b.2),
                b: a.2.max(b.2),
            });
        }
    }
    // Sequential application.
    let mut current = text.to_string();
    let mut applied = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        match apply(&current, edit) {
            Ok((next, done)) => {
                current = next;
                applied.push(done);
            }
            Err(error) => return Err(MultiError::Partial { index, error }),
        }
    }
    Ok((current, applied))
}

/// The 1-based lines `(first, last)` of `new` that differ from `old`, or
/// `None` when the texts are identical: used to decide whether a syntax error
/// is "near the edit" (§6.3.6 step 3).
#[must_use]
pub fn changed_lines(old: &str, new: &str) -> Option<(usize, usize)> {
    if old == new {
        return None;
    }
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut head = 0;
    while head < old_lines.len() && head < new_lines.len() && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < old_lines.len() - head
        && tail < new_lines.len() - head
        && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
    {
        tail += 1;
    }
    let first = head + 1;
    let last = (new_lines.len() - tail).max(first);
    Some((first, last))
}

/// Lines `1..` of `text` that appear in `a` but not `b`, for stale-file
/// reports: a unified-ish diff, at most `max_lines` long.
#[must_use]
pub fn brief_diff(old: &str, new: &str, max_lines: usize) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    let mut shown: BTreeSet<usize> = BTreeSet::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => continue,
        };
        let index = change.new_index().or(change.old_index()).unwrap_or(0);
        if out.len() >= max_lines {
            out.push("… (more differences not shown)".to_string());
            break;
        }
        shown.insert(index);
        out.push(format!("{sign}{}", change.value().trim_end_matches('\n')));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> EditSpec {
        EditSpec::new(old, new)
    }

    fn run(text: &str, spec: &EditSpec) -> Result<(String, Applied), EditError> {
        apply(text, spec)
    }

    // ---- exact matching (§6.3.1) ----

    #[test]
    fn a_unique_match_is_replaced_and_located() {
        let (out, done) = run("a\nb\nc\n", &edit("b", "B")).expect("applies");
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!((done.occurrences, done.replaced), (1, 1));
        assert_eq!((done.start_line, done.end_line), (2, 2));
        assert!(!done.fuzzy_used && done.fuzzy_score.is_none());
    }

    /// T-EDIT-001.
    #[test]
    fn t_edit_001_an_absent_pattern_is_nomatch() {
        let err = run("alpha\nbeta\n", &edit("zzz_not_here_zzz", "x")).expect_err("no match");
        assert!(matches!(err, EditError::NoMatch { .. }));
    }

    /// T-EDIT-002: the error names every line.
    #[test]
    fn t_edit_002_several_matches_are_ambiguous_with_their_lines() {
        let mut text = String::new();
        for i in 1..=600 {
            text.push_str(if [42, 118, 501].contains(&i) {
                "dup();\n"
            } else {
                "other();\n"
            });
        }
        let err = run(&text, &edit("dup();", "x();")).expect_err("ambiguous");
        assert_eq!(
            err,
            EditError::Ambiguous {
                lines: vec![42, 118, 501],
                scores: vec![],
                expected: 1
            }
        );
    }

    /// T-EDIT-004.
    #[test]
    fn t_edit_004_old_equal_to_new_is_nochange() {
        assert_eq!(
            run("a\n", &edit("a", "a")).expect_err("nochange"),
            EditError::NoChange
        );
        assert_eq!(
            run("a\n", &edit("", "a")).expect_err("empty"),
            EditError::EmptyPattern
        );
    }

    /// T-EDIT-018.
    #[test]
    fn t_edit_018_replace_all_takes_every_match() {
        let mut spec = edit("foo", "bar");
        spec.replace_all = true;
        let (out, done) = run("foo foo\nfoo\n", &spec).expect("applies");
        assert_eq!(out, "bar bar\nbar\n");
        assert_eq!((done.occurrences, done.replaced), (3, 3));
        assert_eq!((done.start_line, done.end_line), (1, 2));
    }

    /// T-EDIT-019: `expect_occurrences` of 2 against a single match.
    #[test]
    fn t_edit_019_fewer_matches_than_expected_is_nomatch() {
        let mut spec = edit("only", "x");
        spec.expect_occurrences = Some(2);
        assert!(matches!(
            run("only once\n", &spec).expect_err("short"),
            EditError::NoMatch { .. }
        ));
    }

    #[test]
    fn expect_occurrences_equal_to_the_count_replaces_them_all() {
        let mut spec = edit("x", "y");
        spec.expect_occurrences = Some(2);
        assert_eq!(run("x x z\n", &spec).expect("applies").0, "y y z\n");
        spec.expect_occurrences = Some(1);
        assert!(matches!(
            run("x x z\n", &spec).expect_err("two"),
            EditError::Ambiguous { expected: 1, .. }
        ));
    }

    #[test]
    fn replace_all_needs_at_least_the_expected_count() {
        let mut spec = edit("x", "y");
        spec.replace_all = true;
        spec.expect_occurrences = Some(3);
        assert!(matches!(
            run("x x\n", &spec).expect_err("short"),
            EditError::NoMatch { .. }
        ));
        assert_eq!(run("x x x x\n", &spec).expect("four").1.replaced, 4);
    }

    #[test]
    fn matches_do_not_overlap_when_counted() {
        let mut spec = edit("aa", "b");
        spec.replace_all = true;
        let (out, done) = run("aaaa\n", &spec).expect("applies");
        assert_eq!(out, "bb\n");
        assert_eq!(done.occurrences, 2, "left to right, non-overlapping");
    }

    #[test]
    fn a_multiline_replacement_reports_the_new_span() {
        let (out, done) = run("a\nb\nc\n", &edit("b", "b1\nb2\nb3")).expect("applies");
        assert_eq!(out, "a\nb1\nb2\nb3\nc\n");
        assert_eq!((done.start_line, done.end_line), (2, 4));
    }

    #[test]
    fn deleting_text_is_an_edit_with_an_empty_replacement() {
        let (out, _) = run("keep\ndrop me\nkeep\n", &edit("drop me\n", "")).expect("applies");
        assert_eq!(out, "keep\nkeep\n");
    }

    // ---- fuzzy matching (§6.3.2) ----

    /// T-EDIT-010: tabs versus spaces.
    #[test]
    fn t_edit_010_tabs_for_spaces_matches_fuzzily() {
        let text = "fn main() {\n\tlet x = 1;\n\tlet y = 2;\n}\n";
        let spec = edit(
            "    let x = 1;\n    let y = 2;",
            "\tlet x = 10;\n\tlet y = 20;",
        );
        let (out, done) = run(text, &spec).expect("fuzzy match");
        assert!(done.fuzzy_used);
        assert!(
            done.fuzzy_score.expect("score") >= 0.92,
            "{:?}",
            done.fuzzy_score
        );
        assert!(out.contains("\tlet x = 10;\n\tlet y = 20;"), "{out}");
        assert!(out.starts_with("fn main() {\n") && out.ends_with("}\n"));
    }

    /// T-EDIT-011 / REQ-TOOL-010: same input, same match, every time.
    #[test]
    fn t_edit_011_fuzzy_matching_is_deterministic() {
        let text = "a\n  x = 1;\n  y = 2;\nb\n  x = 1;  \n  y = 2;\nc\n";
        let spec = edit("x = 1;\ny = 2;", "z");
        let first = run(text, &spec);
        for _ in 0..100 {
            assert_eq!(run(text, &spec), first);
        }
    }

    /// T-EDIT-012 (property form): a fuzzy replacement changes only the
    /// matched lines. Random buffers, deterministic generator.
    #[test]
    fn t_edit_012_only_the_matched_span_changes() {
        let mut seed = 0x1234_5678_u64;
        let mut next = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            usize::try_from(seed >> 40).expect("small")
        };
        let vocab = [
            "alpha", "beta", "gamma", "delta", "eps()", "zeta;", "{", "}",
        ];
        for _ in 0..300 {
            let lines: Vec<String> = (0..12 + next() % 20)
                .map(|_| {
                    format!(
                        "{}{}",
                        " ".repeat(next() % 3 * 2),
                        vocab[next() % vocab.len()]
                    )
                })
                .collect();
            let at = next() % (lines.len() - 2);
            // The pattern is lines `at..at+2` with different indentation.
            let pattern = format!("{}\n{}", lines[at].trim_start(), lines[at + 1].trim_start());
            let text = lines.join("\n") + "\n";
            let spec = edit(&pattern, "REPLACED");
            if let Ok((out, done)) = run(&text, &spec) {
                // Removing the replacement from `out` must leave `text`
                // with only the matched lines gone — nothing else moved.
                let (s, e) = done.new_spans[0];
                assert_eq!(&out[s..e], "REPLACED");
                let rebuilt_prefix = &out[..s];
                let rebuilt_suffix = &out[e..];
                assert!(
                    text.starts_with(rebuilt_prefix.trim_end_matches([' ', '\t']))
                        || text.starts_with(rebuilt_prefix)
                );
                assert!(text.ends_with(rebuilt_suffix), "suffix intact");
                assert!(out.len() <= text.len() + "REPLACED".len());
            }
        }
    }

    /// T-EDIT-013: a multi-line pattern with one changed interior line.
    #[test]
    fn t_edit_013_one_changed_line_in_five_matches_under_relaxed() {
        let text = "fn f() {\n    a();\n    b();\n    c();\n    d();\n}\n";
        let spec = EditSpec {
            fuzzy: Fuzzy::Relaxed,
            ..edit(
                "fn f() {\n    a();\n    B_CHANGED();\n    c();\n    d();",
                "fn g() {",
            )
        };
        let (out, done) = run(text, &spec).expect("≥ 80% line-exact");
        assert!(done.fuzzy_used);
        assert_eq!(out, "fn g() {\n}\n");
    }

    /// T-EDIT-014: three of five lines altered is not a match.
    #[test]
    fn t_edit_014_too_many_altered_lines_is_nomatch() {
        let text = "fn f() {\n    a();\n    b();\n    c();\n    d();\n}\n";
        let spec = EditSpec {
            fuzzy: Fuzzy::Relaxed,
            ..edit("fn f() {\n    X();\n    Y();\n    Z();\n    d();", "gone")
        };
        match run(text, &spec).expect_err("below 80%") {
            EditError::NoMatch { closest: Some(c) } => assert_eq!(c.line, 1),
            other => panic!("{other:?}"),
        }
    }

    /// T-EDIT-015: two places that look equally right (both have trailing
    /// whitespace the pattern lacks, so neither is an exact match).
    #[test]
    fn t_edit_015_two_close_candidates_are_ambiguous() {
        let text = "start\nalpha();  \nbeta();\nmiddle\nalpha();  \nbeta();\nend\n";
        let err = run(text, &edit("alpha();\nbeta();", "x")).expect_err("two candidates");
        match err {
            EditError::Ambiguous { lines, scores, .. } => {
                assert_eq!(lines, vec![2, 5]);
                assert_eq!(scores.len(), 2);
                assert!(scores.iter().all(|s| *s >= 0.92), "{scores:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// T-EDIT-016: a match scoring between 0.92 and 0.97 is applied *and*
    /// reported, with the text that was really there. (Five lines, one of
    /// them reworded: ≥ 80% line-exact, a modest text score.)
    #[test]
    fn t_edit_016_a_modest_score_warns_with_the_matched_text() {
        let text = "fn run() {\n    let a = load_configuration();\n    let sum = compute(first, second);\n    log(a);\n}\n";
        let pattern = "fn run() {\n    let a = load_configuration();\n    let total = compute(first, second);\n    log(a);\n}";
        let (out, done) = run(text, &edit(pattern, "fn run() {}")).expect("applies");
        assert!(done.fuzzy_used);
        let score = done.fuzzy_score.expect("score");
        assert!((0.92..0.97).contains(&score), "{score}");
        let note = done.fuzzy_note.expect("W-EDIT-FUZZY");
        assert!(note.matched_text.contains("let sum = compute"), "{note:?}");
        assert_eq!(out, "fn run() {}\n");
    }

    #[test]
    fn a_near_perfect_match_carries_no_warning() {
        let text = "x\n\tfoo();\ny\n";
        let (_, done) = run(text, &edit("    foo();", "bar();")).expect("applies");
        assert!(done.fuzzy_used);
        assert!(done.fuzzy_note.is_none(), "{:?}", done.fuzzy_score);
    }

    /// T-EDIT-017.
    #[test]
    fn t_edit_017_fuzzy_off_does_not_forgive_indentation() {
        let spec = EditSpec {
            fuzzy: Fuzzy::Off,
            ..edit("    foo();", "bar();")
        };
        assert!(matches!(
            run("x\n\tfoo();\n", &spec).expect_err("strict"),
            EditError::NoMatch { closest: None }
        ));
    }

    #[test]
    fn a_wrong_pattern_is_not_forced_into_a_match() {
        let text = "fn alpha() {}\nfn beta() {}\n";
        assert!(matches!(
            run(
                text,
                &edit(
                    "fn completely_different(x: Vec<String>) -> Result<(), Error> {",
                    "x"
                )
            )
            .expect_err("no"),
            EditError::NoMatch { .. }
        ));
    }

    #[test]
    fn the_nomatch_error_points_at_the_nearest_line() {
        let text = "one\ntwo\n    needle(x);\nfour\n";
        match run(text, &edit("needle(y);", "z")).expect_err("no match") {
            EditError::NoMatch { closest } => {
                if let Some(c) = closest {
                    assert_eq!(c.line, 3);
                }
            }
            other => panic!("{other:?}"),
        }
    }

    /// REQ-TOOL-011 at the byte level: only the matched line changes.
    #[test]
    fn a_fuzzy_replacement_touches_nothing_outside_the_matched_lines() {
        let text = "keep 1\n\tmatched_line();\nkeep 2\n";
        let (out, done) = run(text, &edit("    matched_line();", "replaced();")).expect("applies");
        assert!(out.starts_with("keep 1\n") && out.ends_with("\nkeep 2\n"));
        assert_eq!(out.lines().count(), 3);
        assert_eq!(done.start_line, 2);
        assert!(out.contains("replaced();"));
    }

    // ---- line endings and BOM (§6.3.4) ----

    /// T-EDIT-020: an LF pattern against a CRLF file; CRLF survives.
    #[test]
    fn t_edit_020_crlf_files_are_matched_and_written_back_as_crlf() {
        let original = b"first\r\nsecond\r\nthird\r\n";
        let doc = Document::decode(original).expect("utf-8");
        assert_eq!(doc.eol, Eol::Crlf);
        assert!(!doc.mixed);
        let (out, _) = run(&doc.text, &edit("first\nsecond", "ONE\nTWO")).expect("applies");
        assert_eq!(doc.encode(&out), b"ONE\r\nTWO\r\nthird\r\n");
        // A pattern that itself carries CRLF is normalised before searching.
        let (out, _) = run(&doc.text, &edit("first\r\nsecond", "x")).expect("applies");
        assert_eq!(doc.encode(&out), b"x\r\nthird\r\n");
    }

    /// T-EDIT-025: the BOM is restored byte for byte.
    #[test]
    fn t_edit_025_a_bom_is_retained() {
        let original = b"\xEF\xBB\xBFhello\nworld\n";
        let doc = Document::decode(original).expect("utf-8");
        assert!(doc.had_bom);
        assert!(!doc.text.starts_with('\u{FEFF}'));
        let (out, _) = run(&doc.text, &edit("hello", "HELLO")).expect("applies");
        assert_eq!(doc.encode(&out), b"\xEF\xBB\xBFHELLO\nworld\n");
    }

    #[test]
    fn mixed_endings_follow_the_majority_and_say_so() {
        let doc = Document::decode(b"a\r\nb\r\nc\nd\r\n").expect("utf-8");
        assert!(doc.mixed);
        assert_eq!(doc.eol, Eol::Crlf);
        let lf = Document::decode(b"a\nb\nc\r\n").expect("utf-8");
        assert_eq!(lf.eol, Eol::Lf);
        assert_eq!(
            Document::decode(b"no newline").expect("utf-8").eol,
            Eol::Lf,
            "a tie is LF"
        );
    }

    #[test]
    fn a_round_trip_with_no_edit_is_byte_exact() {
        for bytes in [
            &b"a\nb\n"[..],
            b"a\r\nb\r\n",
            b"\xEF\xBB\xBFa\r\nb",
            b"",
            b"no eol",
        ] {
            let doc = Document::decode(bytes).expect("utf-8");
            assert_eq!(doc.encode(&doc.text), bytes, "{bytes:?}");
        }
    }

    #[test]
    fn invalid_utf8_is_reported_with_its_offset() {
        assert_eq!(
            Document::decode(b"ok\xFFbad").expect_err("invalid"),
            NotUtf8 { valid_up_to: 2 }
        );
    }

    // ---- multi_edit (§6.3.5) ----

    /// T-EDIT-005.
    #[test]
    fn t_edit_005_overlapping_edits_conflict() {
        let text = "alpha beta gamma\n";
        let edits = [edit("alpha beta", "A"), edit("beta gamma", "B")];
        assert_eq!(
            apply_multi(text, &edits).expect_err("overlap"),
            MultiError::Conflict { a: 0, b: 1 }
        );
    }

    /// T-EDIT-006: the second of three does not match.
    #[test]
    fn t_edit_006_a_failing_edit_is_partial_and_names_its_index() {
        let text = "one\ntwo\nthree\n";
        let edits = [edit("one", "1"), edit("missing", "x"), edit("three", "3")];
        match apply_multi(text, &edits).expect_err("fails") {
            MultiError::Partial { index, error } => {
                assert_eq!(index, 1);
                assert!(matches!(error, EditError::NoMatch { .. }));
            }
            other @ MultiError::Conflict { .. } => panic!("{other:?}"),
        }
    }

    #[test]
    fn disjoint_edits_apply_in_order() {
        let (out, done) =
            apply_multi("a\nb\nc\n", &[edit("a", "A"), edit("c", "C")]).expect("applies");
        assert_eq!(out, "A\nb\nC\n");
        assert_eq!(done.len(), 2);
    }

    /// An edit may build on an earlier one's output.
    #[test]
    fn a_later_edit_can_depend_on_an_earlier_one() {
        let (out, _) = apply_multi(
            "x = 1\n",
            &[
                edit("x = 1", "x = compute(1)"),
                edit("compute(1)", "compute(2)"),
            ],
        )
        .expect("applies");
        assert_eq!(out, "x = compute(2)\n");
    }

    /// An edit whose target an earlier edit consumes is not a quiet skip: in
    /// the original text the two overlap, which is a conflict.
    #[test]
    fn an_edit_aimed_at_text_another_edit_consumes_is_a_conflict() {
        let err = apply_multi(
            "alpha beta\n",
            &[edit("alpha ", ""), edit("beta", "B"), edit("alpha", "z")],
        )
        .expect_err("overlap");
        assert_eq!(err, MultiError::Conflict { a: 0, b: 2 });
    }

    /// Whereas a target that exists nowhere is `E-EDIT-PARTIAL`, with the
    /// buffer as the earlier edits left it.
    #[test]
    fn an_edit_whose_target_never_existed_is_partial() {
        let err =
            apply_multi("one\n", &[edit("one", "1"), edit("two", "2")]).expect_err("no match");
        assert!(matches!(err, MultiError::Partial { index: 1, .. }));
    }

    // ---- helpers ----

    #[test]
    fn changed_lines_brackets_the_difference() {
        assert_eq!(changed_lines("a\nb\nc\n", "a\nB\nc\n"), Some((2, 2)));
        assert_eq!(changed_lines("a\nb\nc\n", "a\nb1\nb2\nc\n"), Some((2, 3)));
        assert_eq!(
            changed_lines("a\n", "a\n"),
            None,
            "nothing changed, nothing to bracket"
        );
        assert_eq!(
            changed_lines("a\nb\n", "a\n"),
            Some((2, 2)),
            "a deletion points at where it was"
        );
    }

    #[test]
    fn similarity_is_symmetric_bounded_and_exact_for_equal_text() {
        assert!((similarity("same", "same") - 1.0).abs() < f64::EPSILON);
        assert!(similarity("abc", "xyz") < 0.01);
        let (ab, ba) = (
            similarity("kitten", "sitting"),
            similarity("sitting", "kitten"),
        );
        assert!((ab - ba).abs() < 1e-12);
        assert!((0.0..=1.0).contains(&ab));
        assert!((similarity("", "") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_brief_diff_shows_changes_and_stops_at_the_limit() {
        let old = (1..=100).fold(String::new(), |mut acc, i| {
            use std::fmt::Write as _;
            let _ = writeln!(acc, "line {i}");
            acc
        });
        let new = (1..=100)
            .map(|i| {
                if i % 2 == 0 {
                    format!("changed {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect::<String>();
        let diff = brief_diff(&old, &new, 20);
        assert!(diff.lines().count() <= 21, "{}", diff.lines().count());
        assert!(diff.contains("-line 2") && diff.contains("+changed 2"));
        assert!(diff.contains("more differences not shown"));
    }
}
