//! Turning an import statement into the files it names (SPEC §5.2, "Graph
//! construction"). Only files inside the workspace are found; an import of a
//! library resolves to nothing, and nothing is the right answer.

use std::collections::{BTreeMap, BTreeSet};

use crate::extract::Language;

/// Every indexed path, for probing.
#[derive(Debug, Default)]
pub struct Resolver {
    files: BTreeSet<String>,
    /// Directory → the files directly in it.
    dirs: BTreeMap<String, Vec<String>>,
    /// Rust crate name → its `src` directory.
    crates: BTreeMap<String, String>,
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn join(base: &str, rest: &str) -> String {
    if base.is_empty() {
        rest.to_string()
    } else if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{rest}")
    }
}

/// Whether `path` ends in `.ext`, in any case.
fn has_extension(path: &str, ext: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, e)| e.eq_ignore_ascii_case(ext))
}

/// `a/b/../c/./d` → `a/c/d`; `None` if it climbs out of the root.
fn normalize(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    Some(out.join("/"))
}

/// Extensions TypeScript and JavaScript imports may omit.
const SCRIPT_EXTS: [&str; 7] = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "d.ts"];

impl Resolver {
    #[must_use]
    pub fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let mut r = Self::default();
        for path in paths {
            r.dirs
                .entry(dir_of(&path).to_string())
                .or_default()
                .push(path.clone());
            if let Some(src) = path.strip_suffix("/src/lib.rs") {
                let name = src.rsplit('/').next().unwrap_or(src).replace('-', "_");
                r.crates.insert(name, join(src, "src"));
            } else if path == "src/lib.rs" {
                r.crates.insert(String::new(), "src".to_string());
            }
            r.files.insert(path);
        }
        r
    }

    fn has(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    /// The first of `candidates` that exists.
    fn first(&self, candidates: &[String]) -> Option<String> {
        candidates.iter().find(|c| self.has(c)).cloned()
    }

    /// The files `raw` (an import statement of `lang` in file `from`) names.
    #[must_use]
    pub fn resolve(&self, lang: Language, from: &str, raw: &str) -> Vec<String> {
        let found = match lang {
            Language::Rust => self.rust(from, raw),
            Language::Python => self.python(from, raw),
            Language::TypeScript | Language::Tsx => self.script(from, raw),
            Language::Go => self.go(raw),
            Language::Java => self.java(raw),
            Language::Cpp => self.include(from, raw),
        };
        let mut found: Vec<String> = found.into_iter().filter(|p| p != from).collect();
        found.sort();
        found.dedup();
        found
    }

    // ------------------------------------------------------------- python

    fn python(&self, from: &str, raw: &str) -> Vec<String> {
        let text = raw.replace(['(', ')', '\\'], " ");
        let text = text.trim();
        let mut modules: Vec<String> = Vec::new();
        if let Some(rest) = text.strip_prefix("import ") {
            for item in rest.split(',') {
                let name = item.split_whitespace().next().unwrap_or("");
                if !name.is_empty() {
                    modules.push(name.to_string());
                }
            }
        } else if let Some(rest) = text.strip_prefix("from ") {
            let (module, names) = rest.split_once(" import ").unwrap_or((rest, ""));
            let module = module.trim().to_string();
            modules.push(module.clone());
            for item in names.split(',') {
                let name = item.split_whitespace().next().unwrap_or("");
                if name.is_empty() || name == "*" {
                    continue;
                }
                // `from a import b` may name the submodule `a/b.py`.
                let sep = if module.ends_with('.') { "" } else { "." };
                modules.push(format!("{module}{sep}{name}"));
            }
        }
        let mut out = Vec::new();
        for module in modules {
            let dots = module.chars().take_while(|c| *c == '.').count();
            let rest = &module[dots..];
            let parts: Vec<&str> = rest.split('.').filter(|p| !p.is_empty()).collect();
            let rel = parts.join("/");
            let roots: Vec<String> = if dots > 0 {
                let mut base = dir_of(from).to_string();
                for _ in 1..dots {
                    base = dir_of(&base).to_string();
                }
                vec![base]
            } else {
                // Absolute: the workspace root, `src/`, and every directory
                // the importing file sits in.
                let mut roots = vec![String::new(), "src".to_string()];
                let mut at = dir_of(from).to_string();
                loop {
                    roots.push(at.clone());
                    if at.is_empty() {
                        break;
                    }
                    at = dir_of(&at).to_string();
                }
                roots
            };
            for root in roots {
                let base = join(&root, &rel);
                let candidates = if rel.is_empty() {
                    vec![join(&base, "__init__.py")]
                } else {
                    vec![format!("{base}.py"), join(&base, "__init__.py")]
                };
                if let Some(hit) = self.first(&candidates) {
                    out.push(hit);
                    break;
                }
            }
        }
        out
    }

    // ----------------------------------------------------- typescript / js

    fn script(&self, from: &str, raw: &str) -> Vec<String> {
        let Some(spec) = quoted(raw) else {
            return Vec::new();
        };
        if !spec.starts_with('.') {
            return Vec::new();
        }
        let Some(base) = normalize(&join(dir_of(from), &spec)) else {
            return Vec::new();
        };
        let mut candidates = vec![base.clone()];
        // `./x.js` in TypeScript means `./x.ts`.
        let stem = base
            .strip_suffix(".js")
            .or_else(|| base.strip_suffix(".jsx"))
            .or_else(|| base.strip_suffix(".mjs"));
        if let Some(stem) = stem {
            for ext in ["ts", "tsx"] {
                candidates.push(format!("{stem}.{ext}"));
            }
        }
        for ext in SCRIPT_EXTS {
            candidates.push(format!("{base}.{ext}"));
        }
        for ext in SCRIPT_EXTS {
            candidates.push(join(&base, &format!("index.{ext}")));
        }
        self.first(&candidates).into_iter().collect()
    }

    // --------------------------------------------------------------- rust

    /// The directory a Rust file's child modules live in.
    fn module_dir(path: &str) -> String {
        let name = path.rsplit('/').next().unwrap_or(path);
        let stem = name.strip_suffix(".rs").unwrap_or(name);
        if matches!(stem, "mod" | "lib" | "main") {
            dir_of(path).to_string()
        } else {
            join(dir_of(path), stem)
        }
    }

    /// `…/src` for a file inside one.
    fn crate_src(path: &str) -> Option<String> {
        let idx = path.rfind("src/").map(|i| i + 3)?;
        let prefix = &path[..idx];
        (prefix == "src" || prefix.ends_with("/src")).then(|| prefix.to_string())
    }

    fn rust_probe(&self, base: &str, segments: &[&str]) -> Option<String> {
        for k in (1..=segments.len()).rev() {
            let rel = segments[..k].join("/");
            let target = join(base, &rel);
            if let Some(hit) = self.first(&[format!("{target}.rs"), join(&target, "mod.rs")]) {
                return Some(hit);
            }
        }
        None
    }

    fn rust(&self, from: &str, raw: &str) -> Vec<String> {
        let text = raw.trim().trim_end_matches(';').trim();
        if let Some(name) = text.strip_prefix("mod ") {
            let name = name.trim();
            let dir = Self::module_dir(from);
            return self
                .first(&[
                    join(&dir, &format!("{name}.rs")),
                    join(&join(&dir, name), "mod.rs"),
                ])
                .into_iter()
                .collect();
        }
        let text = text.strip_prefix("pub ").unwrap_or(text);
        let text = text
            .strip_prefix("pub(crate) ")
            .unwrap_or(text)
            .strip_prefix("use ")
            .unwrap_or(text)
            .trim();
        // The path before any `{`, `*` or `as`.
        let head = text
            .split(['{', '*'])
            .next()
            .unwrap_or("")
            .split(" as ")
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches("::");
        let segments: Vec<&str> = head.split("::").filter(|s| !s.is_empty()).collect();
        let Some((first, rest)) = segments.split_first() else {
            return Vec::new();
        };
        let hit = match *first {
            "crate" => Self::crate_src(from).and_then(|src| {
                self.rust_probe(&src, rest)
                    .or_else(|| self.first(&[join(&src, "lib.rs"), join(&src, "main.rs")]))
            }),
            "self" => self.rust_probe(&Self::module_dir(from), rest),
            "super" => {
                let mut up = 1;
                while rest.get(up - 1) == Some(&"super") {
                    up += 1;
                }
                let mut dir = Self::module_dir(from);
                for _ in 0..up {
                    dir = dir_of(&dir).to_string();
                }
                self.rust_probe(&dir, &rest[up - 1..]).or_else(|| {
                    // `super::Item`: the item lives in the parent module's
                    // own file.
                    self.first(&[
                        join(&dir, "mod.rs"),
                        format!("{dir}.rs"),
                        join(&dir, "lib.rs"),
                        join(&dir, "main.rs"),
                    ])
                })
            }
            name => {
                let local = self.crates.get(name);
                local
                    .and_then(|src| self.rust_probe(src, rest))
                    .or_else(|| local.and_then(|src| self.first(&[join(src, "lib.rs")])))
                    .or_else(|| {
                        // A sibling module named by the first segment.
                        Self::crate_src(from).and_then(|src| self.rust_probe(&src, &segments))
                    })
            }
        };
        hit.into_iter().collect()
    }

    // ----------------------------------------------------------------- go

    fn go(&self, raw: &str) -> Vec<String> {
        let Some(path) = quoted(raw) else {
            return Vec::new();
        };
        let mut best: Option<(&String, usize)> = None;
        for dir in self.dirs.keys() {
            let hit = dir == &path || dir.ends_with(&format!("/{path}"));
            if hit && best.is_none_or(|(_, len)| dir.len() > len) {
                best = Some((dir, dir.len()));
            }
        }
        best.map(|(dir, _)| {
            self.dirs[dir]
                .iter()
                .filter(|f| has_extension(f, "go") && !f.ends_with("_test.go"))
                .take(8)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
    }

    // --------------------------------------------------------------- java

    fn java(&self, raw: &str) -> Vec<String> {
        let text = raw
            .trim()
            .trim_end_matches(';')
            .trim_start_matches("import")
            .trim()
            .trim_start_matches("static")
            .trim();
        if let Some(pkg) = text.strip_suffix(".*") {
            let dir = pkg.replace('.', "/");
            return self
                .dirs
                .iter()
                .filter(|(d, _)| *d == &dir || d.ends_with(&format!("/{dir}")))
                .flat_map(|(_, files)| files.iter())
                .filter(|f| has_extension(f, "java"))
                .take(8)
                .cloned()
                .collect();
        }
        // `a.b.C` or `a.b.C.member`: try the longest prefix that is a class.
        let parts: Vec<&str> = text.split('.').collect();
        for k in (1..=parts.len()).rev() {
            let rel = format!("{}.java", parts[..k].join("/"));
            let hit = self
                .files
                .iter()
                .find(|f| *f == &rel || f.ends_with(&format!("/{rel}")));
            if let Some(hit) = hit {
                return vec![hit.clone()];
            }
        }
        Vec::new()
    }

    // -------------------------------------------------------------- C, C++

    fn include(&self, from: &str, raw: &str) -> Vec<String> {
        let text = raw
            .trim()
            .trim_start_matches('#')
            .trim_start()
            .strip_prefix("include")
            .unwrap_or("")
            .trim();
        let Some(spec) = text
            .strip_prefix('"')
            .and_then(|t| t.split('"').next())
            .or_else(|| text.strip_prefix('<').and_then(|t| t.split('>').next()))
        else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        if let Some(rel) = normalize(&join(dir_of(from), spec)) {
            candidates.push(rel);
        }
        for root in ["", "include", "src"] {
            if let Some(rel) = normalize(&join(root, spec)) {
                candidates.push(rel);
            }
        }
        self.first(&candidates).into_iter().collect()
    }
}

/// The first quoted string in `raw`.
fn quoted(raw: &str) -> Option<String> {
    let start = raw.find(['"', '\'', '`'])?;
    let quote = raw[start..].chars().next()?;
    let rest = &raw[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(paths: &[&str]) -> Resolver {
        Resolver::new(paths.iter().map(|p| (*p).to_string()))
    }

    #[test]
    fn python_absolute_relative_and_submodule_imports() {
        let r = resolver(&[
            "app/__init__.py",
            "app/models.py",
            "app/util/__init__.py",
            "app/util/text.py",
            "app/main.py",
        ]);
        let f = |raw: &str| r.resolve(Language::Python, "app/main.py", raw);
        assert_eq!(f("import app.models"), ["app/models.py"]);
        assert_eq!(
            f("from app.util import text"),
            ["app/util/__init__.py", "app/util/text.py"]
        );
        assert_eq!(
            f("from . import models"),
            ["app/__init__.py", "app/models.py"]
        );
        assert_eq!(f("from .util.text import slug"), ["app/util/text.py"]);
        assert_eq!(f("import os, sys"), Vec::<String>::new());
        assert_eq!(
            f("from app import (models,\n    util)"),
            ["app/__init__.py", "app/models.py", "app/util/__init__.py"]
        );
    }

    #[test]
    fn typescript_relative_specifiers_with_extension_and_index_probing() {
        let r = resolver(&[
            "src/a.ts",
            "src/lib/index.ts",
            "src/lib/x.tsx",
            "src/b.js",
            "src/c.ts",
        ]);
        let f = |raw: &str| r.resolve(Language::TypeScript, "src/a.ts", raw);
        assert_eq!(f("import { x } from './lib'"), ["src/lib/index.ts"]);
        assert_eq!(f("import X from \"./lib/x\""), ["src/lib/x.tsx"]);
        assert_eq!(f("import b from './b'"), ["src/b.js"]);
        assert_eq!(
            f("import c from './c.js'"),
            ["src/c.ts"],
            "TypeScript's .js spelling"
        );
        assert_eq!(f("import React from 'react'"), Vec::<String>::new());
        assert_eq!(f("import z from '../../outside'"), Vec::<String>::new());
    }

    #[test]
    fn rust_use_mod_crate_self_super_and_workspace_crates() {
        let r = resolver(&[
            "crates/core/src/lib.rs",
            "crates/core/src/error.rs",
            "crates/core/src/model/mod.rs",
            "crates/core/src/model/user.rs",
            "crates/app/src/main.rs",
            "crates/app/src/cli.rs",
            "crates/app/src/cmd/run.rs",
            "crates/app/src/cmd/mod.rs",
        ]);
        let from = "crates/app/src/cmd/run.rs";
        let f = |raw: &str| r.resolve(Language::Rust, from, raw);
        assert_eq!(f("use crate::cli::Args;"), ["crates/app/src/cli.rs"]);
        assert_eq!(f("use super::Thing;"), ["crates/app/src/cmd/mod.rs"]);
        assert_eq!(f("use crate::cli::{A, B};"), ["crates/app/src/cli.rs"]);
        assert_eq!(
            f("use core::model::user::User;"),
            ["crates/core/src/model/user.rs"]
        );
        assert_eq!(f("use core::error::E;"), ["crates/core/src/error.rs"]);
        assert_eq!(f("use std::collections::HashMap;"), Vec::<String>::new());
        // `mod x;` finds the child file.
        assert_eq!(
            r.resolve(Language::Rust, "crates/app/src/main.rs", "mod cli;"),
            ["crates/app/src/cli.rs"]
        );
        assert_eq!(
            r.resolve(Language::Rust, "crates/app/src/main.rs", "mod cmd;"),
            ["crates/app/src/cmd/mod.rs"]
        );
        assert_eq!(
            r.resolve(Language::Rust, "crates/core/src/model/mod.rs", "mod user;"),
            ["crates/core/src/model/user.rs"]
        );
        assert_eq!(f("use self::nothing::X;"), Vec::<String>::new());
    }

    #[test]
    fn go_imports_match_a_directory_by_suffix() {
        let r = resolver(&[
            "pkg/util/a.go",
            "pkg/util/a_test.go",
            "pkg/util/b.go",
            "cmd/main.go",
            "pkg/other/o.go",
        ]);
        let f = |raw: &str| r.resolve(Language::Go, "cmd/main.go", raw);
        assert_eq!(
            f("\"example.com/app/pkg/util\""),
            Vec::<String>::new(),
            "module prefix is not stripped"
        );
        assert_eq!(f("\"pkg/util\""), ["pkg/util/a.go", "pkg/util/b.go"]);
        assert_eq!(f("u \"pkg/util\""), ["pkg/util/a.go", "pkg/util/b.go"]);
        assert_eq!(f("\"fmt\""), Vec::<String>::new());
    }

    #[test]
    fn java_imports_find_classes_and_packages() {
        let r = resolver(&[
            "src/main/java/com/a/Parser.java",
            "src/main/java/com/a/Util.java",
            "src/main/java/com/b/App.java",
        ]);
        let f = |raw: &str| r.resolve(Language::Java, "src/main/java/com/b/App.java", raw);
        assert_eq!(
            f("import com.a.Parser;"),
            ["src/main/java/com/a/Parser.java"]
        );
        assert_eq!(
            f("import static com.a.Util.helper;"),
            ["src/main/java/com/a/Util.java"]
        );
        assert_eq!(
            f("import com.a.*;"),
            [
                "src/main/java/com/a/Parser.java",
                "src/main/java/com/a/Util.java"
            ]
        );
        assert_eq!(f("import java.util.List;"), Vec::<String>::new());
    }

    #[test]
    fn c_includes_resolve_relative_then_to_known_roots() {
        let r = resolver(&["src/a.c", "src/a.h", "include/api.h", "src/sub/b.h"]);
        let f = |raw: &str| r.resolve(Language::Cpp, "src/a.c", raw);
        assert_eq!(f("#include \"a.h\""), ["src/a.h"]);
        assert_eq!(f("#include \"sub/b.h\""), ["src/sub/b.h"]);
        assert_eq!(f("#include <api.h>"), ["include/api.h"]);
        assert_eq!(f("#include <stdio.h>"), Vec::<String>::new());
    }

    #[test]
    fn a_file_never_imports_itself() {
        let r = resolver(&["a/__init__.py"]);
        assert!(r
            .resolve(Language::Python, "a/__init__.py", "import a")
            .is_empty());
    }
}
