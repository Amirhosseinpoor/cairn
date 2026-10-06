//! Running a compaction (SPEC §5.6): ask the model to summarise the older
//! part of a conversation, replace it, and check the result fits.
//!
//! What to summarise is `cairn-context`'s decision; this module is the part
//! that needs a model. Nothing is returned until every summary has arrived, so
//! a cancelled compaction leaves no trace (REQ-CTX-014).

use std::sync::Arc;

use cairn_context::budget::{history_tokens, Budget};
use cairn_context::compact::{
    apply, hard_trim, plan, record, render_for_summary, summary_message, Record, Trigger,
    MAX_ROUNDS, SUMMARY_MAX_TOKENS, SUMMARY_PROMPT,
};
use cairn_core::cancel::CancellationToken;
use cairn_core::error::codes;
use cairn_core::message::{Block, Message, Role};
use cairn_provider::{ModelRequest, Provider, RetryBudget};

use crate::turn::{run_turn, TurnEnd};

/// How a compaction ended.
#[derive(Debug)]
pub enum Outcome {
    /// The history was replaced. `records` are the compactions applied, in
    /// order (several when more than one round was needed).
    Compacted {
        messages: Vec<Message>,
        records: Vec<Record>,
        /// Rounds ran out and the oldest messages were dropped
        /// (`E-CTX-COMPACT`, §5.6 step 6).
        trimmed: bool,
    },
    /// Nothing was old enough to be worth summarising.
    Nothing,
    /// The token was raised; nothing was kept.
    Cancelled,
    /// The summariser failed (`code` and message from the provider).
    Failed { code: String, message: String },
}

/// What a compaction needs.
#[allow(
    missing_debug_implementations,
    reason = "holds a provider trait object with no Debug bound"
)]
pub struct Job<'a> {
    pub provider: Arc<dyn Provider>,
    pub model: &'a str,
    pub retry: &'a RetryBudget,
    pub budget: &'a Budget,
    pub current_turn: u64,
    pub trigger: Trigger,
    /// Id for the first summary this call makes (`[[SUMMARY id=n …]]`).
    pub first_id: u32,
    /// Tokens everything but the history takes (system prompt, tools), so a
    /// round can tell whether the whole request now fits.
    pub fixed_tokens: u32,
}

async fn summarise(
    job: &Job<'_>,
    text: &str,
    extra: Option<&str>,
    cancel: &CancellationToken,
) -> Result<String, Outcome> {
    let mut prompt = format!("{SUMMARY_PROMPT}\n\n---\n\nConversation to compress:\n\n{text}");
    if let Some(extra) = extra.filter(|e| !e.trim().is_empty()) {
        prompt.push_str("\n\n---\n\nAdditional instructions from the user: ");
        prompt.push_str(extra.trim());
    }
    let mut request = ModelRequest::new(
        job.model,
        vec![Message::new(
            Role::User,
            vec![Block::Text { text: prompt }],
            job.current_turn,
        )],
        SUMMARY_MAX_TOKENS,
    );
    request.temperature = Some(0.0);
    request.turn_id = job.current_turn;
    let outcome = run_turn(
        Arc::clone(&job.provider),
        request,
        job.current_turn,
        cancel.clone(),
        job.retry.clone(),
        None,
        |_| {},
    )
    .await;
    match outcome.end {
        TurnEnd::Completed => {
            let text = outcome.assistant.map(|m| m.text()).unwrap_or_default();
            if text.trim().is_empty() {
                return Err(Outcome::Failed {
                    code: codes::CTX_COMPACT.to_string(),
                    message: "the model returned an empty summary".to_string(),
                });
            }
            Ok(text)
        }
        TurnEnd::Cancelled => Err(Outcome::Cancelled),
        TurnEnd::Failed => {
            let (code, message) = outcome.fault.map_or_else(
                || {
                    (
                        codes::CTX_COMPACT.to_string(),
                        "the summariser failed".to_string(),
                    )
                },
                |f| {
                    (
                        f.code().unwrap_or(codes::CTX_COMPACT).to_string(),
                        f.message,
                    )
                },
            );
            Err(Outcome::Failed { code, message })
        }
    }
}

/// Compact `history` (everything after the system prompt).
///
/// `extra` is the user's `/compact` instructions, if any.
pub async fn compact(
    job: &Job<'_>,
    history: &[Message],
    extra: Option<&str>,
    cancel: &CancellationToken,
) -> Outcome {
    let mut current: Vec<Message> = history.to_vec();
    let mut records = Vec::new();
    for id in (job.first_id..).take(MAX_ROUNDS) {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        let Some(p) = plan(&current, job.budget, job.current_turn) else {
            break;
        };
        let mut bodies = Vec::new();
        for chunk in &p.chunks {
            let refs: Vec<&Message> = chunk.iter().map(|i| &current[*i]).collect();
            match summarise(job, &render_for_summary(&refs), extra, cancel).await {
                Ok(body) => bodies.push(body),
                Err(outcome) => return outcome,
            }
        }
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }
        let first = current[p.summarise[0]].id.to_string();
        let last = current[*p.summarise.last().unwrap_or(&0)].id.to_string();
        let summary = summary_message(id, &first, &last, &bodies.join("\n\n"), job.current_turn);
        let next = apply(&current, &p, &summary);
        records.push(record(
            id,
            job.trigger.clone(),
            &current,
            &p,
            &summary,
            &next,
        ));
        current = next;
        let total = job.fixed_tokens.saturating_add(history_tokens(&current));
        if u64::from(total) * 10 <= u64::from(job.budget.usable()) * 9 {
            return Outcome::Compacted {
                messages: current,
                records,
                trimmed: false,
            };
        }
    }
    if records.is_empty() && history_tokens(&current) <= job.budget.compaction_threshold() {
        return Outcome::Nothing;
    }
    // §5.6 step 6: still too big after three rounds (or nothing could be
    // summarised): drop the oldest messages.
    let trimmed = hard_trim(&current, job.budget);
    if trimmed.len() == history.len() && records.is_empty() {
        return Outcome::Nothing;
    }
    Outcome::Compacted {
        messages: trimmed,
        records,
        trimmed: true,
    }
}

/// `compaction.performed`'s numbers for a finished compaction.
#[must_use]
pub fn event_numbers(
    before: &[Message],
    after: &[Message],
    records: &[Record],
) -> (u32, u32, u32, u32, u32) {
    let before_tokens = history_tokens(before);
    let after_tokens = history_tokens(after);
    let summarised: usize = records.iter().map(|r| r.replaced.len()).sum();
    let summary_tokens: u32 = records
        .iter()
        .map(|r| cairn_context::budget::message_tokens(&r.summary))
        .sum();
    let dropped = before.len().saturating_sub(after.len());
    (
        before_tokens,
        after_tokens,
        u32::try_from(dropped).unwrap_or(u32::MAX),
        u32::try_from(summarised).unwrap_or(u32::MAX),
        summary_tokens,
    )
}

/// The notice REQ-CTX-015 requires.
#[must_use]
pub fn notice(before: u32, after: u32) -> String {
    format!("Compacted history: {before} → {after} tokens. /undo compaction to restore.")
}
