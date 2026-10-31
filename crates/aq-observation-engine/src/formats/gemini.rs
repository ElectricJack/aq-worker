//! Pinned6814e00 append-only recorder semantics. Retained native-message state
//! contains only identities, usage and logical digests, never prompt/tool text.
use crate::{lifecycle::*, normalize::*};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
fn remember_mutation(
    state: &mut Normalizer,
    digest: &Digest,
    operation: &str,
    ids: Vec<String>,
) -> Result<Vec<Draft>> {
    ensure!(ids.len() <= 32, "mutation identity bound");
    let mut prior = vec![];
    for id in &ids {
        for key in [format!("gemini-usage:{id}"), format!("gemini-content:{id}")] {
            if let Some(old) = state.seen_get(&key)? {
                prior.push(old.event);
            }
        }
    }
    if state.mutation_known(digest, operation)? {
        return Ok(vec![gap(
            GapReason::ReplayUnavailable,
            format!("gemini-mutation:{operation}:{}", digest.as_str()),
        )]);
    }
    state.mutation_put(Mutation {
        source_record_digest: digest.clone(),
        operation: Text::new(operation)?,
        affected_native_ids: ids
            .iter()
            .map(Text::new)
            .collect::<std::result::Result<Vec<_>, _>>()?,
        prior_events: prior,
    })?;
    Ok(vec![gap(
        GapReason::ReplayUnavailable,
        format!("gemini-mutation:{operation}:{}", digest.as_str()),
    )])
}
fn message(state: &mut Normalizer, value: &Value) -> Result<Vec<Draft>> {
    let id = text(&value["id"])?;
    let kind = text(&value["type"])?;
    ensure!(
        matches!(kind, "user" | "gemini" | "info" | "error" | "warning"),
        "unsupported native message kind"
    );
    let mut drafts = vec![];
    let mut safe = json!({"id":id,"type":kind});
    if let Some(model) = value["model"].as_str() {
        Text::new(model)?;
        safe["model"] = model.into();
    }
    if let Some(tokens) = value.get("tokens").filter(|v| !v.is_null()) {
        ensure!(
            kind == "gemini" && tokens.is_object(),
            "tokens not on native gemini message"
        );
        let counters = Counters {
            input: count(tokens.get("input"))?,
            output: count(tokens.get("output"))?,
            cache_read: count(tokens.get("cached"))?,
            cache_write: unsupported(),
            reasoning: count(tokens.get("thoughts"))?,
        };
        let usage = usage(
            &state.source,
            value["model"].as_str(),
            &format!("gemini-message:{id}"),
            CounterBasis::PerCall,
            counters,
        )?;
        drafts.push(usage_draft(format!("gemini-usage:{id}"), usage));
        let mut selected = serde_json::Map::new();
        for key in ["input", "output", "cached", "thoughts", "tool", "total"] {
            if let Some(n) = tokens.get(key) {
                ensure!(
                    n.is_null() || n.as_u64().is_some(),
                    "invalid native token category"
                );
                selected.insert(key.into(), n.clone());
            }
        }
        safe["tokens"] = Value::Object(selected);
    }
    if let Some(content) = value.get("content") {
        let role = if kind == "user" {
            TextRole::User
        } else {
            TextRole::Assistant
        };
        let draft = content_draft(format!("gemini-content:{id}"), role, content)?;
        safe["content_digest"] = serde_json::to_value(content_ref(content)?)?;
        drafts.push(draft);
    }
    if let Some(tools) = value.get("toolCalls") {
        let tools = tools.as_array().context("tool call array absent")?;
        ensure!(tools.len() <= 12, "native tool call bound");
        let mut safe_tools = vec![];
        for tool in tools {
            let call = text(&tool["id"])?;
            let name = text(&tool["name"])?;
            let mut safe_tool = json!({"id":call,"name":name});
            drafts.push(Draft {
                key: format!("gemini-tool-start:{call}"),
                kind: ObservationKind::ToolStarted {
                    call_id: Text::new(call)?,
                    tool_name: Text::new(name)?,
                },
            });
            if let Some(result) = tool.get("result").filter(|v| !v.is_null()) {
                let content = content_ref(result)?;
                safe_tool["result_digest"] = serde_json::to_value(&content)?;
                drafts.push(Draft {
                    key: format!("gemini-tool-result:{call}"),
                    kind: ObservationKind::ToolResult {
                        call_id: Text::new(call)?,
                        content,
                    },
                });
            }
            safe_tools.push(safe_tool);
        }
        safe["toolCalls"] = safe_tools.into();
    }
    if kind == "error" {
        drafts.push(Draft {
            key: format!("gemini-error:{id}"),
            kind: ObservationKind::ProviderError {
                code: Text::new("native_gemini_error")?,
                retryable: false,
            },
        });
    }
    if state.native_get(id)?.is_none() {
        state.display_append(id)?;
    }
    state.native_put(id.into(), safe)?;
    Ok(drafts)
}
fn patch(state: &mut Normalizer, value: &Value, digest: &Digest) -> Result<Vec<Draft>> {
    let obj = value.as_object().context("patch object absent")?;
    let known = [
        "id",
        "content",
        "toolCalls",
        "updates",
        "removeIds",
        "orderIds",
    ];
    if obj.keys().any(|key| !known.contains(&key.as_str())) {
        let ids = value["id"]
            .as_str()
            .map(|id| vec![id.to_owned()])
            .unwrap_or_default();
        let mut drafts = remember_mutation(state, digest, "unsupported-patch", ids)?;
        for draft in &mut drafts {
            draft.kind = ObservationKind::Gap {
                reason: GapReason::UnsupportedFormat,
                affected: None,
            };
        }
        return Ok(drafts);
    }
    let mut drafts = vec![];
    let mut updates = vec![];
    if value.get("id").is_some() {
        updates.push(value);
    }
    if let Some(more) = value.get("updates") {
        let more = more.as_array().context("updates array absent")?;
        ensure!(more.len() <= 12, "patch update bound");
        updates.extend(more);
    }
    for update in updates {
        if !std::ptr::eq(update, value)
            && update.as_object().is_none_or(|map| {
                map.keys()
                    .any(|key| !["id", "content", "toolCalls"].contains(&key.as_str()))
            })
        {
            drafts.extend(remember_mutation(
                state,
                digest,
                "unsupported-patch-update",
                vec![],
            )?);
            drafts.push(gap(
                GapReason::UnsupportedFormat,
                format!("unsupported-patch-update:{}", digest.as_str()),
            ));
            continue;
        }
        let id = text(&update["id"])?;
        let Some(old) = state.native_get(id)? else {
            drafts.push(gap(
                GapReason::ReplayUnavailable,
                format!("unknown-gemini-patch:{id}:{}", digest.as_str()),
            ));
            continue;
        };
        drafts.extend(remember_mutation(state, digest, "patch", vec![id.into()])?);
        let mut new = old;
        if let Some(content) = update.get("content") {
            drafts.push(content_draft(
                format!("gemini-content:{id}"),
                if new["type"] == "user" {
                    TextRole::User
                } else {
                    TextRole::Assistant
                },
                content,
            )?);
            new["content_digest"] = serde_json::to_value(content_ref(content)?)?;
        }
        if let Some(tools) = update.get("toolCalls") {
            let tools = tools.as_array().context("patch tools array absent")?;
            ensure!(tools.len() <= 12, "patch tool bound");
            for tool in tools {
                let call = text(&tool["id"])?;
                let existing = new["toolCalls"]
                    .as_array_mut()
                    .and_then(|list| list.iter_mut().find(|t| t["id"] == call));
                if let Some(existing) = existing {
                    if let Some(result) = tool.get("result") {
                        let content = content_ref(result)?;
                        existing["result_digest"] = serde_json::to_value(&content)?;
                        drafts.push(Draft {
                            key: format!("gemini-tool-result:{call}"),
                            kind: ObservationKind::ToolResult {
                                call_id: Text::new(call)?,
                                content,
                            },
                        });
                    }
                } else {
                    drafts.push(gap(
                        GapReason::ReplayUnavailable,
                        format!("unknown-gemini-tool:{call}:{}", digest.as_str()),
                    ));
                }
            }
        }
        state.native_put(id.into(), new)?;
    }
    if let Some(ids) = value.get("removeIds") {
        let ids = ids.as_array().context("removeIds array absent")?;
        ensure!(ids.len() <= 32, "removed id bound");
        let ids = ids
            .iter()
            .map(|v| text(v).map(str::to_owned))
            .collect::<Result<Vec<_>>>()?;
        drafts.extend(remember_mutation(state, digest, "remove", ids.clone())?);
        for id in ids {
            state.native_remove(&id)?;
            state.display_remove(&id)?;
        }
    }
    if let Some(ids) = value.get("orderIds") {
        let ids = ids.as_array().context("orderIds array absent")?;
        ensure!(ids.len() <= 32, "orderIds bound");
        let ids = ids
            .iter()
            .map(|v| text(v).map(str::to_owned))
            .collect::<Result<Vec<_>>>()?;
        drafts.extend(remember_mutation(state, digest, "order", ids.clone())?);
        for id in &ids {
            state.display_remove(id)?;
        }
        for id in ids {
            if state.native_get(&id)?.is_some() {
                state.display_append(&id)?;
            }
        }
    }
    Ok(drafts)
}
pub(crate) fn normalize(
    state: &mut Normalizer,
    value: &Value,
    digest: &Digest,
) -> Result<Vec<Draft>> {
    if let Some(key) = value["sessionId"].as_str() {
        identity(key == state.source.native_session_key.as_str())?;
        state.observed_native_identity = true;
    }
    identity(state.observed_native_identity)?;
    if let Some(set) = value.get("$set") {
        let set_obj = set.as_object().context("metadata set absent")?;
        if set_obj.keys().any(|key| {
            ![
                "sessionId",
                "projectHash",
                "startTime",
                "lastUpdated",
                "summary",
                "memoryScratchpad",
                "directories",
                "kind",
                "messages",
            ]
            .contains(&key.as_str())
        }) {
            return Ok(vec![gap(
                GapReason::UnsupportedFormat,
                format!("unsupported-metadata-set:{}", digest.as_str()),
            )]);
        }
        if let Some(session) = set["sessionId"].as_str() {
            identity(session == state.source.native_session_key.as_str())?;
        }
        if let Some(messages) = set.get("messages") {
            let messages = messages.as_array().context("history array absent")?;
            ensure!(messages.len() <= 12, "history snapshot bound");
            let ids = state.display_suffix(None)?;
            let mut drafts = remember_mutation(state, digest, "set-history", ids.clone())?;
            for id in ids {
                state.native_remove(&id)?;
                state.display_remove(&id)?;
            }
            for item in messages {
                drafts.extend(message(state, item)?);
            }
            return Ok(drafts);
        }
        return Ok(vec![]);
    }
    if let Some(value) = value.get("$patch") {
        return patch(state, value, digest);
    }
    if let Some(rewind) = value.get("$rewindTo") {
        let id = text(rewind)?;
        let removed = state.display_suffix(Some(id))?;
        for id in &removed {
            state.native_remove(id)?;
            state.display_remove(id)?;
        }
        return remember_mutation(state, digest, "rewind", removed);
    }
    if value.get("id").is_some() {
        return message(state, value);
    }
    if value.get("sessionId").is_some() {
        return Ok(vec![]);
    }
    Ok(vec![gap(
        GapReason::UnsupportedFormat,
        format!("unsupported-gemini:{}", digest.as_str()),
    )])
}
