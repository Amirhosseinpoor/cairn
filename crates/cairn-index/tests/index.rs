//! The index end to end: scanning, incremental updates, ranking
//! (T-CTX-006..010; SPEC §5.2, §5.3).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cairn_index::{cache_path, Index, Query, ScanOptions};
use cairn_search::{IgnoreEngine, IgnoreOptions};

struct Workspace {
    tmp: tempfile::TempDir,
    root: PathBuf,
}

fn workspace() -> Workspace {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().canonicalize().expect("canonical").join("ws");
    std::fs::create_dir_all(&root).expect("root");
    Workspace { tmp, root }
}

impl Workspace {
    fn write(&self, rel: &str, text: impl AsRef<[u8]>) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, text).expect("write");
    }

    fn engine(&self) -> IgnoreEngine {
        IgnoreEngine::new(&self.root, &IgnoreOptions::default())
    }

    fn index(&self) -> Index {
        Index::open_in_memory(&self.root).expect("index")
    }

    fn scan(&self, index: &Index) -> cairn_index::ScanReport {
        index
            .scan(&self.engine(), &ScanOptions::default())
            .expect("scans")
    }
}

fn paths(ranked: &[cairn_index::Ranked]) -> Vec<&str> {
    ranked.iter().map(|r| r.path.as_str()).collect()
}

#[test]
fn a_scan_stores_symbols_and_resolves_imports_into_edges() {
    let ws = workspace();
    ws.write("app/__init__.py", "");
    ws.write(
        "app/models.py",
        "class User:\n    pass\n\ndef make_user():\n    return User()\n",
    );
    ws.write(
        "app/main.py",
        "from app.models import User, make_user\n\ndef run():\n    return make_user()\n",
    );
    ws.write(
        "web/util.ts",
        "export function slug(s: string) { return s; }\n",
    );
    ws.write(
        "web/page.ts",
        "import { slug } from './util';\nexport function title() { return slug('x'); }\n",
    );
    ws.write(
        "src/lib.rs",
        "pub mod shapes;\nuse crate::shapes::Circle;\npub fn area() -> f64 { Circle::new().r }\n",
    );
    ws.write("src/shapes.rs", "pub struct Circle { pub r: f64 }\nimpl Circle { pub fn new() -> Self { Circle { r: 1.0 } } }\n");
    ws.write("README.md", "# demo\n");
    let index = ws.index();
    assert!(index.degraded(), "nothing scanned yet");
    let report = ws.scan(&index);
    assert!(!index.degraded());
    assert_eq!(report.seen, 8);
    assert_eq!(report.parsed, 8);
    assert_eq!(report.errors, 0);
    let stats = index.stats().unwrap();
    assert_eq!(stats.files, 8);
    assert!(stats.symbols >= 9, "{stats:?}");

    let edges = index.edges().unwrap();
    let has = |from: &str, to: &str, kind: &str| {
        edges
            .iter()
            .any(|(f, t, k)| f == from && t == to && k == kind)
    };
    assert!(has("app/main.py", "app/models.py", "import"), "{edges:?}");
    assert!(has("web/page.ts", "web/util.ts", "import"));
    assert!(has("src/lib.rs", "src/shapes.rs", "import"));
    // `make_user` is called in main.py and defined in the file it imports.
    assert!(has("app/main.py", "app/models.py", "call"), "{edges:?}");
    assert!(has("web/page.ts", "web/util.ts", "call"));
    // Nothing links files that never import each other.
    assert!(!edges
        .iter()
        .any(|(f, t, _)| f == "web/page.ts" && t == "app/models.py"));

    let symbols = index.symbols("app/models.py").unwrap();
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["User", "make_user"]);
}

#[test]
fn ignored_binary_and_oversized_files_are_not_symbols() {
    let ws = workspace();
    ws.write(".gitignore", "generated/\n");
    ws.write("generated/out.rs", "pub fn skipped() {}\n");
    ws.write("src/a.rs", "pub fn kept() {}\n");
    ws.write("blob.rs", b"fn x() {}\0\0\0\x01\x02binary".as_slice());
    ws.write("big.rs", "// padding\n".repeat(200_000));
    ws.write("broken.rs", "fn good() {}\nfn bad( {\n");
    let index = ws.index();
    let report = ws.scan(&index);
    assert_eq!(index.status_of("generated/out.rs").unwrap(), None);
    assert_eq!(
        index.status_of("src/a.rs").unwrap().as_deref(),
        Some("indexed")
    );
    assert_eq!(
        index.status_of("blob.rs").unwrap().as_deref(),
        Some("binary")
    );
    assert_eq!(
        index.status_of("big.rs").unwrap().as_deref(),
        Some("too_large")
    );
    assert_eq!((report.binary, report.too_large), (1, 1));
    // A syntax error does not fail the scan; what parses is kept.
    assert_eq!(
        index.status_of("broken.rs").unwrap().as_deref(),
        Some("indexed")
    );
    let symbols = index.symbols("broken.rs").unwrap();
    assert!(symbols.iter().any(|s| s.simple_name == "good"));
    assert!(index.symbols("blob.rs").unwrap().is_empty());
}

#[test]
fn a_second_scan_skips_what_has_not_changed() {
    let ws = workspace();
    for i in 0..20 {
        ws.write(&format!("src/m{i}.rs"), format!("pub fn f{i}() {{}}\n"));
    }
    let index = ws.index();
    let first = ws.scan(&index);
    assert_eq!(first.parsed, 20);
    let second = ws.scan(&index);
    assert_eq!(
        (second.parsed, second.unchanged, second.removed),
        (0, 20, 0)
    );
}

#[test]
fn edits_additions_and_removals_are_picked_up() {
    let ws = workspace();
    ws.write("src/a.rs", "pub fn old_name() {}\n");
    ws.write("src/b.rs", "pub fn stays() {}\n");
    let index = ws.index();
    ws.scan(&index);
    // Edit (a different size, so the fast path cannot hide it).
    ws.write(
        "src/a.rs",
        "pub fn brand_new_name() {}\npub fn extra() {}\n",
    );
    ws.write("src/c.rs", "pub fn added() {}\n");
    std::fs::remove_file(ws.root.join("src/b.rs")).unwrap();
    let report = ws.scan(&index);
    assert_eq!((report.parsed, report.removed), (2, 1));
    let names: Vec<String> = index
        .symbols("src/a.rs")
        .unwrap()
        .into_iter()
        .map(|s| s.simple_name)
        .collect();
    assert_eq!(names, ["brand_new_name", "extra"]);
    assert_eq!(index.status_of("src/b.rs").unwrap(), None);
    assert_eq!(index.stats().unwrap().files, 2);
}

#[test]
fn a_touched_file_with_the_same_bytes_is_not_reparsed() {
    let ws = workspace();
    ws.write("src/a.rs", "pub fn same() {}\n");
    let index = ws.index();
    ws.scan(&index);
    // Rewrite identical content: new mtime, same hash.
    std::thread::sleep(Duration::from_millis(20));
    ws.write("src/a.rs", "pub fn same() {}\n");
    let report = ws.scan(&index);
    assert_eq!((report.parsed, report.rehashed), (0, 1));
}

#[test]
fn an_import_that_starts_resolving_when_its_target_appears_gains_an_edge() {
    let ws = workspace();
    ws.write(
        "web/page.ts",
        "import { x } from './later';\nexport const y = x;\n",
    );
    let index = ws.index();
    ws.scan(&index);
    assert!(index.edges().unwrap().is_empty());
    ws.write("web/later.ts", "export const x = 1;\n");
    ws.scan(&index);
    assert_eq!(
        index.edges().unwrap(),
        [(
            "web/page.ts".to_string(),
            "web/later.ts".to_string(),
            "import".to_string()
        )]
    );
}

#[test]
fn refresh_updates_only_the_named_paths() {
    let ws = workspace();
    ws.write("src/a.rs", "pub fn a() {}\n");
    ws.write("src/b.rs", "pub fn b() {}\n");
    let index = ws.index();
    ws.scan(&index);
    ws.write("src/a.rs", "pub fn a2() {}\npub fn more() {}\n");
    std::fs::remove_file(ws.root.join("src/b.rs")).unwrap();
    ws.write("src/new.rs", "pub fn n() {}\n");
    let touched = [
        ws.root.join("src/a.rs"),
        ws.root.join("src/b.rs"),
        ws.root.join("src/new.rs"),
    ];
    let report = index.refresh(&touched, &ScanOptions::default()).unwrap();
    assert_eq!((report.parsed, report.removed), (2, 1));
    assert_eq!(index.symbols("src/a.rs").unwrap().len(), 2);
    assert_eq!(index.status_of("src/b.rs").unwrap(), None);
    // Paths outside the workspace are ignored.
    let report = index
        .refresh(
            &[PathBuf::from("/definitely/elsewhere.rs")],
            &ScanOptions::default(),
        )
        .unwrap();
    assert_eq!(report.seen, 0);
}

/// §5.3 rule 4.
#[test]
fn an_index_file_from_another_schema_version_is_rebuilt() {
    let ws = workspace();
    ws.write("src/a.rs", "pub fn a() {}\n");
    let db = ws.tmp.path().join("cache/index.sqlite3");
    {
        let index = Index::open(&db, &ws.root).unwrap();
        ws.scan(&index);
        assert_eq!(index.stats().unwrap().files, 1);
    }
    // A reopen finds the rows.
    assert_eq!(
        Index::open(&db, &ws.root).unwrap().stats().unwrap().files,
        1
    );
    assert!(!Index::open(&db, &ws.root).unwrap().degraded());
    // Another version's file is thrown away.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let index = Index::open(&db, &ws.root).unwrap();
    assert_eq!(index.stats().unwrap().files, 0);
    assert!(index.degraded());
    // So is something that is not a database at all.
    drop(index);
    std::fs::write(&db, b"this is not sqlite").unwrap();
    let index = Index::open(&db, &ws.root).unwrap();
    assert_eq!(index.stats().unwrap().files, 0);
}

#[test]
fn the_cache_path_depends_on_the_workspace() {
    let cache = Path::new("/cache");
    let a = cache_path(cache, Path::new("/work/a"));
    let b = cache_path(cache, Path::new("/work/b"));
    assert_ne!(a, b);
    assert_eq!(a, cache_path(cache, Path::new("/work/a")));
    assert!(a.starts_with("/cache/index"));
    assert!(a.extension().is_some_and(|e| e == "sqlite3"));
}

// ------------------------------------------------------------- ranking

/// A repository big enough (> 50 files) for the blend, with a parser in it.
fn big_repo(ws: &Workspace) {
    ws.write("src/parser.rs", "/// Turns text into a tree.\npub struct Parser { pos: usize }\nimpl Parser {\n    pub fn parse(&mut self) {}\n    pub fn parse_expr(&mut self) {}\n}\n");
    ws.write(
        "src/lexer.rs",
        "use crate::parser::Parser;\npub fn lex(p: &Parser) {}\n",
    );
    ws.write("src/render.rs", "pub fn render() {}\n");
    for i in 0..60 {
        ws.write(
            &format!("src/gen/g{i}.rs"),
            format!("pub fn generated_{i}() {{}}\n"),
        );
    }
}

/// T-CTX-007: the query finds the parser, with the lines that matched.
#[test]
fn t_ctx_007_a_query_ranks_the_matching_file_first_with_lines_of_interest() {
    let ws = workspace();
    big_repo(&ws);
    let index = ws.index();
    ws.scan(&index);
    let ranked = index
        .rank(&Query {
            text: "parser",
            top_k: 40,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(ranked.len(), 40);
    assert_eq!(ranked[0].path, "src/parser.rs");
    assert!(
        ranked[0].lines_of_interest.contains(&2),
        "{:?}",
        ranked[0].lines_of_interest
    );
    assert!(ranked[0].score > ranked[1].score);
    let parse = index
        .rank(&Query {
            text: "parse_expr",
            ..Query::default()
        })
        .unwrap();
    assert_eq!(parse[0].path, "src/parser.rs");
    assert!(parse[0].lines_of_interest.contains(&5));
    // Default top-K.
    assert_eq!(index.rank(&Query::default()).unwrap().len(), 40);
}

#[test]
fn the_file_being_worked_on_pulls_its_neighbours_up() {
    let ws = workspace();
    big_repo(&ws);
    let index = ws.index();
    ws.scan(&index);
    let ranked = index
        .rank(&Query {
            current_file: Some("src/lexer.rs"),
            top_k: 5,
            ..Query::default()
        })
        .unwrap();
    let top = paths(&ranked);
    assert_eq!(top[0], "src/lexer.rs");
    assert!(
        top.contains(&"src/parser.rs"),
        "{top:?}: lexer imports parser"
    );
    assert!(!top.contains(&"src/render.rs"));
    // Files touched this session count too.
    let touched = ["src/render.rs".to_string()];
    let ranked = index
        .rank(&Query {
            touched: &touched,
            top_k: 3,
            ..Query::default()
        })
        .unwrap();
    assert_eq!(ranked[0].path, "src/render.rs");
}

#[test]
fn a_small_repository_ranks_by_text_alone() {
    let ws = workspace();
    ws.write("a.rs", "pub fn alpha() {}\n");
    ws.write("b.rs", "pub fn needle() {}\n");
    ws.write("c.rs", "use crate::a;\npub fn gamma() {}\n");
    let index = ws.index();
    ws.scan(&index);
    let ranked = index
        .rank(&Query {
            text: "needle",
            ..Query::default()
        })
        .unwrap();
    assert_eq!(ranked[0].path, "b.rs");
    assert!(ranked[1].score < ranked[0].score);
}

#[test]
fn ranking_is_deterministic() {
    let ws = workspace();
    big_repo(&ws);
    let index = ws.index();
    ws.scan(&index);
    let q = Query {
        text: "generated",
        current_file: Some("src/lexer.rs"),
        ..Query::default()
    };
    let first = index.rank(&q).unwrap();
    for _ in 0..5 {
        assert_eq!(index.rank(&q).unwrap(), first);
    }
}

/// T-CTX-010 / P-09: a cold 1,000-file index inside 1.5 s, a warm one inside
/// 150 ms.
#[test]
fn t_ctx_010_a_thousand_files_index_cold_in_1500_ms_and_verify_warm_in_150() {
    let ws = workspace();
    for i in 0..1000 {
        let dir = i % 25;
        ws.write(
            &format!("src/pkg{dir}/mod{i}.rs"),
            format!("use crate::pkg{dir}::mod{};\n/// Doc {i}.\npub struct S{i} {{ x: u32 }}\nimpl S{i} {{ pub fn run(&self) -> u32 {{ self.x }} }}\npub fn make_{i}() -> S{i} {{ S{i} {{ x: {i} }} }}\n", (i + 1) % 1000),
        );
    }
    let index = ws.index();
    let cold = Instant::now();
    let report = ws.scan(&index);
    let cold = cold.elapsed();
    assert_eq!(report.parsed, 1000);
    assert!(cold < Duration::from_millis(1500), "cold {cold:?}");
    let warm = Instant::now();
    let report = ws.scan(&index);
    let warm = warm.elapsed();
    assert_eq!(report.unchanged, 1000);
    assert!(warm < Duration::from_millis(150), "warm {warm:?}");
    // Ranking over it is quick too.
    let started = Instant::now();
    let ranked = index
        .rank(&Query {
            text: "make_7",
            ..Query::default()
        })
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(ranked[0].path.ends_with("mod7.rs"), "{:?}", ranked[0].path);
}
