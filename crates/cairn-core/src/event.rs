//! Internal event bus payloads (SPEC §3.5).
//!
//! The envelope is exactly:
//! `{"v":1,"seq":<u64>,"ts":"<RFC3339 ms>","session":<id>,"type":"<name>","data":{...}}`

use serde::{Deserialize, Serialize};

/// Envelope schema version (REQ-ARCH-010).
pub const ENVELOPE_VERSION: u16 = 1;

/// RFC3339 with millisecond precision, always `Z` (SPEC §3.5).
#[must_use]
pub fn format_ts_ms(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Current time as an RFC3339 millisecond string.
#[must_use]
pub fn now_ts_ms() -> String {
    format_ts_ms(chrono::Utc::now())
}

/// The event envelope (SPEC §3.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Event {
    /// Envelope schema version, `1` for Cairn 1.x.
    pub v: u16,
    /// Monotonic per-bus sequence number.
    pub seq: u64,
    /// RFC3339 timestamp with milliseconds.
    pub ts: String,
    /// Session the event belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Event name + payload, flattened into the envelope.
    #[serde(flatten)]
    pub data: EventData,
}

impl Event {
    #[must_use]
    pub fn new(data: EventData, seq: u64, session: Option<String>) -> Self {
        Self {
            v: ENVELOPE_VERSION,
            seq,
            ts: now_ts_ms(),
            session,
            data,
        }
    }

    /// Event name (`type` field).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.data.kind()
    }
}

/// All internal events (SPEC §3.5). One variant per documented event name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventData {
    #[serde(rename = "session.created")]
    SessionCreated {
        session_id: String,
        mode: String,
        model: String,
        workspace: String,
    },
    #[serde(rename = "session.resumed")]
    SessionResumed {
        session_id: String,
        from_record: u64,
    },
    #[serde(rename = "turn.started")]
    TurnStarted { turn_id: u64, prompt: String },
    #[serde(rename = "turn.ended")]
    TurnEnded {
        turn_id: u64,
        status: TurnStatus,
        duration_ms: u64,
        cost_usd: f64,
    },
    #[serde(rename = "message.appended")]
    MessageAppended {
        turn_id: u64,
        message: crate::message::Message,
    },
    #[serde(rename = "model.request")]
    ModelRequest {
        turn_id: u64,
        provider: String,
        model: String,
        estimated_input_tokens: u32,
        cache_hit_tokens: u32,
    },
    #[serde(rename = "model.delta")]
    ModelDelta { turn_id: u64, text: String },
    #[serde(rename = "model.reasoning")]
    ModelReasoning { turn_id: u64, text: String },
    #[serde(rename = "model.usage")]
    ModelUsage {
        turn_id: u64,
        input: u32,
        output: u32,
        cache_read: u32,
        cache_write: u32,
        cost_usd: f64,
    },
    #[serde(rename = "model.error")]
    ModelError {
        turn_id: u64,
        code: String,
        http_status: Option<u16>,
        retryable: bool,
        attempt: u8,
    },
    #[serde(rename = "tool.started")]
    ToolStarted {
        call_id: String,
        name: String,
        input: serde_json::Value,
        parallel_index: u32,
    },
    #[serde(rename = "tool.progress")]
    ToolProgress {
        call_id: String,
        bytes_read: u32,
        lines: u32,
        truncated: bool,
        preview: String,
    },
    #[serde(rename = "tool.finished")]
    ToolFinished {
        call_id: String,
        name: String,
        status: ToolStatus,
        duration_ms: u64,
        output_bytes: u32,
        truncated: bool,
        error: Option<String>,
    },
    #[serde(rename = "approval.requested")]
    ApprovalRequested {
        request_id: String,
        call_id: String,
        kind: String,
        summary: String,
        detail: serde_json::Value,
        expires_in_ms: u32,
    },
    #[serde(rename = "approval.answered")]
    ApprovalAnswered {
        request_id: String,
        answer: String,
        rule: Option<String>,
    },
    #[serde(rename = "permission.denied")]
    PermissionDenied {
        call_id: String,
        rule_id: String,
        reason: String,
    },
    #[serde(rename = "mode.changed")]
    ModeChanged {
        from: String,
        to: String,
        trigger: String,
        in_flight: String,
    },
    #[serde(rename = "plan.created")]
    PlanCreated {
        plan_id: String,
        path: String,
        steps: u32,
    },
    #[serde(rename = "plan.approved")]
    PlanApproved { plan_id: String, edits: u32 },
    #[serde(rename = "plan.step")]
    PlanStep {
        plan_id: String,
        step: u32,
        status: String,
    },
    #[serde(rename = "plan.deviation")]
    PlanDeviation {
        plan_id: String,
        step: u32,
        kind: String,
        detail: String,
    },
    #[serde(rename = "guardrail.trip")]
    GuardrailTrip {
        rule: String,
        limit: serde_json::Value,
        actual: serde_json::Value,
    },
    #[serde(rename = "checkpoint.created")]
    CheckpointCreated {
        checkpoint_id: String,
        r#ref: String,
        bytes: u64,
        files: u32,
    },
    #[serde(rename = "checkpoint.restored")]
    CheckpointRestored {
        checkpoint_id: String,
        policy: String,
        files: u32,
    },
    #[serde(rename = "compaction.performed")]
    CompactionPerformed {
        before_tokens: u32,
        after_tokens: u32,
        messages_dropped: u32,
        messages_summarized: u32,
        summary_tokens: u32,
    },
    #[serde(rename = "subagent.started")]
    SubagentStarted {
        child_session: String,
        depth: u8,
        tools: Vec<String>,
    },
    #[serde(rename = "subagent.finished")]
    SubagentFinished {
        child_session: String,
        status: String,
        tokens: u32,
    },
    #[serde(rename = "job.started")]
    JobStarted {
        job_id: String,
        label: String,
        pid: u32,
    },
    #[serde(rename = "job.output")]
    JobOutput {
        job_id: String,
        lines: u32,
        bytes: u32,
    },
    #[serde(rename = "job.finished")]
    JobFinished {
        job_id: String,
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    #[serde(rename = "error")]
    Error {
        code: String,
        message: String,
        recoverable: bool,
        hint: String,
    },
    #[serde(rename = "usage.totals")]
    UsageTotals {
        turn_id: u64,
        session_cost_usd: f64,
        session_tokens: u64,
    },
}

/// `turn.ended` status values (SPEC §3.5, §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Ok,
    Error,
    Cancelled,
    Guardrail,
    Denied,
}

/// `tool.finished` status values (SPEC §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Denied,
    Timeout,
    Cancelled,
}

impl EventData {
    /// The `type` field (SPEC §3.5).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SessionCreated { .. } => "session.created",
            Self::SessionResumed { .. } => "session.resumed",
            Self::TurnStarted { .. } => "turn.started",
            Self::TurnEnded { .. } => "turn.ended",
            Self::MessageAppended { .. } => "message.appended",
            Self::ModelRequest { .. } => "model.request",
            Self::ModelDelta { .. } => "model.delta",
            Self::ModelReasoning { .. } => "model.reasoning",
            Self::ModelUsage { .. } => "model.usage",
            Self::ModelError { .. } => "model.error",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolProgress { .. } => "tool.progress",
            Self::ToolFinished { .. } => "tool.finished",
            Self::ApprovalRequested { .. } => "approval.requested",
            Self::ApprovalAnswered { .. } => "approval.answered",
            Self::PermissionDenied { .. } => "permission.denied",
            Self::ModeChanged { .. } => "mode.changed",
            Self::PlanCreated { .. } => "plan.created",
            Self::PlanApproved { .. } => "plan.approved",
            Self::PlanStep { .. } => "plan.step",
            Self::PlanDeviation { .. } => "plan.deviation",
            Self::GuardrailTrip { .. } => "guardrail.trip",
            Self::CheckpointCreated { .. } => "checkpoint.created",
            Self::CheckpointRestored { .. } => "checkpoint.restored",
            Self::CompactionPerformed { .. } => "compaction.performed",
            Self::SubagentStarted { .. } => "subagent.started",
            Self::SubagentFinished { .. } => "subagent.finished",
            Self::JobStarted { .. } => "job.started",
            Self::JobOutput { .. } => "job.output",
            Self::JobFinished { .. } => "job.finished",
            Self::Error { .. } => "error",
            Self::UsageTotals { .. } => "usage.totals",
        }
    }

    /// Whether the event MUST survive bus backpressure (REQ-ARCH-005).
    ///
    /// `tool.progress` is the **only** droppable event (it is idempotent and
    /// re-sent at ≤ 10 Hz); every other event MUST be delivered.
    #[must_use]
    pub fn is_critical(&self) -> bool {
        !matches!(self, Self::ToolProgress { .. })
    }

    /// Complete list of event names (used by schema drift tests, REQ-ARCH-009).
    pub const ALL_KINDS: &'static [&'static str] = &[
        "session.created",
        "session.resumed",
        "turn.started",
        "turn.ended",
        "message.appended",
        "model.request",
        "model.delta",
        "model.reasoning",
        "model.usage",
        "model.error",
        "tool.started",
        "tool.progress",
        "tool.finished",
        "approval.requested",
        "approval.answered",
        "permission.denied",
        "mode.changed",
        "plan.created",
        "plan.approved",
        "plan.step",
        "plan.deviation",
        "guardrail.trip",
        "checkpoint.created",
        "checkpoint.restored",
        "compaction.performed",
        "subagent.started",
        "subagent.finished",
        "job.started",
        "job.output",
        "job.finished",
        "error",
        "usage.totals",
    ];
}

/// Generate the JSON Schemas of the event envelope and of every event kind
/// (REQ-ARCH-009, T-SCHEMA-002).
///
/// Returns `(filename, contents)` pairs sorted by filename:
///
/// * `event.schema.json` — the envelope with a `oneOf` over all variants, and
/// * `<kind>.schema.json` — the same envelope narrowed to one event kind, so a
///   consumer can validate a single record against the event it claims to be.
///
/// The committed set under `schemas/events/` must match this exactly; the
/// generator is `cargo run -p cairn-core --example dump_event_schemas`.
#[must_use]
pub fn event_schemas() -> Vec<(String, String)> {
    let root = schemars::schema_for!(Event);
    let envelope = serde_json::to_value(&root).expect("Event's schema serializes");
    let variants: Vec<serde_json::Value> = envelope
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let render = |v: &serde_json::Value| {
        let mut text = serde_json::to_string_pretty(v).expect("schema renders");
        text.push('\n');
        text
    };

    let mut out = Vec::with_capacity(variants.len() + 1);
    out.push(("event.schema.json".to_string(), render(&envelope)));

    for variant in &variants {
        let kind = variant
            .pointer("/properties/type/enum/0")
            .and_then(serde_json::Value::as_str)
            .expect("every variant names its own type");
        let mut single = envelope.clone();
        single["oneOf"] = serde_json::json!([variant]);
        single["title"] = serde_json::json!(kind);
        single["description"] = serde_json::json!(format!(
            "One Cairn envelope event, narrowed to `{kind}` (SPEC §3.5)."
        ));
        out.push((format!("{kind}.schema.json"), render(&single)));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_all_kinds() {
        let mut kinds: Vec<&'static str> = EventData::ALL_KINDS.to_vec();
        kinds.sort_unstable();
        let mut unique = kinds.clone();
        unique.dedup();
        assert_eq!(
            kinds.len(),
            unique.len(),
            "duplicate event name in ALL_KINDS"
        );
        assert_eq!(kinds.len(), 32, "spec §3.5 documents 32 events");
    }

    #[test]
    fn envelope_shape_matches_spec() {
        let ev = Event::new(
            EventData::TurnEnded {
                turn_id: 4,
                status: TurnStatus::Ok,
                duration_ms: 1200,
                cost_usd: 0.0214,
            },
            7,
            Some("01J8X".to_string()),
        );
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["seq"], 7);
        assert!(v["ts"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["session"], "01J8X");
        assert_eq!(v["type"], "turn.ended");
        assert_eq!(v["data"]["status"], "ok");
        assert_eq!(v["data"]["cost_usd"], 0.0214);
        // Round-trip (guards the flatten + adjacently-tagged combination).
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn ts_is_rfc3339_millis() {
        let ts = now_ts_ms();
        // 2026-10-03T09:12:04.123Z
        assert!(ts.ends_with('Z'), "{ts}");
        let frac = ts.split('.').nth(1).expect("millisecond fraction");
        assert_eq!(
            frac.len(),
            4,
            "expects exactly 3 fractional digits + Z: {ts}"
        );
        chrono::DateTime::parse_from_rfc3339(&ts).expect("rfc3339 parse");
    }

    #[test]
    fn critical_events_are_never_droppable() {
        // REQ-ARCH-005: tool.progress is the only droppable event.
        assert!(!EventData::ToolProgress {
            call_id: "c1".into(),
            bytes_read: 1,
            lines: 1,
            truncated: false,
            preview: String::new(),
        }
        .is_critical());
        assert!(EventData::ModelDelta {
            turn_id: 1,
            text: "x".into()
        }
        .is_critical());
        assert!(EventData::ModelRequest {
            turn_id: 1,
            provider: "p".into(),
            model: "m".into(),
            estimated_input_tokens: 1,
            cache_hit_tokens: 0,
        }
        .is_critical());
        assert!(EventData::Error {
            code: "E-X-Y".into(),
            message: "m".into(),
            recoverable: false,
            hint: String::new(),
        }
        .is_critical());
        assert!(EventData::ToolFinished {
            call_id: "c1".into(),
            name: "bash".into(),
            status: ToolStatus::Ok,
            duration_ms: 1,
            output_bytes: 0,
            truncated: false,
            error: None,
        }
        .is_critical());
    }

    #[test]
    fn every_variant_serializes_with_its_spec_name() {
        let samples = vec![
            EventData::SessionCreated {
                session_id: "s".into(),
                mode: "build".into(),
                model: "m".into(),
                workspace: "/w".into(),
            },
            EventData::SessionResumed {
                session_id: "s".into(),
                from_record: 3,
            },
            EventData::TurnStarted {
                turn_id: 1,
                prompt: "p".into(),
            },
            EventData::TurnEnded {
                turn_id: 1,
                status: crate::event::TurnStatus::Ok,
                duration_ms: 10,
                cost_usd: 0.01,
            },
            EventData::MessageAppended {
                turn_id: 1,
                message: crate::message::Message::assistant("hi", 1),
            },
            EventData::ModelRequest {
                turn_id: 1,
                provider: "anthropic".into(),
                model: "m".into(),
                estimated_input_tokens: 10,
                cache_hit_tokens: 0,
            },
            EventData::ModelDelta {
                turn_id: 1,
                text: "t".into(),
            },
            EventData::ModelReasoning {
                turn_id: 1,
                text: "t".into(),
            },
            EventData::ModelUsage {
                turn_id: 1,
                input: 1,
                output: 2,
                cache_read: 0,
                cache_write: 0,
                cost_usd: 0.0,
            },
            EventData::ModelError {
                turn_id: 1,
                code: "E-PROV-SERVER".into(),
                http_status: Some(500),
                retryable: true,
                attempt: 1,
            },
            EventData::ToolStarted {
                call_id: "c".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"true"}),
                parallel_index: 0,
            },
            EventData::ToolProgress {
                call_id: "c".into(),
                bytes_read: 0,
                lines: 0,
                truncated: false,
                preview: String::new(),
            },
            EventData::ToolFinished {
                call_id: "c".into(),
                name: "bash".into(),
                status: ToolStatus::Ok,
                duration_ms: 1,
                output_bytes: 0,
                truncated: false,
                error: None,
            },
            EventData::ApprovalRequested {
                request_id: "r".into(),
                call_id: "c".into(),
                kind: "tool".into(),
                summary: "s".into(),
                detail: serde_json::json!({}),
                expires_in_ms: 1000,
            },
            EventData::ApprovalAnswered {
                request_id: "r".into(),
                answer: "once".into(),
                rule: None,
            },
            EventData::PermissionDenied {
                call_id: "c".into(),
                rule_id: "r1".into(),
                reason: "deny".into(),
            },
            EventData::ModeChanged {
                from: "plan".into(),
                to: "build".into(),
                trigger: "key".into(),
                in_flight: "drained".into(),
            },
            EventData::PlanCreated {
                plan_id: "p".into(),
                path: "/x".into(),
                steps: 3,
            },
            EventData::PlanApproved {
                plan_id: "p".into(),
                edits: 0,
            },
            EventData::PlanStep {
                plan_id: "p".into(),
                step: 1,
                status: "done".into(),
            },
            EventData::PlanDeviation {
                plan_id: "p".into(),
                step: 1,
                kind: "extra_file".into(),
                detail: "d".into(),
            },
            EventData::GuardrailTrip {
                rule: "max_iterations".into(),
                limit: serde_json::json!(40),
                actual: serde_json::json!(41),
            },
            EventData::CheckpointCreated {
                checkpoint_id: "ck".into(),
                r#ref: "refs/cairn/checkpoints/s/1".into(),
                bytes: 10,
                files: 2,
            },
            EventData::CheckpointRestored {
                checkpoint_id: "ck".into(),
                policy: "Full".into(),
                files: 2,
            },
            EventData::CompactionPerformed {
                before_tokens: 100,
                after_tokens: 30,
                messages_dropped: 5,
                messages_summarized: 20,
                summary_tokens: 25,
            },
            EventData::SubagentStarted {
                child_session: "c".into(),
                depth: 1,
                tools: vec!["read_file".into()],
            },
            EventData::SubagentFinished {
                child_session: "c".into(),
                status: "ok".into(),
                tokens: 10,
            },
            EventData::JobStarted {
                job_id: "j".into(),
                label: "l".into(),
                pid: 1,
            },
            EventData::JobOutput {
                job_id: "j".into(),
                lines: 1,
                bytes: 2,
            },
            EventData::JobFinished {
                job_id: "j".into(),
                exit_code: Some(0),
                duration_ms: 5,
            },
            EventData::Error {
                code: "E-X-Y".into(),
                message: "m".into(),
                recoverable: true,
                hint: "h".into(),
            },
            EventData::UsageTotals {
                turn_id: 1,
                session_cost_usd: 0.1,
                session_tokens: 100,
            },
        ];
        let mut seen = std::collections::BTreeSet::new();
        for data in samples {
            let ev = Event::new(data.clone(), 1, None);
            let json = serde_json::to_value(&ev).unwrap();
            let name = json["type"].as_str().unwrap().to_string();
            assert!(
                EventData::ALL_KINDS.contains(&name.as_str()),
                "unexpected event name {name}"
            );
            assert!(seen.insert(name.clone()), "duplicate sample for {name}");
            let back: Event = serde_json::from_value(json).unwrap();
            assert_eq!(back.data, data, "round-trip failed for {name}");
        }
        assert_eq!(
            seen.len(),
            EventData::ALL_KINDS.len(),
            "every kind needs a sample"
        );
    }

    #[test]
    fn json_schema_generates() {
        let schema = schemars::schema_for!(Event);
        let s = serde_json::to_string(&schema).unwrap();
        assert!(s.contains("turn.ended"));
        assert!(s.len() > 500);
    }

    /// T-SCHEMA-002 — one schema per event kind plus the envelope, each a
    /// valid draft-07 document that carries the envelope's own fields
    /// (REQ-ARCH-009).
    #[test]
    fn event_schemas_cover_every_kind() {
        let schemas = event_schemas();
        assert_eq!(schemas.len(), EventData::ALL_KINDS.len() + 1);

        let names: Vec<&str> = schemas.iter().map(|(n, _)| n.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "sorted for a stable diff in CI");
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "filenames must be unique");

        for (name, body) in &schemas {
            let v: serde_json::Value =
                serde_json::from_str(body).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                v["$schema"], "http://json-schema.org/draft-07/schema#",
                "{name}"
            );
            assert_eq!(v["type"], "object", "{name}");
            for field in ["v", "seq", "ts", "session"] {
                assert!(v["properties"][field].is_object(), "{name} lost `{field}`");
            }
            assert!(v["required"].as_array().unwrap().len() >= 3, "{name}");
        }

        let envelope = &schemas
            .iter()
            .find(|(n, _)| n == "event.schema.json")
            .expect("envelope schema")
            .1;
        let v: serde_json::Value = serde_json::from_str(envelope).unwrap();
        assert_eq!(
            v["oneOf"].as_array().unwrap().len(),
            EventData::ALL_KINDS.len(),
            "the envelope covers every variant"
        );

        for kind in EventData::ALL_KINDS {
            let file = format!("{kind}.schema.json");
            let (_, body) = schemas
                .iter()
                .find(|(n, _)| *n == file)
                .unwrap_or_else(|| panic!("no schema generated for `{kind}`"));
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(v["title"], *kind, "{file}");
            assert_eq!(v["oneOf"].as_array().unwrap().len(), 1, "{file}");
            assert_eq!(
                v["oneOf"][0]["properties"]["type"]["enum"][0], *kind,
                "{file}"
            );
        }
    }
}
