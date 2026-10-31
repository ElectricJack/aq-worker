//! Synthetic old-parser misnormalization, not an actual released CLI fixture.
#[allow(dead_code)]
mod support;
use anyhow::{Result, ensure};
use aq_observation::{lifecycle::*, source::*, spool::*, *};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
fn binding() -> Binding {
    serde_json::from_value(json!({"project_id":Uuid::from_u128(1),"task_id":Uuid::from_u128(2),"lineage_id":Uuid::from_u128(3),"attempt_id":Uuid::from_u128(4),"attempt_generation":1,"session":{"kind":"remote","node_id":Uuid::from_u128(5),"boot_id":Uuid::from_u128(6),"session_id":Uuid::from_u128(7),"attempt_id":Uuid::from_u128(4),"assignment_generation":1}})).unwrap()
}
fn node() -> Node {
    Node {
        node_id: Uuid::from_u128(5),
        boot_id: Uuid::from_u128(6),
    }
}
fn json_digest(v: &Value) -> Digest {
    let mut v = v.clone();
    v.sort_all_objects();
    Digest::of(&serde_json::to_vec(&v).unwrap())
}
fn fixture(temp: &tempfile::TempDir) -> (Spool, SourceFile, Vec<u8>, ReparsePlan) {
    let f: Value = serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap();
    let mut bytes = serde_json::to_vec(&f["claude"][0]).unwrap();
    bytes.push(b'\n');
    let path = temp.path().join("native.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        temp.path(),
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    let source = SourceFile::open(permit.clone()).unwrap();
    let normal = Normalizer::new(
        Source {
            binding: binding(),
            source_id: Uuid::from_u128(8),
            generation: 1,
            native_session_key: Text::new("native-fixture").unwrap(),
            format: Format::ClaudeV1Jsonl9a9b8e8,
            provider: Text::new("fixture-provider").unwrap(),
            account_reference: Text::new("fixture-account").unwrap(),
        },
        128,
    )
    .unwrap();
    let root = temp.path().join("spool");
    let mut spool = support::initialize(&root, node(), Limits::default());
    spool.attach_file(normal, permit).unwrap();
    spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    drop(spool);
    // Synthetic durable old producer: same verified native bytes, but a parser bug
    // reported output7 rather than native5. Private metadata checksum is rebuilt.
    let path = root.join("spool.json");
    let mut envelope: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let old_parser = ArtifactRef {
        id: Text::new("synthetic-old-parser-error").unwrap(),
        version: Text::new("0.0.0").unwrap(),
        digest: Digest::of(b"synthetic-old-parser-counts7-from-native5"),
    };
    let source_key = Uuid::from_u128(8).to_string();
    envelope["state"]["sources"][&source_key]["normalizer"]["parser"] = json!(old_parser);
    let mut refs = vec![];
    for (i, record) in envelope["state"]["outbox"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let mut event: ObservationRecord = serde_json::from_value(record.clone()).unwrap();
        event.event_id = Uuid::from_u128(100 + i as u128);
        event.body.provenance.parser = old_parser.clone();
        let key = if let ObservationKind::Usage { usage } = &mut event.body.kind {
            usage.counters.output = Counter::Known { value: 7 };
            "claude-call:call-a"
        } else {
            "claude-text:row-1:0"
        };
        event.body_digest = canonical_digest(&event.body).unwrap();
        let normalized_digest = canonical_digest(&event.body.kind).unwrap();
        refs.push((
            key.to_owned(),
            normalized_digest,
            EventRef {
                event_id: event.event_id,
                digest: event.body_digest.clone(),
            },
        ));
        *record = json!(event);
    }
    for (key, digest, event) in refs {
        let seen = &mut envelope["state"]["sources"][&source_key]["normalizer"]["seen"][key];
        seen["normalized_digest"] = json!(digest);
        seen["event"] = json!(event);
    }
    envelope["digest"] = json!(json_digest(&envelope["state"]));
    std::fs::write(path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    let spool = Spool::open(&root, node(), Limits::default()).unwrap();
    let normal: Normalizer =
        serde_json::from_value(envelope["state"]["sources"][&source_key]["normalizer"].clone())
            .unwrap();
    let known = normal
        .seen
        .iter()
        .map(|(key, seen)| {
            (
                key.clone(),
                ReplayFact {
                    prior: seen.event.clone(),
                    selected_record_digest: Digest::of(&bytes),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let plan = ReparsePlan {
        operation_id: Uuid::from_u128(200),
        source_id: normal.source.source_id,
        source_generation: normal.source.generation,
        native_session_key: normal.source.native_session_key.clone(),
        binding: binding(),
        old_parser: normal.parser.clone(),
        new_parser: normal.source.parser(),
        retained_cursor: spool.cursor(Uuid::from_u128(8)).unwrap(),
        transcript: RetentionReceipt {
            receipt_id: Uuid::from_u128(300),
            owner: Text::new("fake-retention-owner").unwrap(),
            binding_digest: canonical_digest(&binding()).unwrap(),
            artifact: ArtifactRef {
                id: Text::new("synthetic-retained-native-prefix").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(&bytes),
            },
            role: ArtifactRole::Transcript,
            bytes: bytes.len() as u64,
            retained_at_ms: 10,
            retained_until_ms: 10000,
        },
        known,
    };
    (spool, source, bytes, plan)
}
struct Owner(Digest);
impl ReplayAuthority for Owner {
    fn admit(&self, plan: &ReparsePlan) -> Result<()> {
        ensure!(
            plan.digest()? == self.0,
            "retention owner refused exact source/plan"
        );
        Ok(())
    }
}
fn verify(plan: ReparsePlan, bytes: Vec<u8>) -> VerifiedReplay {
    let owner = Owner(plan.digest().unwrap());
    VerifiedReplay::verify(plan, bytes, 100, &owner).unwrap()
}
#[test]
fn explicit_verified_reparse_corrects_only_known_fact_and_finishes_without_duplicate_usage() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, source, bytes, plan) = fixture(&temp);
    assert!(
        spool
            .collect(plan.source_id, &source, 101, OverflowPolicy::Hold)
            .is_err(),
        "implicit parser upgrades refuse"
    );
    let verified = verify(plan.clone(), bytes);
    spool.begin_reparse(&verified).unwrap();
    spool.begin_reparse(&verified).unwrap();
    assert!(
        spool
            .collect(plan.source_id, &source, 102, OverflowPolicy::Hold)
            .is_err()
    );
    assert!(
        spool
            .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
            .is_err()
    );
    let replayed = spool.replay_next(&verified, 200).unwrap();
    assert_eq!(replayed.facts_added, 1);
    spool
        .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
        .unwrap();
    spool
        .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
        .unwrap();
    spool.begin_reparse(&verified).unwrap();
    assert_eq!(
        spool
            .collect(plan.source_id, &source, 201, OverflowPolicy::Hold)
            .unwrap()
            .facts_added,
        0
    );
    let packet = spool.prepare_packet().unwrap().unwrap();
    let Message::Batch(batch) = packet.body.message else {
        panic!()
    };
    let usages = batch
        .events
        .iter()
        .filter_map(|e| {
            if let ObservationKind::Usage { usage } = &e.body.kind {
                Some((e, usage.as_ref()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(usages.len(), 2);
    assert_eq!(usages[0].1.counters.output, Counter::Known { value: 7 });
    assert_eq!(usages[1].1.counters.output, Counter::Known { value: 5 });
    assert_eq!(
        usages[1].1.correction_of,
        Some(EventRef {
            event_id: usages[0].0.event_id,
            digest: usages[0].0.body_digest.clone()
        })
    );
    assert_eq!(
        usages[1].0.body.provenance.source_kind,
        SourceKind::RetainedReplay
    );
    let mut projection = BTreeMap::new();
    for (_, usage) in usages {
        projection.insert(
            usage.call.native_call_id.as_str(),
            usage.counters.output.clone(),
        );
    }
    assert_eq!(projection.len(), 1);
    assert_eq!(projection["call-a"], Counter::Known { value: 5 });
}
#[test]
fn retained_bytes_owner_scope_and_prior_reference_are_required() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, _, bytes, plan) = fixture(&temp);
    assert!(
        VerifiedReplay::verify(
            plan.clone(),
            bytes.clone(),
            100,
            &Owner(Digest::of(b"another-owner-plan"))
        )
        .is_err()
    );
    let mut changed = bytes.clone();
    changed[0] = b'[';
    assert!(
        VerifiedReplay::verify(plan.clone(), changed, 100, &Owner(plan.digest().unwrap())).is_err()
    );
    assert!(
        VerifiedReplay::verify(
            plan.clone(),
            bytes.clone(),
            10000,
            &Owner(plan.digest().unwrap())
        )
        .is_err()
    );
    let mut wrong = plan.clone();
    wrong.source_generation += 1;
    let handle = verify(wrong, bytes.clone());
    assert!(spool.begin_reparse(&handle).is_err());
    let mut wrong = plan.clone();
    wrong.known.values_mut().next().unwrap().prior.event_id = Uuid::from_u128(999);
    let handle = verify(wrong, bytes);
    assert!(spool.begin_reparse(&handle).is_err());
}
#[test]
fn immutable_reparse_intent_and_unknown_historical_facts_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, _, bytes, mut plan) = fixture(&temp);
    plan.known.retain(|key, _| key.starts_with("claude-call:"));
    let handle = verify(plan.clone(), bytes.clone());
    spool.begin_reparse(&handle).unwrap();
    let mut conflict = plan.clone();
    conflict.transcript.retained_until_ms += 1;
    let conflict = verify(conflict, bytes);
    assert!(spool.begin_reparse(&conflict).is_err());
    spool.replay_next(&handle, 200).unwrap();
    spool
        .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
        .unwrap();
    let packet = spool.prepare_packet().unwrap().unwrap();
    let Message::Batch(batch) = packet.body.message else {
        panic!()
    };
    assert!(batch.events.iter().any(|e| matches!(
        e.body.kind,
        ObservationKind::Gap {
            reason: GapReason::ReplayUnavailable,
            ..
        }
    )));
    assert_eq!(
        batch
            .events
            .iter()
            .filter(|e| e.body.kind.is_usage())
            .count(),
        2,
        "unknown historical observations cannot create new usage"
    );
}
#[test]
fn replay_cursor_and_correction_facts_share_crash_boundary() {
    for fault in [WriteFault::BeforeRename, WriteFault::AfterRename] {
        let temp = tempfile::tempdir().unwrap();
        let (mut spool, _, bytes, plan) = fixture(&temp);
        let verified = verify(plan.clone(), bytes);
        spool.begin_reparse(&verified).unwrap();
        spool.inject_write_fault(fault);
        assert!(spool.replay_next(&verified, 200).is_err());
        drop(spool);
        let mut spool = Spool::open(&temp.path().join("spool"), node(), Limits::default()).unwrap();
        spool.begin_reparse(&verified).unwrap();
        let next = spool.replay_next(&verified, 201).unwrap();
        assert_eq!(
            next.facts_added,
            if fault == WriteFault::BeforeRename {
                1
            } else {
                0
            }
        );
        spool
            .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
            .unwrap();
        let packet = spool.prepare_packet().unwrap().unwrap();
        let Message::Batch(batch) = packet.body.message else {
            panic!()
        };
        assert_eq!(
            batch
                .events
                .iter()
                .filter(|e| e.body.kind.is_usage())
                .count(),
            2
        );
    }
}
