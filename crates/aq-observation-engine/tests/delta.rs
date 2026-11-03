#[allow(dead_code)]
mod support;
use anyhow::Result;
use aq_observation_engine::{framing::Frame, lifecycle::*, rows::*, *};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use support::{Memory, Normalizer};
use uuid::Uuid;
fn source(format: Format) -> Source {
    Source{binding:serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap(),source_id:Uuid::from_u128(8),generation:1,native_session_key:Text::new("native-fixture").unwrap(),format,provider:Text::new("fixture-provider").unwrap(),account_reference:Text::new("logical-account").unwrap(), registered_session_key: None }
}
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap()
}
fn frame(row: &Value) -> Frame {
    let mut bytes = serde_json::to_vec(row).unwrap();
    bytes.push(b'\n');
    Frame {
        start: 0,
        bytes,
        oversized: false,
    }
}
fn canonical_kind(mut value: Value, refs: &BTreeMap<String, Value>) -> Value {
    fn visit(value: &mut Value, refs: &BTreeMap<String, Value>) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    if key == "policy" {
                        *v = json!({"explicit_parser_artifact_difference":true});
                    } else if key == "correction_of" && !v.is_null() {
                        if let Some(mapped) = refs.get(&serde_json::to_string(v).unwrap()) {
                            *v = mapped.clone();
                        }
                    } else {
                        visit(v, refs)
                    }
                }
            }
            Value::Array(a) => {
                for v in a {
                    visit(v, refs)
                }
            }
            _ => {}
        }
    }
    visit(&mut value, refs);
    value
}
#[test]
fn four_source_fixture_semantics_match_pinned_reference_with_explicit_parser_identity_mapping() {
    for (name, format) in [
        ("codex", Format::CodexRollout67f0994),
        ("claude", Format::ClaudeV1Jsonl9a9b8e8),
        ("gemini", Format::GeminiRecording6814e00),
        ("opencode", Format::OpenCodeSessionDto6c24ec9),
    ] {
        let configured = source(format.clone());
        let reference_source =
            serde_json::from_value(serde_json::to_value(&configured).unwrap()).unwrap();
        let mut old = aq_observation::Normalizer::new(reference_source, 4096).unwrap();
        let mut new = Normalizer::new(configured, 4096).unwrap();
        if name == "opencode" {
            old = old.bind_scoped_record_channel("native-fixture").unwrap();
            new = new.bind_scoped_record_channel("native-fixture").unwrap();
        }
        let mut refs = BTreeMap::new();
        for row in fixture()[name].as_array().unwrap() {
            let f = frame(row);
            let old_frame = aq_observation::framing::Frame {
                start: f.start,
                bytes: f.bytes.clone(),
                oversized: f.oversized,
            };
            let (next, a) = old.apply(&old_frame, 100).unwrap();
            old = next;
            let (next, b) = new.apply(&f, 100).unwrap();
            new = next;
            assert_eq!(a.len(), b.len(), "{name} output length");
            for (old, new) in a.iter().zip(&b) {
                assert_ne!(
                    old.body.provenance.parser.digest, new.body.provenance.parser.digest,
                    "candidate must not pretend reference implementation bytes"
                );
                assert_eq!(old.body.provenance.cursor, new.body.provenance.cursor);
                assert_eq!(
                    old.body.provenance.raw_record,
                    new.body.provenance.raw_record
                );
                assert_eq!(
                    canonical_kind(serde_json::to_value(&old.body.kind).unwrap(), &refs),
                    canonical_kind(
                        serde_json::to_value(&new.body.kind).unwrap(),
                        &BTreeMap::new()
                    ),
                    "{name} normalized semantics"
                );
                let prior = EventRef {
                    event_id: old.event_id,
                    digest: old.body_digest.clone(),
                };
                let successor = EventRef {
                    event_id: new.event_id,
                    digest: new.body_digest.clone(),
                };
                refs.insert(
                    serde_json::to_string(&serde_json::to_value(&prior).unwrap()).unwrap(),
                    serde_json::to_value(successor).unwrap(),
                );
            }
        }
    }
}
struct Broken;
impl ReadView for Broken {
    fn get(&self, _: Table, _: &str) -> Result<Option<Value>> {
        anyhow::bail!("storage unavailable")
    }
    fn count(&self, _: Table) -> Result<usize> {
        anyhow::bail!("storage unavailable")
    }
    fn display_suffix(&self, _: u64, _: usize) -> Result<Vec<(u64, String)>> {
        anyhow::bail!("storage unavailable")
    }
}
#[test]
fn row_storage_failure_holds_instead_of_advancing_with_invented_native_gap() {
    let context = Context::new(source(Format::ClaudeV1Jsonl9a9b8e8), 4096).unwrap();
    let f = frame(&fixture()["claude"][0]);
    assert!(normalize(&context, &Broken, &f, 100).is_err());
}
#[test]
fn replay_point_reads_and_delta_size_do_not_scale_with_unrelated_seen_history() {
    let context = Context::new(source(Format::ClaudeV1Jsonl9a9b8e8), 4096).unwrap();
    let f = frame(&fixture()["claude"][0]);
    let first = normalize(&context, &Memory::default(), &f, 100).unwrap();
    let mut small = Memory::default();
    small.commit(&first.rows);
    let mut large = small.clone();
    let template = large.tables[&Table::Seen].values().next().unwrap().clone();
    for i in 0..4000 {
        let mut row = template.clone();
        row["raw_record_digests"] = json!(vec![Digest::of(format!("raw-{i}").as_bytes()); 32]);
        large
            .tables
            .get_mut(&Table::Seen)
            .unwrap()
            .insert(format!("irrelevant-{i}"), row);
    }
    small.reads.set(0);
    large.reads.set(0);
    let a = normalize(&first.context, &small, &f, 100).unwrap();
    let b = normalize(&first.context, &large, &f, 100).unwrap();
    assert!(a.events.is_empty() && b.events.is_empty());
    assert_eq!(a.rows, b.rows);
    assert_eq!(small.reads.get(), large.reads.get());
    assert!(large.reads.get() < 32);
    assert_eq!(
        serde_json::to_vec(&a).unwrap(),
        serde_json::to_vec(&b).unwrap()
    );
}
#[test]
fn gemini_suffix_over_budget_is_explicit_gap_and_never_partial_history_deletion() {
    let mut context = Context::new(source(Format::GeminiRecording6814e00), 4096).unwrap();
    context.observed_native_identity = true;
    context.next_display_ordinal = 300;
    let mut memory = Memory::default();
    for i in 0..300u64 {
        let id = format!("id-{i}");
        for (t, k, v) in [
            (Table::Native, id.clone(), json!({"id":id,"type":"gemini"})),
            (Table::DisplayById, id.clone(), json!(i)),
            (Table::DisplayByOrdinal, i.to_string(), json!(id)),
        ] {
            memory.tables.entry(t).or_default().insert(k, v);
        }
        memory.display.insert(i, id);
    }
    let over = normalize(&context, &memory, &frame(&json!({"$rewindTo":"id-0"})), 100).unwrap();
    assert!(matches!(
        over.events[0].body.kind,
        ObservationKind::Gap { .. }
    ));
    assert!(
        over.rows
            .writes
            .keys()
            .all(|(t, _)| *t != Table::Native && *t != Table::DisplayByOrdinal)
    );
    let bounded = normalize(
        &context,
        &memory,
        &frame(&json!({"$rewindTo":"id-280"})),
        100,
    )
    .unwrap();
    assert_eq!(
        bounded
            .rows
            .writes
            .iter()
            .filter(|((t, _), v)| *t == Table::Native && v.is_none())
            .count(),
        20
    );
    assert!(bounded.indexed_lookups < 128);
    assert!(
        bounded
            .events
            .iter()
            .all(|e| matches!(e.body.kind, ObservationKind::Gap { .. }))
    );
}
