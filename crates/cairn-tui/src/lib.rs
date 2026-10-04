//! `cairn-tui` — ratatui interface: panes, stream renderer, palette (SPEC 10, D-03)
//!
//! Delivered in milestone **M3** (SPEC §15.4). This crate is a compile-checked
//! placeholder so workspace dependency direction (REQ-ARCH-003) is enforced from
//! day one; the module boundary and public API land with the milestone.
//!
//! The one piece that is normative today is the **slash-command registry**
//! (§10.3): it is pure data, and test `table_and_registry_match_1_to_1`
//! (T-CLI-003) keeps it 1:1 with the spec table.

pub mod slash;

pub use slash::{
    is_slash, levenshtein, lookup, names, suggest, unknown_message, SlashCommand, MODE_ALL,
    MODE_AUTO, MODE_BUILD, MODE_PLAN, MODE_UNSAFE, SLASH_COMMANDS, SLASH_COMMAND_COUNT,
};
