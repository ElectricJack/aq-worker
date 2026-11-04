//! Local pin of the additive agent-access v1 supervisor mail shapes.
//!
//! Vendored aq-agent-api 0.1.0 knows only task/session recipients and
//! owner/session senders. The server already uses these supervisor shapes for
//! the global supervisor conversation; until the contract publishes them, the
//! two new `oneOf` branches are pinned here exactly and every other part of a
//! document is still checked by the vendored schema. Drop this extension when
//! aq-agent-api is re-vendored with them.
use anyhow::{Result, bail, ensure};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Inclusive UTF-8 byte bounds of every message body.
pub const MAX_BODY_BYTES: usize = 16_384;

fn uuid(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() == 36
            && s.bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-'))
            && Uuid::parse_str(s).is_ok_and(|id| !id.is_nil())
    })
}
fn exact<'a>(value: &'a Value, kind: &str, keys: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;
    (object.get("kind") == Some(&json!(kind))
        && object.len() == keys.len() + 1
        && keys.iter().all(|k| object.contains_key(*k)))
    .then_some(object)
}
/// `{"kind":"global_supervisor","conversation_id"}`: the supervisor conversation mailbox.
pub fn supervisor_mailbox(value: &Value) -> bool {
    exact(value, "global_supervisor", &["conversation_id"])
        .is_some_and(|o| uuid(&o["conversation_id"]))
}
/// `{"kind":"central_supervisor","actor_id","generation","session_id","conversation_id"}`.
pub fn supervisor_sender(value: &Value) -> bool {
    exact(
        value,
        "central_supervisor",
        &["actor_id", "generation", "session_id", "conversation_id"],
    )
    .is_some_and(|o| {
        uuid(&o["actor_id"])
            && uuid(&o["session_id"])
            && uuid(&o["conversation_id"])
            && o["generation"]
                .as_u64()
                .is_some_and(|g| (1..=i64::MAX as u64).contains(&g))
    })
}
fn substitute(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(substitute),
        Value::Object(values) => {
            for (key, v) in values.iter_mut() {
                if key == "recipient" && supervisor_mailbox(v) {
                    let id = json!(Uuid::from_u128(1));
                    *v = json!({"kind":"task","workspace_id":id,"project_id":id,"task_id":id});
                } else if key == "sender" && supervisor_sender(v) {
                    *v = json!({"kind":"owner","user_id":Uuid::from_u128(1)});
                } else {
                    substitute(v);
                }
            }
        }
        _ => {}
    }
}
/// The vendored schema plus the pinned supervisor recipient/sender branches.
pub fn valid(name: &str, value: &Value) -> bool {
    if aq_agent_api::valid(name, value) {
        return true;
    }
    let mut pinned = value.clone();
    substitute(&mut pinned);
    pinned != *value && aq_agent_api::valid(name, &pinned)
}
pub fn check_body(body: &str) -> Result<()> {
    ensure!(
        (1..=MAX_BODY_BYTES).contains(&body.len()),
        "message body must be 1..=16384 UTF-8 bytes"
    );
    Ok(())
}
/// The mailbox a reply to `sender` goes to: the original sender's mailbox.
pub fn reply_recipient(sender: &Value) -> Result<Value> {
    if supervisor_sender(sender) {
        return Ok(json!({"kind":"global_supervisor","conversation_id":sender["conversation_id"]}));
    }
    match sender["kind"].as_str() {
        Some("session") => {
            let b = &sender["binding"];
            Ok(json!({"kind":"session","workspace_id":b["workspace_id"],
                "project_id":b["project_id"],"task_id":b["task_id"],
                "session_id":b["session_id"],"attempt_id":b["attempt_id"],
                "generation":b["generation"]}))
        }
        Some("owner") => bail!("agent-access v1 has no owner mailbox to reply to"),
        _ => bail!("unrecognized message sender"),
    }
}
