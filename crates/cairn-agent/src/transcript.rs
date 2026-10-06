//! The session side of a turn: appending §11.7 records one at a time, and
//! reading them back as resume state (§8.7).
//!
//! Every record goes through `Store::append` — one `write` + `fsync` per
//! record (REQ-LOOP-006) — so a `kill -9` between two records leaves a
//! readable file whose last turn is *dangling*: started, never ended.
//! [`recover`] closes that turn exactly once, whichever way the user chooses,
//! which is what makes running resume twice a no-op (REQ-LOOP-007).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cairn_core::message::{Block, Message, Role, Usage};
use cairn_core::CairnError;
use cairn_session::record::kind;
use cairn_session::{Header, Record, SessionFile, Store};

type Result<T> = std::result::Result<T, CairnError>;

/// Turn statuses this module writes into `turn_ended`.
pub mod status {
    pub const OK: &str = "ok";
    pub const ERROR: &str = "error";
    pub const CANCELLED: &str = "cancelled";
    pub const GUARDRAIL: &str = "guardrail";
    pub const DENIED: &str = "denied";
    /// `d` in §8.7: the partial turn stays on disk but leaves the context.
    pub const ABANDONED: &str = "abandoned";
    /// `k` in §8.7: the partial turn stays and becomes context as it is.
    pub const KEPT: &str = "recovered";
    /// `r` in §8.7: the partial turn is closed and a fresh call replaces it.
    pub const REBUILT: &str = "interrupted";
}

/// Appends records to one session file, owning the `seq` counter.
#[derive(Debug)]
pub struct SessionWriter {
    store: Store,
    path: PathBuf,
    seq: u64,
}

impl SessionWriter {
    /// Create the session file (header only) and write after it.
    ///
    /// # Errors
    /// `E-SESS-CORRUPT` / `E-FS-*` from the store.
    pub fn create(store: Store, header: &Header) -> Result<Self> {
        let file = store.create(header)?;
        Ok(Self::resume(store, &file))
    }

    /// Continue an existing, already-loaded session.
    #[must_use]
    pub fn resume(store: Store, file: &SessionFile) -> Self {
        Self {
            store,
            path: file.path.clone(),
            seq: file.next_seq(),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&mut self, build: impl FnOnce(u64) -> Record) -> Result<()> {
        let record = build(self.seq);
        self.store.append(&self.path, &record)?;
        self.seq += 1;
        Ok(())
    }

    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn turn_started(&mut self, turn_id: u64) -> Result<()> {
        self.append(|seq| Record::turn_started(turn_id, seq))
    }

    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn message(&mut self, message: &Message) -> Result<()> {
        self.append(|seq| Record::message(message, seq))
    }

    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the §11.7 tool_result record"
    )]
    pub fn tool_result(
        &mut self,
        turn_id: u64,
        call_id: &str,
        name: &str,
        ok: bool,
        output: &str,
        duration_ms: u64,
        truncated: bool,
    ) -> Result<()> {
        self.append(|seq| {
            Record::tool_result(
                turn_id,
                seq,
                call_id,
                name,
                ok,
                output,
                duration_ms,
                truncated,
            )
        })
    }

    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn turn_ended(
        &mut self,
        turn_id: u64,
        status: &str,
        usage: Option<Usage>,
        cost_usd: Option<f64>,
        duration_ms: u64,
    ) -> Result<()> {
        self.append(|seq| Record::turn_ended(turn_id, seq, status, usage, cost_usd, duration_ms))
    }

    /// A tripped guardrail (§7.5, §11.7's `guardrail` record).
    ///
    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn guardrail(&mut self, rule: &str, limit: u64, actual: u64) -> Result<()> {
        self.append(|seq| {
            Record::new(
                kind::GUARDRAIL,
                Some(seq),
                serde_json::json!({ "rule": rule, "limit": limit, "actual": actual }),
            )
        })
    }

    /// A compaction (§5.6): the summary and what it replaced, durable before
    /// the next request relies on it.
    ///
    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn compaction(&mut self, record: &cairn_context::compact::Record) -> Result<()> {
        let body = serde_json::json!({ "action": "performed", "compaction": record });
        self.append(|seq| Record::new(kind::COMPACTION, Some(seq), body))
    }

    /// `/undo compaction`: the most recent compaction no longer applies.
    ///
    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn compaction_undone(&mut self, id: u32) -> Result<()> {
        let body = serde_json::json!({ "action": "undone", "id": id });
        self.append(|seq| Record::new(kind::COMPACTION, Some(seq), body))
    }

    /// # Errors
    /// `E-SESS-FLUSH` when the record cannot be written and synced.
    pub fn error(&mut self, code: &str, message: &str) -> Result<()> {
        self.append(|seq| Record::error(seq, code, message))
    }
}

/// The answer M1 gives a tool call: no tool is registered, so no input schema
/// can match. It is an ordinary `ok:false` result (REQ-TOOL-019: the turn
/// does not abort), and it keeps §4.1's orphan invariant — every `ToolCall`
/// has a `ToolResult` — true in the stored history.
#[must_use]
pub fn unavailable_tool_results(assistant: &Message) -> Vec<Message> {
    let results: Vec<Block> = assistant
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::ToolCall {
                call_id,
                name,
                parse_error,
                ..
            } => {
                let (code, text) = match parse_error {
                    Some(why) => (
                        "E-TOOL-BADJSON",
                        format!("tool arguments are not valid JSON: {why}"),
                    ),
                    None => (
                        "E-TOOL-BADSCHEMA",
                        format!("no tool named `{name}` is available in this run"),
                    ),
                };
                Some(Block::ToolResult {
                    call_id: call_id.clone(),
                    content: vec![Block::Text {
                        text: serde_json::json!({
                            "ok": false,
                            "error": {
                                "code": code,
                                "message": text,
                                "recovery": "answer in text; tools arrive in a later milestone",
                            }
                        })
                        .to_string(),
                    }],
                    is_error: true,
                })
            }
            _ => None,
        })
        .collect();
    if results.is_empty() {
        return Vec::new();
    }
    vec![Message::new(Role::Tool, results, assistant.turn_id)]
}

/// A turn that started and never ended (§8.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dangling {
    pub turn_id: u64,
    /// `message` records the interrupted turn had already made durable.
    pub messages_committed: usize,
    /// `tool_result` records likewise.
    pub tool_results_committed: usize,
}

impl Dangling {
    /// §8.7 step 2's banner.
    #[must_use]
    pub fn banner(&self) -> String {
        format!(
            "Recovered interrupted turn {} ({} tool calls committed, model stream incomplete).",
            self.turn_id, self.tool_results_committed
        )
    }
}

/// What resuming restores (T-SESS-013). Todos, plan, pins and file-state have
/// no records in M1 and join this struct with the milestones that write them.
#[derive(Debug, Clone)]
pub struct ResumeState {
    pub header: Header,
    /// Header mode, overridden by the last `mode_changed` (REQ-MODE-003).
    pub mode: String,
    /// The workspace the session was created in.
    pub cwd: String,
    /// Context to send: every message except those of abandoned turns.
    pub messages: Vec<Message>,
    /// One past the highest turn id on disk.
    pub next_turn_id: u64,
    /// Sum of every `turn_ended.cost_usd` that had a price.
    pub cost_usd: f64,
    pub dangling: Option<Dangling>,
    /// Compactions in force, oldest first (§5.6).
    pub compactions: Vec<cairn_context::compact::Record>,
    /// The messages as they were written, before any compaction.
    pub original_messages: Vec<Message>,
}

/// Fold a session's records into [`ResumeState`].
#[must_use]
pub fn resume_state(file: &SessionFile) -> ResumeState {
    let mut mode = file.header.mode.clone();
    let mut cost_usd = 0.0;
    let mut max_turn = 0_u64;
    let mut open: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
    let mut abandoned: BTreeSet<u64> = BTreeSet::new();
    let mut messages: Vec<(u64, Message)> = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut compactions: Vec<cairn_context::compact::Record> = Vec::new();

    for record in file.records() {
        let turn_id = record.field("turn_id").and_then(serde_json::Value::as_u64);
        if let Some(id) = turn_id {
            max_turn = max_turn.max(id);
        }
        match record.kind.as_str() {
            kind::TURN_STARTED => {
                if let Some(id) = turn_id {
                    open.insert(id, (0, 0));
                }
            }
            kind::TURN_ENDED => {
                if let Some(id) = turn_id {
                    open.remove(&id);
                    if record.field("status").and_then(serde_json::Value::as_str)
                        == Some(status::ABANDONED)
                    {
                        abandoned.insert(id);
                    }
                }
                if let Some(cost) = record.field("cost_usd").and_then(serde_json::Value::as_f64) {
                    cost_usd += cost;
                }
            }
            kind::MESSAGE => {
                if let Ok(message) = record.as_message() {
                    // REQ-LOOP-007: a record replayed twice is one message.
                    if seen_ids.insert(message.id.to_string()) {
                        if let Some(counts) = turn_id.and_then(|id| open.get_mut(&id)) {
                            counts.0 += 1;
                        }
                        messages.push((turn_id.unwrap_or(message.turn_id), message));
                    }
                }
            }
            kind::TOOL_RESULT => {
                if let Some(counts) = turn_id.and_then(|id| open.get_mut(&id)) {
                    counts.1 += 1;
                }
            }
            kind::COMPACTION => match record.field("action").and_then(serde_json::Value::as_str) {
                Some("performed") => {
                    if let Some(parsed) = record
                        .field("compaction")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                    {
                        compactions.push(parsed);
                    }
                }
                Some("undone") => {
                    compactions.pop();
                }
                _ => {}
            },
            kind::MODE_CHANGED => {
                if let Some(to) = record.field("to").and_then(serde_json::Value::as_str) {
                    mode = to.to_string();
                }
            }
            _ => {}
        }
    }

    let dangling =
        open.iter()
            .next_back()
            .map(
                |(turn_id, (messages_committed, tool_results_committed))| Dangling {
                    turn_id: *turn_id,
                    messages_committed: *messages_committed,
                    tool_results_committed: *tool_results_committed,
                },
            );
    let original_messages: Vec<Message> = messages
        .into_iter()
        .filter(|(turn, _)| !abandoned.contains(turn))
        .map(|(_, message)| message)
        .collect();
    ResumeState {
        header: file.header.clone(),
        mode,
        cwd: file.header.workspace.clone(),
        messages: cairn_context::compact::replay(&original_messages, &compactions),
        compactions,
        original_messages,
        next_turn_id: max_turn + 1,
        cost_usd,
        dangling,
    }
}

/// §8.7 step 3's three choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// `r`: close the partial turn; the caller re-issues the model call over
    /// the committed messages (tool results are already durable).
    Rebuild,
    /// `d`: close it as abandoned; its messages leave the context.
    Discard,
    /// `k`: close it as recovered; its messages stay as context, no re-run.
    Keep,
}

/// Close a dangling turn. Returns the closed turn, or `None` when nothing
/// was dangling — which is what makes a second resume a no-op.
///
/// # Errors
/// `E-SESS-FLUSH` when the closing record cannot be written.
pub fn recover(
    writer: &mut SessionWriter,
    state: &ResumeState,
    choice: Recovery,
) -> Result<Option<Dangling>> {
    let Some(dangling) = state.dangling.clone() else {
        return Ok(None);
    };
    let closing = match choice {
        Recovery::Rebuild => status::REBUILT,
        Recovery::Discard => status::ABANDONED,
        Recovery::Keep => status::KEPT,
    };
    writer.turn_ended(dangling.turn_id, closing, None, None, 0)?;
    Ok(Some(dangling))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path().join("sessions"));
        (dir, store)
    }

    fn header(dir: &Path) -> Header {
        Header::new("01SESSION", dir.to_string_lossy(), "build", "mock/m")
    }

    fn reload(store: &Store, writer: &SessionWriter) -> SessionFile {
        store.load(writer.path()).expect("loads")
    }

    fn complete_turn(writer: &mut SessionWriter, turn: u64, prompt: &str, answer: &str, cost: f64) {
        writer.turn_started(turn).expect("started");
        writer.message(&Message::user(prompt, turn)).expect("user");
        writer
            .message(&Message::assistant(answer, turn))
            .expect("assistant");
        writer
            .turn_ended(turn, status::OK, None, Some(cost), 10)
            .expect("ended");
    }

    /// T-SESS-013 for the state M1 writes: mode (with `mode_changed`),
    /// workspace, history, next turn id and cost all come back.
    #[test]
    fn t_sess_013_resume_restores_mode_cwd_history_and_cost() {
        let (dir, store) = store();
        let mut writer = SessionWriter::create(store.clone(), &header(dir.path())).expect("create");
        complete_turn(&mut writer, 1, "hi", "hello", 0.25);
        complete_turn(&mut writer, 2, "again", "yes", 0.5);
        writer
            .append(|seq| Record::mode_changed(seq, "build", "plan", "slash"))
            .expect("mode");

        let state = resume_state(&reload(&store, &writer));
        assert_eq!(state.mode, "plan");
        assert_eq!(state.cwd, dir.path().to_string_lossy());
        assert_eq!(state.messages.len(), 4);
        assert_eq!(state.next_turn_id, 3);
        assert!((state.cost_usd - 0.75).abs() < 1e-9);
        assert!(state.dangling.is_none());
        assert_eq!(state.messages[0].text(), "hi");
        assert_eq!(state.messages[3].text(), "yes");
    }

    fn dangling_session() -> (tempfile::TempDir, Store, SessionWriter) {
        let (dir, store) = store();
        let mut writer = SessionWriter::create(store.clone(), &header(dir.path())).expect("create");
        complete_turn(&mut writer, 1, "first", "done", 0.1);
        writer.turn_started(2).expect("started");
        writer
            .message(&Message::user("second", 2))
            .expect("user committed");
        writer
            .tool_result(2, "c1", "read_file", true, "contents", 3, false)
            .expect("tool result committed");
        (dir, store, writer)
    }

    #[test]
    fn a_turn_without_an_end_is_dangling_and_counted() {
        let (_dir, store, writer) = dangling_session();
        let state = resume_state(&reload(&store, &writer));
        let dangling = state.dangling.expect("dangling");
        assert_eq!(dangling.turn_id, 2);
        assert_eq!(dangling.messages_committed, 1);
        assert_eq!(dangling.tool_results_committed, 1);
        assert_eq!(
            dangling.banner(),
            "Recovered interrupted turn 2 (1 tool calls committed, model stream incomplete)."
        );
    }

    /// `d`: records stay on disk, the partial turn leaves the context.
    #[test]
    fn discard_removes_the_partial_turn_from_context_but_not_from_disk() {
        let (_dir, store, mut writer) = dangling_session();
        let before = reload(&store, &writer);
        let state = resume_state(&before);
        recover(&mut writer, &state, Recovery::Discard).expect("recovers");
        let after = reload(&store, &writer);
        assert!(after.records().count() > before.records().count());
        let state = resume_state(&after);
        assert!(state.dangling.is_none());
        assert_eq!(
            state.messages.iter().map(Message::text).collect::<Vec<_>>(),
            vec!["first".to_string(), "done".to_string()]
        );
    }

    /// `k`: the partial turn becomes context as it stands.
    #[test]
    fn keep_makes_the_partial_turn_context() {
        let (_dir, store, mut writer) = dangling_session();
        let state = resume_state(&reload(&store, &writer));
        recover(&mut writer, &state, Recovery::Keep).expect("recovers");
        let state = resume_state(&reload(&store, &writer));
        assert!(state.dangling.is_none());
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[2].text(), "second");
    }

    /// `r`: the turn is closed; the committed user message stays so the
    /// caller can re-issue the call over it.
    #[test]
    fn rebuild_closes_the_turn_and_keeps_committed_messages_for_the_resend() {
        let (_dir, store, mut writer) = dangling_session();
        let state = resume_state(&reload(&store, &writer));
        recover(&mut writer, &state, Recovery::Rebuild).expect("recovers");
        let state = resume_state(&reload(&store, &writer));
        assert!(state.dangling.is_none());
        assert_eq!(
            state.messages.last().map(Message::text).as_deref(),
            Some("second")
        );
        assert_eq!(state.next_turn_id, 3, "the resend is a new turn");
    }

    /// T-SESS-021 / REQ-LOOP-007: resuming twice never duplicates anything —
    /// the second pass finds nothing dangling and writes nothing.
    #[test]
    fn t_sess_021_recovering_twice_changes_nothing_the_second_time() {
        for choice in [Recovery::Rebuild, Recovery::Discard, Recovery::Keep] {
            let (_dir, store, mut writer) = dangling_session();
            let state = resume_state(&reload(&store, &writer));
            assert!(recover(&mut writer, &state, choice)
                .expect("first")
                .is_some());
            let once = reload(&store, &writer);

            let state = resume_state(&once);
            assert!(recover(&mut writer, &state, choice)
                .expect("second")
                .is_none());
            let twice = reload(&store, &writer);
            assert_eq!(
                once.records().count(),
                twice.records().count(),
                "{choice:?}"
            );
            assert_eq!(
                resume_state(&once).messages.len(),
                resume_state(&twice).messages.len()
            );
        }
    }

    /// A message record replayed twice in the file is one message.
    #[test]
    fn duplicate_message_records_collapse() {
        let (dir, store) = store();
        let mut writer = SessionWriter::create(store.clone(), &header(dir.path())).expect("create");
        writer.turn_started(1).expect("started");
        let message = Message::user("only once", 1);
        writer.message(&message).expect("first");
        writer.message(&message).expect("replayed");
        writer
            .turn_ended(1, status::OK, None, None, 1)
            .expect("ended");
        assert_eq!(resume_state(&reload(&store, &writer)).messages.len(), 1);
    }

    /// A kill between two appends tears the last line; the file still loads
    /// and the turn it belonged to is dangling (T-SESS-020 + §8.7).
    #[test]
    fn a_torn_tail_still_resumes_with_the_turn_dangling() {
        let (_dir, store, writer) = dangling_session();
        let mut bytes = std::fs::read(writer.path()).expect("read");
        bytes.extend_from_slice(b"{\"type\":\"message\",\"seq\":99,\"tur");
        std::fs::write(writer.path(), bytes).expect("tear");
        let file = store
            .load(writer.path())
            .expect("a torn tail is not corrupt");
        assert!(file.torn_tail);
        assert_eq!(resume_state(&file).dangling.map(|d| d.turn_id), Some(2));
    }

    #[test]
    fn writing_continues_after_the_last_seq() {
        let (_dir, store, writer) = dangling_session();
        let file = reload(&store, &writer);
        let last = file.next_seq() - 1;
        let mut again = SessionWriter::resume(store.clone(), &file);
        again.error("E-PROV-NET", "x").expect("appends");
        let file = reload(&store, &again);
        assert_eq!(file.next_seq(), last + 2);
    }

    #[test]
    fn unavailable_tools_answer_every_call_and_flag_bad_json() {
        let assistant = Message::new(
            Role::Assistant,
            vec![
                Block::Text { text: "t".into() },
                Block::ToolCall {
                    call_id: "a".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                    partial: false,
                    parse_error: None,
                },
                Block::ToolCall {
                    call_id: "b".into(),
                    name: "edit_file".into(),
                    input: serde_json::Value::Null,
                    partial: false,
                    parse_error: Some("EOF".into()),
                },
            ],
            3,
        );
        let answers = unavailable_tool_results(&assistant);
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].role, Role::Tool);
        let Block::ToolResult {
            call_id, is_error, ..
        } = &answers[0].blocks[0]
        else {
            panic!("a tool result")
        };
        assert_eq!((call_id.as_str(), *is_error), ("a", true));
        let bodies: Vec<String> = answers[0]
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::ToolResult { content, .. } => Some(format!("{content:?}")),
                _ => None,
            })
            .collect();
        assert!(bodies[0].contains("E-TOOL-BADSCHEMA"), "{}", bodies[0]);
        assert!(bodies[1].contains("E-TOOL-BADJSON"), "{}", bodies[1]);
        assert!(unavailable_tool_results(&Message::assistant("plain", 1)).is_empty());
    }
}
