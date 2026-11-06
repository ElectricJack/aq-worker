//! Agent task comments: aq-server `protocol/agent-comments/v1`, pinned in
//! `vendor/agent-comments-v1` (see `vendor/agent-comments-v1-source.json`).
//!
//! The per-attempt credential binds the project, task, session and attempt,
//! so an agent can comment only on its own task and read only its thread.
//! The comment ID is the caller's operation ID: replaying the same ID with the
//! same body returns the same comment, and a different body conflicts.
use anyhow::{Result, ensure};
use aq_agent_api::Binding;
use serde_json::{Value, json};
use std::sync::OnceLock;
use uuid::Uuid;

pub const ADD_PATH: &str = "/v1/agent/comments";
pub const LIST_PATH: &str = "/v1/agent/comments/list";
/// Inclusive UTF-8 byte bound of a comment body.
pub const MAX_BODY_BYTES: usize = 16_384;
/// Server page bound; the CLI asks for fewer so a page fits the response cap.
pub const MAX_LIMIT: u16 = 100;
/// Bound on an opaque cursor the agent passes back.
pub const MAX_CURSOR_BYTES: usize = 256;
/// The pinned route table (`routes.json`).
pub const ROUTES: &str = include_str!("../../../vendor/agent-comments-v1/routes.json");

/// The pinned human-graph v1 schema that defines `HumanComment`.
fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../../vendor/agent-comments-v1/human-graph-v1.schema.json"
        ))
        .expect("pinned human-graph schema")
    })
}

/// One comment in the pinned `HumanComment` shape.
pub fn valid_comment(value: &Value) -> bool {
    let root = schema();
    aq_agent_api::check(root, &root["components"]["schemas"]["HumanComment"], value)
}

/// The server's body rule: not blank, at most 16 KiB, and no control
/// characters other than tab, LF and CR.
pub fn check_body(body: &str) -> Result<()> {
    ensure!(!body.trim().is_empty(), "comment body is blank");
    ensure!(
        body.len() <= MAX_BODY_BYTES,
        "comment body exceeds {MAX_BODY_BYTES} bytes"
    );
    ensure!(
        !body
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "comment body holds a control character"
    );
    Ok(())
}

pub fn add_request(operation_id: Uuid, body: &str) -> Result<Value> {
    ensure!(!operation_id.is_nil(), "operation ID required");
    check_body(body)?;
    Ok(json!({"api_version":1,"operation_id":operation_id,"body":body}))
}

pub fn list_request(cursor: Option<&str>, limit: Option<u16>) -> Result<Value> {
    let mut request = json!({"api_version":1});
    if let Some(limit) = limit {
        ensure!(
            (1..=MAX_LIMIT).contains(&limit),
            "limit must be 1..={MAX_LIMIT}"
        );
        request["limit"] = json!(limit);
    }
    if let Some(cursor) = cursor {
        ensure!(
            !cursor.is_empty()
                && cursor.len() <= MAX_CURSOR_BYTES
                && cursor.bytes().all(|b| b.is_ascii_graphic()),
            "cursor shape refused"
        );
        request["cursor"] = json!(cursor);
    }
    Ok(request)
}

/// The add response belongs to this request and this attempt.
pub fn added(binding: &Binding, request: &Value, response: &Value) -> bool {
    let c = &response["comment"];
    response["api_version"] == 1
        && response.as_object().is_some_and(|o| o.len() == 2)
        && valid_comment(c)
        && c["comment_id"] == request["operation_id"]
        && c["task_id"] == json!(binding.task_id)
        && c["body"] == request["body"]
        && c["author"]["kind"] == "agent"
        && c["author"]["session_id"] == json!(binding.session_id)
        && c["author"]["attempt_id"] == json!(binding.attempt_id)
}

/// The list response is a page of this attempt's own task thread.
pub fn page(binding: &Binding, request: &Value, response: &Value) -> bool {
    let task = json!(binding.task_id);
    let limit = request["limit"].as_u64().unwrap_or(50) as usize;
    response["api_version"] == 1
        && response.as_object().is_some_and(|o| o.len() == 4)
        && response["task_id"] == task
        && (response["next_cursor"].is_null()
            || response["next_cursor"]
                .as_str()
                .is_some_and(|c| !c.is_empty() && c.len() <= MAX_CURSOR_BYTES))
        && response["items"].as_array().is_some_and(|items| {
            items.len() <= limit
                && items
                    .iter()
                    .all(|c| valid_comment(c) && c["task_id"] == task)
        })
}

fn author(comment: &Value) -> String {
    let a = &comment["author"];
    match a["kind"].as_str().unwrap_or("unknown") {
        "agent" => match a["attempt_id"].as_str() {
            Some(id) => format!("agent (attempt {})", &id[..id.len().min(8)]),
            None => "agent".into(),
        },
        kind => match a["display_name"].as_str() {
            Some(name) => format!("{kind} {name}"),
            None => kind.into(),
        },
    }
}

/// One comment as text: a header line, then the body indented.
pub fn render_comment(comment: &Value) -> String {
    let mut out = format!(
        "{}  {}  {}",
        comment["created_at"].as_str().unwrap_or("?"),
        author(comment),
        comment["comment_id"].as_str().unwrap_or("?"),
    );
    if comment["edited_at"].is_string() {
        out.push_str("  (edited)");
    }
    out.push('\n');
    match comment["body"].as_str() {
        Some(body) => {
            for line in body.lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
        }
        None => out.push_str("    [deleted]\n"),
    }
    out
}

pub fn render_added(response: &Value) -> String {
    let c = &response["comment"];
    format!(
        "Comment {} recorded on task {}.",
        c["comment_id"].as_str().unwrap_or("?"),
        c["task_id"].as_str().unwrap_or("?"),
    )
}

/// A page newest first, with the command for the next older page.
pub fn render_page(response: &Value) -> String {
    let items = response["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut out = format!(
        "Task {}: {} comment{} (newest first)\n",
        response["task_id"].as_str().unwrap_or("?"),
        items.len(),
        if items.len() == 1 { "" } else { "s" },
    );
    for c in items {
        out.push('\n');
        out.push_str(&render_comment(c));
    }
    if let Some(next) = response["next_cursor"].as_str() {
        out.push_str(&format!(
            "\nOlder comments: aq-agent comment list --cursor {next}\n"
        ));
    }
    out.trim_end().to_owned()
}

/// A refusal as one line: the error code and what to do about it.
pub fn render_refusal(value: &Value) -> String {
    let code = value["code"].as_str().unwrap_or("refused");
    let hint = match code {
        "id_conflict" => {
            "this operation ID already holds a different comment; omit --operation-id to add a new one"
        }
        "invalid_request" => {
            "the server refused the request shape (body 1..16384 bytes, not blank, limit 1..100)"
        }
        "unauthorized" | "session_revoked" | "session_expired" => {
            "the attempt credential is no longer valid"
        }
        "unsupported_api_version" => "the server does not speak agent-comments v1",
        _ => "the server refused the request",
    };
    format!("aq-agent comment: {code}: {hint}")
}
