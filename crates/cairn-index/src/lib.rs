//! `cairn-index` — the workspace symbol index and repository map (SPEC §5.2,
//! §5.3).

pub mod extract;
pub mod rank;
pub mod resolve;
pub mod store;

pub use extract::{extract, Extracted, Language, Symbol};
pub use rank::Params;
pub use store::{
    cache_path, Index, IndexError, Query, Ranked, ScanOptions, ScanReport, Stats, SymbolRow,
    MAX_FILE_BYTES, TOP_K,
};
pub mod watch;
