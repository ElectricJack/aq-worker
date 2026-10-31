//! Public scoped Session.Message DTO at official6c24ec9. The source handle must
//! be bound to the exact requested native session; no private database access.
use crate::{lifecycle::*, normalize::*};
use anyhow::{Result, ensure};
use serde_json::Value;
pub(crate) fn normalize(state: &mut Normalizer, value: &Value) -> Result<Vec<Draft>> {
    // A public-record channel carries its authenticated request scope separately;
    // it cannot discover a session by scanning records or accepting another key.
    identity(state.observed_native_identity)?;
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
        ensure!(tokens.is_object(), "tokens object absent");
        let counters = Counters {
            input: count(tokens.get("input"))?,
            output: count(tokens.get("output"))?,
            cache_read: count(tokens["cache"].get("read"))?,
            cache_write: count(tokens["cache"].get("write"))?,
            reasoning: count(tokens.get("reasoning"))?,
        };
        let provider = value["model"]["providerID"].as_str();
        let mut usage = usage(
            &state.source,
            value["model"]["id"].as_str(),
            id,
            CounterBasis::PerCall,
            counters,
        )?;
        if value.get("cost").is_some_and(|v| !v.is_null()) {
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
