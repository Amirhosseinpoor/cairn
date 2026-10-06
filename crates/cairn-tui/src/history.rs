//! Prompt history (SPEC §10.2): persisted, de-duplicated, searched by prefix
//! with `Up`/`Down` and by fuzzy match with `Ctrl+R`.

use std::io::Write;
use std::path::PathBuf;

use crate::fuzzy;

/// §10.2: how many entries are kept.
pub const MAX_ENTRIES: usize = 5_000;

/// Past prompts and the position of a walk through them.
#[derive(Debug, Clone, Default)]
pub struct History {
    entries: Vec<String>,
    path: Option<PathBuf>,
    /// Index of the entry shown, while walking.
    at: Option<usize>,
    /// What was being typed when the walk began, and what it must start with.
    draft: String,
}

impl History {
    /// An in-memory history (tests, or when there is nowhere to write).
    #[must_use]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load `path`, tolerating lines that are not JSON strings. New entries
    /// are appended to it.
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let mut entries: Vec<String> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(entry) = serde_json::from_str::<String>(line) {
                    if entries.last() != Some(&entry) {
                        entries.push(entry);
                    }
                }
            }
        }
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
        }
        Self {
            entries,
            path: Some(path),
            at: None,
            draft: String::new(),
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record a submitted prompt. Blank prompts and a repeat of the last one
    /// are not kept.
    pub fn push(&mut self, text: &str) {
        self.at = None;
        if text.trim().is_empty() || self.entries.last().is_some_and(|l| l == text) {
            return;
        }
        self.entries.push(text.to_string());
        let over = self.entries.len() > MAX_ENTRIES;
        if over {
            self.entries.remove(0);
        }
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if over {
            // Rewrite so the file stays bounded.
            let body: String = self
                .entries
                .iter()
                .filter_map(|e| serde_json::to_string(e).ok())
                .map(|l| l + "\n")
                .collect();
            let _ = std::fs::write(path, body);
        } else if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            if let Ok(line) = serde_json::to_string(text) {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    /// Stop walking (the prompt was edited or submitted).
    pub fn reset(&mut self) {
        self.at = None;
    }

    /// `Up`: the previous entry that starts with what was typed when the walk
    /// began. The first call remembers `current` as that prefix.
    pub fn previous(&mut self, current: &str) -> Option<&str> {
        let start = if let Some(i) = self.at {
            i
        } else {
            self.draft = current.to_string();
            self.entries.len()
        };
        let found = (0..start)
            .rev()
            .find(|i| self.entries[*i].starts_with(&self.draft) && self.entries[*i] != current);
        match found {
            Some(i) => {
                self.at = Some(i);
                Some(&self.entries[i])
            }
            None => self.at.map(|i| self.entries[i].as_str()),
        }
    }

    /// `Down`: the next matching entry, or the original draft at the end.
    pub fn following(&mut self) -> Option<String> {
        let at = self.at?;
        let found =
            (at + 1..self.entries.len()).find(|i| self.entries[*i].starts_with(&self.draft));
        if let Some(i) = found {
            self.at = Some(i);
            Some(self.entries[i].clone())
        } else {
            self.at = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    /// `Ctrl+R`: entries matching `query`, best first, newest winning ties.
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<&str> {
        fuzzy::rank(query, self.entries.iter().rev().map(String::as_str), limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(items: &[&str]) -> History {
        let mut h = History::in_memory();
        for i in items {
            h.push(i);
        }
        h
    }

    /// T-TUI-026.
    #[test]
    fn up_with_a_prefix_walks_only_matching_entries() {
        let mut h = history(&["git status", "cargo test", "git diff", "ls", "git log"]);
        assert_eq!(h.previous("git "), Some("git log"));
        assert_eq!(h.previous("git "), Some("git diff"));
        assert_eq!(h.previous("git "), Some("git status"));
        assert_eq!(
            h.previous("git "),
            Some("git status"),
            "the oldest stays put"
        );
        assert_eq!(h.following().as_deref(), Some("git diff"));
        assert_eq!(h.following().as_deref(), Some("git log"));
        assert_eq!(h.following().as_deref(), Some("git "), "back to the draft");
        assert_eq!(h.following(), None);
    }

    #[test]
    fn an_empty_prefix_walks_everything_newest_first() {
        let mut h = history(&["a", "b", "c"]);
        assert_eq!(h.previous(""), Some("c"));
        assert_eq!(h.previous(""), Some("b"));
        assert_eq!(h.previous(""), Some("a"));
    }

    #[test]
    fn blank_and_repeated_prompts_are_not_kept() {
        let h = history(&["one", "one", "  ", "", "two", "one"]);
        assert_eq!(h.entries(), ["one", "two", "one"]);
    }

    #[test]
    fn history_persists_dedups_and_survives_garbage_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history/ws.jsonl");
        {
            let mut h = History::load(path.clone());
            h.push("first");
            h.push("second\nwith newline");
            h.push("second\nwith newline");
        }
        std::fs::write(
            &path,
            format!(
                "{}this is not json\n{}\"first\"\n",
                std::fs::read_to_string(&path).unwrap(),
                ""
            ),
        )
        .unwrap();
        let h = History::load(path);
        assert_eq!(h.entries(), ["first", "second\nwith newline", "first"]);
    }

    #[test]
    fn the_history_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.jsonl");
        let mut h = History::load(path.clone());
        for i in 0..MAX_ENTRIES + 20 {
            h.push(&format!("prompt {i}"));
        }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        assert_eq!(h.entries()[0], "prompt 20");
        assert_eq!(History::load(path).entries().len(), MAX_ENTRIES);
    }

    /// T-TUI-027's data half.
    #[test]
    fn fuzzy_search_finds_old_prompts_newest_first_on_ties() {
        let h = history(&[
            "fix the parser",
            "add tests for lexer",
            "fix the printer",
            "unrelated",
        ]);
        assert_eq!(h.search("fix", 5), ["fix the printer", "fix the parser"]);
        assert_eq!(h.search("tlx", 5), ["add tests for lexer"]);
        assert!(h.search("zzzz", 5).is_empty());
        assert_eq!(h.search("", 2).len(), 2);
    }
}
