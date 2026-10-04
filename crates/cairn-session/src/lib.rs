//! `cairn-session` — the JSONL session store (SPEC §11.7).
//!
//! Sessions are the agent's memory: a `.jsonl` file per session, one JSON
//! object per line, a mandatory `header` first. Everything in Cairn that needs
//! to remember something across a restart — resume, export, migration, GC —
//! goes through this crate.
//!
//! Dependency direction (REQ-ARCH-001): `session → core`. Nothing here knows
//! about providers, tools or the TUI (§3.2).
//!
//! | Module | What it owns |
//! |--------|--------------|
//! | [`record`] | the record vocabulary and the `header` schema |
//! | [`store`] | paths, create/append/load/save/list, the §11.7 read contract |
//! | [`migrate`] | forward-only version migration with an atomic rewrite |
//! | [`export`] | `md` / `json` / `html` transcripts (§11.7) |
//! | [`gc`] | retention and `max_sessions` with `in_progress` protection |
//!
//! Every failure is a [`CairnError`] carrying a stable `E-*` code, so the CLI
//! can turn it into an §11.2 exit status in one place.

#![deny(missing_debug_implementations)]

use cairn_core::error::CairnError;

/// Errors from this crate always carry a stable code (REQ-CLI-002).
pub type Result<T> = std::result::Result<T, CairnError>;

pub mod export;
pub mod gc;
pub mod migrate;
pub mod record;
pub mod store;

pub use record::{Header, Record, CURRENT_SCHEMA_VERSION, RECORD_VERSION};
pub use store::{load_path, Entry, ListFilter, SessionFile, Store, Summary};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_types_are_usable_together() {
        let store = Store::new("/tmp/unused");
        let header = Header::new("ses_1", "/ws", "build", "anthropic/claude-sonnet-4-5");
        assert_eq!(header.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(header.to_record().v, RECORD_VERSION);
        assert_eq!(store.root(), std::path::Path::new("/tmp/unused"));
    }

    #[test]
    fn the_result_alias_is_a_cairn_error() {
        fn boom() -> Result<()> {
            Err(record::corrupt("nope"))
        }
        let err = boom().unwrap_err();
        assert!(err.code.starts_with("E-"), "{}", err.code);
    }
}
