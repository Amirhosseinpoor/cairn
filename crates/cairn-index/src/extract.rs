//! Symbols, imports and references from source text (SPEC §5.2).
//!
//! Each language has a tree-sitter query (`queries/<language>/symbols.scm`)
//! naming the definitions (`@def`, with `@name`), imports (`@import`) and
//! uses (`@call`, `@type_ref`). Code here turns matches into [`Symbol`]s:
//! kind, enclosing container, signature and leading documentation.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use cairn_parse::Lang;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

/// The languages with symbol extraction (§5.2's MVP set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
    Go,
    Java,
    Cpp,
}

impl Language {
    pub const ALL: [Self; 7] = [
        Self::Rust,
        Self::Python,
        Self::TypeScript,
        Self::Tsx,
        Self::Go,
        Self::Java,
        Self::Cpp,
    ];

    /// By extension. JavaScript goes through the TSX grammar; `.c` and `.h`
    /// through the C++ one (§5.2: one grammar for both).
    #[must_use]
    pub fn detect(rel: &str) -> Option<Self> {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" | "js" | "jsx" | "mjs" | "cjs" => Self::Tsx,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Self::Cpp,
            _ => return None,
        })
    }

    /// The name stored in `files.language`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Tsx => "javascript",
            Self::Go => "go",
            Self::Java => "java",
            Self::Cpp => "cpp",
        }
    }

    fn parse_lang(self) -> Lang {
        match self {
            Self::Rust => Lang::Rust,
            Self::Python => Lang::Python,
            Self::TypeScript => Lang::TypeScript,
            Self::Tsx => Lang::Tsx,
            Self::Go => Lang::Go,
            Self::Java => Lang::Java,
            Self::Cpp => Lang::Cpp,
        }
    }

    fn query_source(self) -> &'static str {
        match self {
            Self::Rust => include_str!("../queries/rust/symbols.scm"),
            Self::Python => include_str!("../queries/python/symbols.scm"),
            Self::TypeScript | Self::Tsx => include_str!("../queries/typescript/symbols.scm"),
            Self::Go => include_str!("../queries/go/symbols.scm"),
            Self::Java => include_str!("../queries/java/symbols.scm"),
            Self::Cpp => include_str!("../queries/cpp/symbols.scm"),
        }
    }
}

/// One definition (§5.2's `symbols` row, minus the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// `Container::name`, or just the name.
    pub name: String,
    pub simple_name: String,
    /// `func|struct|enum|class|method|field|module|trait|impl|var`
    pub kind: &'static str,
    /// 1-based.
    pub line: u32,
    pub end_line: u32,
    pub container: Option<String>,
    /// At most 500 characters.
    pub signature: String,
    /// At most 1,000 characters of leading documentation.
    pub doc: String,
}

/// What one file yielded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    pub symbols: Vec<Symbol>,
    /// Raw import statements.
    pub imports: Vec<String>,
    /// Names called, with how often.
    pub calls: BTreeMap<String, u32>,
    /// Type names mentioned, with how often.
    pub type_refs: BTreeMap<String, u32>,
    /// The parser recovered from errors (the symbols are still real).
    pub had_errors: bool,
}

/// The file could not be parsed at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ExtractError(pub String);

fn queries() -> &'static BTreeMap<&'static str, Result<Query, String>> {
    static CELL: OnceLock<BTreeMap<&'static str, Result<Query, String>>> = OnceLock::new();
    CELL.get_or_init(|| {
        Language::ALL
            .iter()
            .map(|lang| {
                let grammar = lang.parse_lang().grammar();
                let query = grammar
                    .ok_or_else(|| "no grammar".to_string())
                    .and_then(|g| Query::new(&g, lang.query_source()).map_err(|e| e.to_string()));
                (lang.name(), query)
            })
            .collect()
    })
}

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.byte_range()]
}

/// Whether `kind` is a node a definition can sit inside.
fn container_name(node: Node<'_>, source: &str, lang: Language) -> Option<(String, &'static str)> {
    let mut at = node.parent();
    while let Some(parent) = at {
        let name_of = |field: &str| {
            parent
                .child_by_field_name(field)
                .map(|n| text(source, n).to_string())
        };
        let found = match (lang, parent.kind()) {
            (Language::Rust, "impl_item") => parent.child_by_field_name("type").map(|n| {
                let inner = if n.kind() == "generic_type" {
                    n.child_by_field_name("type").unwrap_or(n)
                } else {
                    n
                };
                (text(source, inner).to_string(), "impl")
            }),
            (Language::Rust, "trait_item") => name_of("name").map(|n| (n, "trait")),
            (Language::Rust, "mod_item") => name_of("name").map(|n| (n, "module")),
            (Language::Python, "class_definition")
            | (
                Language::TypeScript | Language::Tsx,
                "class_declaration" | "interface_declaration",
            )
            | (
                Language::Java,
                "class_declaration" | "interface_declaration" | "enum_declaration",
            ) => name_of("name").map(|n| (n, "class")),
            (Language::Cpp, "class_specifier" | "struct_specifier") => {
                name_of("name").map(|n| (n, "class"))
            }
            _ => None,
        };
        if found.is_some() {
            return found;
        }
        at = parent.parent();
    }
    None
}

/// Go methods name their receiver type; C++ out-of-class definitions name
/// theirs in the qualified identifier.
fn special_container(node: Node<'_>, source: &str, lang: Language) -> Option<String> {
    match (lang, node.kind()) {
        (Language::Go, "method_declaration") => {
            let receiver = node.child_by_field_name("receiver")?;
            let mut cursor = receiver.walk();
            let found = receiver
                .named_children(&mut cursor)
                .find_map(|param| param.child_by_field_name("type"))?;
            Some(text(source, found).trim_start_matches('*').to_string())
        }
        (Language::Cpp, "function_definition") => {
            let declarator = node.child_by_field_name("declarator")?;
            let inner = declarator.child_by_field_name("declarator")?;
            if inner.kind() == "qualified_identifier" {
                let scope = inner.child_by_field_name("scope")?;
                Some(text(source, scope).to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn kind_of(node: Node<'_>, container_kind: Option<&str>, lang: Language) -> &'static str {
    match node.kind() {
        "function_item"
        | "function_signature_item"
        | "function_definition"
        | "function_declaration"
        | "generator_function_declaration" => {
            if container_kind.is_some() && !matches!(container_kind, Some("module")) {
                "method"
            } else {
                "func"
            }
        }
        "method_definition" | "method_declaration" => "method",
        "struct_item" | "struct_specifier" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" | "interface_declaration" => "trait",
        "impl_item" => "impl",
        "mod_item" => "module",
        "class_definition" | "class_declaration" | "class_specifier" => "class",
        "field_declaration" => "field",
        "variable_declarator" => {
            let function_valued = node.child_by_field_name("value").is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function" | "function_expression"
                )
            });
            if function_valued {
                "func"
            } else {
                "var"
            }
        }
        "type_declaration" => {
            let spec = node.named_child(0);
            match spec
                .and_then(|s| s.child_by_field_name("type"))
                .map(|t| t.kind())
            {
                Some("struct_type") => "struct",
                Some("interface_type") => "trait",
                _ => {
                    let _ = lang;
                    "var"
                }
            }
        }
        _ => "var",
    }
}

/// A TypeScript `variable_declarator` is a symbol at the top of a file or in
/// a class, not a local inside a function.
fn is_local_variable(node: Node<'_>) -> bool {
    let mut at = node.parent();
    while let Some(parent) = at {
        if matches!(
            parent.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "arrow_function"
                | "function_expression"
                | "function"
                | "method_definition"
        ) {
            return true;
        }
        at = parent.parent();
    }
    false
}

fn signature_of(node: Node<'_>, source: &str, lang: Language) -> String {
    let body = text(source, node);
    let end = match lang {
        Language::Python => body.find(":\n").map_or(body.len(), |i| i + 1),
        _ => body.find(['{', ';']).unwrap_or(body.len()),
    };
    let cut = &body[..end];
    let collapsed = cut.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(500).collect()
}

fn strip_comment(line: &str) -> &str {
    let t = line.trim();
    for prefix in ["///", "//!", "/**", "/*", "//", "#", "*/", "*"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return rest.trim().trim_end_matches("*/").trim();
        }
    }
    t.trim_end_matches("*/").trim()
}

fn leading_doc(node: Node<'_>, source: &str, lang: Language) -> String {
    // Python keeps its documentation inside the body.
    if lang == Language::Python {
        let body = node.child_by_field_name("body");
        let first = body.and_then(|b| b.named_child(0));
        if let Some(stmt) = first.filter(|s| s.kind() == "expression_statement") {
            if let Some(string) = stmt.named_child(0).filter(|s| s.kind() == "string") {
                let raw = text(source, string);
                let inner = raw
                    .trim_start_matches(['r', 'b', 'u', 'R', 'B', 'U'])
                    .trim_matches(|c| c == '"' || c == '\'');
                return inner.trim().chars().take(1000).collect();
            }
        }
        return String::new();
    }
    let mut anchor = node;
    if let Some(parent) = node.parent() {
        if matches!(parent.kind(), "export_statement" | "decorated_definition") {
            anchor = parent;
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut next_row = anchor.start_position().row;
    let mut at = anchor.prev_sibling();
    while let Some(prev) = at {
        if !prev.kind().contains("comment") || prev.end_position().row + 1 < next_row {
            break;
        }
        let block: Vec<&str> = text(source, prev).lines().map(strip_comment).collect();
        lines.push(block.join(" "));
        next_row = prev.start_position().row;
        at = prev.prev_sibling();
    }
    lines.reverse();
    let doc = lines.join(" ");
    doc.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1000)
        .collect()
}

/// Read `source` as `lang`.
///
/// # Errors
/// [`ExtractError`] when no tree could be produced (a grammar problem, not a
/// syntax error: tree-sitter recovers from those).
pub fn extract(lang: Language, source: &str) -> Result<Extracted, ExtractError> {
    let grammar = lang
        .parse_lang()
        .grammar()
        .ok_or_else(|| ExtractError("no grammar".into()))?;
    let query = queries()
        .get(lang.name())
        .and_then(|q| q.as_ref().ok())
        .ok_or_else(|| ExtractError(format!("the {} query does not compile", lang.name())))?;
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|e| ExtractError(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ExtractError("the parser gave up".into()))?;
    let names = query.capture_names();
    let mut out = Extracted {
        had_errors: tree.root_node().has_error(),
        ..Extracted::default()
    };
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    let mut seen: Vec<(usize, usize)> = Vec::new();
    while let Some(m) = matches.next() {
        let mut def = None;
        let mut name = None;
        for capture in m.captures {
            match names[capture.index as usize] {
                "def" => def = Some(capture.node),
                "name" => name = Some(capture.node),
                "import" => out
                    .imports
                    .push(text(source, capture.node).trim().to_string()),
                "call" => {
                    *out.calls
                        .entry(text(source, capture.node).to_string())
                        .or_default() += 1;
                }
                "type_ref" => {
                    *out.type_refs
                        .entry(text(source, capture.node).to_string())
                        .or_default() += 1;
                }
                _ => {}
            }
        }
        let (Some(def), Some(name)) = (def, name) else {
            continue;
        };
        let key = (def.start_byte(), name.start_byte());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if lang == Language::Rust
            && def.kind() == "mod_item"
            && def.child_by_field_name("body").is_none()
        {
            out.imports.push(format!("mod {};", text(source, name)));
        }
        if def.kind() == "variable_declarator" && is_local_variable(def) {
            continue;
        }
        let found = container_name(def, source, lang);
        let container =
            special_container(def, source, lang).or_else(|| found.as_ref().map(|c| c.0.clone()));
        let container_kind = found
            .as_ref()
            .map(|c| c.1)
            .or_else(|| container.as_ref().map(|_| "class"));
        let simple = text(source, name).to_string();
        out.symbols.push(Symbol {
            name: container
                .as_ref()
                .map_or_else(|| simple.clone(), |c| format!("{c}::{simple}")),
            simple_name: simple,
            kind: kind_of(def, container_kind, lang),
            line: u32::try_from(def.start_position().row + 1).unwrap_or(u32::MAX),
            end_line: u32::try_from(def.end_position().row + 1).unwrap_or(u32::MAX),
            container,
            signature: signature_of(def, source, lang),
            doc: leading_doc(def, source, lang),
        });
    }
    out.symbols.sort_by_key(|s| (s.line, s.end_line));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(lang: Language, source: &str) -> Vec<(String, &'static str, u32)> {
        extract(lang, source)
            .expect("extracts")
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.line))
            .collect()
    }

    fn pairs(list: &[(&str, &'static str, u32)]) -> Vec<(String, &'static str, u32)> {
        list.iter()
            .map(|(n, k, l)| ((*n).to_string(), *k, *l))
            .collect()
    }

    /// T-CTX-006, one language at a time: golden names, kinds and lines.
    #[test]
    fn rust_symbols() {
        let src = "\
use std::fmt;

/// A parser for things.
pub struct Parser {
    pos: usize,
}

pub enum Token { A, B }

pub trait Visit {
    fn visit(&self);
}

impl Parser {
    /// Advance one token.
    pub fn parse(&mut self) -> Token {
        helper();
        Token::A
    }
}

mod inner {
    pub fn deep() {}
}

fn helper() {}
";
        assert_eq!(
            symbols(Language::Rust, src),
            pairs(&[
                ("Parser", "struct", 4),
                ("Token", "enum", 8),
                ("Visit", "trait", 10),
                ("Visit::visit", "method", 11),
                ("Parser", "impl", 14),
                ("Parser::parse", "method", 16),
                ("inner", "module", 22),
                ("inner::deep", "func", 23),
                ("helper", "func", 26),
            ])
        );
        let e = extract(Language::Rust, src).unwrap();
        assert_eq!(e.imports, ["use std::fmt;"]);
        assert_eq!(e.calls.get("helper"), Some(&1));
        let parse = e.symbols.iter().find(|s| s.simple_name == "parse").unwrap();
        assert_eq!(parse.signature, "pub fn parse(&mut self) -> Token");
        assert_eq!(parse.doc, "Advance one token.");
        assert_eq!(parse.container.as_deref(), Some("Parser"));
        let parser = e.symbols.iter().find(|s| s.kind == "struct").unwrap();
        assert_eq!(parser.doc, "A parser for things.");
        assert_eq!((parser.line, parser.end_line), (4, 6));
    }

    #[test]
    fn python_symbols() {
        let src = "\
import os
from a.b import c

class Parser:
    \"\"\"Parses.\"\"\"
    def parse(self, text):
        return helper(text)

def helper(x):
    return x
";
        assert_eq!(
            symbols(Language::Python, src),
            pairs(&[
                ("Parser", "class", 4),
                ("Parser::parse", "method", 6),
                ("helper", "func", 9),
            ])
        );
        let e = extract(Language::Python, src).unwrap();
        assert_eq!(e.imports, ["import os", "from a.b import c"]);
        assert_eq!(e.symbols[0].doc, "Parses.");
        assert_eq!(e.symbols[1].signature, "def parse(self, text):");
        assert!(e.calls.contains_key("helper"));
    }

    #[test]
    fn typescript_symbols() {
        let src = "\
import { x } from './x';

/** A parser. */
export class Parser {
  parse(text: string): number { return helper(text); }
}

export function helper(s: string) { return s.length; }

const make = (n: number) => n + 1;
const LIMIT = 10;

function outer() {
  const local = 1;
}
";
        assert_eq!(
            symbols(Language::TypeScript, src),
            pairs(&[
                ("Parser", "class", 4),
                ("Parser::parse", "method", 5),
                ("helper", "func", 8),
                ("make", "func", 10),
                ("LIMIT", "var", 11),
                ("outer", "func", 13),
            ])
        );
        let e = extract(Language::TypeScript, src).unwrap();
        assert_eq!(e.symbols[0].doc, "A parser.");
        assert_eq!(e.imports.len(), 1);
    }

    #[test]
    fn javascript_goes_through_the_tsx_grammar() {
        let src = "export function add(a, b) { return a + b; }\nclass K { m() {} }\n";
        assert_eq!(Language::detect("lib/a.js"), Some(Language::Tsx));
        assert_eq!(
            symbols(Language::Tsx, src),
            pairs(&[("add", "func", 1), ("K", "class", 2), ("K::m", "method", 2)])
        );
    }

    #[test]
    fn go_symbols() {
        let src = "\
package main

import \"fmt\"

type Parser struct{ pos int }

type Visitor interface{ Visit() }

func (p *Parser) Parse() int { return helper() }

func helper() int { return 1 }
";
        assert_eq!(
            symbols(Language::Go, src),
            pairs(&[
                ("Parser", "struct", 5),
                ("Visitor", "trait", 7),
                ("Parser::Parse", "method", 9),
                ("helper", "func", 11),
            ])
        );
        assert_eq!(extract(Language::Go, src).unwrap().imports, ["\"fmt\""]);
    }

    #[test]
    fn java_symbols() {
        let src = "\
package a;

import java.util.List;

public class Parser {
    private int pos;

    /** Parse it. */
    public int parse() { return pos; }
}

interface Visit { void visit(); }

enum Kind { A, B }
";
        assert_eq!(
            symbols(Language::Java, src),
            pairs(&[
                ("Parser", "class", 5),
                ("Parser::pos", "field", 6),
                ("Parser::parse", "method", 9),
                ("Visit", "trait", 12),
                ("Visit::visit", "method", 12),
                ("Kind", "enum", 14),
            ])
        );
        let e = extract(Language::Java, src).unwrap();
        assert_eq!(e.imports, ["import java.util.List;"]);
        assert_eq!(e.symbols[2].doc, "Parse it.");
    }

    #[test]
    fn cpp_symbols_cover_c_headers_too() {
        let src = "\
#include \"x.h\"

struct Point { int x; };

class Parser {
public:
  int parse() { return 1; }
};

int Parser::other() { return 2; }

int helper() { return 3; }
";
        assert_eq!(Language::detect("a/b.h"), Some(Language::Cpp));
        assert_eq!(Language::detect("a/b.c"), Some(Language::Cpp));
        assert_eq!(
            symbols(Language::Cpp, src),
            pairs(&[
                ("Point", "struct", 3),
                ("Parser", "class", 5),
                ("Parser::parse", "method", 7),
                ("Parser::other", "method", 10),
                ("helper", "func", 12),
            ])
        );
        assert_eq!(
            extract(Language::Cpp, src).unwrap().imports,
            ["#include \"x.h\""]
        );
    }

    #[test]
    fn a_broken_file_still_yields_what_could_be_read() {
        let e = extract(Language::Rust, "fn good() {}\nfn bad( {\nstruct Late;\n").unwrap();
        assert!(e.had_errors);
        assert!(e.symbols.iter().any(|s| s.simple_name == "good"));
    }

    #[test]
    fn every_language_has_a_compiling_query() {
        for lang in Language::ALL {
            assert!(extract(lang, "").is_ok(), "{lang:?}");
        }
    }

    #[test]
    fn extension_detection() {
        for (path, want) in [
            ("src/lib.rs", Some(Language::Rust)),
            ("a/b.py", Some(Language::Python)),
            ("x.ts", Some(Language::TypeScript)),
            ("x.tsx", Some(Language::Tsx)),
            ("x.mjs", Some(Language::Tsx)),
            ("main.go", Some(Language::Go)),
            ("A.java", Some(Language::Java)),
            ("x.hpp", Some(Language::Cpp)),
            ("README.md", None),
            ("Makefile", None),
        ] {
            assert_eq!(Language::detect(path), want, "{path}");
        }
    }
}
