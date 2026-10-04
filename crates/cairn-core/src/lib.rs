//! `cairn-core` — the dependency root of the Cairn workspace.
//!
//! Contains only pure domain types: no I/O, no subprocess handling, no network
//! (REQ-ARCH-002), and no `unsafe` (REQ-ARCH-004).
//!
//! `forbid` here is stronger than the workspace's `deny`: no future lint
//! attribute or feature flag can weaken it. Test T-ARCH-002 asserts both this
//! attribute and the absence of the I/O tokens.
#![forbid(unsafe_code)]

pub mod cancel;
pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod mode;
pub mod redact;
pub mod shutdown;

pub use cancel::CancellationToken;
pub use error::{is_valid_code, CairnError, ExitStatus};
pub use event::{Event, EventData};
pub use ids::{CheckpointId, JobId, MessageId, SessionId, Ulid};
pub use message::{Block, MediaType, Message, Role, StopReason, Usage};
pub use mode::Mode;
pub use shutdown::{shutdown_with_flush, FLUSH_BUDGET};

/// Stable error-code areas used by [`error::codes`]. Kept here so tooling can
/// enumerate them (spec §0 ID conventions).
pub const CODE_AREAS: &[&str] = &[
    "ARCH", "ASK", "AUTH", "CFG", "CHK", "CLI", "CRED", "CTX", "DISC", "EDIT", "FS", "GIT", "GLOB",
    "GREP", "HOOK", "IDX", "IMPL", "INJ", "JOB", "LOOP", "MCP", "MODE", "OPS", "ORPHAN", "PARSE",
    "PERF", "PERM", "PLAN", "PROD", "PROV", "REGEX", "SAFE", "SANDBOX", "SESS", "SHELL", "STATE",
    "SUB", "TECH", "TODO", "TOOL", "TUI", "UPDATE", "WEB",
];

#[cfg(test)]
mod code_area_tests {
    use super::*;

    /// Every registered code has an area §0 knows about, and the list itself
    /// has no duplicates.
    #[test]
    fn code_areas_are_unique_and_cover_every_registered_code() {
        let mut seen = std::collections::HashSet::new();
        for area in CODE_AREAS {
            assert!(seen.insert(area), "duplicate CODE_AREAS entry: {area}");
        }
        for code in error::ALL_CODES {
            let area = code.split('-').nth(1).expect("three-part code");
            assert!(
                CODE_AREAS.contains(&area),
                "`{code}` uses unknown area `{area}` (§0 ID conventions)"
            );
        }
    }
}
