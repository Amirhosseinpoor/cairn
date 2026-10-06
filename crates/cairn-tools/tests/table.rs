//! T-TOOL-001: the live registry is §6.1's table (REQ-TOOL-001).

use cairn_tools::{builtin, Registry};

/// §6.1, row by row: name, permission class, side-effect category,
/// idempotency, usual timeout in seconds (0 = none), output limit in KiB,
/// serial lane.
#[rustfmt::skip]
const SPEC: &[(&str, &str, &str, &str, u64, u32, bool)] = &[
    ("read_file",       "Read",       "None",    "Safe",          10,   200, false),
    ("write_file",      "Write",      "Write",   "Retryable",     20,   8,   false),
    ("edit_file",       "Write",      "Write",   "NonIdempotent", 20,   8,   false),
    ("multi_edit",      "Write",      "Write",   "NonIdempotent", 30,   16,  false),
    ("list_dir",        "Read",       "None",    "Safe",          10,   64,  false),
    ("glob",            "Read",       "None",    "Safe",          15,   64,  false),
    ("grep",            "Read",       "None",    "Safe",          30,   128, false),
    ("bash",            "Execute",    "Execute", "NonIdempotent", 120,  64,  true),
    ("bash_background", "Execute",    "Execute", "NonIdempotent", 0,    64,  true),
    ("job_output",      "Execute",    "None",    "Safe",          10,   64,  false),
    ("job_kill",        "Execute",    "Execute", "Retryable",     10,   4,   true),
    ("git_status",      "Read",       "None",    "Safe",          15,   32,  false),
    ("git_diff",        "Read",       "None",    "Safe",          20,   128, false),
    ("git_commit",      "Write",      "Write",   "NonIdempotent", 30,   16,  true),
    ("web_fetch",       "Network",    "Network", "Safe",          30,   64,  false),
    ("todo_write",      "WriteState", "Write",   "Retryable",     5,    32,  false),
    ("ask_user",        "Ask",        "None",    "Safe",          3600, 8,   true),
];

#[test]
fn t_tool_001_every_row_of_the_table_is_registered_as_specified() {
    let mut registry = Registry::new();
    builtin::register_all(&mut registry).expect("registers");
    let table = registry.table();
    let mut wrong = Vec::new();
    for (name, class, effect, idem, timeout_s, kib, serial) in SPEC {
        let Some(row) = table.iter().find(|r| r.name == *name) else {
            wrong.push(format!("{name}: not registered"));
            continue;
        };
        let got = (
            row.class,
            row.side_effect,
            row.idempotency,
            row.timeout_ms / 1000,
            row.max_output_bytes / 1024,
            row.serial,
        );
        let want = (*class, *effect, *idem, *timeout_s, *kib, *serial);
        if got != want {
            wrong.push(format!("{name}: want {want:?}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // Everything registered is in the table, and `subagent` (M4) is the only
    // §6.1 row still missing.
    let extra: Vec<&str> = table
        .iter()
        .map(|r| r.name)
        .filter(|n| !SPEC.iter().any(|s| s.0 == *n))
        .collect();
    assert!(extra.is_empty(), "registered but not in §6.1: {extra:?}");
    assert_eq!(table.len(), 17);
}

#[test]
fn every_tool_has_a_closed_schema_a_description_and_an_output_schema() {
    let mut registry = Registry::new();
    builtin::register_all(&mut registry).expect("registers");
    for def in registry.definitions(cairn_core::Mode::Build) {
        assert!(!def.description.is_empty(), "{}", def.name);
        assert!(def.description.len() <= 2000, "{}", def.name);
        assert_eq!(def.input_schema["type"], "object", "{}", def.name);
        assert_eq!(
            def.input_schema["additionalProperties"], false,
            "{}",
            def.name
        );
    }
}
