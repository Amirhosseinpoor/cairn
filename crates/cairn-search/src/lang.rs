//! Language by extension and shebang (SPEC §6.3.6 step 1).

/// The language name tools report (`rust`, `python`, ...), or `None`.
#[must_use]
pub fn language_for(path: &str, first_line: Option<&str>) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let by_name = match name {
        "Makefile" | "makefile" | "GNUmakefile" => Some("make"),
        "Dockerfile" => Some("dockerfile"),
        "CMakeLists.txt" => Some("cmake"),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    if let Some((_, ext)) = name.rsplit_once('.') {
        let by_ext = match ext.to_ascii_lowercase().as_str() {
            "rs" => Some("rust"),
            "py" | "pyi" => Some("python"),
            "ts" | "tsx" | "mts" | "cts" => Some("typescript"),
            "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
            "go" => Some("go"),
            "java" => Some("java"),
            "c" | "h" => Some("c"),
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some("cpp"),
            "sh" | "bash" | "zsh" => Some("bash"),
            "json" => Some("json"),
            "toml" => Some("toml"),
            "yaml" | "yml" => Some("yaml"),
            "md" | "markdown" => Some("markdown"),
            "html" | "htm" => Some("html"),
            "css" => Some("css"),
            "sql" => Some("sql"),
            "rb" => Some("ruby"),
            "php" => Some("php"),
            "kt" | "kts" => Some("kotlin"),
            "swift" => Some("swift"),
            "cs" => Some("csharp"),
            _ => None,
        };
        if by_ext.is_some() {
            return by_ext;
        }
    }
    let shebang = first_line?.strip_prefix("#!")?;
    let program = shebang
        .split_whitespace()
        .filter_map(|word| word.rsplit('/').next())
        .find(|word| *word != "env")?;
    match program {
        "bash" | "sh" | "zsh" | "dash" => Some("bash"),
        "python" | "python3" | "python2" => Some("python"),
        "node" | "nodejs" | "deno" | "bun" => Some("javascript"),
        "ruby" => Some("ruby"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_name_languages() {
        for (path, want) in [
            ("src/lib.rs", "rust"),
            ("a/b/main.py", "python"),
            ("x.TSX", "typescript"),
            ("Makefile", "make"),
            ("deep/Dockerfile", "dockerfile"),
            ("c.toml", "toml"),
            ("notes.md", "markdown"),
        ] {
            assert_eq!(language_for(path, None), Some(want), "{path}");
        }
        assert_eq!(language_for("LICENSE", None), None);
        assert_eq!(language_for("a.unknownext", None), None);
    }

    #[test]
    fn a_shebang_names_an_extensionless_script() {
        assert_eq!(
            language_for("run", Some("#!/usr/bin/env python3")),
            Some("python")
        );
        assert_eq!(language_for("run", Some("#!/bin/bash -e")), Some("bash"));
        assert_eq!(
            language_for("run", Some("#!/usr/bin/env node")),
            Some("javascript")
        );
        assert_eq!(language_for("run", Some("# not a shebang")), None);
        // The extension wins over the shebang.
        assert_eq!(language_for("x.rs", Some("#!/bin/bash")), Some("rust"));
    }
}
