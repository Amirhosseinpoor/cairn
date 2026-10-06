//! Overlays: what sits on top of the transcript (SPEC §10.1) — the approval
//! prompt, the diff viewer, the plan card, help, history search and the
//! yes/no questions — with the state each one needs.

use serde_json::Value;

/// What a person can answer an approval with (§10.4: `a`/`A`/`d`/`e`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAnswer {
    Once,
    Always,
    Deny,
    Edit,
}

impl ApprovalAnswer {
    /// The four buttons, in the order focus cycles through them.
    pub const BUTTONS: [Self; 4] = [Self::Once, Self::Always, Self::Deny, Self::Edit];

    /// How the wireframe labels it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Once => "[a]llow once",
            Self::Always => "[A]lways allow (project)",
            Self::Deny => "[d]eny",
            Self::Edit => "[e]dit request",
        }
    }
}

/// An approval request on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    pub request_id: String,
    pub call_id: String,
    pub tool: String,
    pub files: Vec<String>,
    /// What the action is: a command, a URL, a note about the write.
    pub body: Vec<String>,
    /// For edits: the first changed line and the diff lines.
    pub diff: Option<(u32, Vec<String>)>,
    pub rule: Option<String>,
    /// Which button has focus (index into [`ApprovalAnswer::BUTTONS`]).
    pub focus: usize,
}

impl Approval {
    /// Build the prompt from an `approval.requested` event's fields.
    #[must_use]
    pub fn from_event(
        request_id: &str,
        call_id: &str,
        kind: &str,
        summary: &str,
        detail: &Value,
    ) -> Self {
        let tool = detail
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or(kind)
            .to_string();
        let files: Vec<String> = detail
            .get("paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let input = detail.get("input").cloned().unwrap_or(Value::Null);
        let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
        let mut body = Vec::new();
        let mut diff = None;
        match tool.as_str() {
            "bash" | "bash_background" => {
                body.push(text("command").unwrap_or_else(|| summary.to_string()));
            }
            "web_fetch" => body.push(text("url").unwrap_or_else(|| summary.to_string())),
            "edit_file" | "multi_edit" => {
                if let (Some(old), Some(new)) = (text("old_string"), text("new_string")) {
                    let mut lines: Vec<String> = old.lines().map(|l| format!("- {l}")).collect();
                    lines.extend(new.lines().map(|l| format!("+ {l}")));
                    diff = Some((0, lines));
                } else if let Some(edits) = input.get("edits").and_then(Value::as_array) {
                    body.push(format!("{} edit(s)", edits.len()));
                }
            }
            "write_file" => {
                let lines = text("content").map_or(0, |c| c.lines().count());
                body.push(format!("write {lines} line(s)"));
            }
            "git_commit" => body.push(text("message").unwrap_or_default()),
            _ => {
                if body.is_empty() {
                    body.push(summary.to_string());
                }
            }
        }
        let rule = detail.get("rule").and_then(Value::as_str).map(|r| {
            match detail
                .get("reason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && !s.starts_with("rule "))
            {
                Some(reason) => format!("{r} — {reason}"),
                None => r.to_string(),
            }
        });
        Self {
            request_id: request_id.to_string(),
            call_id: call_id.to_string(),
            tool,
            files,
            body,
            diff,
            rule,
            focus: 0,
        }
    }

    /// The answer the focused button gives.
    #[must_use]
    pub fn focused(&self) -> ApprovalAnswer {
        ApprovalAnswer::BUTTONS[self.focus % ApprovalAnswer::BUTTONS.len()]
    }

    /// Move focus by `delta` buttons, wrapping.
    pub fn cycle(&mut self, delta: isize) {
        let n = ApprovalAnswer::BUTTONS.len();
        self.focus = if delta >= 0 {
            (self.focus + delta.unsigned_abs()) % n
        } else {
            (self.focus + n - delta.unsigned_abs() % n) % n
        };
    }
}

// ----------------------------------------------------------------- diffs

/// Whether a diff line is context, added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One line of a hunk, with its numbers on each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

/// What the person decided about a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkStatus {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
    pub status: HunkStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub hunks: Vec<Hunk>,
}

/// Read a unified diff (what `git_diff` returns).
#[must_use]
pub fn parse_unified(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut old_no = 0;
    let mut new_no = 0;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.split(" b/").nth(1).unwrap_or(rest).trim().to_string();
            files.push(FileDiff {
                path,
                hunks: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if let (Some(file), Some(path)) = (files.last_mut(), rest.strip_prefix("b/")) {
                file.path = path.to_string();
            }
        } else if line.starts_with("--- ")
            || line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
        {
            // File header lines carry nothing a hunk needs.
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            let nums = rest.split(" @@").next().unwrap_or("");
            let mut parts = nums.split_whitespace();
            let start = |p: Option<&str>| {
                p.and_then(|s| s.trim_start_matches(['-', '+']).split(',').next())
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or(1)
            };
            old_no = start(parts.next());
            new_no = start(parts.next());
            if files.is_empty() {
                files.push(FileDiff {
                    path: String::new(),
                    hunks: Vec::new(),
                });
            }
            if let Some(file) = files.last_mut() {
                file.hunks.push(Hunk {
                    old_start: old_no,
                    new_start: new_no,
                    lines: Vec::new(),
                    status: HunkStatus::Pending,
                });
            }
        } else if let Some(hunk) = files.last_mut().and_then(|f| f.hunks.last_mut()) {
            let (kind, text) = match line.chars().next() {
                Some('+') => (LineKind::Added, &line[1..]),
                Some('-') => (LineKind::Removed, &line[1..]),
                Some(' ') => (LineKind::Context, &line[1..]),
                // "\ No newline at end of file" and anything else.
                _ => continue,
            };
            let (o, n) = match kind {
                LineKind::Context => {
                    let r = (Some(old_no), Some(new_no));
                    old_no += 1;
                    new_no += 1;
                    r
                }
                LineKind::Removed => {
                    let r = (Some(old_no), None);
                    old_no += 1;
                    r
                }
                LineKind::Added => {
                    let r = (None, Some(new_no));
                    new_no += 1;
                    r
                }
            };
            hunk.lines.push(DiffLine {
                kind,
                text: text.to_string(),
                old_no: o,
                new_no: n,
            });
        }
    }
    files.retain(|f| !f.hunks.is_empty());
    files
}

/// The diff viewer's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView {
    pub files: Vec<FileDiff>,
    pub file: usize,
    pub hunk: usize,
}

impl DiffView {
    #[must_use]
    pub fn new(files: Vec<FileDiff>) -> Self {
        Self {
            files,
            file: 0,
            hunk: 0,
        }
    }

    fn hunks(&self) -> usize {
        self.files.get(self.file).map_or(0, |f| f.hunks.len())
    }

    pub fn next_hunk(&mut self) {
        if self.hunk + 1 < self.hunks() {
            self.hunk += 1;
        }
    }

    pub fn prev_hunk(&mut self) {
        self.hunk = self.hunk.saturating_sub(1);
    }

    pub fn next_file(&mut self) {
        if self.file + 1 < self.files.len() {
            self.file += 1;
            self.hunk = 0;
        }
    }

    pub fn prev_file(&mut self) {
        if self.file > 0 {
            self.file -= 1;
            self.hunk = 0;
        }
    }

    pub fn set_status(&mut self, status: HunkStatus) {
        if let Some(h) = self
            .files
            .get_mut(self.file)
            .and_then(|f| f.hunks.get_mut(self.hunk))
        {
            h.status = status;
        }
    }
}

// ------------------------------------------------------------------ plans

/// A step of a plan, as the card lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub files: usize,
    pub detail: String,
}

/// An assumption, flagged when nobody has checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assumption {
    pub id: String,
    pub text: String,
    pub verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Risk {
    pub id: String,
    pub text: String,
    pub severity: String,
    pub mitigation: String,
}

/// The plan card's content and state (§10.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanView {
    pub id: String,
    pub goal: String,
    pub assumptions: Vec<Assumption>,
    pub steps: Vec<PlanStep>,
    pub risks: Vec<Risk>,
    pub test: String,
    pub rollback: String,
    pub selected: usize,
    pub show_detail: bool,
}

/// What the plan card can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Approve,
    Edit,
    SaveOnly,
    Dismiss,
}

// ------------------------------------------------------- search & prompts

/// `Ctrl+R`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HistorySearch {
    pub query: String,
    pub selected: usize,
}

/// A question the prompt asks and waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// `Paste 12,412 chars? [Y/n]`
    Paste { chars: usize },
    /// `Quit? [y/N]`
    Quit,
    /// Jobs are still running (§10.9).
    QuitWithJobs { names: Vec<String> },
}

/// Which overlay is open, with its own state.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Overlay {
    #[default]
    None,
    Approval(Approval),
    Diff(DiffView),
    Plan(PlanView),
    Help,
    History(HistorySearch),
    Confirm(Confirm),
}

impl Overlay {
    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIFF: &str = "\
diff --git a/src/range.rs b/src/range.rs
index 111..222 100644
--- a/src/range.rs
+++ b/src/range.rs
@@ -118,4 +118,4 @@ fn x
 }
 fn clamp(i: usize) {
-  if i > end {
+  if i >= end {
     return end;
@@ -200,2 +200,3 @@
 a
+b
 c
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-old
+new
";

    #[test]
    fn a_unified_diff_becomes_files_hunks_and_numbered_lines() {
        let files = parse_unified(DIFF);
        assert_eq!(
            files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["src/range.rs", "README.md"]
        );
        let first = &files[0];
        assert_eq!(first.hunks.len(), 2);
        let h = &first.hunks[0];
        assert_eq!((h.old_start, h.new_start), (118, 118));
        let kinds: Vec<LineKind> = h.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context
            ]
        );
        assert_eq!(h.lines[2].old_no, Some(120));
        assert_eq!(h.lines[2].new_no, None);
        assert_eq!(h.lines[3].new_no, Some(120));
        assert_eq!(h.lines[4].old_no, Some(121));
        assert_eq!(h.lines[4].new_no, Some(121));
        assert_eq!(first.hunks[1].lines[1].text, "b");
        assert_eq!(files[1].hunks[0].lines.len(), 2);
    }

    #[test]
    fn nothing_that_is_not_a_diff_survives() {
        assert!(parse_unified("").is_empty());
        assert!(parse_unified("just some text\nwith lines").is_empty());
        // A file with only a mode change has no hunks and is dropped.
        assert!(parse_unified("diff --git a/x b/x\nold mode 100644\nnew mode 100755\n").is_empty());
    }

    #[test]
    fn hunk_navigation_stays_in_bounds_and_files_reset_the_hunk() {
        let mut v = DiffView::new(parse_unified(DIFF));
        v.prev_hunk();
        assert_eq!((v.file, v.hunk), (0, 0));
        v.next_hunk();
        v.next_hunk();
        assert_eq!(v.hunk, 1);
        v.next_file();
        assert_eq!((v.file, v.hunk), (1, 0));
        v.next_file();
        assert_eq!(v.file, 1);
        v.set_status(HunkStatus::Accepted);
        assert_eq!(v.files[1].hunks[0].status, HunkStatus::Accepted);
        v.prev_file();
        assert_eq!((v.file, v.hunk), (0, 0));
    }

    #[test]
    fn an_edit_approval_shows_the_change_a_bash_one_the_command() {
        let a = Approval::from_event(
            "ap_1",
            "c1",
            "write",
            "src/range.rs",
            &json!({
                "tool": "edit_file", "paths": ["src/range.rs"], "mode": "build",
                "input": {"path": "src/range.rs", "old_string": "if i > end  {", "new_string": "if i >= end {"},
                "rule": "D3", "reason": "rule D3"
            }),
        );
        assert_eq!(a.tool, "edit_file");
        assert_eq!(a.files, ["src/range.rs"]);
        assert_eq!(
            a.diff.as_ref().unwrap().1,
            ["- if i > end  {", "+ if i >= end {"]
        );
        assert_eq!(a.rule.as_deref(), Some("D3"));
        let b = Approval::from_event(
            "ap_2",
            "c2",
            "execute",
            "cargo test",
            &json!({"tool": "bash", "paths": [], "input": {"command": "cargo test --quiet"}, "rule": "D5", "reason": "Command runs a program"}),
        );
        assert_eq!(b.body, ["cargo test --quiet"]);
        assert_eq!(b.rule.as_deref(), Some("D5 — Command runs a program"));
        assert!(b.diff.is_none());
        let w = Approval::from_event(
            "ap_3",
            "c3",
            "write",
            "x",
            &json!({"tool": "write_file", "input": {"content": "a\nb\nc\n"}}),
        );
        assert_eq!(w.body, ["write 3 line(s)"]);
    }

    #[test]
    fn focus_cycles_through_the_four_buttons_both_ways() {
        let mut a = Approval::from_event("r", "c", "write", "s", &json!({}));
        assert_eq!(a.focused(), ApprovalAnswer::Once);
        a.cycle(1);
        assert_eq!(a.focused(), ApprovalAnswer::Always);
        a.cycle(1);
        a.cycle(1);
        assert_eq!(a.focused(), ApprovalAnswer::Edit);
        a.cycle(1);
        assert_eq!(a.focused(), ApprovalAnswer::Once, "wraps");
        a.cycle(-1);
        assert_eq!(a.focused(), ApprovalAnswer::Edit);
    }
}
