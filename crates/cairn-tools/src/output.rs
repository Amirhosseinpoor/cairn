//! What happens to a tool's output before the model sees it (SPEC §6.5 steps
//! 9–10, §5.5, §9.6, §9.7): cut it to size deterministically, then scrub it.

use cairn_core::redact::{sanitize_untrusted, Redactor};
use serde_json::Value;

/// §5.5: a line is cut at this many characters.
pub const MAX_LINE_CHARS: usize = 2000;

/// §5.5's "syntax-error block" markers: a dropped region containing one of
/// these keeps the whole block (at most [`MAX_KEPT_BLOCKS`] of them).
const ERROR_MARKERS: [&str; 5] = [
    "error[",
    "Traceback (most recent call last)",
    "FAIL",
    "✗",
    "ERROR:",
];
const MAX_KEPT_BLOCKS: usize = 8;
/// A kept block ends at a blank line, or after this many lines.
const MAX_BLOCK_LINES: usize = 12;

/// The §5.5 marker, verbatim (REQ-CTX-013).
fn marker(dropped_lines: usize, dropped_bytes: usize) -> String {
    format!(
        "... [cairn: truncated {} lines ({}) from middle; head 60% / tail 40% kept] ...",
        group(dropped_lines),
        human(dropped_bytes)
    )
}

fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MB", bytes / (1024 * 1024))
    } else {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    }
}

/// Cut one line at [`MAX_LINE_CHARS`] characters, on a character boundary.
fn cap_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}…[line truncated]")
}

/// What [`truncate_text`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    pub dropped_lines: usize,
}

/// The first line index of each error block inside `lines[from..to]`, with
/// the exclusive end of the block.
fn error_blocks(lines: &[String], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut i = from;
    while i < to && blocks.len() < MAX_KEPT_BLOCKS {
        if ERROR_MARKERS.iter().any(|m| lines[i].contains(m)) {
            let mut end = i + 1;
            while end < to && end - i < MAX_BLOCK_LINES && !lines[end].trim().is_empty() {
                end += 1;
            }
            blocks.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    blocks
}

fn bytes_of(lines: &[String]) -> usize {
    lines.iter().map(|l| l.len() + 1).sum()
}

/// Cut `text` to at most about `max_bytes`: keep the head (60%) and tail
/// (40%) by lines, say so with the §5.5 marker, and keep whole any error
/// blocks that fell in the dropped middle. Pure and deterministic
/// (REQ-CTX-012): the same text and limit always give the same result.
#[must_use]
pub fn truncate_text(text: &str, max_bytes: usize) -> Truncated {
    let lines: Vec<String> = text.split('\n').map(cap_line).collect();
    if bytes_of(&lines) <= max_bytes {
        let joined = lines.join("\n");
        let changed = joined != text;
        return Truncated {
            text: joined,
            truncated: changed,
            dropped_lines: 0,
        };
    }

    // Reserve room for the marker, then give the retained blocks first call
    // on the budget; head and tail share what is left 60/40.
    let reserve = marker(lines.len(), text.len()).len() + 2;
    let usable = max_bytes.saturating_sub(reserve);
    let mut kept: Vec<(usize, usize)> = Vec::new();
    // A first pass over the whole text finds candidate blocks; those inside
    // the final head or tail are dropped from the list below.
    let candidates = error_blocks(&lines, 0, lines.len());
    let block_budget = usable / 4;
    let mut spent = 0;
    for (start, end) in candidates {
        let size = bytes_of(&lines[start..end]);
        if spent + size <= block_budget {
            kept.push((start, end));
            spent += size;
        }
    }
    let remaining = usable.saturating_sub(spent);
    let head_budget = remaining * 60 / 100;
    let tail_budget = remaining - head_budget;

    let mut head_end = 0;
    let mut used = 0;
    while head_end < lines.len() && used + lines[head_end].len() < head_budget {
        used += lines[head_end].len() + 1;
        head_end += 1;
    }
    let mut tail_start = lines.len();
    used = 0;
    while tail_start > head_end && used + lines[tail_start - 1].len() < tail_budget {
        used += lines[tail_start - 1].len() + 1;
        tail_start -= 1;
    }
    // Blocks wholly inside head or tail are already there.
    let middle_blocks: Vec<(usize, usize)> = kept
        .into_iter()
        .filter(|(s, e)| *s >= head_end && *e <= tail_start)
        .collect();

    let blocks_lines: usize = middle_blocks.iter().map(|(s, e)| e - s).sum();
    let dropped_lines = tail_start - head_end - blocks_lines;
    let dropped_bytes = bytes_of(&lines[head_end..tail_start])
        - middle_blocks
            .iter()
            .map(|(s, e)| bytes_of(&lines[*s..*e]))
            .sum::<usize>();

    let mut out: Vec<String> = lines[..head_end].to_vec();
    out.push(marker(dropped_lines, dropped_bytes));
    for (i, (start, end)) in middle_blocks.iter().enumerate() {
        if i > 0 {
            out.push("...".to_string());
        }
        out.extend(lines[*start..*end].iter().cloned());
    }
    if !middle_blocks.is_empty() {
        out.push("...".to_string());
    }
    out.extend(lines[tail_start..].iter().cloned());
    Truncated {
        text: out.join("\n"),
        truncated: true,
        dropped_lines,
    }
}

/// The longest string leaf in `value`, as a mutable reference.
fn longest_string(value: &mut Value) -> Option<&mut String> {
    match value {
        Value::String(s) => Some(s),
        Value::Array(items) => items
            .iter_mut()
            .filter_map(longest_string)
            .max_by_key(|s| s.len()),
        Value::Object(map) => map
            .values_mut()
            .filter_map(longest_string)
            .max_by_key(|s| s.len()),
        _ => None,
    }
}

/// The array in `value` whose serialized form is largest, and that size.
fn heaviest_array(value: &mut Value) -> Option<(&mut Vec<Value>, usize)> {
    match value {
        Value::Array(items) => {
            let own = items.iter().map(|i| i.to_string().len() + 1).sum::<usize>();
            Some((items, own))
        }
        Value::Object(map) => map
            .values_mut()
            .filter_map(heaviest_array)
            .max_by_key(|(_, size)| *size),
        _ => None,
    }
}

/// Shrink `value` until its JSON is at most `max_bytes`: truncate the
/// longest string (head/tail), or — when a list of results is what makes it
/// big — drop items from the end of the heaviest array. The structure stays
/// intact, so a result the model must parse never turns into half a
/// document. Returns whether anything was cut.
pub fn fit(value: &mut Value, max_bytes: usize) -> bool {
    let mut cut = false;
    for _ in 0..64 {
        let size = value.to_string().len();
        if size <= max_bytes {
            return cut;
        }
        let over = size - max_bytes;
        let string_len = longest_string(value).map_or(0, |s| s.len());
        let array = heaviest_array(value);
        // Whichever is the bigger part of the problem is the one to cut.
        if let Some((items, array_bytes)) = array {
            if array_bytes > string_len && items.len() > 1 {
                let average = (array_bytes / items.len()).max(1);
                let drop = (over / average + 1).min(items.len() - 1);
                let keep = items.len() - drop;
                items.truncate(keep);
                cut = true;
                continue;
            }
        }
        let Some(longest) = longest_string(value) else {
            return cut;
        };
        // Aim a little under the limit so one pass usually suffices.
        let target = longest.len().saturating_sub(over + 64).max(256);
        if target >= longest.len() {
            return cut;
        }
        let truncated = truncate_text(longest, target);
        if truncated.text.len() >= longest.len() {
            return cut;
        }
        *longest = truncated.text;
        cut = true;
    }
    cut
}

/// Run every string in `value` through the redactor and the untrusted-content
/// sanitiser (§9.6 patterns, §9.7 mitigations 8 and 10). Returns whether the
/// sanitiser removed zero-width or ANSI bytes (`W-INJ-OBSCURE`).
pub fn scrub(value: &mut Value, redactor: &Redactor) -> bool {
    match value {
        Value::String(text) => {
            let (clean, removed) = sanitize_untrusted(text);
            *text = redactor.redact(&clean);
            removed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, item| scrub(item, redactor) | acc),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |acc, item| scrub(item, redactor) | acc),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line {i:04}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn small_text_is_untouched() {
        let t = truncate_text("a\nb\nc", 1000);
        assert_eq!(t.text, "a\nb\nc");
        assert!(!t.truncated);
    }

    /// REQ-CTX-013: the marker is present verbatim, with the real counts.
    #[test]
    fn big_text_keeps_head_and_tail_and_says_what_was_dropped() {
        let text = numbered(2000);
        let t = truncate_text(&text, 4096);
        assert!(t.truncated);
        assert!(t.text.len() <= 4096 + 128, "{} bytes", t.text.len());
        assert!(t.text.starts_with("line 0001\n"));
        assert!(t.text.ends_with("line 2000"));
        let marker_line = t
            .text
            .lines()
            .find(|l| l.contains("[cairn: truncated"))
            .expect("marker");
        assert!(
            marker_line.starts_with("... [cairn: truncated "),
            "{marker_line}"
        );
        assert!(
            marker_line.ends_with("from middle; head 60% / tail 40% kept] ..."),
            "{marker_line}"
        );
        assert!(t.dropped_lines > 1500, "{}", t.dropped_lines);
        // Head is about 60% of what was kept.
        let before = t
            .text
            .split("... [cairn")
            .next()
            .expect("head")
            .lines()
            .count();
        let after = t
            .text
            .split("kept] ...\n")
            .nth(1)
            .expect("tail")
            .lines()
            .count();
        assert!(before > after, "head {before} should outweigh tail {after}");
    }

    /// T-CTX-014: determinism.
    #[test]
    fn truncation_is_deterministic() {
        let text = format!(
            "{}\nerror[E0308]: x\n  --> a.rs\n\n{}",
            numbered(900),
            numbered(900)
        );
        let first = truncate_text(&text, 3000);
        for _ in 0..100 {
            assert_eq!(truncate_text(&text, 3000), first);
        }
    }

    /// §5.5's regex-preserving rule: an error block in the dropped middle
    /// survives whole.
    #[test]
    fn an_error_block_in_the_middle_is_kept_whole() {
        let mut lines: Vec<String> = (1..=1500).map(|i| format!("noise {i:04}")).collect();
        lines[700] = "error[E0308]: mismatched types".to_string();
        lines[701] = "  --> src/main.rs:7:5".to_string();
        lines[702] = "   = note: expected `u32`, found `&str`".to_string();
        lines[703] = String::new();
        let t = truncate_text(&lines.join("\n"), 4096);
        assert!(t.text.contains("error[E0308]: mismatched types"));
        assert!(t.text.contains("--> src/main.rs:7:5"));
        assert!(
            t.text.contains("expected `u32`, found `&str`"),
            "{}",
            t.text
        );
        assert!(t.text.contains("[cairn: truncated"));
    }

    #[test]
    fn at_most_eight_error_blocks_are_kept() {
        let mut lines: Vec<String> = (1..=3000).map(|i| format!("noise {i:04}")).collect();
        for k in 0..20 {
            lines[300 + k * 60] = format!("ERROR: failure number {k}");
        }
        let t = truncate_text(&lines.join("\n"), 8192);
        let kept = t.text.matches("ERROR: failure number").count();
        assert!(kept <= 8, "{kept} blocks kept");
        assert!(kept >= 1);
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let line = "é".repeat(2500);
        let t = truncate_text(&line, 100_000);
        assert!(t.truncated);
        assert!(t.text.ends_with("…[line truncated]"));
        assert_eq!(
            t.text.chars().count(),
            2000 + "…[line truncated]".chars().count()
        );
    }

    #[test]
    fn numbers_in_the_marker_are_grouped_and_sized() {
        assert_eq!(group(4318), "4,318");
        assert_eq!(group(12), "12");
        assert_eq!(group(1_000_000), "1,000,000");
        assert_eq!(human(182 * 1024), "182 KB");
        assert_eq!(human(3 * 1024 * 1024), "3 MB");
    }

    /// `fit` keeps JSON valid and under the cap by shrinking strings.
    #[test]
    fn fit_shrinks_the_longest_string_and_keeps_the_structure() {
        let mut v = json!({"path": "a.rs", "content": numbered(5000), "n": 3});
        assert!(fit(&mut v, 8192));
        assert!(v.to_string().len() <= 8192, "{}", v.to_string().len());
        assert_eq!(v["path"], "a.rs");
        assert_eq!(v["n"], 3);
        assert!(v["content"]
            .as_str()
            .expect("string")
            .contains("[cairn: truncated"));

        let mut small = json!({"a": "b"});
        assert!(!fit(&mut small, 8192));
    }

    #[test]
    fn scrub_redacts_every_string_and_reports_hidden_characters() {
        let redactor = cairn_core::redact::default_redactor();
        let mut v = json!({
            "a": "token = abcdef123456789",
            "b": ["fine", "key sk-abcdefghijklmnopqrstuvwxyz0123"],
            "c": {"d": "zero\u{200B}width"}
        });
        let removed = scrub(&mut v, redactor);
        assert!(removed, "the zero-width character was reported");
        let text = v.to_string();
        assert!(!text.contains("abcdef123456789"), "{text}");
        assert!(!text.contains("sk-abcdefghijklmnopqrstuvwxyz"), "{text}");
        assert!(text.contains("REDACTED"));
        assert_eq!(v["c"]["d"], "zerowidth");
        assert_eq!(v["b"][0], "fine");
    }
}
