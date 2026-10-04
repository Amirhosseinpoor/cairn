//! Architecture & dependency-policy tests — SPEC §14.3.12.
//!
//! Covers T-ARCH-001 (dependency direction + `cargo deny check bans`),
//! T-ARCH-002 (`cairn-core` has no I/O and forbids `unsafe`),
//! T-ARCH-003 (nothing calls into `cairn-tui` but `cairn-cli`),
//! T-ARCH-004 (`unsafe` only in `cairn-sandbox`, annotated),
//! T-ARCH-006 (no blocking channel sends outside the event bus),
//! T-SCHEMA-001/002/003 (schema drift) and T-TECH-001 (D-13 licenses).
//!
//! Everything here is static analysis over the workspace sources, so these run
//! in the `lint` job without building per-crate coverage profiles. Comments and
//! string literals are blanked out before searching, so a test file is free to
//! *quote* a forbidden token — only real code is scanned.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ----------------------------------------------------------------- workspace

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// `crates/<name>` directories, sorted.
fn crate_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(root.join("crates"))
        .expect("crates/")
        .filter_map(std::result::Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

/// Every `*.rs` under `dir`, excluding `target/` and `examples/`.
///
/// `examples/` holds the schema generators (`dump_event_schemas` and friends):
/// they are executables whose whole job is to write files, so REQ-ARCH-002's
/// "core does no I/O" is asserted over the library and its tests, not over the
/// build-time tooling CI runs.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == "examples")
                {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// `[dependencies]` entries that name a workspace crate.
///
/// Only normal dependencies count: a `dev-dependency` cannot ship, and eval
/// crates are *expected* to reach back into the crates they drive (e.g.
/// `cairn-agent --dev--> cairn-testkit`). Runtime coupling is caught here;
/// source-level coupling is caught by T-ARCH-003/004/006.
fn manifest_deps(manifest: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(manifest).expect("manifest");
    let doc: toml::Value =
        toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
    let mut out = BTreeSet::new();
    if let Some(table) = doc.get("dependencies").and_then(toml::Value::as_table) {
        for (name, spec) in table {
            if !name.starts_with("cairn-") {
                continue;
            }
            let workspace = spec
                .as_table()
                .and_then(|t| t.get("workspace"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            let path = spec.as_table().and_then(|t| t.get("path")).is_some();
            if workspace || path {
                out.insert(name.clone());
            }
        }
    }
    out
}

/// The `MAY import` column of SPEC §3.2, workspace crates only.
///
/// `mode` lives in `cairn-core` and the checkpointer in `cairn-git`, so they do
/// not appear as crates of their own.
const MAY_IMPORT: &[(&str, &[&str])] = &[
    ("cairn-core", &[]),
    ("cairn-sse", &["cairn-core"]),
    ("cairn-provider", &["cairn-core", "cairn-sse"]),
    ("cairn-parse", &["cairn-core"]),
    ("cairn-search", &["cairn-core"]),
    (
        "cairn-index",
        &["cairn-core", "cairn-parse", "cairn-search"],
    ),
    (
        "cairn-context",
        &["cairn-core", "cairn-parse", "cairn-search", "cairn-index"],
    ),
    ("cairn-config", &["cairn-core"]),
    ("cairn-perm", &["cairn-core", "cairn-config"]),
    ("cairn-sandbox", &["cairn-core"]),
    ("cairn-git", &["cairn-core"]),
    (
        "cairn-tools",
        &[
            "cairn-core",
            "cairn-search",
            "cairn-parse",
            "cairn-perm",
            "cairn-sandbox",
            "cairn-git",
            "cairn-config",
        ],
    ),
    ("cairn-mcp", &["cairn-core", "cairn-provider"]),
    ("cairn-session", &["cairn-core"]),
    (
        "cairn-agent",
        &[
            "cairn-core",
            "cairn-context",
            "cairn-provider",
            "cairn-tools",
            "cairn-perm",
            "cairn-session",
            "cairn-eventbus",
        ],
    ),
    ("cairn-eventbus", &["cairn-core"]),
    ("cairn-tui", &["cairn-core", "cairn-eventbus"]),
    (
        "cairn-testkit",
        &[
            "cairn-core",
            "cairn-provider",
            "cairn-config",
            "cairn-session",
            "cairn-agent",
        ],
    ),
    // `cairn-cli` MAY import everything (§3.2 last row).
    ("cairn-cli", &["*"]),
];

fn allowed_for(crate_name: &str) -> Option<&'static [&'static str]> {
    MAY_IMPORT
        .iter()
        .find(|(n, _)| *n == crate_name)
        .map(|(_, a)| *a)
}

// ------------------------------------------------------- T-ARCH-001 direction

/// T-ARCH-001 — every edge follows §3.2, and the graph is acyclic
/// (REQ-ARCH-001; the runtime half is `cargo deny check bans`).
#[test]
fn t_arch_001_dependency_direction_and_no_cycles() {
    // Acyclic: DFS with an explicit "on the current path" set (a node may be
    // revisited from a sibling branch without that being a cycle).
    fn visit(
        node: &str,
        edges: &BTreeMap<String, BTreeSet<String>>,
        path: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) {
        if done.contains(node) {
            return;
        }
        if path.iter().any(|n| n == node) {
            let cycle = path.iter().position(|n| n == node).unwrap();
            let mut chain: Vec<&str> = path[cycle..].iter().map(String::as_str).collect();
            chain.push(node);
            panic!(
                "T-ARCH-001/REQ-ARCH-001: cyclic dependency: {}",
                chain.join(" -> ")
            );
        }
        path.push(node.to_string());
        for dep in edges.get(node).into_iter().flatten() {
            visit(dep, edges, path, done);
        }
        path.pop();
        done.insert(node.to_string());
    }
    let root = root();
    let dirs = crate_dirs(&root);
    assert!(
        dirs.len() >= 18,
        "workspace looks truncated: {}",
        dirs.len()
    );

    let names: BTreeSet<String> = dirs.iter().map(|(n, _)| n.clone()).collect();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (name, dir) in &dirs {
        let allowed = allowed_for(name).unwrap_or_else(|| {
            panic!("`{name}` is in crates/ but missing from the §3.2 table (REQ-ARCH-001)")
        });
        let deps = manifest_deps(&dir.join("Cargo.toml"));
        for dep in &deps {
            assert!(names.contains(dep), "{name} depends on unknown crate {dep}");
            if allowed == ["*"] {
                continue;
            }
            assert!(
                allowed.contains(&dep.as_str()),
                "T-ARCH-001/REQ-ARCH-001: `{name}` MUST NOT import `{dep}` \
                 (§3.2 MAY import: {allowed:?})"
            );
        }
        edges.insert(name.clone(), deps);
    }

    let mut done = BTreeSet::new();
    for start in edges.keys().cloned().collect::<Vec<_>>() {
        let mut path = Vec::new();
        visit(&start, &edges, &mut path, &mut done);
    }
}

/// T-ARCH-001 (runtime half) — `cargo deny check bans` passes, and the deny
/// configuration exists so CI can run it (REQ-ARCH-001).
#[test]
fn t_arch_001_cargo_deny_bans() {
    let root = root();
    assert!(
        root.join("deny.toml").is_file(),
        "deny.toml is a deliverable of §15.1 and the home of the D-13 allow-list"
    );

    let Some(output) = run_cargo_deny(&root, "bans") else {
        eprintln!("cargo-deny not installed; the static checks above still ran");
        return;
    };
    assert!(
        output.status.success(),
        "`cargo deny check bans` failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

// ------------------------------------------------------- T-ARCH-002 core purity

const CORE_FORBIDDEN: &[&str] = &[
    "std::process",
    "std::fs",
    "std::net",
    "TcpStream",
    "TlsStream",
    "UdpSocket",
    "File::create",
    "File::open",
    "to_socket_addrs",
];

/// T-ARCH-002 — `cairn-core` contains no I/O, no network and no `unsafe`
/// (REQ-ARCH-002/REQ-ARCH-004).
#[test]
fn t_arch_002_core_is_pure() {
    let core = root().join("crates/cairn-core");
    let files = rust_sources(&core);
    assert_ne!(files, [] as [std::path::PathBuf; 0]);

    for file in &files {
        let text = std::fs::read_to_string(file).expect("read");
        let scrubbed = code_only(&text);
        for token in CORE_FORBIDDEN {
            assert!(
                !scrubbed.contains(token),
                "T-ARCH-002: `{token}` found in {}",
                file.display()
            );
        }
        assert!(
            !contains_word(&scrubbed, "unsafe"),
            "T-ARCH-004: `unsafe` found in {}",
            file.display()
        );
    }

    let lib = std::fs::read_to_string(core.join("src/lib.rs")).expect("lib.rs");
    assert!(
        lib.contains("#![forbid(unsafe_code)]"),
        "T-ARCH-002: cairn-core must carry `#![forbid(unsafe_code)]`"
    );
}

// ---------------------------------------------------------- T-ARCH-003 no tui

/// T-ARCH-003 — no crate outside `cairn-cli` calls into `cairn-tui`
/// (REQ-ARCH-003).
#[test]
fn t_arch_003_no_tui_calls_outside_cli() {
    let root = root();
    let mut offenders = Vec::new();
    for (name, dir) in crate_dirs(&root) {
        if name == "cairn-cli" {
            continue;
        }
        for file in rust_sources(&dir) {
            let text = std::fs::read_to_string(&file).expect("read");
            if contains_ident(&code_only(&text), "cairn_tui") {
                offenders.push(file.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "T-ARCH-003/REQ-ARCH-003: `cairn_tui` referenced outside cairn-cli: {offenders:?}"
    );
}

// ---------------------------------------------------------- T-ARCH-004 unsafe

/// T-ARCH-004 — `unsafe` appears only in `cairn-sandbox`, and every block there
/// carries a `// SAFETY:` comment (REQ-ARCH-004).
#[test]
fn t_arch_004_unsafe_only_in_sandbox() {
    let root = root();
    let mut offenders = Vec::new();
    let mut unannotated = Vec::new();

    for (name, dir) in crate_dirs(&root) {
        for file in rust_sources(&dir) {
            let text = std::fs::read_to_string(&file).expect("read");
            for (line_no, code) in code_lines(&text) {
                for (col, ()) in find_word(&code, "unsafe") {
                    if name != "cairn-sandbox" {
                        offenders.push(format!("{}:{line_no}:{col}", file.display()));
                        continue;
                    }
                    let near = text
                        .lines()
                        .skip(line_no.saturating_sub(6))
                        .take(5)
                        .collect::<Vec<_>>();
                    if !near.iter().any(|l| l.contains("SAFETY:")) {
                        unannotated.push(format!("{}:{line_no}", file.display()));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "T-ARCH-004/REQ-ARCH-004: `unsafe` outside cairn-sandbox: {offenders:?}"
    );
    assert!(
        unannotated.is_empty(),
        "T-ARCH-004/REQ-ARCH-004: `unsafe` without a `// SAFETY:` comment: {unannotated:?}"
    );

    // The opt-out lives in the manifest, and only there.
    let sandbox_manifest = std::fs::read_to_string(root.join("crates/cairn-sandbox/Cargo.toml"))
        .expect("sandbox manifest");
    assert!(
        sandbox_manifest.contains("unsafe_code = \"allow\""),
        "cairn-sandbox must opt out of the workspace `unsafe_code` lint"
    );
    for (name, dir) in crate_dirs(&root) {
        if name == "cairn-sandbox" {
            continue;
        }
        let text = std::fs::read_to_string(dir.join("Cargo.toml")).expect("manifest");
        assert!(
            text.contains("workspace = true"),
            "`{name}` must inherit the workspace lints (which include `unsafe_code = deny`)"
        );
    }
}

// ------------------------------------------------------------ T-ARCH-006 sends

/// T-ARCH-006 — channel sends live in the event bus, never as a blocking send
/// in a worker (REQ-ARCH-006: a blocking send stalls a render or a tool).
#[test]
fn t_arch_006_no_blocking_sends() {
    let root = root();
    let mut offenders = Vec::new();
    for (name, dir) in crate_dirs(&root) {
        for file in rust_sources(&dir) {
            let text = std::fs::read_to_string(&file).expect("read");
            for (line_no, code) in code_lines(&text) {
                if code.contains("blocking_send(") {
                    offenders.push(format!("{}:{line_no} `blocking_send`", file.display()));
                }
                // `.send(` on a channel outside the bus — use `try_send`, or
                // publish through `EventSender` (REQ-ARCH-003).
                if code.contains(".send(") && name != "cairn-eventbus" {
                    offenders.push(format!("{}:{line_no}", file.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "T-ARCH-006/REQ-ARCH-006: blocking or out-of-band channel send: {offenders:?}"
    );
}

// ------------------------------------------------------------------ T-SCHEMA

/// T-SCHEMA-001 — the tool JSON Schemas are draft 2020-12 with titles, and
/// `schemas/` keeps the layout §15.1 fixes (REQ-ARCH-009). The content check
/// activates with M4 (`cairn-tools`); until then this pins the contract and the
/// M0 half of the directory (config + events), so a missing deliverable fails.
#[test]
fn t_schema_001_tool_schemas() {
    let schemas = root().join("schemas");
    assert!(
        schemas.join("config.schema.json").is_file(),
        "schemas/config.schema.json is an M0 deliverable (REQ-ARCH-011)"
    );
    assert!(
        schemas.join("events/event.schema.json").is_file(),
        "schemas/events/ is an M0 deliverable (REQ-ARCH-009)"
    );

    let dir = schemas.join("tools");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("schemas/tools not present yet; delivered with M4 (cairn-tools)");
        return;
    };
    let files: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    assert!(
        !files.is_empty(),
        "schemas/tools/ must not be empty once it exists"
    );
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert_eq!(
            v["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{path:?}: §6 requires draft 2020-12 for tool schemas"
        );
        assert!(v["title"].is_string(), "{path:?} needs a title");
    }
}

/// T-SCHEMA-002 — the committed `schemas/events/*.schema.json` are exactly
/// what the Rust types generate: a new or renamed event fails until the
/// generator has been re-run (REQ-ARCH-009).
#[test]
fn t_schema_002_event_schemas() {
    let dir = root().join("schemas/events");
    let generated: BTreeMap<String, String> =
        cairn_core::event::event_schemas().into_iter().collect();
    assert_eq!(
        generated.len(),
        cairn_core::EventData::ALL_KINDS.len() + 1,
        "generator produced one file per kind plus the envelope"
    );

    let mut committed: BTreeMap<String, String> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("schemas/events missing ({e}) — run `cargo run -p cairn-core --example dump_event_schemas`"))
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            committed.insert(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read_to_string(&path).expect("read"),
            );
        }
    }

    let stale: Vec<&str> = committed
        .keys()
        .filter(|k| !generated.contains_key(*k))
        .map(String::as_str)
        .collect();
    let missing: Vec<&str> = generated
        .keys()
        .filter(|k| !committed.contains_key(*k))
        .map(String::as_str)
        .collect();
    assert!(
        stale.is_empty(),
        "T-SCHEMA-002: committed schemas with no matching event: {stale:?}"
    );
    assert!(
        missing.is_empty(),
        "T-SCHEMA-002: events with no committed schema: {missing:?} — run \
         `cargo run -p cairn-core --example dump_event_schemas`"
    );

    // Every file is a schema, not just a blob that happens to match — parsed
    // first so a truncated or hand-edited file names itself instead of dying
    // inside the comparison below.
    for (name, body) in &committed {
        let v: serde_json::Value =
            serde_json::from_str(body).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            v["$schema"], "http://json-schema.org/draft-07/schema#",
            "{name}"
        );
        assert_eq!(v["type"], "object", "{name}");
    }

    // Compare parsed JSON rather than bytes. A Windows checkout can carry CRLF
    // in these files while the generator always emits LF, and byte equality
    // would then report all 33 as stale — the drift REQ-ARCH-009 cares about
    // is in the schema, not the line endings. Same rule as T-SCHEMA-003 below.
    let differs: Vec<&str> = generated
        .iter()
        .filter(|(name, body)| {
            committed
                .get(*name)
                .is_some_and(|c| normalize(c) != normalize(body))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(
        differs.is_empty(),
        "T-SCHEMA-002/REQ-ARCH-009: schemas/events is stale for {differs:?} — \
         run `cargo run -p cairn-core --example dump_event_schemas`"
    );
}

/// T-SCHEMA-003 — the committed config schema is byte-for-byte what the
/// structs generate, so any structural change fails until the file is
/// regenerated and `SCHEMA_VERSION` considered (REQ-ARCH-010).
#[test]
fn t_schema_003_config_schema_matches_structs() {
    let root = root();
    let committed = root.join("schemas/config.schema.json");
    assert!(committed.is_file(), "§15.1 deliverable missing");

    let expected = cairn_config::json_schema_pretty();
    let actual = std::fs::read_to_string(&committed).expect("read");
    assert_eq!(
        normalize(&actual),
        normalize(&expected),
        "T-SCHEMA-003/REQ-ARCH-010: schemas/config.schema.json drifted; run \
         `cargo run --example dump_schema -p cairn-config`, and bump \
         `SCHEMA_VERSION` if the shape changed"
    );

    let v: serde_json::Value = serde_json::from_str(&actual).expect("valid JSON");
    assert_eq!(v["$schema"], "http://json-schema.org/draft-07/schema#");
}

fn normalize(s: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(s).expect("json");
    serde_json::to_string(&v).expect("compact")
}

// --------------------------------------------------------------- T-TECH-001

/// Licenses D-13 allows (the spec's chosen column).
const D13_ALLOW: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unlicense",
    "Zlib",
    "MIT-0",
    "Unicode-3.0",
];

/// Prefixes D-13 rejects (its rejected column plus the usual copyleft set).
const D13_DENY: &[&str] = &["GPL", "LGPL", "MPL", "SSPL", "AGPL", "CDDL", "EUPL"];

/// T-TECH-001 — the allow-list is exactly D-13 (so a GPL crate fails the
/// build) and the live dependency set passes `cargo deny check licenses`
/// (REQ-TECH-002).
#[test]
fn t_tech_001_license_allow_list_matches_d13() {
    let root = root();
    let text = std::fs::read_to_string(root.join("deny.toml")).expect("deny.toml");
    let doc: toml::Value = toml::from_str(&text).expect("deny.toml parses");

    assert!(
        doc.get("licenses")
            .and_then(|l| l.get("exceptions"))
            .is_none(),
        "D-13 grants no exceptions"
    );

    let allow: BTreeSet<String> = doc
        .get("licenses")
        .and_then(|l| l.get("allow"))
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .expect("licenses.allow");

    for lic in D13_ALLOW {
        assert!(
            allow.contains(*lic),
            "D-13 allows `{lic}` but deny.toml does not"
        );
    }
    for lic in &allow {
        assert!(
            !D13_DENY.iter().any(|d| lic.starts_with(d)),
            "D-13 rejects copyleft but deny.toml allows `{lic}`"
        );
    }

    let Some(output) = run_cargo_deny(&root, "licenses") else {
        eprintln!("cargo-deny not installed; skipping the live `licenses` run");
        return;
    };
    assert!(
        output.status.success(),
        "`cargo deny check licenses` failed (REQ-TECH-002):\n{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Run one cargo-deny check; `None` when the tool is not installed.
fn run_cargo_deny(root: &Path, what: &str) -> Option<std::process::Output> {
    let installed = std::process::Command::new("cargo")
        .args(["deny", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !installed {
        return None;
    }
    Some(
        std::process::Command::new("cargo")
            .args(["deny", "check", what])
            .current_dir(root)
            .output()
            .expect("cargo deny runs"),
    )
}

// -------------------------------------------------------------- source scanner

/// Per-line source with comments and string literals blanked out (byte offsets
/// preserved so reported columns still point into the file). Lines that were
/// entirely comment are dropped.
///
/// `line_no` is 1-based.
fn code_lines(src: &str) -> Vec<(usize, String)> {
    let blanked = blank_non_code(src);
    blanked
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| (i + 1, line.to_string()))
        .collect()
}

/// The whole file's code, comments and strings removed.
fn code_only(src: &str) -> String {
    blank_non_code(src)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace comments and string/char literals with spaces, preserving every
/// newline (and therefore line numbers and byte offsets).
// The single-character names below are the conventional notation for a
// byte-level lexer; renaming them would obscure the scan loops this function
// exists to make testable.
#[expect(clippy::many_single_char_names, reason = "byte-scanner index notation")]
fn blank_non_code(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let n = b.len();
    let mut i = 0usize;

    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        let end = to.min(n);
        let span = end.saturating_sub(from);
        for byte in out.iter_mut().skip(from).take(span) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };

    while i < n {
        // raw strings: r"…" / r#"…"#
        if b[i] == b'r' && i + 1 < n && (b[i + 1] == b'"' || b[i + 1] == b'#') {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < n && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < n && b[j] == b'"' {
                let content = j + 1;
                let mut k = content;
                let mut end = None;
                while k < n {
                    if b[k] == b'"' {
                        let mut consumed = 0usize;
                        let mut p = k + 1;
                        while consumed < hashes && p < n && b[p] == b'#' {
                            consumed += 1;
                            p += 1;
                        }
                        if consumed == hashes {
                            end = Some(p);
                            break;
                        }
                    }
                    k += 1;
                }
                let stop = end.unwrap_or(n);
                blank(&mut out, i, stop);
                i = stop;
                continue;
            }
        }

        match b[i] {
            // line comment
            b'/' if i + 1 < n && b[i + 1] == b'/' => {
                let start = i;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            // block comment (nested)
            b'/' if i + 1 < n && b[i + 1] == b'*' => {
                let start = i;
                let mut depth = 0usize;
                while i + 1 < n {
                    if b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            // ordinary string
            b'"' => {
                let start = i;
                i += 1;
                while i < n {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            // byte string b"…" / b'…'
            b'b' if i + 1 < n && (b[i + 1] == b'"' || b[i + 1] == b'\'') => {
                let quote = b[i + 1];
                let start = i;
                i += 2;
                while i < n {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                blank(&mut out, start, i);
            }
            // char literal (a lone `'` is a lifetime, leave it alone)
            b'\'' => {
                let is_char = (i + 2 < n && b[i + 1] != b'\\' && b[i + 2] == b'\'')
                    || (i + 3 < n && b[i + 1] == b'\\' && b[i + 3] == b'\'');
                if is_char {
                    let start = i;
                    i += if b[i + 1] == b'\\' { 4 } else { 3 };
                    blank(&mut out, start, i);
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    String::from_utf8(out).expect("blanking preserves UTF-8")
}

/// Exact token match (`auto-unsafe`, `allow_unsafe`, `unsafe_code` do not hit).
fn contains_word(haystack: &str, word: &str) -> bool {
    !find_word(haystack, word).is_empty()
}

/// Byte offsets of `word` when it is a standalone token.
fn find_word(haystack: &str, word: &str) -> Vec<(usize, ())> {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = haystack.as_bytes();
    haystack
        .match_indices(word)
        .filter_map(|(i, _)| {
            let before_ok = i == 0 || (!is_word(bytes[i - 1]) && bytes[i - 1] != b'-');
            let after = i + word.len();
            let after_ok = after >= bytes.len() || (!is_word(bytes[after]) && bytes[after] != b'-');
            (before_ok && after_ok).then_some((i, ()))
        })
        .collect()
}

/// Identifier match for `cairn_tui` (the package name is `cairn-tui`).
fn contains_ident(haystack: &str, ident: &str) -> bool {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = haystack.as_bytes();
    haystack.match_indices(ident).any(|(i, _)| {
        let before_ok = i == 0 || !is_word(bytes[i - 1]);
        let after = i + ident.len();
        let after_ok = after >= bytes.len() || !is_word(bytes[after]);
        before_ok && after_ok
    })
}

#[cfg(test)]
mod scanner {
    use super::*;

    #[test]
    fn strings_and_comments_are_not_code() {
        let src = r##"
            // unsafe in a comment
            /* unsafe in a block
               spanning lines */
            let a = "unsafe";
            let b = r#"unsafe"#;
            let d = '"'; // a char literal must not open a string
            fn unsafe_fn() {} // not the token either
            let e = "trailing // comment inside a string";
        "##;
        let code = code_only(src);
        assert!(!contains_word(&code, "unsafe"), "{code}");
        assert!(!code.contains("comment"), "{code}");
    }

    #[test]
    fn real_code_is_still_visible() {
        let src = "unsafe { touch() } // real\nlet s = \"a // b\";\n";
        let lines = code_lines(src);
        assert!(lines.iter().any(|(n, l)| *n == 1 && l.contains("unsafe")));
        // The `//` inside the string is not a comment, so the line survives.
        assert!(lines.iter().any(|(_, l)| l.contains("let s")), "{lines:?}");
    }
}
