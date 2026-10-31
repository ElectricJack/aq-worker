//! Native rollout fields at official source67f0994; source-format evidence only.
use crate::{lifecycle::*, normalize::*};
use anyhow::{Context as _, Result, ensure};
use serde_json::Value;
pub(crate) fn normalize(
    state: &mut Normalizer,
    value: &Value,
    raw_digest: &Digest,
) -> Result<Vec<Draft>> {
    let kind = text(&value["type"])?;
    let p = &value["payload"];
    if kind == "session_meta" {
        identity(p["id"].as_str() == Some(state.source.native_session_key.as_str()))?;
        state.observed_native_identity = true;
        return Ok(vec![]);
    }
    identity(state.observed_native_identity)?;
    if kind == "turn_context" {
        if let Some(model) = p["model"].as_str() {
            let model = Text::new(model)?;
            state.mixed_models |= state.model.as_ref().is_some_and(|old| old != &model);
            state.model = Some(model);
        }
        return Ok(vec![]);
    }
    let subtype = p["type"].as_str().context("rollout payload type absent")?;
    let turn = p["turn_id"].as_str();
    if kind == "event_msg" {
        match subtype {
            "token_count" => {
                let info = &p["info"];
                if info.is_null() {
                    return Ok(vec![]);
                }
                let total = info
                    .get("total_token_usage")
                    .context("native cumulative totals absent")?;
                ensure!(total.is_object(), "native cumulative object absent");
                let counters = Counters {
                    input: count(total.get("input_tokens"))?,
                    output: count(total.get("output_tokens"))?,
                    cache_read: count(total.get("cached_input_tokens"))?,
                    cache_write: count(total.get("cache_write_input_tokens"))?,
                    reasoning: count(total.get("reasoning_output_tokens"))?,
                };
                if let Some(reparse) = &state.reparse {
                    let digest = value_digest(total)?;
                    let known = reparse
                        .known
                        .iter()
                        .find(|(key, fact)| {
                            key.starts_with("codex-total:")
                                && key.ends_with(digest.as_str())
                                && fact.selected_record_digest == *raw_digest
                        })
                        .map(|(key, _)| key.clone());
                    let Some(key) = known else {
                        return Ok(vec![gap(
                            GapReason::ReplayUnavailable,
                            format!("unknown-reparse-cumulative:{}", digest.as_str()),
                        )]);
                    };
                    let generation = key
                        .rsplit(':')
                        .nth(1)
                        .and_then(|v| v.parse::<u64>().ok())
                        .context("known cumulative generation absent")?;
                    state.counter_generation = generation;
                    state.last_cumulative = Some(total.clone());
                    let counter_id =
                        format!("codex-total:{}", state.source.native_session_key.as_str());
                    let model = if state.context_gap {
                        Some("unattributed:source-gap")
                    } else if state.mixed_models {
                        Some("unattributed:mixed-model-cumulative")
                    } else {
                        state.model.as_ref().map(Text::as_str)
                    };
                    let usage = usage(
                        &state.source,
                        model,
                        &counter_id,
                        CounterBasis::Cumulative {
                            counter_id: Text::new(counter_id.as_str())?,
                            counter_generation: generation,
                        },
                        counters,
                    )?;
                    return Ok(vec![usage_draft(key, usage)]);
                }
                if state.last_cumulative.as_ref() == Some(total) {
                    return Ok(vec![]);
                }
                let incoming_digest = value_digest(total)?;
                let snapshot_known = state.snapshot_known(&incoming_digest)?;
                if snapshot_known {
                    return Ok(if state.history_replay {
                        vec![]
                    } else {
                        vec![gap(
                            GapReason::ReplayUnavailable,
                            format!("ambiguous-old-cumulative:{}", incoming_digest.as_str()),
                        )]
                    });
                }
                if state.history_replay {
                    return Ok(vec![gap(
                        GapReason::ReplayUnavailable,
                        format!(
                            "historical-cumulative-unresolved:{}",
                            incoming_digest.as_str()
                        ),
                    )]);
                }
                let reset = state.last_cumulative.as_ref().is_some_and(|old| {
                    [
                        "input_tokens",
                        "output_tokens",
                        "cached_input_tokens",
                        "cache_write_input_tokens",
                        "reasoning_output_tokens",
                    ]
                    .iter()
                    .any(
                        |k| matches!((old[*k].as_u64(),total[*k].as_u64()),(Some(a),Some(b))if b<a),
                    )
                });
                let mut drafts = vec![];
                if state.mixed_models {
                    drafts.push(gap(
                        GapReason::ReplayUnavailable,
                        format!("mixed-model-cumulative:{}", incoming_digest.as_str()),
                    ));
                }
                if reset {
                    state.counter_generation = state
                        .counter_generation
                        .checked_add(1)
                        .context("counter generation exhausted")?;
                    drafts.push(gap(
                        GapReason::ReplayUnavailable,
                        format!("counter-reset:{}", state.counter_generation),
                    ));
                }
                state.last_cumulative = Some(total.clone());
                let counter_id =
                    format!("codex-total:{}", state.source.native_session_key.as_str());
                let model = if state.context_gap {
                    Some("unattributed:source-gap")
                } else if state.mixed_models {
                    Some("unattributed:mixed-model-cumulative")
                } else {
                    state.model.as_ref().map(Text::as_str)
                };
                let usage = usage(
                    &state.source,
                    model,
                    &counter_id,
                    CounterBasis::Cumulative {
                        counter_id: Text::new(&counter_id)?,
                        counter_generation: state.counter_generation,
                    },
                    counters,
                )?;
                let total_digest = value_digest(total)?;
                drafts.push(usage_draft(
                    format!(
                        "{counter_id}:{}:{}",
                        state.counter_generation,
                        total_digest.as_str()
                    ),
                    usage,
                ));
                Ok(drafts)
            }
            "task_started" | "task_complete" => {
                let turn = turn.context("native turn id absent")?;
                let in_turn = subtype == "task_started";
                state.active_turn = if in_turn {
                    Some(Text::new(turn)?)
                } else {
                    None
                };
                Ok(vec![Draft {
                    key: format!("turn:{turn}:{subtype}"),
                    kind: ObservationKind::TurnBoundary { in_turn },
                }])
            }
            "agent_message" | "user_message" => {
                let content = p.get("message").context("message content absent")?;
                ensure!(content.is_string(), "native text absent");
                let mut drafts = vec![content_draft(
                    format!(
                        "visible:{}:{}:{}",
                        subtype,
                        state
                            .active_turn
                            .as_ref()
                            .map(Text::as_str)
                            .unwrap_or("unattributed-turn"),
                        value_digest(value)?.as_str()
                    ),
                    if subtype == "agent_message" {
                        TextRole::Assistant
                    } else {
                        TextRole::User
                    },
                    content,
                )?];
                if let Some(questions) = p.get("questions").filter(|v| !v.is_null()) {
                    let questions = questions
                        .as_array()
                        .context("native questions array absent")?;
                    ensure!(questions.len() <= 8, "native question bound");
                    for (index, question) in questions.iter().enumerate() {
                        ensure!(
                            question["title"].as_str().is_some_and(|s| s.len() <= 4096)
                                && question["options"].as_array().is_none_or(|options| options
                                    .len()
                                    <= 16
                                    && options
                                        .iter()
                                        .all(|v| v.as_str().is_some_and(|s| s.len() <= 512))),
                            "native question shape/bound"
                        );
                        let id =
                            format!("source-question:{}:{index}", value_digest(value)?.as_str());
                        drafts.push(Draft {
                            key: id.clone(),
                            kind: ObservationKind::Question {
                                question_id: Text::new(id)?,
                                content: content_ref(question)?,
                            },
                        });
                    }
                }
                Ok(drafts)
            }
            "error" => Ok(vec![Draft {
                key: format!("error:{}", value_digest(p)?.as_str()),
                kind: ObservationKind::ProviderError {
                    code: Text::new("native_codex_error")?,
                    retryable: false,
                },
            }]),
            "agent_reasoning" | "agent_reasoning_raw_content" => Ok(vec![]),
            _ => Ok(vec![gap(
                GapReason::UnsupportedFormat,
                format!("unsupported-codex-event:{}", value_digest(value)?.as_str()),
            )]),
        }
    } else if kind == "response_item" {
        match subtype {
            "function_call" | "custom_tool_call" => {
                let call = text(&p["call_id"])?;
                Ok(vec![Draft {
                    key: format!("tool-start:{call}"),
                    kind: ObservationKind::ToolStarted {
                        call_id: Text::new(call)?,
                        tool_name: Text::new(text(&p["name"])?)?,
                    },
                }])
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call = text(&p["call_id"])?;
                Ok(vec![Draft {
                    key: format!("tool-result:{call}"),
                    kind: ObservationKind::ToolResult {
                        call_id: Text::new(call)?,
                        content: content_ref(&p["output"])?,
                    },
                }])
            }
            "message" | "reasoning" => Ok(vec![]),
            _ => Ok(vec![]),
        }
    } else {
        Ok(vec![gap(
            GapReason::UnsupportedFormat,
            format!("unsupported-codex:{}", value_digest(value)?.as_str()),
        )])
    }
}
