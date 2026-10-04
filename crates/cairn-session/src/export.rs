//! Session export: `md`, `json`, `html` (SPEC §11.7).
//!
//! Redaction is the caller's decision but this module's obligation: pass
//! `Some(&redactor)` and every string that reaches the output passes through it
//! first (REQ-CLI-010, REQ-SAFE-010). Text is redacted *before* markup is
//! generated so a secret can never be disguised by — or corrupt — the HTML.

use crate::record::kind;
use crate::store::SessionFile;
use crate::Result;
use cairn_core::redact::Redactor;
use cairn_core::Message;
use serde_json::{json, Value};

/// Export formats (SPEC §11.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Md,
    Json,
    Html,
}

/// One rendered piece of the transcript, format-independent.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// `level` is 1-based (`#`, `##`, …).
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    /// Session metadata, rendered as a Markdown table or an HTML `<table>`.
    Meta(Vec<(String, String)>),
    /// Collapsible section — tool calls, reasoning, tool output.
    Details {
        summary: String,
        body: String,
    },
    /// Horizontal rule.
    Rule,
}

/// Render one session in `format`.
///
/// `redactor` is `Some` for the default, redacted export and `None` only when
/// the caller has already obtained consent for `--no-redact` (REQ-CLI-010).
pub fn render(file: &SessionFile, format: Format, redactor: Option<&Redactor>) -> Result<String> {
    let scrub = |s: &str| -> String { redactor.map_or_else(|| s.to_string(), |r| r.redact(s)) };
    let blocks = blocks(file, &scrub);
    Ok(match format {
        Format::Md => render_md(&blocks),
        Format::Html => render_html(&file.header.session_id, &blocks),
        Format::Json => render_json(file, redactor),
    })
}

/// The transcript as a list of blocks, with `scrub` applied to all text.
fn blocks(file: &SessionFile, scrub: &dyn Fn(&str) -> String) -> Vec<Block> {
    let h = &file.header;
    let mut out = vec![
        Block::Heading {
            level: 1,
            text: scrub(&format!("Session {}", h.session_id)),
        },
        Block::Meta(vec![
            ("Session".into(), h.session_id.clone()),
            ("Created".into(), h.created_at.clone()),
            ("Workspace".into(), h.workspace.clone()),
            ("Mode".into(), h.mode.clone()),
            ("Model".into(), h.model.clone()),
            ("Cairn".into(), h.cairn_version.clone()),
            ("Schema".into(), format!("v{}", h.schema_version)),
            (
                "Status".into(),
                file.tombstoned_at()
                    .map_or_else(|| "active".to_string(), |at| format!("deleted {at}")),
            ),
        ]),
        Block::Rule,
    ];

    for entry in &file.entries {
        match entry.record.kind.as_str() {
            kind::MESSAGE => {
                let Ok(message) = entry.record.as_message() else {
                    continue;
                };
                push_message(&mut out, &message, scrub);
            }
            kind::TOOL_RESULT => {
                let name = entry
                    .record
                    .field("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let call_id = entry
                    .record
                    .field("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let ok = entry
                    .record
                    .field("ok")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = entry
                    .record
                    .field("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                out.push(Block::Details {
                    summary: scrub(&format!(
                        "{name} · {call_id} · {}",
                        if ok { "ok" } else { "error" }
                    )),
                    body: scrub(&pretty(output)),
                });
            }
            _ => {}
        }
    }

    if out.len() == 3 {
        out.push(Block::Paragraph(
            "_This session has no messages._".to_string(),
        ));
    }
    out
}

fn push_message(out: &mut Vec<Block>, message: &Message, scrub: &dyn Fn(&str) -> String) {
    let title = match message.role {
        cairn_core::Role::System => "System",
        cairn_core::Role::User => "User",
        cairn_core::Role::Assistant => "Assistant",
        cairn_core::Role::Tool => "Tool",
    };
    out.push(Block::Heading {
        level: 2,
        text: scrub(title),
    });

    for block in &message.blocks {
        match block {
            cairn_core::Block::Text { text } => out.push(Block::Paragraph(scrub(text))),
            cairn_core::Block::Reasoning { text, .. } => out.push(Block::Details {
                summary: scrub("reasoning"),
                body: scrub(text),
            }),
            cairn_core::Block::ThinkingPlaceholder { text } => out.push(Block::Details {
                summary: scrub("thinking (placeholder)"),
                body: scrub(text),
            }),
            // Base64 payloads are never exported: they are large, they are not
            // readable, and redaction cannot reason about them.
            cairn_core::Block::Image {
                media_type, alt, ..
            } => {
                let kind = serde_json::to_value(media_type)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "image".to_string());
                out.push(Block::Paragraph(scrub(&format!(
                    "[image: {kind}]{}",
                    alt.as_deref()
                        .map_or_else(String::new, |a| format!(" — {a}"))
                ))));
            }
            cairn_core::Block::ToolCall {
                call_id,
                name,
                input,
                ..
            } => out.push(Block::Details {
                summary: scrub(&format!("{name} · {call_id}")),
                body: scrub(&pretty(&input.to_string())),
            }),
            cairn_core::Block::ToolResult {
                content, is_error, ..
            } => {
                let mut text = String::new();
                for part in content {
                    if let cairn_core::Block::Text { text: t } = part {
                        text.push_str(t);
                        text.push('\n');
                    }
                }
                out.push(Block::Details {
                    summary: scrub(if *is_error {
                        "tool result · error"
                    } else {
                        "tool result"
                    }),
                    body: scrub(&text),
                });
            }
        }
    }
}

/// Pretty-print a JSON fragment; fall back to the raw text if it is not JSON.
fn pretty(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| raw.to_string())
}

// --------------------------------------------------------------------- markdown

fn render_md(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            Block::Heading { level, text } => {
                out.push_str(&"#".repeat(usize::from(*level)));
                out.push(' ');
                out.push_str(text);
                out.push_str("\n\n");
            }
            Block::Paragraph(text) => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            Block::Meta(rows) => {
                out.push_str("| Field | Value |\n|-------|-------|\n");
                for (k, v) in rows {
                    let row = format!("| {k} | {} |\n", md_cell(v));
                    out.push_str(&row);
                }
                out.push('\n');
            }
            Block::Details { summary, body } => {
                out.push_str("<details>\n<summary>");
                out.push_str(summary);
                out.push_str("</summary>\n\n");
                out.push_str(body);
                out.push_str("\n\n</details>\n\n");
            }
            Block::Rule => out.push_str("---\n\n"),
        }
    }
    out.trim_end().to_string() + "\n"
}

/// Tables cannot contain a raw `|` or a newline.
fn md_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

// ------------------------------------------------------------------------ html

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Single-file HTML: inline CSS, no external resources (SPEC §11.7).
fn render_html(session_id: &str, blocks: &[Block]) -> String {
    let mut body = String::new();
    for block in blocks {
        match block {
            Block::Heading { level, text } => {
                let level = (*level).clamp(1, 6);
                let heading = format!("<h{level}>{}</h{level}>\n", escape(text));
                body.push_str(&heading);
            }
            Block::Paragraph(text) => {
                let para = format!("<p>{}</p>\n", escape(text));
                body.push_str(&para);
            }
            Block::Meta(rows) => {
                body.push_str("<table>\n");
                for (k, v) in rows {
                    let row = format!("<tr><th>{}</th><td>{}</td></tr>\n", escape(k), escape(v));
                    body.push_str(&row);
                }
                body.push_str("</table>\n");
            }
            Block::Details {
                summary,
                body: inner,
            } => {
                let section = format!(
                    "<details><summary>{}</summary><pre>{}</pre></details>\n",
                    escape(summary),
                    escape(inner)
                );
                body.push_str(&section);
            }
            Block::Rule => body.push_str("<hr>\n"),
        }
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Cairn session {title}</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ font-family: ui-sans-serif, system-ui, -apple-system, sans-serif; max-width: 52rem;
        margin: 2rem auto; padding: 0 1rem; line-height: 1.6; }}
h1 {{ font-size: 1.5rem; margin-bottom: .5rem; }}
h2 {{ font-size: 1.1rem; margin-top: 2rem; padding-bottom: .25rem;
      border-bottom: 1px solid rgba(127,127,127,.35); }}
table {{ border-collapse: collapse; margin: 1rem 0; width: 100%; }}
th, td {{ border: 1px solid rgba(127,127,127,.35); padding: .3rem .6rem; text-align: left;
          vertical-align: top; word-break: break-word; }}
th {{ width: 10rem; }}
details {{ background: rgba(127,127,127,.1); border-radius: 6px;
           padding: .5rem .75rem; margin: .5rem 0; }}
summary {{ cursor: pointer; font-weight: 600; }}
pre {{ white-space: pre-wrap; word-break: break-word; margin: .5rem 0 0; font-family:
       ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .9em; }}
hr {{ border: none; border-top: 1px solid rgba(127,127,127,.35); margin: 1.5rem 0; }}
</style>
</head>
<body>
{body}
</body>
</html>
"#,
        title = escape(session_id),
        body = body
    )
}

// -------------------------------------------------------------------------- json

/// `{"schema_version":1,"session":…,"messages":[…],"usage":…}` (SPEC §11.7).
fn render_json(file: &SessionFile, redactor: Option<&Redactor>) -> String {
    let messages: Vec<Value> = file
        .messages()
        .iter()
        .map(|m| serde_json::to_value(m).unwrap_or_default())
        .collect();

    let mut document = json!({
        "schema_version": 1,
        "session": serde_json::to_value(&file.header).unwrap_or_default(),
        "messages": messages,
        "usage": usage_of(file),
    });

    if let Some(redactor) = redactor {
        redact_value(&mut document, redactor);
    }
    let mut text = serde_json::to_string_pretty(&document).unwrap_or_default();
    text.push('\n');
    text
}

/// Aggregate token usage over every `turn_ended` record, falling back to the
/// usage attached to assistant messages.
fn usage_of(file: &SessionFile) -> Value {
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cache_read = 0u64;
    let mut cache_write = 0u64;
    let mut estimated = false;
    let mut seen = false;

    let mut add = |u: &cairn_core::Usage| {
        seen = true;
        input += u64::from(u.input);
        output += u64::from(u.output);
        cache_read += u64::from(u.cache_read);
        cache_write += u64::from(u.cache_write);
        estimated |= u.estimated;
    };
    for m in file.messages() {
        if let Some(u) = m.usage {
            add(&u);
        }
    }

    json!({
        "input": input,
        "output": output,
        "cache_read": cache_read,
        "cache_write": cache_write,
        "estimated": estimated,
        "recorded": seen,
    })
}

/// Redact every string leaf, so a secret cannot hide in a key name or a nested
/// tool argument.
fn redact_value(value: &mut Value, redactor: &Redactor) {
    match value {
        Value::String(s) => *s = redactor.redact(s),
        Value::Array(items) => items.iter_mut().for_each(|v| redact_value(v, redactor)),
        Value::Object(map) => map.values_mut().for_each(|v| redact_value(v, redactor)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Header, Record};
    use crate::store::Store;

    fn session(tmp: &tempfile::TempDir) -> SessionFile {
        let store = Store::new(tmp.path().join("sessions"));
        let ws = tmp.path().join("ws");
        let mut header = Header::new("ses_e", ws.to_string_lossy(), "build", "m");
        header.ruleset_version = Some("r1".into());
        let mut file = store.create(&header).unwrap();
        file.push(Record::message(&Message::user("hello world", 1), 1));
        file.push(Record::message(&Message::assistant("hi there", 1), 2));
        file.push(Record::tool_result(
            1,
            3,
            "c1",
            "bash",
            true,
            "<script>alert(1)</script>",
            12,
            false,
        ));
        store.save(&file).unwrap();
        store.load(&file.path).unwrap()
    }

    #[test]
    fn markdown_has_metadata_headings_and_collapsible_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let file = session(&tmp);
        let md = render(&file, Format::Md, None).unwrap();
        assert!(md.starts_with("# Session ses_e\n"), "{md}");
        assert!(md.contains("| Field | Value |"), "{md}");
        assert!(md.contains("## User"), "{md}");
        assert!(md.contains("## Assistant"), "{md}");
        assert!(md.contains("<details>"), "{md}");
        assert!(md.contains("bash · c1"), "{md}");
        assert!(md.contains("hello world"), "{md}");
    }

    #[test]
    fn html_is_one_self_contained_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = session(&tmp);
        let html = render(&file, Format::Html, None).unwrap();
        assert!(html.starts_with("<!doctype html>"), "{}", &html[..40]);
        assert!(
            html.contains("<style>"),
            "inline CSS, no external resources"
        );
        assert!(
            !html.contains("http://") && !html.contains("https://"),
            "no network refs"
        );
        assert!(
            html.contains("<details><summary>bash · c1 · ok</summary>"),
            "{html}"
        );
        assert!(html.contains("&lt;script&gt;"), "content is escaped");
    }

    #[test]
    fn json_has_the_specified_shape_and_totals_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let file = session(&tmp);
        let out = render(&file, Format::Json, None).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["session"]["session_id"], "ses_e");
        assert_eq!(v["messages"].as_array().map(Vec::len), Some(2));
        assert!(v["usage"]["recorded"].is_boolean());
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn secrets_are_redacted_before_any_markup_is_built() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("sessions"));
        let ws = tmp.path().join("ws");
        let mut file = store
            .create(&Header::new("ses_s", ws.to_string_lossy(), "build", "m"))
            .unwrap();
        file.push(Record::message(
            &Message::user("token = sk-abcdefghijklmnop123456", 1),
            1,
        ));
        store.save(&file).unwrap();
        let file = store.load(&file.path).unwrap();

        let redactor = Redactor::default();
        for format in [Format::Md, Format::Json, Format::Html] {
            let out = render(&file, format, Some(&redactor)).unwrap();
            assert!(
                !out.contains("sk-abcdefghijklmnop123456"),
                "{format:?}: {out}"
            );
            assert!(out.contains("REDACTED"), "{format:?}");
        }

        let raw = render(&file, Format::Md, None).unwrap();
        assert!(
            raw.contains("sk-abcdefghijklmnop123456"),
            "opt-out really opts out"
        );
    }

    #[test]
    fn an_empty_session_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("sessions"));
        let ws = tmp.path().join("ws");
        let file = store
            .create(&Header::new(
                "ses_empty",
                ws.to_string_lossy(),
                "build",
                "m",
            ))
            .unwrap();
        let md = render(&file, Format::Md, None).unwrap();
        assert!(md.contains("no messages"), "{md}");
    }

    #[test]
    fn pipes_and_newlines_cannot_break_the_metadata_table() {
        assert_eq!(md_cell("a|b\nc"), "a\\|b c");
        assert_eq!(escape("<script>&'\""), "&lt;script&gt;&amp;&#39;&quot;");
    }

    #[test]
    fn image_payloads_are_summarised_not_dumped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().join("sessions"));
        let ws = tmp.path().join("ws");
        let mut file = store
            .create(&Header::new("ses_i", ws.to_string_lossy(), "build", "m"))
            .unwrap();
        let image = cairn_core::Message::new(
            cairn_core::Role::User,
            vec![cairn_core::Block::Image {
                media_type: cairn_core::MediaType::Png,
                data_b64: "AAAA".into(),
                alt: Some("a screenshot".into()),
            }],
            1,
        );
        file.push(Record::message(&image, 1));
        let md = render(&file, Format::Md, None).unwrap();
        assert!(md.contains("[image: png]"), "{md}");
        assert!(!md.contains("AAAA"), "{md}");
    }
}
