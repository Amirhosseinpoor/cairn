//! The token budget (SPEC §5.4): how much of the model's window each kind of
//! content may take, and what gives way when it does not fit.
//!
//! Everything here is arithmetic on token counts. Counting is the caller's
//! business (a provider's tokenizer when there is one, the §4.8 estimate when
//! there is not), so the same rules serve a real request and a test.

use std::fmt::Write as _;

use cairn_core::error::codes;
use cairn_core::message::{Block, Message};
use cairn_core::tokens::estimate_tokens;

/// The window below which the fixed categories shrink (REQ-CTX-011).
const SMALL_WINDOW: u32 = 32_000;

/// Hard caps from §5.4's table.
const SYSTEM_CAP: u32 = 12_000;
const TOOLS_CAP: u32 = 6_000;
const REPO_MAP_CAP: u32 = 16_000;
const PINNED_CAP: u32 = 20_000;
const TOOL_OUTPUT_CAP: u32 = 8_000;
const RESERVE_CAP: u32 = 32_000;

/// What each category may use, in tokens, for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// The model's context window, `W`.
    pub window: u32,
    /// System prompt including `AGENTS.md`.
    pub system: u32,
    /// Tool definitions.
    pub tools: u32,
    pub repo_map: u32,
    /// Files pinned with `@file`.
    pub pinned: u32,
    /// Stored conversation (messages and the tool results already in them).
    pub history: u32,
    /// One live tool result.
    pub tool_output: u32,
    /// Space that must stay free for the answer.
    pub output_reserve: u32,
}

fn percent(window: u32, pct: u32) -> u32 {
    u32::try_from(u64::from(window) * u64::from(pct) / 100).unwrap_or(u32::MAX)
}

impl Budget {
    /// §5.4 for a window of `window` tokens.
    ///
    /// Above 32,000 tokens each category is its percentage of `W` bounded by
    /// its hard cap. At or below, the fixed categories' percentages scale by
    /// `W / 32,000` (REQ-CTX-011) so a small window is not eaten by prompt
    /// furniture, and the output reserve is never below 10% of `W`.
    #[must_use]
    pub fn for_window(window: u32) -> Self {
        let scaled = |pct: u32, cap: u32| {
            let share = if window <= SMALL_WINDOW {
                u32::try_from(
                    u64::from(percent(window, pct)) * u64::from(window) / u64::from(SMALL_WINDOW),
                )
                .unwrap_or(u32::MAX)
            } else {
                percent(window, pct)
            };
            share.min(cap)
        };
        let mut reserve = percent(window, 12).min(RESERVE_CAP);
        if window <= SMALL_WINDOW {
            reserve = reserve.max(percent(window, 10));
        }
        Self {
            window,
            system: scaled(8, SYSTEM_CAP),
            tools: scaled(6, TOOLS_CAP),
            repo_map: scaled(10, REPO_MAP_CAP),
            pinned: scaled(15, PINNED_CAP),
            history: percent(window, 55),
            tool_output: scaled(10, TOOL_OUTPUT_CAP),
            output_reserve: reserve,
        }
    }

    /// What a request may carry in total: the window less the reserve.
    #[must_use]
    pub const fn usable(&self) -> u32 {
        self.window.saturating_sub(self.output_reserve)
    }

    /// C-1's threshold: 80% of the usable window (§5.6).
    #[must_use]
    pub fn compaction_threshold(&self) -> u32 {
        percent(self.usable(), 80)
    }
}

/// A request that does not fit.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct BudgetError {
    pub code: &'static str,
    pub message: String,
}

/// REQ-CTX-010: `estimated + output_reserve <= W`, or the request must not be
/// sent. The caller's answer to a failure is compaction.
///
/// # Errors
/// [`BudgetError`] with `E-CTX-COMPACT` when the request is too large.
pub fn verify_budget(estimated: u32, budget: &Budget) -> Result<(), BudgetError> {
    let needed = u64::from(estimated) + u64::from(budget.output_reserve);
    if needed <= u64::from(budget.window) {
        return Ok(());
    }
    Err(BudgetError {
        code: codes::CTX_COMPACT,
        message: format!(
            "the request needs {estimated} tokens plus {} reserved for the answer, \
             but the window is {}",
            budget.output_reserve, budget.window
        ),
    })
}

/// Tokens in one message: its text, reasoning, tool calls and results, and a
/// little for the framing every provider adds.
#[must_use]
pub fn message_tokens(message: &Message) -> u32 {
    const FRAMING: u32 = 4;
    const IMAGE: u32 = 800;
    fn block(b: &Block) -> u32 {
        match b {
            Block::Text { text }
            | Block::Reasoning { text, .. }
            | Block::ThinkingPlaceholder { text } => estimate_tokens(text),
            Block::Image { .. } => IMAGE,
            Block::ToolCall { name, input, .. } => {
                estimate_tokens(name) + estimate_tokens(&input.to_string())
            }
            Block::ToolResult { content, .. } => content.iter().map(block).sum::<u32>() + 4,
        }
    }
    FRAMING + message.blocks.iter().map(block).sum::<u32>()
}

/// Tokens in a conversation.
#[must_use]
pub fn history_tokens(messages: &[Message]) -> u32 {
    messages.iter().map(message_tokens).sum()
}

/// A repository-map entry: a file, how relevant it is, and what to show.
#[derive(Debug, Clone, PartialEq)]
pub struct MapEntry {
    pub path: String,
    pub score: f64,
    pub text: String,
}

/// REQ-CTX-007: keep the highest-scored entries that fit `budget` tokens; the
/// rest are dropped, lowest score first. Entries come back best first.
#[must_use]
pub fn fit_repo_map(mut entries: Vec<MapEntry>, budget: u32) -> Vec<MapEntry> {
    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut used = 0_u32;
    let mut kept = Vec::new();
    for entry in entries {
        let cost = estimate_tokens(&entry.text) + 2;
        if used + cost > budget {
            // A smaller, lower-ranked entry may still fit; only the order of
            // preference is fixed.
            continue;
        }
        used += cost;
        kept.push(entry);
    }
    kept
}

/// A file pin that would not fit.
///
/// # Errors
/// `E-CTX-PINFULL` when `already + new` would exceed the pinned-files cap.
pub fn check_pin(already: u32, new: u32, budget: &Budget) -> Result<(), BudgetError> {
    if u64::from(already) + u64::from(new) <= u64::from(budget.pinned) {
        return Ok(());
    }
    Err(BudgetError {
        code: codes::CTX_PINFULL,
        message: format!(
            "pinning this file ({new} tokens) would take pinned files to {} of {} tokens",
            already.saturating_add(new),
            budget.pinned
        ),
    })
}

/// How much of `AGENTS.md` stays when the system prompt is over its share:
/// the first 2,000 tokens and the last 500 (§5.4), cut at line boundaries
/// with a marker between them.
#[must_use]
pub fn truncate_instructions(text: &str, max_tokens: u32) -> (String, bool) {
    if estimate_tokens(text) <= max_tokens {
        return (text.to_string(), false);
    }
    let head_tokens = 2_000.min(max_tokens.saturating_mul(4) / 5);
    let tail_tokens = 500.min(max_tokens / 5);
    let lines: Vec<&str> = text.lines().collect();
    let mut head = Vec::new();
    let mut used = 0;
    for line in &lines {
        let cost = estimate_tokens(line) + 1;
        if used + cost > head_tokens {
            break;
        }
        used += cost;
        head.push(*line);
    }
    let mut tail = Vec::new();
    let mut used = 0;
    for line in lines.iter().skip(head.len()).rev() {
        let cost = estimate_tokens(line) + 1;
        if used + cost > tail_tokens {
            break;
        }
        used += cost;
        tail.push(*line);
    }
    tail.reverse();
    let dropped = lines.len().saturating_sub(head.len() + tail.len());
    let mut out = head.join("\n");
    let _ = write!(
        out,
        "\n\n[cairn: {dropped} lines of project instructions omitted to fit the system prompt]\n\n"
    );
    out.push_str(&tail.join("\n"));
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::message::Role;

    #[test]
    fn a_large_window_takes_percentages_up_to_the_caps() {
        let b = Budget::for_window(200_000);
        assert_eq!(b.system, 12_000, "8% is 16,000, capped");
        assert_eq!(b.tools, 6_000);
        assert_eq!(b.repo_map, 16_000);
        assert_eq!(b.pinned, 20_000);
        assert_eq!(b.tool_output, 8_000);
        assert_eq!(b.history, 110_000);
        assert_eq!(b.output_reserve, 24_000);
        assert_eq!(b.usable(), 176_000);
    }

    #[test]
    fn a_middling_window_is_all_percentages() {
        let b = Budget::for_window(100_000);
        assert_eq!(b.system, 8_000);
        assert_eq!(b.repo_map, 10_000);
        assert_eq!(b.pinned, 15_000);
        assert_eq!(b.output_reserve, 12_000);
    }

    #[test]
    fn the_reserve_is_capped_for_huge_windows() {
        assert_eq!(Budget::for_window(1_000_000).output_reserve, 32_000);
    }

    /// T-CTX-013 / REQ-CTX-011.
    #[test]
    fn a_small_window_shrinks_the_fixed_categories_but_not_the_reserve() {
        let full = Budget::for_window(32_000);
        assert_eq!(full.system, 2_560);
        assert!(full.output_reserve >= 3_200);
        let small = Budget::for_window(16_000);
        // 8% of 16,000 is 1,280; scaled by 16,000/32,000 it is 640.
        assert_eq!(small.system, 640);
        assert_eq!(small.repo_map, 800);
        assert!(small.output_reserve >= 1_600, "at least 10% of W");
        assert_eq!(small.output_reserve, 1_920);
        for window in [4_000, 8_000, 16_000, 32_000] {
            let b = Budget::for_window(window);
            assert!(
                u64::from(b.output_reserve) * 10 >= u64::from(window),
                "{window}"
            );
        }
    }

    /// T-CTX-011 / T-CTX-012.
    #[test]
    fn a_request_that_does_not_leave_the_reserve_is_refused() {
        let b = Budget::for_window(10_000);
        assert!(verify_budget(8_800, &b).is_ok());
        let err = verify_budget(8_801, &b).unwrap_err();
        assert_eq!(err.code, "E-CTX-COMPACT");
        assert!(err.message.contains("8801"));
        assert!(verify_budget(u32::MAX, &b).is_err(), "no overflow");
    }

    #[test]
    fn compaction_starts_at_eighty_percent_of_what_a_request_may_hold() {
        let b = Budget::for_window(100_000);
        assert_eq!(b.compaction_threshold(), 70_400);
    }

    /// T-CTX-020.
    #[test]
    fn pins_stop_at_their_cap() {
        let b = Budget::for_window(200_000);
        assert!(check_pin(0, 20_000, &b).is_ok());
        assert!(check_pin(19_000, 1_000, &b).is_ok());
        let err = check_pin(19_000, 1_001, &b).unwrap_err();
        assert_eq!(err.code, "E-CTX-PINFULL");
        // Forty pinned files of 600 tokens each cross the 15% cap of a
        // 100k window.
        let b = Budget::for_window(100_000);
        let mut total = 0;
        let mut refused = false;
        for _ in 0..40 {
            match check_pin(total, 600, &b) {
                Ok(()) => total += 600,
                Err(e) => {
                    assert_eq!(e.code, "E-CTX-PINFULL");
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
    }

    fn entry(path: &str, score: f64, words: usize) -> MapEntry {
        MapEntry {
            path: path.to_string(),
            score,
            text: "word ".repeat(words),
        }
    }

    /// T-CTX-008's budget half: lowest scores go first.
    #[test]
    fn the_repo_map_drops_the_lowest_scored_files_first() {
        let entries = vec![
            entry("c.rs", 0.1, 100),
            entry("a.rs", 0.9, 100),
            entry("b.rs", 0.5, 100),
        ];
        // 100 words is 125 tokens; two entries fit in 300.
        let kept = fit_repo_map(entries, 300);
        let paths: Vec<&str> = kept.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "b.rs"]);
    }

    #[test]
    fn a_small_entry_can_still_fit_after_a_large_one_does_not() {
        let entries = vec![entry("big.rs", 0.9, 1000), entry("small.rs", 0.2, 5)];
        let kept = fit_repo_map(entries, 100);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "small.rs");
    }

    #[test]
    fn equal_scores_order_by_path_so_the_result_is_deterministic() {
        let a = fit_repo_map(vec![entry("z.rs", 0.5, 1), entry("a.rs", 0.5, 1)], 100);
        assert_eq!(a[0].path, "a.rs");
    }

    #[test]
    fn instructions_keep_their_start_and_end_when_too_long() {
        let mut body = String::new();
        for i in 0..400 {
            let _ = writeln!(body, "rule number {i} says do the thing");
        }
        let (out, cut) = truncate_instructions(&body, 500);
        assert!(cut);
        assert!(out.starts_with("rule number 0 "));
        assert!(out.contains("rule number 399 "));
        assert!(out.contains("lines of project instructions omitted"));
        assert!(estimate_tokens(&out) <= 600, "{}", estimate_tokens(&out));
        let (same, cut) = truncate_instructions("short", 500);
        assert!(!cut);
        assert_eq!(same, "short");
    }

    #[test]
    fn message_tokens_count_every_block_kind() {
        let m = Message::new(
            Role::Assistant,
            vec![
                Block::Text {
                    text: "a".repeat(400),
                },
                Block::ToolCall {
                    call_id: "c".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/lib.rs"}),
                    partial: false,
                    parse_error: None,
                },
            ],
            1,
        );
        let t = message_tokens(&m);
        assert!(t > 100 && t < 140, "{t}");
        assert_eq!(history_tokens(&[m.clone(), m]), t * 2);
    }
}
