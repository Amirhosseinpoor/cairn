//! `grep` (SPEC §6.2.7): the ripgrep engine over the walker. Binary files are
//! skipped and counted (REQ-CTX-004); results are deterministic — files in
//! path order, matches in line order.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use globset::GlobBuilder;
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};

use crate::binary::{classify, Content, SNIFF_BYTES};
use crate::ignore_rules::IgnoreEngine;
use crate::walk::{walk, Kind, WalkError, WalkOptions};

/// A compiled-regex ceiling that rejects pathological patterns without
/// refusing ordinary Unicode classes.
const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// A reported line is cut here (§5.5).
const MAX_TEXT_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct GrepOptions {
    pub pattern: String,
    /// `*.rs`: matched against the file name when it has no `/`, otherwise
    /// against the path relative to the search root.
    pub glob: Option<String>,
    pub case_insensitive: bool,
    pub multiline: bool,
    pub context_lines: usize,
    pub max_results: usize,
    pub max_file_size_kb: u64,
    pub respect_ignore: bool,
    pub include_binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    /// 1-based, in characters.
    pub column: usize,
    pub text: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub match_count: usize,
    pub files_searched: usize,
    pub binary_skipped: usize,
    /// Files over `max_file_size_kb`.
    pub too_large_skipped: usize,
    pub truncated: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrepError {
    /// `E-REGEX-SYNTAX`.
    Syntax(String),
    /// `E-REGEX-TOOBIG`.
    TooBig(String),
    /// `E-GLOB-SYNTAX` for the `glob` filter.
    GlobSyntax(String),
    /// `E-FS-NOTFOUND`.
    NotFound,
    /// `E-GREP-WALK`.
    Walk(String),
    /// `E-GREP-CAP`: the scan hit its entry cap before a single match.
    Cap,
}

fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        text.to_string()
    } else {
        let cut: String = text.chars().take(MAX_TEXT_CHARS).collect();
        format!("{cut}…[line truncated]")
    }
}

fn line_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    cap(text.trim_end_matches(['\n', '\r']))
}

fn build_matcher(options: &GrepOptions) -> Result<RegexMatcher, GrepError> {
    RegexMatcherBuilder::new()
        .case_insensitive(options.case_insensitive)
        .multi_line(true)
        .dot_matches_new_line(options.multiline)
        .size_limit(REGEX_SIZE_LIMIT)
        .build(&options.pattern)
        .map_err(|e| {
            let message = e.to_string();
            if message.contains("size limit") {
                GrepError::TooBig(message)
            } else {
                GrepError::Syntax(message)
            }
        })
}

struct Collector<'a> {
    matcher: &'a RegexMatcher,
    rel: &'a str,
    out: &'a mut Vec<GrepMatch>,
    max: usize,
    before: Vec<String>,
    truncated: &'a mut bool,
}

impl Sink for Collector<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, found: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.out.len() >= self.max {
            *self.truncated = true;
            return Ok(false);
        }
        let bytes = found.bytes();
        let column = self.matcher.find(bytes).ok().flatten().map_or(1, |m| {
            let start = m.start();
            String::from_utf8_lossy(&bytes[..start]).chars().count()
                - String::from_utf8_lossy(&bytes[..start])
                    .rfind('\n')
                    .map_or(0, |nl| {
                        String::from_utf8_lossy(&bytes[..start])[..=nl]
                            .chars()
                            .count()
                    })
                + 1
        });
        self.out.push(GrepMatch {
            path: self.rel.to_string(),
            line: found.line_number().unwrap_or(0),
            column,
            text: line_text(bytes),
            before: std::mem::take(&mut self.before),
            after: Vec::new(),
        });
        Ok(true)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        let text = line_text(ctx.bytes());
        match ctx.kind() {
            SinkContextKind::Before => self.before.push(text),
            SinkContextKind::After => {
                if let Some(last) = self.out.last_mut() {
                    last.after.push(text);
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }

    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.before.clear();
        Ok(true)
    }
}

fn sniff(path: &Path, size: u64) -> Option<Content> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; SNIFF_BYTES];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    Some(classify(&buffer, size))
}

/// Search below `start`.
///
/// # Errors
/// [`GrepError`].
pub fn grep(
    engine: &IgnoreEngine,
    start: &Path,
    options: &GrepOptions,
) -> Result<GrepResult, GrepError> {
    let began = Instant::now();
    let matcher = build_matcher(options)?;
    let name_filter = options
        .glob
        .as_deref()
        .map(|g| {
            GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map(|glob| (glob.compile_matcher(), g.contains('/')))
                .map_err(|e| GrepError::GlobSyntax(e.to_string()))
        })
        .transpose()?;
    let walked = walk(
        engine,
        start,
        &WalkOptions {
            respect_ignore: options.respect_ignore,
            ..WalkOptions::default()
        },
    )
    .map_err(|e| match e {
        WalkError::NotFound => GrepError::NotFound,
        WalkError::Unreadable(why) => GrepError::Walk(why),
    })?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(options.context_lines)
        .after_context(options.context_lines)
        .multi_line(options.multiline)
        .binary_detection(BinaryDetection::quit(0))
        .build();

    let mut result = GrepResult::default();
    for entry in walked.entries.iter().filter(|e| e.kind == Kind::File) {
        if let Some((glob, by_path)) = &name_filter {
            let candidate = if *by_path {
                entry.path.strip_prefix(start).map_or_else(
                    |_| entry.rel.clone(),
                    |p| p.to_string_lossy().replace('\\', "/"),
                )
            } else {
                entry
                    .path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            };
            if !glob.is_match(&candidate) {
                continue;
            }
        }
        if entry.size > options.max_file_size_kb * 1024 {
            result.too_large_skipped += 1;
            continue;
        }
        if !options.include_binary {
            match sniff(&entry.path, entry.size) {
                Some(Content::Text { .. }) => {}
                Some(_) => {
                    result.binary_skipped += 1;
                    continue;
                }
                None => continue,
            }
        }
        result.files_searched += 1;
        let mut truncated = false;
        let mut sink = Collector {
            matcher: &matcher,
            rel: &entry.rel,
            out: &mut result.matches,
            max: options.max_results,
            before: Vec::new(),
            truncated: &mut truncated,
        };
        // A file that vanishes or cannot be read mid-search is skipped, not
        // fatal: the tree changes under a running agent.
        let _ = searcher.search_path(&matcher, &entry.path, &mut sink);
        if truncated {
            result.truncated = true;
            break;
        }
    }
    result.match_count = result.matches.len();
    if walked.capped {
        if result.matches.is_empty() {
            return Err(GrepError::Cap);
        }
        result.truncated = true;
    }
    result.duration_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore_rules::IgnoreOptions;
    use std::path::PathBuf;

    fn setup(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf, IgnoreEngine) {
        let dir = tempfile::tempdir().expect("tmp");
        let root = dir.path().canonicalize().expect("canonical").join("ws");
        std::fs::create_dir_all(&root).expect("root");
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
            std::fs::write(full, content).expect("file");
        }
        let engine = IgnoreEngine::new(&root, &IgnoreOptions::default());
        (dir, root, engine)
    }

    fn opts(pattern: &str) -> GrepOptions {
        GrepOptions {
            pattern: pattern.to_string(),
            glob: None,
            case_insensitive: false,
            multiline: false,
            context_lines: 0,
            max_results: 200,
            max_file_size_kb: 1024,
            respect_ignore: true,
            include_binary: false,
        }
    }

    #[test]
    fn finds_lines_with_path_line_and_column() {
        let (_d, root, engine) = setup(&[
            ("src/a.rs", b"fn main() {}\nfn parse() {}\n"),
            ("src/b.rs", b"nothing here\n"),
        ]);
        let got = grep(&engine, &root, &opts("fn parse")).expect("runs");
        assert_eq!(got.match_count, 1);
        let m = &got.matches[0];
        assert_eq!((m.path.as_str(), m.line, m.column), ("src/a.rs", 2, 1));
        assert_eq!(m.text, "fn parse() {}");
        assert_eq!(got.files_searched, 2);
        assert!(!got.truncated);
    }

    #[test]
    fn columns_count_characters_not_bytes() {
        let (_d, root, engine) = setup(&[("a.txt", "héllo wörld\n".as_bytes())]);
        let got = grep(&engine, &root, &opts("wörld")).expect("runs");
        assert_eq!(
            got.matches[0].column, 7,
            "h é l l o ␠ = 6 characters before it"
        );
    }

    #[test]
    fn case_insensitive_and_regex_features() {
        let (_d, root, engine) = setup(&[("a.txt", b"Hello\nHELLO\nhello\nworld42\n")]);
        assert_eq!(
            grep(&engine, &root, &opts("hello"))
                .expect("runs")
                .match_count,
            1
        );
        let mut ci = opts("hello");
        ci.case_insensitive = true;
        assert_eq!(grep(&engine, &root, &ci).expect("runs").match_count, 3);
        assert_eq!(
            grep(&engine, &root, &opts(r"world\d+"))
                .expect("runs")
                .match_count,
            1
        );
        assert_eq!(
            grep(&engine, &root, &opts("^hello$"))
                .expect("runs")
                .match_count,
            1
        );
    }

    #[test]
    fn context_lines_are_attached_to_each_match() {
        let (_d, root, engine) = setup(&[("a.txt", b"l1\nl2\nTARGET\nl4\nl5\nl6\n")]);
        let mut o = opts("TARGET");
        o.context_lines = 2;
        let m = &grep(&engine, &root, &o).expect("runs").matches[0];
        assert_eq!(m.before, ["l1", "l2"]);
        assert_eq!(m.after, ["l4", "l5"]);
        // At the start of a file there is less before.
        let (_d2, root2, engine2) = setup(&[("a.txt", b"TARGET\nl2\n")]);
        let m = &grep(&engine2, &root2, &o).expect("runs").matches[0];
        assert!(m.before.is_empty());
        assert_eq!(m.after, ["l2"]);
    }

    #[test]
    fn multiline_patterns_span_lines_only_when_asked() {
        let (_d, root, engine) = setup(&[("a.txt", b"struct A {\n    x: u32,\n}\n")]);
        assert_eq!(
            grep(&engine, &root, &opts(r"struct A \{\n    x"))
                .expect("runs")
                .match_count,
            0
        );
        let mut m = opts(r"struct A \{\n    x");
        m.multiline = true;
        let got = grep(&engine, &root, &m).expect("runs");
        assert_eq!(got.match_count, 1);
        assert_eq!(got.matches[0].line, 1);
    }

    /// REQ-CTX-004: binary files are skipped, and counted.
    #[test]
    fn binary_files_are_skipped_and_counted() {
        let (_d, root, engine) = setup(&[
            ("text.txt", b"needle\n"),
            ("blob.bin", b"\0\0needle\0\0"),
            ("utf16.txt", &[0xFF, 0xFE, b'n', 0, b'e', 0]),
        ]);
        let got = grep(&engine, &root, &opts("needle")).expect("runs");
        assert_eq!(got.match_count, 1);
        assert_eq!(got.binary_skipped, 2);
        let mut all = opts("needle");
        all.include_binary = true;
        assert!(grep(&engine, &root, &all).expect("runs").match_count >= 1);
    }

    #[test]
    fn the_glob_filter_matches_names_or_paths() {
        let (_d, root, engine) = setup(&[
            ("a.rs", b"x\n"),
            ("b.py", b"x\n"),
            ("src/c.rs", b"x\n"),
            ("lib/d.rs", b"x\n"),
        ]);
        let mut o = opts("x");
        o.glob = Some("*.rs".into());
        assert_eq!(
            grep(&engine, &root, &o).expect("runs").match_count,
            3,
            "a bare glob matches names at any depth"
        );
        o.glob = Some("src/*.rs".into());
        assert_eq!(grep(&engine, &root, &o).expect("runs").match_count, 1);
    }

    #[test]
    fn ignored_files_are_not_searched_unless_asked() {
        let (_d, root, engine) = setup(&[
            ("a.rs", b"x\n"),
            ("target/b.rs", b"x\n"),
            (".gitignore", b"*.gen\n"),
            ("c.gen", b"x\n"),
        ]);
        assert_eq!(
            grep(&engine, &root, &opts("x")).expect("runs").match_count,
            1
        );
        let mut o = opts("x");
        o.respect_ignore = false;
        assert_eq!(grep(&engine, &root, &o).expect("runs").match_count, 3);
    }

    #[test]
    fn max_results_and_file_size_limits() {
        let (_d, root, engine) = setup(&[
            ("a.txt", b"hit\nhit\nhit\nhit\n"),
            ("big.txt", &vec![b'x'; 3 * 1024]),
        ]);
        let mut o = opts("hit");
        o.max_results = 2;
        let got = grep(&engine, &root, &o).expect("runs");
        assert_eq!(got.matches.len(), 2);
        assert!(got.truncated);
        let mut small = opts("x");
        small.max_file_size_kb = 1;
        assert_eq!(
            grep(&engine, &root, &small)
                .expect("runs")
                .too_large_skipped,
            1
        );
    }

    #[test]
    fn errors_are_typed() {
        let (_d, root, engine) = setup(&[("a.txt", b"x\n")]);
        assert!(matches!(
            grep(&engine, &root, &opts("(unclosed")),
            Err(GrepError::Syntax(_))
        ));
        assert!(matches!(
            grep(&engine, &root, &opts("[")),
            Err(GrepError::Syntax(_))
        ));
        let mut bad_glob = opts("x");
        bad_glob.glob = Some("[".into());
        assert!(matches!(
            grep(&engine, &root, &bad_glob),
            Err(GrepError::GlobSyntax(_))
        ));
        assert_eq!(
            grep(&engine, &root.join("missing"), &opts("x")),
            Err(GrepError::NotFound)
        );
        // A pattern whose compiled form is enormous.
        let huge = format!("(?:{}){{100}}", "[a-z]{1,1000}");
        assert!(
            matches!(
                grep(&engine, &root, &opts(&huge)),
                Err(GrepError::TooBig(_))
            ),
            "size limit"
        );
    }

    #[test]
    fn long_lines_are_cut() {
        let (_d, root, engine) =
            setup(&[("a.txt", format!("{}needle\n", "x".repeat(3000)).as_bytes())]);
        let got = grep(&engine, &root, &opts("needle")).expect("runs");
        assert!(got.matches[0].text.ends_with("…[line truncated]"));
    }

    #[test]
    fn output_is_deterministic() {
        let (_d, root, engine) =
            setup(&[("b.txt", b"x\nx\n"), ("a.txt", b"x\n"), ("c/d.txt", b"x\n")]);
        let first = grep(&engine, &root, &opts("x")).expect("runs");
        for _ in 0..20 {
            let again = grep(&engine, &root, &opts("x")).expect("runs");
            assert_eq!(again.matches, first.matches);
        }
        let paths: Vec<&str> = first.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "b.txt", "b.txt", "c/d.txt"]);
    }
}
