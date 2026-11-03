#[allow(dead_code)]
mod support;
use aq_observation::{lifecycle::*, source::*, spool::*, *};
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf};
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
fn normalizer(format: Format) -> Normalizer {
    Normalizer::new(
        Source {
            binding: binding(),
            source_id: Uuid::from_u128(8),
            generation: 1,
            native_session_key: Text::new("native-fixture").unwrap(),
            format,
            provider: Text::new("fixture-provider").unwrap(),
            account_reference: Text::new("logical-account").unwrap(),
            registered_session_key: None,
        },
        128,
    )
    .unwrap()
}
fn rows() -> Vec<Value> {
    let f: Value = serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap();
    f["claude"].as_array().unwrap().clone()
}
fn lines(rows: &[Value]) -> Vec<u8> {
    let mut out = vec![];
    for r in rows {
        out.extend(serde_json::to_vec(r).unwrap());
        out.push(b'\n');
    }
    out
}
fn fixture(temp: &tempfile::TempDir, bytes: &[u8], limits: Limits) -> (Spool, SourceFile, PathBuf) {
    let path = temp.path().join("native.jsonl");
    std::fs::write(&path, bytes).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        temp.path(),
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    let source = SourceFile::open(permit.clone()).unwrap();
    let mut spool = support::initialize(&temp.path().join("spool"), node(), limits);
    spool
        .attach_file(normalizer(Format::ClaudeV1Jsonl9a9b8e8), permit)
        .unwrap();
    (spool, source, path)
}
fn peer() -> ConformancePeer {
    ConformancePeer::new(
        binding(),
        [OBSERVATION_CAPABILITY, USAGE_CAPABILITY]
            .iter()
            .map(|id| Capability {
                id: Text::new(*id).unwrap(),
                version: 1,
            })
            .collect(),
    )
    .unwrap()
}
fn batch_events(packet: &Packet) -> &[ObservationRecord] {
    let Message::Batch(b) = &packet.body.message else {
        panic!()
    };
    &b.events
}
#[test]
fn source_cursor_facts_and_native_dedupe_commit_atomically_and_never_store_raw_text() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = lines(&rows());
    let (mut spool, source, path) = fixture(&temp, &bytes, Limits::default());
    let result = spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(result.consumed_bytes, bytes.len() as u64);
    assert!(result.facts_added > 0);
    let packet = spool.prepare_packet().unwrap().unwrap();
    for e in batch_events(&packet) {
        e.validate().unwrap();
    }
    let stored = std::fs::read_to_string(spool.path().join("spool.json")).unwrap();
    assert!(!stored.contains("synthetic assistant text"));
    let mut coordinator = peer();
    let ack = coordinator.receive(&packet).unwrap();
    spool.acknowledge(&ack).unwrap();
    assert_eq!(spool.health().unwrap().queued_events, 0);
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let replay = spool
        .collect(Uuid::from_u128(8), &source, 200, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(replay.facts_added, 0);
    assert_eq!(
        spool.health().unwrap().acked_through,
        coordinator.last_sequence()
    );
}
#[test]
fn crash_faults_have_both_cursor_and_facts_or_neither() {
    for fault in [WriteFault::BeforeRename, WriteFault::AfterRename] {
        let temp = tempfile::tempdir().unwrap();
        let bytes = lines(&rows()[..1]);
        let (mut spool, source, _) = fixture(&temp, &bytes, Limits::default());
        spool.inject_write_fault(fault);
        assert!(
            spool
                .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
                .is_err()
        );
        assert!(spool.health().unwrap().persistence_uncertain);
        drop(spool);
        let restored = Spool::open(&temp.path().join("spool"), node(), Limits::default()).unwrap();
        if fault == WriteFault::BeforeRename {
            assert_eq!(restored.cursor(Uuid::from_u128(8)).unwrap().offset, 0);
            assert_eq!(restored.health().unwrap().queued_events, 0);
        } else {
            assert_eq!(
                restored.cursor(Uuid::from_u128(8)).unwrap().offset,
                bytes.len() as u64
            );
            assert!(restored.health().unwrap().queued_events > 0);
        }
    }
}
#[test]
fn lost_batch_ack_replays_identical_packet_after_restart_and_stale_or_changed_receipt_cannot_prune()
{
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, source, _) = fixture(&temp, &lines(&rows()), Limits::default());
    spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    let packet = spool.begin_send(100).unwrap().unwrap();
    let mut coordinator = peer();
    let receipt = coordinator.receive(&packet).unwrap();
    drop(spool);
    let mut spool = Spool::open(&temp.path().join("spool"), node(), Limits::default()).unwrap();
    assert!(spool.begin_send(99).is_err());
    let replay = spool.begin_send(350).unwrap().unwrap();
    assert_eq!(packet, replay);
    assert_eq!(coordinator.receive(&replay).unwrap(), receipt);
    let mut wrong = receipt.clone();
    wrong.body_digest = Digest::of(b"different body");
    assert!(spool.acknowledge(&wrong).is_err());
    assert!(spool.health().unwrap().queued_events > 0);
    spool.acknowledge(&receipt).unwrap();
    spool.acknowledge(&receipt).unwrap();
    assert_eq!(spool.health().unwrap().queued_events, 0);
}
#[test]
fn overflow_holds_cursor_or_records_reserved_gap_and_holds_reconciliation_while_control_remains_readable()
 {
    let temp = tempfile::tempdir().unwrap();
    let limits = Limits {
        ordinary_events: 2,
        ..Limits::default()
    };
    let (mut spool, source, _) = fixture(&temp, &lines(&rows()), limits);
    let first = spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    assert!(first.blocked);
    assert_eq!(first.consumed_bytes, lines(&rows()[..1]).len() as u64);
    let before = spool.cursor(Uuid::from_u128(8)).unwrap();
    let status = spool.health().unwrap();
    assert_eq!(status.queued_events, 2);
    assert_eq!(status.reserved_gap_events, 0);
    let dropped = spool
        .collect(Uuid::from_u128(8), &source, 101, OverflowPolicy::GapAndHold)
        .unwrap();
    assert!(dropped.blocked && dropped.consumed_bytes > 0);
    let control = spool.health().unwrap();
    assert_eq!(control.reserved_gap_events, 1);
    assert_eq!(control.held_sources, 1);
    assert!(spool.cursor(Uuid::from_u128(8)).unwrap().offset > before.offset);
    assert!(
        spool
            .collect(Uuid::from_u128(8), &source, 102, OverflowPolicy::Hold)
            .is_err()
    );
    let packet = spool.begin_send(1000).unwrap().unwrap();
    assert!(batch_events(&packet).iter().any(|e| matches!(
        e.body.kind,
        ObservationKind::Gap {
            reason: GapReason::SpoolOverflow,
            ..
        }
    )));
}
#[test]
fn missing_rotation_and_truncation_are_durable_gaps_not_empty_transcript_success() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, source, path) = fixture(&temp, &lines(&rows()[..1]), Limits::default());
    spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    let missing = spool
        .collect(Uuid::from_u128(8), &source, 101, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(missing.source_change, Some(SourceChange::Missing));
    assert_eq!(missing.facts_added, 1);
    assert_eq!(
        spool
            .collect(Uuid::from_u128(8), &source, 102, OverflowPolicy::Hold)
            .unwrap()
            .facts_added,
        0
    );
    std::fs::write(&path, lines(&rows()[..1])).unwrap();
    assert_eq!(
        spool
            .collect(Uuid::from_u128(8), &source, 103, OverflowPolicy::Hold)
            .unwrap()
            .source_change,
        Some(SourceChange::Rotated)
    );
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        temp.path(),
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    spool
        .adopt_file_generation(Uuid::from_u128(8), permit.clone(), 2, 104)
        .unwrap();
    assert!(
        spool
            .collect(Uuid::from_u128(8), &source, 105, OverflowPolicy::Hold)
            .is_err()
    );
    let new_source = SourceFile::open(permit).unwrap();
    spool
        .collect(Uuid::from_u128(8), &new_source, 106, OverflowPolicy::Hold)
        .unwrap();
    let packet = spool.prepare_packet().unwrap().unwrap();
    assert!(batch_events(&packet).iter().any(|e| matches!(
        e.body.kind,
        ObservationKind::Gap {
            reason: GapReason::SourceMissing,
            ..
        }
    )));
    assert_eq!(
        batch_events(&packet)
            .iter()
            .filter(|e| e.body.kind.is_usage())
            .count(),
        1,
        "replayed history on new inode cannot create another usage fact"
    );
}
#[test]
fn retry_exhaustion_keeps_facts_and_accepts_exact_late_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let limits = Limits {
        max_send_attempts: 2,
        ..Limits::default()
    };
    let (mut spool, source, _) = fixture(&temp, &lines(&rows()[..1]), limits);
    spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    let packet = spool.begin_send(100).unwrap().unwrap();
    assert_eq!(spool.begin_send(350).unwrap().unwrap(), packet);
    assert!(spool.begin_send(1000).is_err());
    assert!(spool.health().unwrap().queued_events > 0);
    let receipt = peer().receive(&packet).unwrap();
    spool.acknowledge(&receipt).unwrap();
    assert_eq!(spool.health().unwrap().queued_events, 0);
}
#[test]
fn public_record_cursor_cas_and_native_record_provenance_use_no_private_database() {
    let temp = tempfile::tempdir().unwrap();
    let mut spool = support::initialize(&temp.path().join("spool"), node(), Limits::default());
    let normal = normalizer(Format::OpenCodeSessionDto6c24ec9)
        .bind_scoped_record_channel("native-fixture")
        .unwrap();
    spool.attach_record_channel(normal).unwrap();
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/native-synthetic.json")).unwrap();
    let bytes = serde_json::to_vec(&fixtures["opencode"][0]).unwrap();
    spool
        .ingest_record(
            Uuid::from_u128(8),
            None,
            Text::new("public-cursor-1").unwrap(),
            bytes.clone(),
            100,
        )
        .unwrap();
    assert!(
        spool
            .ingest_record(
                Uuid::from_u128(8),
                None,
                Text::new("public-cursor-2").unwrap(),
                bytes.clone(),
                101
            )
            .is_err()
    );
    assert_eq!(
        spool
            .ingest_record(
                Uuid::from_u128(8),
                Some("public-cursor-1"),
                Text::new("public-cursor-2").unwrap(),
                bytes,
                102
            )
            .unwrap(),
        0
    );
    let packet = spool.prepare_packet().unwrap().unwrap();
    assert!(
        batch_events(&packet)
            .iter()
            .all(|e| matches!(e.body.provenance.cursor, SourceCursor::Record { .. }))
    );
}
#[test]
fn journal_corruption_and_orphan_temporary_refuse_without_deleting_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let (spool, _, _) = fixture(&temp, b"", Limits::default());
    let path = spool.path().to_owned();
    drop(spool);
    let original = std::fs::read(path.join("spool.json")).unwrap();
    let mut value: Value = serde_json::from_slice(&original).unwrap();
    value["state"]["next_sequence"] = 99.into();
    std::fs::write(path.join("spool.json"), serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(Spool::open(&path, node(), Limits::default()).is_err());
    std::fs::write(path.join("spool.json"), original).unwrap();
    std::fs::write(path.join("pending.tmp"), b"uncertain local commit").unwrap();
    assert!(Spool::open(&path, node(), Limits::default()).is_err());
    assert_eq!(
        std::fs::read(path.join("pending.tmp")).unwrap(),
        b"uncertain local commit"
    );
}

#[test]
fn frozen_pending_packet_does_not_change_when_new_facts_arrive_and_changed_id_payload_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, source, path) = fixture(&temp, &lines(&rows()[..1]), Limits::default());
    spool
        .collect(Uuid::from_u128(8), &source, 100, OverflowPolicy::Hold)
        .unwrap();
    let first = spool.prepare_packet().unwrap().unwrap();
    let mut peer = peer();
    let receipt = peer.receive(&first).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(&lines(&rows()[1..2]))
        .unwrap();
    spool
        .collect(Uuid::from_u128(8), &source, 101, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(spool.prepare_packet().unwrap().unwrap(), first);
    let mut changed = first.clone();
    let Message::Batch(batch) = &mut changed.body.message else {
        panic!()
    };
    let ObservationKind::Usage { usage } = &mut batch.events[0].body.kind else {
        panic!()
    };
    usage.counters.input = Counter::Known { value: 999 };
    batch.events[0].body_digest = canonical_digest(&batch.events[0].body).unwrap();
    changed.body_digest = canonical_digest(&changed.body).unwrap();
    changed.validate().unwrap();
    assert!(peer.receive(&changed).is_err());
    spool.acknowledge(&receipt).unwrap();
    let second = spool.prepare_packet().unwrap().unwrap();
    assert_ne!(first.body.message_id, second.body.message_id);
    assert_eq!(batch_events(&second)[0].body.sequence, 3);
    spool.acknowledge(&receipt).unwrap();
    assert_eq!(spool.prepare_packet().unwrap().unwrap(), second);
}
#[test]
fn multiple_sources_share_one_node_sequence_without_cross_binding_or_capability_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let (mut spool, first, _) = fixture(&temp, &lines(&rows()[..1]), Limits::default());
    let mut second_normal = normalizer(Format::ClaudeV1Jsonl9a9b8e8);
    second_normal.source.source_id = Uuid::from_u128(9);
    second_normal.source.native_session_key = Text::new("native-other").unwrap();
    second_normal.source.binding.attempt_id = Uuid::from_u128(40);
    second_normal.source.binding.session = SessionIdentity::Remote {
        node_id: node().node_id,
        boot_id: node().boot_id,
        attempt_id: Uuid::from_u128(40),
        assignment_generation: 1,
        session_id: Uuid::from_u128(70),
    };
    let mut row = rows()[0].clone();
    row["sessionId"] = "native-other".into();
    std::fs::write(temp.path().join("second.jsonl"), lines(&[row])).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(9),
        temp.path(),
        "second.jsonl",
        Text::new("native-other").unwrap(),
    )
    .unwrap();
    spool.attach_file(second_normal, permit.clone()).unwrap();
    let second = SourceFile::open(permit).unwrap();
    spool
        .collect(Uuid::from_u128(8), &first, 100, OverflowPolicy::Hold)
        .unwrap();
    spool
        .collect(Uuid::from_u128(9), &second, 101, OverflowPolicy::Hold)
        .unwrap();
    let packet = spool.prepare_packet().unwrap().unwrap();
    assert_eq!(
        batch_events(&packet)
            .iter()
            .map(|e| e.body.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_ne!(
        batch_events(&packet)[0].body.binding.attempt_id,
        batch_events(&packet)[2].body.binding.attempt_id
    );
    let mut unsupported = ConformancePeer::new(
        binding(),
        vec![Capability {
            id: Text::new(OBSERVATION_CAPABILITY).unwrap(),
            version: 1,
        }],
    )
    .unwrap();
    assert!(unsupported.receive(&packet).is_err());
    assert_eq!(spool.health().unwrap().queued_events, 4);
}
