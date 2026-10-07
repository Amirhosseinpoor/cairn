//! Golden screens (T-TUI-001..006, 010..019): the interface rendered into a
//! buffer and read back as text.

use crate::app::{App, Branch, ErrorCard, Item, Look, NoticeKind, Startup};
use crate::theme::ColorSupport;
use crate::view::{draw, rows};
use cairn_core::event::{EventData, ToolStatus};
use cairn_core::Mode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;

fn screen(app: &App, w: u16, h: u16) -> String {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    let _ = draw(app, &mut buf, area);
    rows(&buf).join("\n")
}

fn app() -> App {
    let look = Look {
        support: ColorSupport::None,
        ..Look::default()
    };
    App::new(look, Mode::Build, "claude-sonnet-4-5")
}

fn tool(a: &mut App, id: &str, name: &str, input: serde_json::Value, ms: u64, bytes: u32) {
    a.apply(&EventData::ToolStarted {
        call_id: id.into(),
        name: name.into(),
        input,
        parallel_index: 0,
    });
    a.apply(&EventData::ToolFinished {
        call_id: id.into(),
        name: name.into(),
        status: ToolStatus::Ok,
        duration_ms: ms,
        output_bytes: bytes,
        truncated: false,
        error: None,
    });
}

/// T-TUI-001: the canonical layout with a finished turn.
#[test]
fn the_canonical_layout() {
    let mut a = app();
    a.session = Some("main".into());
    a.branch = Some(Branch {
        name: "main".into(),
        added: 1,
        removed: 0,
    });
    a.todos = Some((3, 7));
    a.context = Some((12_880, 200_000));
    a.cost_usd = Some(0.041);
    a.tokens_in = 11_000;
    a.tokens_out = 1_800;
    a.submit("Fix the failing test in tests/parser_test.py");
    tool(
        &mut a,
        "1",
        "read_file",
        json!({"path": "src/parser.rs"}),
        14,
        1946,
    );
    tool(
        &mut a,
        "2",
        "edit_file",
        json!({"path": "src/parser.rs", "start_line": 120}),
        3,
        0,
    );
    if let Some(Item::Tool(c)) = a.transcript.last_mut() {
        c.diff = Some((
            120,
            vec!["- if i > end  {".into(), "+ if i >= end {".into()],
        ));
    }
    tool(
        &mut a,
        "3",
        "bash",
        json!({"command": "cargo test --quiet"}),
        4100,
        0,
    );
    a.transcript.push(Item::Notice(
        NoticeKind::Success,
        "Done. 1 file changed (+1 −1). Verification passed.".into(),
    ));
    a.running = None;
    let want = r"
 › Fix the failing test in tests/parser_test.py

 ⏺ read_file(src/parser.rs)
   ⎿  ✓ 1.9 KiB · 14 ms                                                                         [▸]
 ⏺ edit_file(src/parser.rs:120)
   ⎿  ✓ 3 ms                                                                                    [▸]
      1 line added, 1 removed
       120 - if i > end  {
       120 + if i >= end {
 ⏺ bash(cargo test --quiet)
   ⎿  ✓ 4.1 s                                                                                   [▸]

 ✓ Done. 1 file changed (+1 −1). Verification passed.













 ╭────────────────────────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything · / for commands · @ for files                                                  │
 ╰────────────────────────────────────────────────────────────────────────────────────────────────╯
   ⏵ build mode (shift+tab to cycle)    todos 3/7 · ctx 12,880/200k (6%) · git:main +1 · $0.041 ●";
    assert_eq!(screen(&a, 100, 30), want.trim_start_matches('\n'));
}

/// T-TUI-002: idle.
#[test]
fn the_idle_screen() {
    let mut a = app();
    a.startup = Startup {
        instructions: 2,
        repo_files: Some(1842),
        repo_ms: Some(40),
        indexing: false,
    };
    a.context = Some((9_410, 200_000));
    let want = r"
 ╭──────────────────────────────────────────────────────────────╮
 │ ✻ Welcome to Cairn                                           │
 │                                                              │
 │   model claude-sonnet-4-5                                    │
 │   mode  build · shift+tab to change                          │
 ╰──────────────────────────────────────────────────────────────╯

 Ready. Type a prompt, @ to mention a file, / for commands.
 AGENTS.md loaded (2 instructions) · repo map: 1,842 files (warm 40 ms)

 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything · / for commands · @ for files                              │
 ╰────────────────────────────────────────────────────────────────────────────╯
   ⏵ build mode (shift+tab to cycle)                    ctx 9,410/200k (5%) ●";
    assert_eq!(screen(&a, 80, 14), want.trim_start_matches('\n'));
}

/// T-TUI-003: streaming — a thinking line, then text arriving with a cursor.
#[test]
fn the_streaming_screen() {
    let mut a = app();
    a.context = Some((13_102, 200_000));
    a.submit("Why does it fail?");
    // Before any text: the thinking line, with how to stop it.
    let thinking = screen(&a, 80, 14);
    assert!(
        thinking.contains("Thinking… (0s · esc to interrupt)"),
        "{thinking}"
    );
    a.apply(&EventData::ModelDelta {
        turn_id: 1,
        text: "The parser fails when the stream ends without a terminator. I'll add a check in `next_token` and cover it with a test.".into(),
    });
    a.apply(&EventData::ModelUsage {
        turn_id: 1,
        input: 10,
        output: 96,
        cache_read: 0,
        cache_write: 0,
        cost_usd: 0.0,
    });
    for _ in 0..80 {
        a.tick();
    }
    let got = screen(&a, 80, 14);
    assert!(got.contains(" › Why does it fail?"), "{got}");
    assert!(
        got.contains(
            " ⏺ The parser fails when the stream ends without a terminator. I'll add a check"
        ),
        "{got}"
    );
    assert!(
        got.contains("   in `next_token` and cover it with a test.▌"),
        "{got}"
    );
    assert!(got.contains("Responding… (6s · esc to interrupt)"), "{got}");
    assert!(got.contains("ctx 13,102/200k (7%)"), "{got}");
}

/// T-TUI-004: a tool running, with its rolling output.
#[test]
fn the_tool_running_screen() {
    let mut a = app();
    a.submit("run the tests");
    a.apply(&EventData::ToolStarted {
        call_id: "b".into(),
        name: "bash".into(),
        input: json!({"command": "cargo test --quiet"}),
        parallel_index: 0,
    });
    for l in [
        "test tests::parses_eof ... ok",
        "test tests::rejects_nul ...",
    ] {
        a.apply(&EventData::ToolProgress {
            call_id: "b".into(),
            bytes_read: 40,
            lines: 2,
            truncated: false,
            preview: format!("{l}\n"),
        });
    }
    for _ in 0..52 {
        a.tick();
    }
    let got = screen(&a, 80, 16);
    let lines: Vec<&str> = got.lines().collect();
    assert_eq!(lines[0], " › run the tests");
    // The spinner frame changes; the rest does not.
    assert!(lines[2].ends_with(" bash(cargo test --quiet)"), "{got}");
    assert_eq!(lines[3], "   ⎿  test tests::parses_eof ... ok");
    assert_eq!(lines[4], "      test tests::rejects_nul ...");
    assert_eq!(
        lines[5],
        "      Running… (4s · esc to cancel · ctrl+o details)"
    );
}

/// T-TUI-005: the error state.
#[test]
fn the_error_state() {
    let mut a = app();
    a.submit("go");
    a.transcript.push(Item::Error(ErrorCard {
        title: "Provider error".into(),
        code: "E-PROV-RATELIMIT".into(),
        extra: Some("HTTP 429".into()),
        message: "OpenAI rate limit reached; retrying in 12s (retry 1/5).".into(),
        detail: Some("{\"error\":{\"type\":\"rate_limit_exceeded\"}}".into()),
        actions: Some("[r]etry now  [m]odel  [v]iew log  [Esc] dismiss".into()),
    }));
    a.running = None;
    let want = r#"
 › go

 ✗ Provider error  E-PROV-RATELIMIT (HTTP 429)
   OpenAI rate limit reached; retrying in 12s (retry 1/5).
   ╭ detail ────────────────────────────────────────────────────────────────╮
   │ {"error":{"type":"rate_limit_exceeded"}}                               │
   ╰────────────────────────────────────────────────────────────────────────╯
 [r]etry now  [m]odel  [v]iew log  [Esc] dismiss





 ╭────────────────────────────────────────────────────────────────────────────╮
 │ › Ask anything · / for commands · @ for files                              │
 ╰────────────────────────────────────────────────────────────────────────────╯
   ⏵ build mode (shift+tab to cycle)                      claude-sonnet-4-5 ●"#;
    assert_eq!(screen(&a, 80, 17), want.trim_start_matches('\n'));
}

/// §10.1: below 40×12 only the resize message.
#[test]
fn a_terminal_that_is_too_small_gets_only_the_resize_message() {
    let a = app();
    let got = screen(&a, 30, 10);
    assert!(
        got.starts_with("Terminal too small (need\n40x12, have 30x10). Resize to\ncontinue."),
        "{got}"
    );
    assert!(!got.contains('╭'));
    // Exactly the minimum draws the interface.
    assert!(screen(&a, 40, 12).contains('╭'));
    assert!(screen(&a, 39, 12).starts_with("Terminal too small"));
    assert!(screen(&a, 40, 11).starts_with("Terminal too small"));
}

#[test]
#[ignore = "prints screens for eyeballing: cargo test -p cairn-tui dump_screens -- --ignored --nocapture"]
fn dump_screens() {
    let mut a = app();
    a.submit("Fix the off-by-one in clamp()");
    a.apply(&EventData::ModelDelta { turn_id: 1, text: "I'll look at the **clamp** function first.\n\n- check `range.rs`\n- add a test\n\n```rust\nfn clamp(i: usize) -> usize { i.min(end) }\n```".into() });
    tool(
        &mut a,
        "c1",
        "read_file",
        json!({"path": "src/range.rs"}),
        12,
        2048,
    );
    println!("{}", screen(&a, 100, 30));
    println!("{}", screen(&a, 60, 20));
}
