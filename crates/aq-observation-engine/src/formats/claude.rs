//! Native JSONL shape witnessed by v1 revision9a9b8e8, qualified against a
//! real Claude Code 2.1.318 headless session (I1). No claim beyond the
//! qualified version; unknown record kinds become explicit gaps.
use crate::{lifecycle::*, normalize::*};
use anyhow::{Context as _, Result, ensure};
use serde_json::Value;
pub(crate) fn normalize(state: &mut Normalizer, value: &Value) -> Result<Vec<Draft>> {
    if let Some(session) = value["sessionId"].as_str() {
        identity(session == state.source.native_session_key.as_str())?;
        state.observed_native_identity = true;
    }
    identity(state.observed_native_identity)?;
    let kind = text(&value["type"])?;
    // Metadata records that carry no usage, tool or turn facts. The headless
    // and interactive-UI kinds were witnessed on Claude Code 2.1.318 (I1). The
    // per-call facts they summarize (`api-request*`, `cost-state`) are read
    // from the assistant rows instead. `system` (including context-loss
    // `compact_boundary`) and unknown kinds stay explicit gaps.
    if matches!(
        kind,
        "summary"
            | "queue-operation"
            | "attachment"
            | "atis-latch"
            | "api-request-shape"
            | "api-request-blob"
            | "api-request"
            | "tools-used"
            | "last-prompt"
            | "cost-state"
            | "mode"
            | "permission-mode"
            | "session-owner"
            | "ai-title"
            | "pr-link"
            | "file-history-snapshot"
            | "file-history-delta"
    ) {
        return Ok(vec![]);
    }
    if !matches!(kind, "user" | "assistant") {
        return Ok(vec![gap(
            GapReason::UnsupportedFormat,
            format!("unsupported-claude:{}", value_digest(value)?.as_str()),
        )]);
    }
    let row_id = text(&value["uuid"])?;
    let message = &value["message"];
    ensure!(message.is_object(), "native message absent");
    let mut drafts = vec![];
    if let Some(tokens) = message.get("usage").filter(|v| !v.is_null()) {
        ensure!(
            kind == "assistant" && tokens.is_object(),
            "usage not on native assistant message"
        );
        let call_id = text(&message["id"])?;
        let counters = Counters {
            input: count(tokens.get("input_tokens"))?,
            output: count(tokens.get("output_tokens"))?,
            cache_read: count(tokens.get("cache_read_input_tokens"))?,
            cache_write: count(tokens.get("cache_creation_input_tokens"))?,
            reasoning: unsupported(),
        };
        let usage = usage(
            &state.source,
            message["model"].as_str(),
            call_id,
            CounterBasis::PerCall,
            counters,
        )?;
        drafts.push(usage_draft(format!("claude-call:{call_id}"), usage));
    }
    if let Some(content) = message.get("content") {
        if content.is_string() {
            drafts.push(content_draft(
                format!("claude-text:{row_id}"),
                if kind == "assistant" {
                    TextRole::Assistant
                } else {
                    TextRole::User
                },
                content,
            )?);
        } else if let Some(blocks) = content.as_array() {
            ensure!(blocks.len() <= 24, "native content block bound");
            for (i, block) in blocks.iter().enumerate() {
                match block["type"].as_str() {
                    Some("text") => drafts.push(content_draft(
                        format!("claude-text:{row_id}:{i}"),
                        if kind == "assistant" {
                            TextRole::Assistant
                        } else {
                            TextRole::User
                        },
                        block.get("text").context("text absent")?,
                    )?),
                    Some("tool_use") => {
                        let call_id = text(&block["id"])?;
                        drafts.push(Draft {
                            key: format!("tool-start:{call_id}"),
                            kind: ObservationKind::ToolStarted {
                                call_id: Text::new(call_id)?,
                                tool_name: Text::new(text(&block["name"])?)?,
                            },
                        })
                    }
                    Some("tool_result") => {
                        let call_id = text(&block["tool_use_id"])?;
                        drafts.push(Draft {
                            key: format!("tool-result:{call_id}"),
                            kind: ObservationKind::ToolResult {
                                call_id: Text::new(call_id)?,
                                content: content_ref(&block["content"])?,
                            },
                        })
                    }
                    Some("thinking" | "redacted_thinking") => {}
                    _ => drafts.push(gap(
                        GapReason::UnsupportedFormat,
                        format!("claude-block:{row_id}:{i}"),
                    )),
                }
            }
        }
    }
    if kind == "assistant" && message["stop_reason"] == "end_turn" {
        drafts.push(Draft {
            key: format!("claude-turn:{row_id}"),
            kind: ObservationKind::TurnBoundary { in_turn: false },
        });
    }
    Ok(drafts)
}
