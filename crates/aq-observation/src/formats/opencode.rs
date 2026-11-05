//! Public scoped Session.Message DTO at official6c24ec9, and the public
//! `{info, parts}` message records (`/session/:id/message`, `opencode export`)
//! that OpenCode 1.18.35 writes for `opencode run` sessions. The source handle
//! must be bound to the exact requested native session; no private database
//! access.
use crate::{lifecycle::*, normalize::*};
use anyhow::{Result, ensure};
use serde_json::Value;
pub(crate) fn normalize(state: &mut Normalizer, value: &Value) -> Result<Vec<Draft>> {
    // A public-record channel carries its authenticated request scope separately;
    // it cannot discover a session by scanning records or accepting another key.
    identity(state.observed_native_identity)?;
    if value.get("info").is_some() {
        return with_parts(state, value);
    }
    let id = text(&value["id"])?;
    ensure!(id.starts_with("msg_"), "native message id class");
    let kind = text(&value["type"])?;
    if !matches!(
        kind,
        "assistant" | "user" | "system" | "synthetic" | "compaction"
    ) {
        return Ok(vec![gap(
            GapReason::UnsupportedFormat,
            format!("opencode-kind:{id}"),
        )]);
    }
    let mut drafts = vec![];
    if let Some(tokens) = value.get("tokens").filter(|v| !v.is_null()) {
        drafts.extend(usage_drafts(
            state,
            id,
            tokens,
            value["model"]["id"].as_str(),
            value["model"]["providerID"].as_str(),
            &value["cost"],
        )?);
    }
    if let Some(content) = value.get("content") {
        let blocks = content
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("native content array absent"))?;
        ensure!(blocks.len() <= 12, "native content block bound");
        for (i, block) in blocks.iter().enumerate() {
            match block["type"].as_str() {
                Some("text") => drafts.push(content_draft(
                    format!("opencode-text:{id}:{i}"),
                    TextRole::Assistant,
                    &block["text"],
                )?),
                Some("tool") => {
                    let call = text(&block["id"])?;
                    drafts.push(Draft {
                        key: format!("opencode-tool-start:{call}"),
                        kind: ObservationKind::ToolStarted {
                            call_id: Text::new(call)?,
                            tool_name: Text::new(text(&block["name"])?)?,
                        },
                    });
                    if matches!(
                        block["state"]["status"].as_str(),
                        Some("completed" | "error")
                    ) {
                        drafts.push(Draft {
                            key: format!("opencode-tool-result:{call}"),
                            kind: ObservationKind::ToolResult {
                                call_id: Text::new(call)?,
                                content: content_ref(&block["state"]["content"])?,
                            },
                        });
                    }
                }
                Some("reasoning") => {}
                _ => drafts.push(gap(
                    GapReason::UnsupportedFormat,
                    format!("opencode-content:{id}:{i}"),
                )),
            }
        }
    } else if let Some(value) = value.get("text") {
        drafts.push(content_draft(
            format!("opencode-text:{id}"),
            if kind == "user" {
                TextRole::User
            } else {
                TextRole::Assistant
            },
            value,
        )?);
    }
    if !value["error"].is_null() {
        drafts.push(Draft {
            key: format!("opencode-error:{id}"),
            kind: ObservationKind::ProviderError {
                code: Text::new("native_opencode_error")?,
                retryable: false,
            },
        });
    }
    if !value["finish"].is_null() {
        drafts.push(Draft {
            key: format!("opencode-turn:{id}"),
            kind: ObservationKind::TurnBoundary { in_turn: false },
        });
    }
    Ok(drafts)
}
fn usage_drafts(
    state: &Normalizer,
    id: &str,
    tokens: &Value,
    model: Option<&str>,
    provider: Option<&str>,
    cost: &Value,
) -> Result<Vec<Draft>> {
    ensure!(tokens.is_object(), "tokens object absent");
    let counters = Counters {
        input: count(tokens.get("input"))?,
        output: count(tokens.get("output"))?,
        cache_read: count(tokens["cache"].get("read"))?,
        cache_write: count(tokens["cache"].get("write"))?,
        reasoning: count(tokens.get("reasoning"))?,
    };
    let mut drafts = vec![];
    let mut usage = usage(&state.source, model, id, CounterBasis::PerCall, counters)?;
    if !cost.is_null() {
        usage.cost = Cost::Unknown {
            reason: UnknownReason::Unsupported,
        };
    }
    if let Some(provider) = provider {
        usage.call.provider = Text::new(provider)?;
        if provider != state.source.provider.as_str() {
            usage.call.account_reference = Text::new("unattributed:account")?;
            drafts.push(gap(
                GapReason::ReplayUnavailable,
                format!("opencode-provider-attribution:{id}"),
            ));
        }
    }
    drafts.push(usage_draft(format!("opencode-message:{id}"), usage));
    Ok(drafts)
}
/// OpenCode 1.18.35 `MessageV2.WithParts`: `{info: Message, parts: Part[]}`.
fn with_parts(state: &Normalizer, value: &Value) -> Result<Vec<Draft>> {
    let info = &value["info"];
    ensure!(info.is_object(), "native info object absent");
    let id = text(&info["id"])?;
    ensure!(id.starts_with("msg_"), "native message id class");
    if let Some(session) = info.get("sessionID") {
        identity(text(session)? == state.source.native_session_key.as_str())?;
    }
    let role = match text(&info["role"])? {
        "user" => TextRole::User,
        "assistant" => TextRole::Assistant,
        _ => {
            return Ok(vec![gap(
                GapReason::UnsupportedFormat,
                format!("opencode-role:{id}"),
            )]);
        }
    };
    let mut drafts = vec![];
    if let Some(tokens) = info.get("tokens").filter(|v| !v.is_null()) {
        drafts.extend(usage_drafts(
            state,
            id,
            tokens,
            info["modelID"].as_str(),
            info["providerID"].as_str(),
            &info["cost"],
        )?);
    }
    let parts = value["parts"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("native parts array absent"))?;
    ensure!(parts.len() <= MAX_PARTS, "native part bound");
    for (i, part) in parts.iter().enumerate() {
        match part["type"].as_str() {
            Some("text") => {
                if part["synthetic"].as_bool() != Some(true) {
                    drafts.push(content_draft(
                        format!("opencode-text:{id}:{i}"),
                        role.clone(),
                        &part["text"],
                    )?);
                }
            }
            Some("tool") => {
                let call = text(&part["callID"])?;
                drafts.push(Draft {
                    key: format!("opencode-tool-start:{call}"),
                    kind: ObservationKind::ToolStarted {
                        call_id: Text::new(call)?,
                        tool_name: Text::new(text(&part["tool"])?)?,
                    },
                });
                let state = &part["state"];
                let output = match state["status"].as_str() {
                    Some("completed") => Some(&state["output"]),
                    Some("error") => Some(&state["error"]),
                    _ => None,
                };
                if let Some(output) = output {
                    drafts.push(Draft {
                        key: format!("opencode-tool-result:{call}"),
                        kind: ObservationKind::ToolResult {
                            call_id: Text::new(call)?,
                            content: content_ref(output)?,
                        },
                    });
                }
            }
            // Step markers, reasoning and workspace snapshots carry no
            // transcript content the H2 contract consumes.
            Some("step-start" | "step-finish" | "reasoning" | "snapshot" | "patch") => {}
            _ => drafts.push(gap(
                GapReason::UnsupportedFormat,
                format!("opencode-part:{id}:{i}"),
            )),
        }
    }
    if !info["error"].is_null() {
        drafts.push(Draft {
            key: format!("opencode-error:{id}"),
            kind: ObservationKind::ProviderError {
                code: Text::new("native_opencode_error")?,
                retryable: false,
            },
        });
    }
    // `tool-calls` continues the agent loop; any other finish ends the turn.
    if info["finish"].as_str().is_some_and(|f| f != "tool-calls") {
        drafts.push(Draft {
            key: format!("opencode-turn:{id}"),
            kind: ObservationKind::TurnBoundary { in_turn: false },
        });
    }
    Ok(drafts)
}
const MAX_PARTS: usize = 256;
