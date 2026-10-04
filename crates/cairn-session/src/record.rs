//! Session record types (SPEC §11.7).
//!
//! A session file is JSON Lines: one JSON object per line, first line a
//! mandatory `header`. Every record carries the four *common fields* (`v`,
//! `type`, `seq`, `ts`); everything else is data.
//!
//! Unknown record types are first-class: [`Record::body`] keeps every field a
//! build does not recognise (and, with `serde_json`'s `preserve_order`, their
//! order too), so a file written by a newer Cairn survives being read and
//! rewritten by this one without losing a byte of meaning (REQ-CLI-009,
//! T-SESS-012).

use cairn_core::error::{codes, CairnError};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// `v` stamped on records this build writes (SPEC §11.7 common fields).
pub const RECORD_VERSION: u32 = 1;

/// `header.schema_version` this build writes (SPEC §11.7).
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The record discriminators defined by §11.7. Anything else is a record from
/// the future (or from a plugin) and is preserved, never interpreted.
pub mod kind {
    /// First line of every session file.
    pub const HEADER: &str = "header";
    /// A §4.1 `Message`.
    pub const MESSAGE: &str = "message";
    /// Result of one tool call.
    pub const TOOL_RESULT: &str = "tool_result";
    /// Turn lifecycle.
    pub const TURN_STARTED: &str = "turn_started";
    /// Turn lifecycle.
    pub const TURN_ENDED: &str = "turn_ended";
    /// Operating mode changed.
    pub const MODE_CHANGED: &str = "mode_changed";
    /// Checkpoint created.
    pub const CHECKPOINT: &str = "checkpoint";
    /// Context compaction happened.
    pub const COMPACTION: &str = "compaction";
    /// Plan artifact event.
    pub const PLAN_EVENT: &str = "plan_event";
    /// Guardrail limit hit.
    pub const GUARDRAIL: &str = "guardrail";
    /// A failed step carrying a stable `E-*`/`W-*` code.
    pub const ERROR: &str = "error";
    /// Last line of a deleted session.
    pub const TOMBSTONE: &str = "tombstone";

    /// `true` if this build knows how to read `kind`.
    #[must_use]
    pub fn is_known(kind: &str) -> bool {
        matches!(
            kind,
            HEADER
                | MESSAGE
                | TOOL_RESULT
                | TURN_STARTED
                | TURN_ENDED
                | MODE_CHANGED
                | CHECKPOINT
                | COMPACTION
                | PLAN_EVENT
                | GUARDRAIL
                | ERROR
                | TOMBSTONE
        )
    }
}

fn now_ms() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn record_version() -> u32 {
    RECORD_VERSION
}

/// One JSON object from a `.jsonl` session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Record schema version (common field).
    #[serde(default = "record_version")]
    pub v: u32,
    /// Discriminator (the spec's `type` key).
    #[serde(rename = "type")]
    pub kind: String,
    /// Monotonic per file (common field); `None` on a header or on a record
    /// written before sequence numbering was understood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// RFC3339 with milliseconds (common field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    /// Every other field, retained verbatim.
    #[serde(flatten)]
    pub body: serde_json::Map<String, serde_json::Value>,
}

impl Record {
    /// A record of `kind` with the given body, timestamped now.
    #[must_use]
    pub fn new(kind: &str, seq: Option<u64>, body: serde_json::Value) -> Self {
        let body = match body {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        Self {
            v: RECORD_VERSION,
            kind: kind.to_string(),
            seq,
            ts: Some(now_ms()),
            body,
        }
    }

    /// The `header` record for a session (SPEC §11.7).
    #[must_use]
    pub fn from_header(header: &Header) -> Self {
        let value = serde_json::to_value(header).unwrap_or_default();
        let mut rec = Self::new(kind::HEADER, None, value);
        rec.ts = None;
        rec
    }

    /// Read this record as a `header`. Fails with `E-SESS-CORRUPT` when the
    /// mandatory §11.7 fields are missing or of the wrong type.
    pub fn as_header(&self) -> Result<Header, CairnError> {
        if self.kind != kind::HEADER {
            return Err(corrupt(format!(
                "expected a `header` record, found `{}`",
                self.kind
            )));
        }
        header_from_body(&self.body)
    }

    /// Read the §4.1 `Message` out of a `message` record.
    ///
    /// The payload lives under `message`, not flattened: §11.7 lists this
    /// record's own fields as `seq`, `turn_id`, `message` — the message is a
    /// value in the record, not the record itself.
    pub fn as_message(&self) -> Result<cairn_core::Message, CairnError> {
        if self.kind != kind::MESSAGE {
            return Err(corrupt(format!(
                "expected a `message` record, found `{}`",
                self.kind
            )));
        }
        let Some(inner) = self.field("message") else {
            return Err(corrupt(
                "`message` record has no `message` field (SPEC §11.7)",
            ));
        };
        serde_json::from_value(inner.clone())
            .map_err(|e| corrupt(format!("`message` record is malformed: {e}")))
    }

    /// A `message` record holding one of the session's `Message`s (SPEC §11.7).
    #[must_use]
    pub fn message(message: &cairn_core::Message, seq: u64) -> Self {
        let value = serde_json::to_value(message).unwrap_or_default();
        Self::new(
            kind::MESSAGE,
            Some(seq),
            serde_json::json!({
                "turn_id": message.turn_id,
                "message": value,
            }),
        )
    }

    /// A `turn_started` record.
    #[must_use]
    pub fn turn_started(turn_id: u64, seq: u64) -> Self {
        Self::new(
            kind::TURN_STARTED,
            Some(seq),
            serde_json::json!({ "turn_id": turn_id }),
        )
    }

    /// A `turn_ended` record (SPEC §11.7).
    #[must_use]
    pub fn turn_ended(
        turn_id: u64,
        seq: u64,
        status: &str,
        usage: Option<cairn_core::Usage>,
        cost_usd: Option<f64>,
        duration_ms: u64,
    ) -> Self {
        Self::new(
            kind::TURN_ENDED,
            Some(seq),
            serde_json::json!({
                "turn_id": turn_id,
                "status": status,
                "usage": usage,
                "cost_usd": cost_usd,
                "duration_ms": duration_ms,
            }),
        )
    }

    /// A `tool_result` record (SPEC §11.7).
    ///
    /// One parameter per field of the §11.7 record: collapsing them into a
    /// builder would hide the correspondence with the spec, which is the only
    /// place these names are defined.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the SPEC 11.7 tool_result record field for field"
    )]
    pub fn tool_result(
        turn_id: u64,
        seq: u64,
        call_id: &str,
        name: &str,
        ok: bool,
        output: &str,
        duration_ms: u64,
        truncated: bool,
    ) -> Self {
        Self::new(
            kind::TOOL_RESULT,
            Some(seq),
            serde_json::json!({
                "turn_id": turn_id,
                "call_id": call_id,
                "name": name,
                "ok": ok,
                "output": output,
                "duration_ms": duration_ms,
                "truncated": truncated,
            }),
        )
    }

    /// A `mode_changed` record.
    #[must_use]
    pub fn mode_changed(seq: u64, from: &str, to: &str, trigger: &str) -> Self {
        Self::new(
            kind::MODE_CHANGED,
            Some(seq),
            serde_json::json!({ "from": from, "to": to, "trigger": trigger }),
        )
    }

    /// An `error` record carrying a stable code (SPEC §11.7).
    #[must_use]
    pub fn error(seq: u64, code: &str, message: &str) -> Self {
        Self::new(
            kind::ERROR,
            Some(seq),
            serde_json::json!({ "code": code, "message": message }),
        )
    }

    /// The `tombstone` that makes a session ineligible for resume before the
    /// file is unlinked (SPEC §11.7 retention).
    #[must_use]
    pub fn tombstone(reason: &str) -> Self {
        let mut rec = Self::new(
            kind::TOMBSTONE,
            None,
            serde_json::json!({ "reason": reason }),
        );
        rec.body.insert("deleted_at".into(), now_ms().into());
        rec
    }

    /// One field of the record body.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&serde_json::Value> {
        self.body.get(key)
    }

    /// The body as a JSON object (the fields outside the common four).
    #[must_use]
    pub fn body_value(&self) -> serde_json::Value {
        serde_json::Value::Object(self.body.clone())
    }
}

/// Why a file that exists cannot be loaded as a Cairn session (SPEC §11.7
/// read contract).
#[must_use]
pub fn corrupt(message: impl Into<String>) -> CairnError {
    CairnError::new(codes::SESS_CORRUPT, message).with_recovery(
        "the file is not a loadable Cairn session; restore it from a backup or move it aside",
    )
}

/// The `header` record's fields (SPEC §11.7).
///
/// `ruleset_version`, `parent_session` and `plan_id` are optional; everything
/// else is mandatory, and a missing `schema_version` reads as `0` so that a
/// pre-versioned file is *migrated* rather than silently trusted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// `0` when the file predates versioned session records.
    #[serde(default)]
    pub schema_version: u32,
    pub session_id: String,
    pub created_at: String,
    /// Absolute workspace root this session belongs to.
    pub workspace: String,
    pub mode: String,
    pub model: String,
    pub cairn_version: String,
    /// §11.7 marks `ruleset_version` and `parent_session` mandatory — only
    /// `plan_id` carries the `?` — so they are always *present*, with `null`
    /// how a first session says it has no parent. They are written on every
    /// header: a rewrite that dropped them would make the next read corrupt.
    pub ruleset_version: Option<String>,
    pub parent_session: Option<String>,
    /// Pointer only: `.cairn/plans/<plan_id>.json` is authoritative (§7.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
}

impl Header {
    /// A header for a brand-new session at the current schema version.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        workspace: impl Into<String>,
        mode: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: session_id.into(),
            created_at: now_ms(),
            workspace: workspace.into(),
            mode: mode.into(),
            model: model.into(),
            cairn_version: env!("CARGO_PKG_VERSION").to_string(),
            ruleset_version: None,
            parent_session: None,
            plan_id: None,
        }
    }

    /// The header as a `header` record.
    #[must_use]
    pub fn to_record(&self) -> Record {
        Record::from_header(self)
    }

    /// Compact JSON used by `--grep` (T-SESS: substring over the header).
    #[must_use]
    pub fn search_blob(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

fn header_from_body(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<Header, CairnError> {
    let mut obj = body.clone();
    // `type` and the other common fields are not part of `Header`, but a
    // hand-written file may carry `seq`/`ts` on the header; both are ignored.
    for key in ["type", "seq", "ts", "v"] {
        obj.remove(key);
    }
    serde_json::from_value(serde_json::Value::Object(obj))
        .map_err(|e| corrupt(format!("`header` record is malformed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header::new("ses_1", "/ws", "build", "anthropic/claude-sonnet-4-5")
    }

    #[test]
    fn header_round_trips_through_a_record() {
        let mut h = header();
        h.plan_id = Some("2026-10-03T09-12-04Z-goal".into());
        let rec = h.to_record();
        assert_eq!(rec.kind, "header");
        assert_eq!(rec.as_header().unwrap(), h);
        assert!(rec.seq.is_none());
        assert!(rec.ts.is_none());
    }

    #[test]
    fn record_json_is_tagged_and_carries_the_common_fields() {
        let rec = Record::error(7, "E-SESS-NOTFOUND", "gone");
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["v"], 1);
        assert_eq!(v["seq"], 7);
        assert!(v["ts"].is_string());
        let back: Record = serde_json::from_value(v).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        // A record type (and fields) this build has never heard of.
        let line = r#"{"v":3,"type":"from_the_future","seq":9,"a":1,"b":[true,null]}"#;
        let rec: Record = serde_json::from_str(line).unwrap();
        assert!(!kind::is_known(&rec.kind));
        assert_eq!(rec.field("a"), Some(&serde_json::json!(1)));
        let out = serde_json::to_string(&rec).unwrap();
        assert!(out.contains(r#""type":"from_the_future""#), "{out}");
        assert!(out.contains(r#""b":[true,null]"#), "{out}");
    }

    #[test]
    fn a_missing_schema_version_reads_as_zero_needing_migration() {
        let rec: Record = serde_json::from_str(
            r#"{"v":1,"type":"header","session_id":"s","created_at":"t","workspace":"/w",
                "mode":"build","model":"m","cairn_version":"0.1.0",
                "ruleset_version":null,"parent_session":null}"#,
        )
        .unwrap();
        assert_eq!(rec.as_header().unwrap().schema_version, 0);
    }

    #[test]
    fn a_header_without_required_fields_is_corrupt() {
        let rec: Record = serde_json::from_str(r#"{"v":1,"type":"header"}"#).unwrap();
        let err = rec.as_header().unwrap_err();
        assert_eq!(err.code, codes::SESS_CORRUPT);
        assert!(err.message.contains("malformed"), "{}", err.message);
    }

    #[test]
    fn a_non_header_record_cannot_be_read_as_one() {
        let rec = Record::new(kind::MESSAGE, Some(1), serde_json::json!({}));
        assert_eq!(rec.as_header().unwrap_err().code, codes::SESS_CORRUPT);
    }

    #[test]
    fn message_records_carry_the_whole_message() {
        let m = cairn_core::Message::user("hello", 1);
        let rec = Record::message(&m, 3);
        assert_eq!(rec.seq, Some(3));
        assert_eq!(rec.as_message().unwrap(), m);
    }

    #[test]
    fn every_spec_record_kind_is_known() {
        for k in [
            "header",
            "message",
            "tool_result",
            "turn_started",
            "turn_ended",
            "mode_changed",
            "checkpoint",
            "compaction",
            "plan_event",
            "guardrail",
            "error",
            "tombstone",
        ] {
            assert!(kind::is_known(k), "{k}");
        }
        assert!(!kind::is_known("telepathy"));
    }

    #[test]
    fn tombstone_records_the_deletion() {
        let t = Record::tombstone("retention");
        assert_eq!(t.kind, kind::TOMBSTONE);
        assert!(t.field("deleted_at").unwrap().is_string());
        assert_eq!(t.field("reason"), Some(&serde_json::json!("retention")));
    }
}
