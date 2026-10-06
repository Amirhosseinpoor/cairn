//! `cairn-search` — file discovery, ignore rules, glob and grep
//! (SPEC §5.1, §6.2.5–6.2.7).
//!
//! Everything that looks at the tree goes through one [`IgnoreEngine`], so
//! `read_file`, `list_dir`, `glob` and `grep` cannot disagree about what is
//! excluded (REQ-CTX-002).

pub mod binary;
pub mod glob;
pub mod grep;
pub mod ignore_rules;
pub mod lang;
pub mod walk;

pub use binary::{classify, Content, SNIFF_BYTES};
pub use glob::{glob, GlobError, GlobOptions, GlobResult};
pub use grep::{grep, GrepError, GrepMatch, GrepOptions, GrepResult};
pub use ignore_rules::{default_global_ignore, IgnoreEngine, IgnoreOptions};
pub use lang::language_for;
pub use walk::{walk, Entry, Kind, WalkError, WalkOptions, Walked};
