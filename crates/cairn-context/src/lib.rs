//! `cairn-context` — what goes into a request, and what is left out when it
//! does not fit (SPEC §5).
//!
//! * [`budget`] — the token budget table, packing and refusal.
//! * [`instructions`] — `AGENTS.md` discovery and merging.
//! * [`compact`] — when and how history is summarised.
//! * [`repo_map`] — the ranked file map for the system prompt.
//!
//! The crate does arithmetic and text; it calls no model and touches no
//! network. Counting tokens, asking a model to summarise and persisting the
//! result are the agent's job.

pub mod budget;
pub mod compact;
pub mod instructions;
pub mod repo_map;
