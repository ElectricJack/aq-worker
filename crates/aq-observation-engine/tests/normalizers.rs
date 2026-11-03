mod support;
use aq_observation_engine::{framing::*, lifecycle::*, *};
use serde_json::{Value, json};
use support::Normalizer;
use uuid::Uuid;
fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap()
}
fn source(format: Format) -> Source {
    Source{binding:serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap(),source_id:Uuid::from_u128(8),generation:1,native_session_key:Text::new("native-fixture").unwrap(),format,provider:Text::new("fixture-provider").unwrap(),account_reference:Text::new("fixture-logical-account").unwrap(), registered_session_key: None}
}
fn apply(state: &mut Normalizer, value: &Value) -> Vec<ObservationRecord> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    let frame = Frame {
        start: 0,
        bytes,
        oversized: false,
    };
    let (next, events) = state.apply(&frame, 100).unwrap();
    *state = next;
    for event in &events {
        event.validate().unwrap();
    }
    events
}
fn usages(events: &[ObservationRecord]) -> Vec<&Usage> {
    events
        .iter()
        .filter_map(|v| {
            if let ObservationKind::Usage { usage } = &v.body.kind {
                Some(usage.as_ref())
            } else {
                None
            }
        })
        .collect()
}
#[test]
fn framing_partial_budget_and_oversized_records_remain_explicit() {
    let f = frame(b"{}\n{", 10, false).unwrap();
    assert_eq!(f.records.len(), 1);
    assert_eq!(f.consumed, 3);
    assert!(f.partial);
    assert_eq!(f.records[0].end().unwrap(), 13);
    let f = frame(&vec![b'x'; MAX_RECORD_BYTES], 0, false).unwrap();
    assert!(f.records[0].oversized && f.discarding_oversized);
    let next = frame(b"tail\n{}\n", MAX_RECORD_BYTES as u64, true).unwrap();
    assert!(next.records[0].oversized);
    assert!(!next.records[1].oversized);
    assert!(frame(&vec![0; MAX_READ_BYTES + 1], 0, false).is_err());
    let many = b"{}\n".repeat(100);
    let f = frame(&many, 0, false).unwrap();
    assert_eq!(f.records.len(), MAX_RECORDS);
    assert!(f.consumed < many.len());
}
#[test]
fn codex_cumulative_native_counts_unknown_zero_and_repeated_last_are_distinct() {
    let mut s = Normalizer::new(source(Format::CodexRollout67f0994), 128).unwrap();
    let rows = fixtures()["codex"].as_array().unwrap().clone();
    assert!(apply(&mut s, &rows[0]).is_empty());
    assert!(apply(&mut s, &rows[1]).is_empty());
    let first = apply(&mut s, &rows[2]);
    let usage = usages(&first)[0];
    assert_eq!(usage.counters.input, Counter::Known { value: 100 });
    assert_eq!(usage.counters.cache_read, Counter::Known { value: 25 });
    assert!(matches!(
        usage.counters.cache_write,
        Counter::Unknown { .. }
    ));
    assert!(matches!(
        usage.basis,
        CounterBasis::Cumulative {
            counter_generation: 1,
            ..
        }
    ));
    assert!(matches!(usage.cost, Cost::Unknown { .. }));
    let mut repeated = rows[2].clone();
    repeated["payload"]["info"]["last_token_usage"]["input_tokens"] = 999.into();
    assert!(
        apply(&mut s, &repeated).is_empty(),
        "quota-only reemit cannot rebill last usage"
    );
    let next = apply(&mut s, &rows[3]);
    assert_eq!(
        usages(&next)[0].counters.input,
        Counter::Known { value: 150 }
    );
    assert!(
        usages(&next)[0].correction_of.is_none(),
        "a new cumulative snapshot does not retract its predecessor"
    );
}
#[test]
fn cumulative_reset_and_mixed_model_attribution_never_invent_delta_or_invoice() {
    let mut s = Normalizer::new(source(Format::CodexRollout67f0994), 128).unwrap();
    let rows = fixtures()["codex"].as_array().unwrap().clone();
    for r in &rows {
        apply(&mut s, r);
    }
    apply(
        &mut s,
        &json!({"type":"turn_context","payload":{"model":"model-b"}}),
    );
    let mut changed = rows[3].clone();
    changed["payload"]["info"]["total_token_usage"]["input_tokens"] = 200.into();
    let out = apply(&mut s, &changed);
    assert_eq!(
        usages(&out)[0].call.model.as_str(),
        "unattributed:mixed-model-cumulative"
    );
    assert!(
        out.iter()
            .any(|v| matches!(v.body.kind, ObservationKind::Gap { .. }))
    );
    let mut reset = rows[2].clone();
    reset["payload"]["info"]["total_token_usage"] = json!({"input_tokens":3,"output_tokens":1,"cached_input_tokens":0,"reasoning_output_tokens":0});
    let out = apply(&mut s, &reset);
    assert!(matches!(
        usages(&out)[0].basis,
        CounterBasis::Cumulative {
            counter_generation: 2,
            ..
        }
    ));
    assert!(out.iter().any(|v| matches!(
        v.body.kind,
        ObservationKind::Gap {
            reason: GapReason::ReplayUnavailable,
            ..
        }
    )));
}
#[test]
fn claude_call_chunks_dedupe_and_corrections_reference_known_prior_fact() {
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let rows = fixtures()["claude"].as_array().unwrap().clone();
    let first = apply(&mut s, &rows[0]);
    let prior = first.iter().find(|e| e.body.kind.is_usage()).unwrap();
    assert!(usages(&apply(&mut s, &rows[1])).is_empty());
    let changed = apply(&mut s, &rows[2]);
    let correction = usages(&changed)[0];
    assert_eq!(correction.counters.output, Counter::Known { value: 8 });
    assert_eq!(
        correction.correction_of,
        Some(EventRef {
            event_id: prior.event_id,
            digest: prior.body_digest.clone()
        })
    );
    assert!(matches!(
        correction.counters.reasoning,
        Counter::Unknown {
            reason: UnknownReason::Unsupported
        }
    ));
    assert!(
        apply(&mut s, &rows[0]).is_empty(),
        "historical exact record cannot rollback corrected usage"
    );
}
#[test]
fn historical_changed_usage_is_gap_not_new_charge_or_budget_reset() {
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let rows = fixtures()["claude"].as_array().unwrap().clone();
    apply(&mut s, &rows[0]);
    s.begin_history_replay();
    let out = apply(&mut s, &rows[2]);
    assert!(usages(&out).is_empty());
    assert!(
        out.iter()
            .any(|v| matches!(v.body.kind, ObservationKind::Gap { .. }))
    );
    s.finish_history_replay();
    let out = apply(&mut s, &rows[2]);
    assert_eq!(usages(&out)[0].counters.output, Counter::Known { value: 8 });
}
#[test]
fn gemini_patch_rewind_and_replay_keep_mutation_provenance_without_raw_content() {
    let mut s = Normalizer::new(source(Format::GeminiRecording6814e00), 128).unwrap();
    let rows = fixtures()["gemini"].as_array().unwrap().clone();
    apply(&mut s, &rows[0]);
    let first = apply(&mut s, &rows[1]);
    assert_eq!(
        usages(&first)[0].counters.cache_read,
        Counter::Known { value: 0 }
    );
    assert!(matches!(
        usages(&first)[0].counters.cache_write,
        Counter::Unknown { .. }
    ));
    let patch = apply(&mut s, &rows[2]);
    assert!(usages(&patch).is_empty());
    assert!(!s.mutations[0].prior_events.is_empty());
    let retained = serde_json::to_string(&s).unwrap();
    assert!(!retained.contains("synthetic patched text"));
    assert!(!retained.contains("synthetic assistant text"));
    apply(&mut s, &rows[3]);
    assert!(s.display_order.is_empty());
    assert!(!s.seen.is_empty());
    assert!(
        usages(&apply(&mut s, &rows[1])).is_empty(),
        "rewind never undoes incurred usage"
    );
    let out = apply(
        &mut s,
        &json!({"$patch":{"id":"gemini-a","tokens":{"input":999}}}),
    );
    assert!(out.iter().any(|v| matches!(
        v.body.kind,
        ObservationKind::Gap {
            reason: GapReason::UnsupportedFormat,
            ..
        }
    )));
    assert_eq!(
        s.mutations.last().unwrap().operation.as_str(),
        "unsupported-patch"
    );
}
#[test]
fn opencode_public_dto_preserves_all_native_usage_but_cost_has_no_invoice_receipt() {
    let s = Normalizer::new(source(Format::OpenCodeSessionDto6c24ec9), 128).unwrap();
    assert!(
        s.clone()
            .bind_scoped_record_channel("other-session")
            .is_err()
    );
    let mut s = s.bind_scoped_record_channel("native-fixture").unwrap();
    let out = apply(&mut s, &fixtures()["opencode"][0]);
    let u = usages(&out)[0];
    assert_eq!(u.counters.cache_write, Counter::Known { value: 1 });
    assert_eq!(u.counters.reasoning, Counter::Known { value: 2 });
    assert!(matches!(u.cost, Cost::Unknown { .. }));
    assert!(
        out.iter()
            .any(|v| matches!(v.body.kind, ObservationKind::ToolStarted { .. }))
    );
    assert!(
        out.iter()
            .any(|v| matches!(v.body.kind, ObservationKind::ToolResult { .. }))
    );
}
#[test]
fn malformed_duplicate_negative_and_identity_mismatch_are_not_empty_success() {
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let raw = Frame {
        start: 10,
        bytes: b"{\"type\":\"assistant\",\"type\":\"user\"}\n".to_vec(),
        oversized: false,
    };
    let (_, out) = s.apply(&raw, 100).unwrap();
    assert!(matches!(
        out[0].body.kind,
        ObservationKind::Gap {
            reason: GapReason::Malformed,
            ..
        }
    ));
    let mut other = fixtures()["claude"][0].clone();
    other["sessionId"] = "foreign-session".into();
    let raw = Frame {
        start: 0,
        bytes: serde_json::to_vec(&other).unwrap(),
        oversized: false,
    };
    assert!(s.apply(&raw, 100).is_err());
    let mut negative = fixtures()["claude"][0].clone();
    negative["message"]["usage"]["input_tokens"] = (-1).into();
    let out = apply(&mut s, &negative);
    assert!(usages(&out).is_empty());
    assert!(matches!(
        out[0].body.kind,
        ObservationKind::Gap {
            reason: GapReason::Malformed,
            ..
        }
    ));
}
#[test]
fn state_restart_replays_exact_native_records_and_parser_drift_refuses() {
    let mut s = Normalizer::new(source(Format::ClaudeV1Jsonl9a9b8e8), 128).unwrap();
    let row = &fixtures()["claude"][0];
    apply(&mut s, row);
    let bytes = serde_json::to_vec(&s).unwrap();
    let mut restored: Normalizer = serde_json::from_slice(&bytes).unwrap();
    assert!(apply(&mut restored, row).is_empty());
    restored.parser.digest = Digest::of(b"another-parser-build");
    let frame = Frame {
        start: 0,
        bytes: serde_json::to_vec(row).unwrap(),
        oversized: false,
    };
    assert!(restored.apply(&frame, 100).is_err());
}

#[test]
fn codex_structured_questions_and_lost_context_do_not_become_task_or_model_authority() {
    let mut s = Normalizer::new(source(Format::CodexRollout67f0994), 128).unwrap();
    let rows = fixtures()["codex"].as_array().unwrap().clone();
    apply(&mut s, &rows[0]);
    apply(&mut s, &rows[1]);
    let q = apply(
        &mut s,
        &json!({"type":"event_msg","payload":{"type":"agent_message","message":"synthetic clarification","questions":[{"title":"Synthetic choice?","options":["one","two"]}]}}),
    );
    assert!(
        q.iter()
            .any(|e| matches!(e.body.kind, ObservationKind::Question { .. }))
    );
    let raw = Frame {
        start: 10,
        bytes: b"malformed native record\n".to_vec(),
        oversized: false,
    };
    let (next, _) = s.apply(&raw, 100).unwrap();
    s = next;
    assert!(s.context_gap);
    let out = apply(&mut s, &rows[2]);
    assert_eq!(
        usages(&out)[0].call.model.as_str(),
        "unattributed:source-gap"
    );
    assert!(matches!(usages(&out)[0].cost, Cost::Unknown { .. }));
}
