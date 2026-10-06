//! Compaction (SPEC §5.6): when the conversation is too long, older messages
//! are replaced by a summary.
//!
//! This module decides *what* to summarise and builds the result; asking a
//! model to write the summary, and saving the outcome, are the agent's job.
//! Two rules keep the result usable: a tool call and its result are never
//! separated (a provider rejects a result with no call), and nothing the
//! current turn wrote is summarised away.

use std::fmt::Write as _;

use cairn_core::message::{Block, Message, Role};
use serde::{Deserialize, Serialize};

use crate::budget::{history_tokens, message_tokens, Budget};

/// §5.6 step 1: messages always kept verbatim from the end.
pub const KEEP_RECENT: usize = 12;
/// §5.6 step 2: messages per summarisation request.
pub const CHUNK: usize = 24;
/// §5.6 step 6: rounds before giving up and trimming.
pub const MAX_ROUNDS: usize = 3;
/// §5.6: `max_tokens` of one summary request.
pub const SUMMARY_MAX_TOKENS: u32 = 2_000;
/// REQ-CTX-015: how many compactions can be undone.
pub const UNDO_DEPTH: usize = 5;

/// §5.6.1, character for character.
pub const SUMMARY_PROMPT: &str = "\
You are compressing the conversation history of a coding agent so it fits in the model's
context window. Produce a durable record that lets work continue without the original messages.

Write a structured summary with these headings, in this order, using terse bullet points:

## Goal
The user's objective and any constraints they stated.

## Decisions
Decisions made and why. Include exact identifiers: file paths, function names, symbols,
branch names, config keys, commands. Never invent an identifier that is not in the source.

## State of the code
Files created or modified so far, with a one-line description of each change.
Include the state of todos/tasks.

## Errors and dead ends
Errors encountered, what was tried, and what must NOT be retried.

## Open threads
Outstanding questions, TODOs, and next actions. Number them.

Rules:
- Preserve verbatim: exact file paths, command lines, error messages (≤ 3 lines each), and code identifiers.
- Drop: pleasantries, restated tool output, reasoning traces, and duplicate information.
- Do not add advice, opinions, or new plans.
- Target length: 10-25% of the original text, hard maximum 800 tokens.
- Output plain Markdown only, no preamble.";

/// Why a compaction is happening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    /// C-1: 80% of what a request may hold.
    Threshold,
    /// C-3: the provider said the context was too long.
    ProviderError,
    /// C-4: fifty turns since the last one.
    TurnCount,
    /// C-5: `/compact [instructions]`.
    User(Option<String>),
    /// §5.4: the output reserve would be violated.
    Reserve,
}

/// What the loop remembers between turns.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct State {
    pub turns_since: u32,
    /// The last compaction's reduction (`0.3` = 30% fewer tokens).
    pub last_reduction: Option<f64>,
}

/// §5.6's triggers C-1, C-2 and C-4 for `tokens` of history.
///
/// C-2: if the last compaction saved less than a quarter, automatic
/// compaction is off and only `/compact` (or a provider error) runs one.
#[must_use]
pub fn automatic_trigger(tokens: u32, budget: &Budget, state: &State) -> Option<Trigger> {
    if state.last_reduction.is_some_and(|r| r < 0.25) {
        return None;
    }
    if tokens >= budget.compaction_threshold() {
        return Some(Trigger::Threshold);
    }
    (state.turns_since >= 50).then_some(Trigger::TurnCount)
}

/// Which messages are summarised and which stay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Indexes to summarise, in order.
    pub summarise: Vec<usize>,
    /// Those indexes in groups of at most [`CHUNK`].
    pub chunks: Vec<Vec<usize>>,
}

fn is_edit_call(block: &Block) -> bool {
    matches!(block, Block::ToolCall { name, .. }
        if matches!(name.as_str(), "git_commit" | "edit_file" | "multi_edit" | "write_file"))
}

/// A message that must survive: the current turn's file changes.
fn pinned_by_turn(message: &Message, current_turn: u64) -> bool {
    message.turn_id == current_turn && message.blocks.iter().any(is_edit_call)
}

fn calls_in(message: &Message) -> Vec<&str> {
    message
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect()
}

fn answers_in(message: &Message) -> Vec<&str> {
    message
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect()
}

/// Decide what to summarise: everything older than the most recent
/// [`KEEP_RECENT`] messages (or the recent messages that fit half the history
/// budget, if that is more), except the current turn's edits and anything
/// bound to them by a call/result pair.
///
/// `None` when there is nothing worth summarising.
#[must_use]
pub fn plan(messages: &[Message], budget: &Budget, current_turn: u64) -> Option<Plan> {
    // The recent tail: at least KEEP_RECENT, or as many as fit half the
    // history budget.
    let half = budget.history / 2;
    let mut tail_start = messages.len().saturating_sub(KEEP_RECENT);
    let mut used = 0;
    let mut by_budget = messages.len();
    for (i, message) in messages.iter().enumerate().rev() {
        used += message_tokens(message);
        if used > half {
            break;
        }
        by_budget = i;
    }
    tail_start = tail_start.min(by_budget);
    // Never start the tail with a result whose call is before it.
    while tail_start > 0 && messages[tail_start].role == Role::Tool {
        tail_start -= 1;
    }

    let mut keep = vec![false; messages.len()];
    for k in &mut keep[tail_start..] {
        *k = true;
    }
    for (i, message) in messages.iter().enumerate().take(tail_start) {
        if pinned_by_turn(message, current_turn) {
            keep[i] = true;
        }
    }
    // Bind each kept call to its result and each kept result to its call.
    loop {
        let mut changed = false;
        for i in 0..messages.len() {
            if !keep[i] {
                continue;
            }
            let calls = calls_in(&messages[i]);
            let answers = answers_in(&messages[i]);
            for (j, other) in messages.iter().enumerate() {
                if keep[j] || i == j {
                    continue;
                }
                let pairs = answers_in(other).iter().any(|a| calls.contains(a))
                    || calls_in(other).iter().any(|c| answers.contains(c));
                if pairs {
                    keep[j] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let summarise: Vec<usize> = (0..messages.len()).filter(|i| !keep[*i]).collect();
    // A summary of one or two short messages saves nothing.
    if summarise.len() < 2 {
        return None;
    }
    let chunks = summarise.chunks(CHUNK).map(<[usize]>::to_vec).collect();
    Some(Plan { summarise, chunks })
}

fn one_line(text: &str, max: usize) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if first.chars().count() <= max {
        first.to_string()
    } else {
        let cut: String = first.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// The text of `messages` as the summariser reads it: roles, text, tool calls
/// by name and arguments, tool results as one line (§5.6 step 5), no
/// reasoning.
#[must_use]
pub fn render_for_summary(messages: &[&Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
            Role::System => "System",
        };
        for block in &message.blocks {
            match block {
                Block::Text { text } => {
                    let _ = writeln!(out, "{role}: {}", text.trim());
                }
                Block::ToolCall { name, input, .. } => {
                    let _ = writeln!(out, "{role} called {name} {input}");
                }
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let text: String = content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    let status = if *is_error { "error" } else { "ok" };
                    let _ = writeln!(out, "Tool result ({status}): {}", one_line(&text, 200));
                }
                Block::Reasoning { .. }
                | Block::ThinkingPlaceholder { .. }
                | Block::Image { .. } => {}
            }
        }
    }
    out
}

/// The `[[SUMMARY …]]` message that stands in for summarised ones.
#[must_use]
pub fn summary_message(id: u32, first: &str, last: &str, body: &str, turn_id: u64) -> Message {
    let text = format!("[[SUMMARY id={id} range={first}..{last}]]\n{}", body.trim());
    Message::new(Role::User, vec![Block::Text { text }], turn_id)
}

/// Whether `message` is one of these summaries.
#[must_use]
pub fn is_summary(message: &Message) -> bool {
    message.role == Role::User
        && message
            .blocks
            .first()
            .is_some_and(|b| matches!(b, Block::Text { text } if text.starts_with("[[SUMMARY id=")))
}

/// What happened, as the session log records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: u32,
    pub trigger: Trigger,
    /// Ids of the messages the summary replaced, in order.
    pub replaced: Vec<String>,
    /// The summary message, whole.
    pub summary: Message,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

impl Record {
    /// `0.7` = 70% fewer tokens.
    #[must_use]
    pub fn reduction(&self) -> f64 {
        if self.tokens_before == 0 {
            return 0.0;
        }
        1.0 - f64::from(self.tokens_after) / f64::from(self.tokens_before)
    }
}

/// Replace `plan`'s messages with `summary`, keeping the rest in order. The
/// summary goes where the first replaced message was.
#[must_use]
pub fn apply(messages: &[Message], plan: &Plan, summary: &Message) -> Vec<Message> {
    let first = plan.summarise.first().copied().unwrap_or(0);
    let mut out = Vec::with_capacity(messages.len());
    let mut placed = false;
    for (i, message) in messages.iter().enumerate() {
        if plan.summarise.contains(&i) {
            if !placed && i == first {
                out.push(summary.clone());
                placed = true;
            }
            continue;
        }
        out.push(message.clone());
    }
    out
}

/// Build the [`Record`] for an applied plan.
#[must_use]
pub fn record(
    id: u32,
    trigger: Trigger,
    before: &[Message],
    plan: &Plan,
    summary: &Message,
    after: &[Message],
) -> Record {
    Record {
        id,
        trigger,
        replaced: plan
            .summarise
            .iter()
            .map(|i| before[*i].id.to_string())
            .collect(),
        summary: summary.clone(),
        tokens_before: history_tokens(before),
        tokens_after: history_tokens(after),
    }
}

/// Rebuild a conversation from its original messages and the compactions
/// that were applied to it, oldest first. This is how a resumed session gets
/// the same history the live one had.
#[must_use]
pub fn replay(messages: &[Message], records: &[Record]) -> Vec<Message> {
    let mut out: Vec<Message> = messages.to_vec();
    for record in records {
        let replaced: std::collections::HashSet<&str> =
            record.replaced.iter().map(String::as_str).collect();
        let mut placed = false;
        let mut next = Vec::with_capacity(out.len());
        for message in out {
            if replaced.contains(message.id.to_string().as_str()) {
                if !placed {
                    next.push(record.summary.clone());
                    placed = true;
                }
                continue;
            }
            next.push(message);
        }
        out = next;
    }
    out
}

/// §5.6 step 6 and `/undo compaction`: the conversation as it was before the
/// last `count` compactions (at most [`UNDO_DEPTH`]).
#[must_use]
pub fn undo(messages: &[Message], records: &[Record], count: usize) -> Vec<Message> {
    let keep = records.len().saturating_sub(count.min(UNDO_DEPTH));
    replay(messages, &records[..keep])
}

/// The last resort when compaction cannot get under the line: drop the oldest
/// messages (never splitting a call from its result) until `tokens` is under
/// the compaction threshold.
#[must_use]
pub fn hard_trim(messages: &[Message], budget: &Budget) -> Vec<Message> {
    let mut start = 0;
    let mut tokens = history_tokens(messages);
    while start + 1 < messages.len() && tokens > budget.compaction_threshold() {
        tokens -= message_tokens(&messages[start]);
        start += 1;
    }
    // The first kept message may not be a result.
    while start < messages.len() && messages[start].role == Role::Tool {
        start += 1;
    }
    messages[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(role: Role, turn: u64, body: &str) -> Message {
        Message::new(
            role,
            vec![Block::Text {
                text: body.to_string(),
            }],
            turn,
        )
    }

    fn call(turn: u64, id: &str, name: &str) -> Message {
        Message::new(
            Role::Assistant,
            vec![Block::ToolCall {
                call_id: id.into(),
                name: name.into(),
                input: serde_json::json!({"path": "src/lib.rs"}),
                partial: false,
                parse_error: None,
            }],
            turn,
        )
    }

    fn result(turn: u64, id: &str, body: &str) -> Message {
        Message::new(
            Role::Tool,
            vec![Block::ToolResult {
                call_id: id.into(),
                content: vec![Block::Text {
                    text: body.to_string(),
                }],
                is_error: false,
            }],
            turn,
        )
    }

    /// A budget whose history share is nil, so the tail is exactly the
    /// twelve most recent messages.
    fn tight() -> Budget {
        Budget {
            history: 0,
            ..Budget::for_window(100_000)
        }
    }

    fn chat(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                text(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    1,
                    &format!("message number {i} with some words in it"),
                )
            })
            .collect()
    }

    /// T-CTX-025: the prompt is §5.6.1's text exactly.
    #[test]
    fn the_summary_prompt_is_the_specified_text() {
        let spec = include_str!("../../../SPEC.md");
        let start = spec
            .find("You are compressing the conversation history")
            .expect("prompt in spec");
        let end = spec[start..].find("```").expect("fence") + start;
        assert_eq!(SUMMARY_PROMPT, spec[start..end].trim_end());
    }

    /// T-CTX-017: C-1 at 80% of the usable window.
    #[test]
    fn the_threshold_trigger_fires_at_eighty_percent() {
        let b = Budget::for_window(100_000);
        let state = State::default();
        assert_eq!(automatic_trigger(70_399, &b, &state), None);
        assert_eq!(
            automatic_trigger(70_400, &b, &state),
            Some(Trigger::Threshold)
        );
    }

    #[test]
    fn fifty_turns_trigger_a_compaction_by_themselves() {
        let b = Budget::for_window(100_000);
        let state = State {
            turns_since: 50,
            last_reduction: None,
        };
        assert_eq!(automatic_trigger(10, &b, &state), Some(Trigger::TurnCount));
        let state = State {
            turns_since: 49,
            last_reduction: None,
        };
        assert_eq!(automatic_trigger(10, &b, &state), None);
    }

    /// C-2: a compaction that saved less than 25% turns the automatic ones off.
    #[test]
    fn a_poor_compaction_turns_automatic_ones_off() {
        let b = Budget::for_window(100_000);
        let state = State {
            turns_since: 60,
            last_reduction: Some(0.24),
        };
        assert_eq!(automatic_trigger(99_000, &b, &state), None);
        let state = State {
            turns_since: 0,
            last_reduction: Some(0.25),
        };
        assert_eq!(
            automatic_trigger(99_000, &b, &state),
            Some(Trigger::Threshold)
        );
    }

    #[test]
    fn the_twelve_most_recent_messages_are_never_summarised() {
        let messages = chat(30);
        let p = plan(&messages, &tight(), 1).expect("plan");
        assert_eq!(p.summarise, (0..18).collect::<Vec<_>>());
        assert_eq!(p.chunks.len(), 1);
        assert!(
            plan(&chat(13), &tight(), 1).is_none(),
            "one message is not worth it"
        );
    }

    #[test]
    fn older_messages_are_chunked_in_groups_of_twenty_four() {
        let messages = chat(12 + 60);
        let p = plan(&messages, &tight(), 1).expect("plan");
        let sizes: Vec<usize> = p.chunks.iter().map(Vec::len).collect();
        assert_eq!(sizes, [24, 24, 12]);
    }

    #[test]
    fn a_big_recent_tail_is_kept_when_it_fits_half_the_history_budget() {
        // 400 tiny messages fit easily in half of a 200k window's history.
        let messages = chat(400);
        let p = plan(&messages, &Budget::for_window(200_000), 1);
        assert!(p.is_none(), "everything is recent enough to stay");
    }

    #[test]
    fn a_call_and_its_result_are_never_split() {
        let mut messages = chat(20);
        // The result lands exactly where the recent tail would start.
        messages.insert(8, call(1, "c1", "read_file"));
        messages.insert(9, result(1, "c1", "contents"));
        let p = plan(&messages, &tight(), 1).expect("plan");
        let has = |i: usize| p.summarise.contains(&i);
        assert_eq!(
            has(8),
            has(9),
            "call and result go together: {:?}",
            p.summarise
        );
        // Put the pair across the tail boundary: 22 messages leave the tail
        // starting at index 10, which is the result.
        let mut messages = chat(20);
        messages.insert(9, call(1, "c2", "grep"));
        messages.insert(10, result(1, "c2", "hits"));
        let p = plan(&messages, &tight(), 1).expect("plan");
        assert!(
            !p.summarise.contains(&9),
            "the call follows its result into the tail"
        );
        assert!(!p.summarise.contains(&10));
    }

    #[test]
    fn this_turns_edits_survive_with_their_results() {
        let mut messages = chat(30);
        messages.insert(3, call(7, "e1", "edit_file"));
        messages.insert(4, result(7, "e1", "ok"));
        messages.insert(5, call(6, "e0", "write_file"));
        messages.insert(6, result(6, "e0", "ok"));
        let p = plan(&messages, &tight(), 7).expect("plan");
        assert!(!p.summarise.contains(&3) && !p.summarise.contains(&4));
        assert!(
            p.summarise.contains(&5),
            "an earlier turn's edit is history"
        );
        assert!(p.summarise.contains(&6));
    }

    #[test]
    fn applying_a_plan_puts_one_summary_where_the_first_message_was() {
        let messages = chat(30);
        let p = plan(&messages, &tight(), 1).expect("plan");
        let first = messages[p.summarise[0]].id.to_string();
        let last = messages[*p.summarise.last().unwrap()].id.to_string();
        let summary = summary_message(1, &first, &last, "## Goal\nfix it\n", 1);
        assert!(is_summary(&summary));
        let out = apply(&messages, &p, &summary);
        assert_eq!(out.len(), 30 - p.summarise.len() + 1);
        assert_eq!(out[0].id, summary.id);
        assert_eq!(out[1].id, messages[18].id);
        let Block::Text { text } = &out[0].blocks[0] else {
            panic!()
        };
        assert!(text.starts_with(&format!("[[SUMMARY id=1 range={first}..{last}]]\n## Goal")));
        assert!(!is_summary(&messages[0]));
    }

    #[test]
    fn the_summariser_sees_one_line_per_tool_result_and_no_reasoning() {
        let long = "first line\nsecond line\nthird".to_string();
        let m = [
            text(Role::User, 1, "please fix"),
            call(1, "c", "read_file"),
            result(1, "c", &long),
            Message::new(
                Role::Assistant,
                vec![
                    Block::Reasoning {
                        text: "secret thoughts".into(),
                        signature: None,
                    },
                    Block::Text {
                        text: "done".into(),
                    },
                ],
                1,
            ),
        ];
        let refs: Vec<&Message> = m.iter().collect();
        let rendered = render_for_summary(&refs);
        assert!(rendered.contains("User: please fix"));
        assert!(rendered.contains("Assistant called read_file"));
        assert!(rendered.contains("Tool result (ok): first line\n"));
        assert!(!rendered.contains("second line"));
        assert!(!rendered.contains("secret thoughts"));
        assert!(rendered.contains("Assistant: done"));
    }

    /// T-CTX-018: undo restores up to five compactions.
    #[test]
    fn records_replay_and_undo_in_order() {
        let original = chat(60);
        let b = tight();
        // First compaction.
        let p1 = plan(&original, &b, 1).expect("plan");
        let s1 = summary_message(1, "a", "b", "summary one", 1);
        let after1 = apply(&original, &p1, &s1);
        let r1 = record(1, Trigger::Threshold, &original, &p1, &s1, &after1);
        assert!(r1.reduction() > 0.0);
        // Second compaction, over the first one's result.
        let p2 = plan(&after1, &b, 1);
        let (records, after2) = if let Some(p2) = p2 {
            let s2 = summary_message(2, "c", "d", "summary two", 1);
            let after2 = apply(&after1, &p2, &s2);
            (
                vec![
                    r1.clone(),
                    record(2, Trigger::Threshold, &after1, &p2, &s2, &after2),
                ],
                after2,
            )
        } else {
            (vec![r1.clone()], after1.clone())
        };
        assert_eq!(
            replay(&original, &records),
            after2,
            "a resumed session sees the same history"
        );
        assert_eq!(
            undo(&original, &records, 1).len(),
            replay(&original, &records[..records.len() - 1]).len()
        );
        assert_eq!(undo(&original, &records, 5), original, "everything undone");
        assert_eq!(undo(&original, &[], 1), original);
    }

    /// T-CTX-021: the last resort.
    #[test]
    fn a_hard_trim_drops_the_oldest_and_never_starts_on_a_result() {
        let mut messages = chat(40);
        messages.insert(10, call(1, "x", "grep"));
        messages.insert(11, result(1, "x", &"hit ".repeat(2000)));
        let b = Budget::for_window(2_000);
        let trimmed = hard_trim(&messages, &b);
        assert!(trimmed.len() < messages.len());
        assert!(history_tokens(&trimmed) <= b.compaction_threshold() || trimmed.len() == 1);
        assert_ne!(trimmed[0].role, Role::Tool);
        // What remains is the newest suffix.
        let offset = messages.len() - trimmed.len();
        assert_eq!(trimmed[0].id, messages[offset].id);
    }
}
