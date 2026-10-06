//! Keys, popups and overlays (T-TUI-020..043 subset): keys go in, commands
//! and screens come out.

use std::fmt::Write as _;

use crate::app::{App, Item, Look, NoticeKind};
use crate::interact::{Command, PopupKind};
use crate::keymap::{Chords, Keymap};
use crate::keys::{Code, Key};
use crate::overlay::{parse_unified, ApprovalAnswer, DiffView, HunkStatus, Overlay, PlanAction};
use crate::theme::ColorSupport;
use crate::view::{draw, rows};
use cairn_core::event::EventData;
use cairn_core::Mode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;

fn app() -> App {
    let look = Look {
        support: ColorSupport::None,
        ..Look::default()
    };
    App::new(look, Mode::Build, "m")
}

fn screen(app: &App, w: u16, h: u16) -> String {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    let _ = draw(app, &mut buf, area);
    rows(&buf).join("\n")
}

struct Rig {
    app: App,
    keymap: Keymap,
    chords: Chords,
}

impl Rig {
    fn new() -> Self {
        Self {
            app: app(),
            keymap: Keymap::default(),
            chords: Chords::default(),
        }
    }
    fn key(&mut self, key: Key) -> Vec<Command> {
        self.app.on_key(key, &self.keymap, &mut self.chords)
    }
    fn type_text(&mut self, text: &str) -> Vec<Command> {
        let mut out = Vec::new();
        for c in text.chars() {
            out.extend(self.key(Key::char(c)));
        }
        out
    }
    fn enter(&mut self) -> Vec<Command> {
        self.key(Key::new(Code::Enter))
    }
}

fn approval_event(tool: &str, input: &serde_json::Value) -> EventData {
    EventData::ApprovalRequested {
        request_id: "r1".into(),
        call_id: "c1".into(),
        kind: "tool".into(),
        summary: "run".into(),
        detail: json!({"tool": tool, "input": input, "paths": ["src/lib.rs"], "rule": "ask:bash", "reason": ""}),
        expires_in_ms: 60_000,
    }
}

const DIFF: &str = "\
diff --git a/src/range.rs b/src/range.rs
--- a/src/range.rs
+++ b/src/range.rs
@@ -118,3 +118,3 @@
 fn clamp(i: usize) {
-  if i > end {
+  if i >= end {
     return end;
@@ -200,2 +200,3 @@
 a
+b
 c
";

#[test]
fn typing_and_enter_submit_a_prompt() {
    let mut r = Rig::new();
    r.type_text("fix the bug");
    let out = r.enter();
    assert_eq!(out, vec![Command::Submit("fix the bug".into())]);
    assert!(r.app.editor.is_empty());
}

#[test]
fn an_unknown_slash_command_is_answered_inline_and_never_sent() {
    let mut r = Rig::new();
    r.type_text("/hlep");
    let out = r.enter();
    assert!(out.is_empty());
    let found = r
        .app
        .transcript
        .iter()
        .any(|i| matches!(i, Item::Notice(NoticeKind::Warning, m) if m.contains("/help")));
    assert!(found, "{:?}", r.app.transcript);
}

#[test]
fn a_known_slash_command_comes_back_as_a_command() {
    let mut r = Rig::new();
    r.type_text("/compact keep the tests");
    let out = r.enter();
    assert_eq!(
        out,
        vec![Command::Slash {
            name: "compact",
            args: "keep the tests".into()
        }]
    );
}

#[test]
fn the_slash_popup_filters_and_tab_completes() {
    let mut r = Rig::new();
    r.type_text("/co");
    let popup = r.app.popup().expect("popup");
    assert_eq!(popup.kind, PopupKind::Slash);
    assert!(popup.items.iter().any(|i| i == "compact"));
    assert!(screen(&r.app, 100, 24).contains("/compact"));
    r.key(Key::new(Code::Tab));
    assert!(r.app.editor.text().starts_with('/'));
    assert!(r.app.editor.text().len() > 3);
}

#[test]
fn escape_closes_the_popup_without_touching_the_text() {
    let mut r = Rig::new();
    r.type_text("/co");
    r.key(Key::new(Code::Esc));
    assert!(r.app.popup().is_none());
    assert_eq!(r.app.editor.text(), "/co");
}

#[test]
fn an_at_fragment_asks_for_candidates_once_per_change() {
    let mut r = Rig::new();
    let out = r.type_text("look at @sr");
    let queries: Vec<_> = out
        .iter()
        .filter_map(|c| match c {
            Command::MentionQuery(q) => Some(q.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(queries.last().map(String::as_str), Some("sr"));
    r.app.mention_candidates = vec!["src/lib.rs".into(), "README.md".into()];
    let popup = r.app.popup().expect("popup");
    assert_eq!(popup.kind, PopupKind::Mention);
    assert_eq!(popup.items[0], "src/lib.rs");
}

#[test]
fn escape_cancels_a_turn_and_twice_forces_it() {
    let mut r = Rig::new();
    r.app.submit("go");
    let first = r.key(Key::new(Code::Esc));
    assert_eq!(first, vec![Command::Cancel]);
    let second = r.key(Key::new(Code::Esc));
    assert_eq!(second, vec![Command::ForceCancel]);
    // Far apart: a plain cancel again.
    for _ in 0..20 {
        r.app.tick();
    }
    assert_eq!(r.key(Key::new(Code::Esc)), vec![Command::Cancel]);
}

#[test]
fn ctrl_c_idle_asks_before_quitting() {
    let mut r = Rig::new();
    assert!(r.key(Key::ctrl('c')).is_empty());
    assert!(matches!(r.app.overlay, Overlay::Confirm(_)));
    assert!(screen(&r.app, 100, 24).contains("Quit? [y/N]"));
    assert!(r.enter().is_empty());
    assert!(!r.app.overlay.is_open());
    r.key(Key::ctrl('c'));
    assert_eq!(r.type_text("y"), vec![Command::Quit { kill_jobs: false }]);
}

#[test]
fn ctrl_d_quits_only_when_the_prompt_is_empty() {
    let mut r = Rig::new();
    r.type_text("x");
    assert!(r.key(Key::ctrl('d')).is_empty());
    r.key(Key::ctrl('u'));
    assert_eq!(
        r.key(Key::ctrl('d')),
        vec![Command::Quit { kill_jobs: false }]
    );
}

#[test]
fn quitting_with_jobs_asks_what_to_do() {
    let mut r = Rig::new();
    r.app.jobs = 1;
    r.app.job_names = vec!["npm run dev".into()];
    r.key(Key::ctrl('c'));
    assert!(screen(&r.app, 120, 24).contains("npm run dev"));
    assert_eq!(r.type_text("k"), vec![Command::Quit { kill_jobs: true }]);
}

#[test]
fn a_big_paste_asks_and_enter_accepts() {
    let mut r = Rig::new();
    r.app.on_paste(&"x".repeat(12_412));
    assert!(screen(&r.app, 100, 24).contains("Paste 12,412 chars? [Y/n]"));
    r.enter();
    assert!(!r.app.overlay.is_open());
    assert_eq!(r.app.editor.text().len(), 12_412);
}

#[test]
fn a_big_paste_can_be_refused() {
    let mut r = Rig::new();
    r.app.on_paste(&"x".repeat(12_412));
    r.type_text("n");
    assert!(r.app.editor.is_empty());
}

#[test]
fn history_walks_back_with_up_and_down() {
    let mut r = Rig::new();
    r.type_text("one");
    r.enter();
    r.type_text("two");
    r.enter();
    r.key(Key::new(Code::Up));
    assert_eq!(r.app.editor.text(), "two");
    r.key(Key::new(Code::Up));
    assert_eq!(r.app.editor.text(), "one");
    r.key(Key::new(Code::Down));
    assert_eq!(r.app.editor.text(), "two");
}

#[test]
fn ctrl_r_searches_history() {
    let mut r = Rig::new();
    for t in ["cargo build", "git status", "cargo test"] {
        r.type_text(t);
        r.enter();
    }
    r.key(Key::ctrl('r'));
    r.type_text("stat");
    assert!(screen(&r.app, 100, 24).contains("git status"));
    r.enter();
    assert_eq!(r.app.editor.text(), "git status");
}

#[test]
fn the_approval_modal_answers_with_one_key() {
    let mut r = Rig::new();
    r.app
        .apply(&approval_event("bash", &json!({"command": "cargo test"})));
    let shown = screen(&r.app, 100, 24);
    assert!(shown.contains("Approval · bash"), "{shown}");
    assert!(shown.contains("cargo test"));
    assert!(shown.contains("[a]llow once"));
    assert!(shown.contains("[d]eny"));
    let out = r.key(Key::char('a'));
    assert_eq!(
        out,
        vec![Command::Approval {
            request_id: "r1".into(),
            answer: ApprovalAnswer::Once
        }]
    );
    assert!(!r.app.overlay.is_open());
}

#[test]
fn the_approval_modal_maps_every_key() {
    for (key, answer) in [
        (Key::char('A'), ApprovalAnswer::Always),
        (Key::char('d'), ApprovalAnswer::Deny),
        (Key::char('e'), ApprovalAnswer::Edit),
        (Key::new(Code::Esc), ApprovalAnswer::Deny),
    ] {
        let mut r = Rig::new();
        r.app
            .apply(&approval_event("bash", &json!({"command": "ls"})));
        let out = r.key(key);
        assert_eq!(
            out,
            vec![Command::Approval {
                request_id: "r1".into(),
                answer
            }],
            "{key:?}"
        );
    }
}

#[test]
fn tab_moves_focus_and_enter_answers_the_focused_button() {
    let mut r = Rig::new();
    r.app
        .apply(&approval_event("bash", &json!({"command": "ls"})));
    r.key(Key::new(Code::Tab));
    r.key(Key::new(Code::Tab));
    let out = r.enter();
    assert_eq!(
        out,
        vec![Command::Approval {
            request_id: "r1".into(),
            answer: ApprovalAnswer::Deny
        }]
    );
}

#[test]
fn an_edit_approval_shows_the_change_and_the_rule() {
    let mut r = Rig::new();
    r.app.apply(&approval_event(
        "edit_file",
        &json!({"old_string": "a > b", "new_string": "a >= b"}),
    ));
    let shown = screen(&r.app, 100, 24);
    assert!(shown.contains("- a > b"), "{shown}");
    assert!(shown.contains("+ a >= b"));
    assert!(shown.contains("rule: ask:bash"));
    assert!(shown.contains("src/lib.rs"));
}

#[test]
fn an_answer_elsewhere_closes_the_modal() {
    let mut r = Rig::new();
    r.app
        .apply(&approval_event("bash", &json!({"command": "ls"})));
    r.app.apply(&EventData::ApprovalAnswered {
        request_id: "r1".into(),
        answer: "allow".into(),
        rule: None,
    });
    assert!(!r.app.overlay.is_open());
}

#[test]
fn the_diff_viewer_is_inline_below_100_columns_and_side_by_side_from_100() {
    let mut r = Rig::new();
    r.app.overlay = Overlay::Diff(DiffView::new(parse_unified(DIFF)));
    let narrow = screen(&r.app, 99, 30);
    assert!(narrow.contains("src/range.rs"), "{narrow}");
    assert!(narrow.contains("- "), "{narrow}");
    assert!(!narrow.contains(" │ "), "{narrow}");
    let wide = screen(&r.app, 100, 30);
    assert!(wide.contains(" │ "), "{wide}");
}

#[test]
fn the_diff_viewer_accepts_and_rejects_hunks() {
    let mut r = Rig::new();
    r.app.overlay = Overlay::Diff(DiffView::new(parse_unified(DIFF)));
    let out = r.key(Key::char(' '));
    assert_eq!(
        out,
        vec![Command::Hunk {
            file: 0,
            hunk: 0,
            status: HunkStatus::Accepted
        }]
    );
    let out = r.key(Key::char('r'));
    assert_eq!(
        out,
        vec![Command::Hunk {
            file: 0,
            hunk: 1,
            status: HunkStatus::Rejected
        }]
    );
    let shown = screen(&r.app, 100, 30);
    assert!(shown.contains('✓') && shown.contains('✗'), "{shown}");
    assert!(r.key(Key::new(Code::Esc)).is_empty());
    assert!(!r.app.overlay.is_open());
}

#[test]
fn the_diff_viewer_scrolls_to_the_current_hunk() {
    let mut text = String::from("diff --git a/f b/f\n--- a/f\n+++ b/f\n");
    for h in 0..30 {
        let _ = writeln!(text, "@@ -{0},1 +{0},1 @@\n-old{h}\n+new{h}", h * 10 + 1);
    }
    let mut r = Rig::new();
    let mut view = DiffView::new(parse_unified(&text));
    for _ in 0..25 {
        view.next_hunk();
    }
    r.app.overlay = Overlay::Diff(view);
    let shown = screen(&r.app, 100, 20);
    assert!(shown.contains("new25"), "{shown}");
    assert!(shown.contains("Esc close"));
}

#[test]
fn help_lists_keys_and_closes_with_escape() {
    let mut r = Rig::new();
    r.key(Key::new(Code::F(1)));
    assert!(matches!(r.app.overlay, Overlay::Help));
    assert!(screen(&r.app, 100, 30).contains("search history"));
    r.key(Key::new(Code::Esc));
    assert!(!r.app.overlay.is_open());
}

#[test]
fn the_plan_card_sends_one_action() {
    use crate::overlay::{Assumption, PlanStep, PlanView};
    let plan = PlanView {
        id: "p1".into(),
        goal: "Fix range".into(),
        assumptions: vec![Assumption {
            id: "A1".into(),
            text: "end is exclusive".into(),
            verified: false,
        }],
        steps: vec![PlanStep {
            id: "s1".into(),
            title: "Change the compare".into(),
            files: 1,
            detail: "details".into(),
        }],
        risks: vec![],
        test: "cargo test".into(),
        rollback: "git checkout".into(),
        selected: 0,
        show_detail: false,
    };
    let mut r = Rig::new();
    r.app.overlay = Overlay::Plan(plan);
    let shown = screen(&r.app, 100, 30);
    assert!(shown.contains("Goal: Fix range"), "{shown}");
    assert!(shown.contains("unverified"));
    assert!(shown.contains("Change the compare"));
    assert_eq!(r.enter(), vec![Command::Plan(PlanAction::Approve)]);
}

#[test]
fn shift_tab_cycles_the_mode_and_ctrl_l_clears_the_view() {
    let mut r = Rig::new();
    let st = Key {
        shift: true,
        ..Key::new(Code::Tab)
    };
    assert!(
        r.key(st).contains(&Command::CycleMode)
            || r.key(Key::new(Code::BackTab)).contains(&Command::CycleMode)
    );
    r.app.submit("x");
    assert_eq!(r.key(Key::ctrl('l')), vec![Command::ClearView]);
    assert!(r.app.transcript.is_empty());
}
