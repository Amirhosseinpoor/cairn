//! `cairn-tui` — the terminal interface (SPEC §10, D-03).
//!
//! The crate is built so that everything interesting is testable without a
//! terminal: [`keys`] and [`editor`] are plain data and functions, rendering
//! draws into a ratatui buffer that tests read directly, and only the loop
//! that owns the real terminal (in `cairn-cli`) touches `crossterm`.
//!
//! The **slash-command registry** (§10.3) is pure data kept 1:1 with the spec
//! table by test `table_and_registry_match_1_to_1` (T-CLI-003).

pub mod app;
pub mod editor;
pub mod fuzzy;
pub mod history;
pub mod interact;
pub mod keymap;
pub mod keys;
pub mod markdown;
pub mod overlay;
pub mod overlay_view;
pub mod slash;
pub mod theme;
pub mod view;

#[cfg(test)]
mod tests;

pub use slash::{
    is_slash, levenshtein, lookup, names, suggest, unknown_message, SlashCommand, MODE_ALL,
    MODE_AUTO, MODE_BUILD, MODE_PLAN, MODE_UNSAFE, SLASH_COMMANDS, SLASH_COMMAND_COUNT,
};
