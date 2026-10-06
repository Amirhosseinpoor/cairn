//! `cairn-agent` — the turn loop: modes, guardrails, plan lifecycle
//! (SPEC §7, §8, §12).
//!
//! M1 delivers the model half of §8.1 — [`turn::run_turn`] folds a provider
//! stream into one committed assistant message — plus the session side of
//! §8.7 ([`transcript`]). Tool execution, modes and guardrails arrive in
//! M2–M4 on top of the same turn runner.

pub mod transcript;
pub mod turn;
pub mod usage;
