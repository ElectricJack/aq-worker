#[allow(dead_code)]
mod support;
use aq_observation_engine::{self as engine, framing::Frame, rows::Table};
use aq_observation_redb_prototype::{Limits, Store};
use uuid::Uuid;
fn source() -> engine::Source {
    serde_json::from_value(serde_json::to_value(support::source(8)).unwrap()).unwrap()
}
#[test]
fn normalized_delta_cursor_seen_facts_and_delivery_survive_atomic_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach_engine(source(), 4096, false).unwrap();
    let bytes = support::row(1);
    let f = Frame {
        start: 0,
        bytes: bytes.clone(),
        oversized: false,
    };
    let result = store.collect_frame(Uuid::from_u128(8), &f, 100).unwrap();
    assert_eq!(result.consumed_bytes, bytes.len() as u64);
    assert_eq!(result.facts_added, 1);
    let row = store.source(Uuid::from_u128(8)).unwrap();
    assert_eq!(row.engine.as_ref().unwrap().revision, 2);
    assert_eq!(row.engine.as_ref().unwrap().counts[&Table::Seen], 1);
    let packet = store.prepare().unwrap().unwrap();
    store.audit().unwrap();
    drop(store);
    let store = Store::open(&root, Limits::fixture()).unwrap();
    assert_eq!(store.source(Uuid::from_u128(8)).unwrap(), row);
    assert_eq!(store.prepare().unwrap().unwrap(), packet);
    assert!(store.collect_frame(Uuid::from_u128(8), &f, 100).is_err());
    let replay = Frame {
        start: bytes.len() as u64,
        bytes,
        oversized: false,
    };
    assert_eq!(
        store
            .collect_frame(Uuid::from_u128(8), &replay, 101)
            .unwrap()
            .facts_added,
        0
    );
    assert_eq!(store.snapshot().unwrap().next, 2);
    let mut peer = support::peer();
    let receipt = peer.receive(&packet).unwrap();
    store.acknowledge(&receipt).unwrap();
    store.audit().unwrap();
}
#[test]
fn raw_bypass_and_wrong_native_scope_are_refused_and_logical_quota_rolls_back_whole_delta() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach_engine(source(), 4096, false).unwrap();
    let (facts, seen) = support::normalize(1, 1);
    assert!(
        store
            .append(Uuid::from_u128(8), 0, 1, &facts, &seen)
            .is_err()
    );
    drop(store);
    let store = Store::open(&root, Limits::fixture()).unwrap();
    let before = store.source(Uuid::from_u128(8)).unwrap();
    let mut foreign: serde_json::Value = serde_json::from_slice(&support::row(1)).unwrap();
    foreign["sessionId"] = "foreign-native-key".into();
    assert!(
        store
            .collect_frame(
                Uuid::from_u128(8),
                &Frame {
                    start: 0,
                    bytes: serde_json::to_vec(&foreign).unwrap(),
                    oversized: false
                },
                100
            )
            .is_err()
    );
    assert_eq!(store.source(Uuid::from_u128(8)).unwrap(), before);
    let mut limits = Limits::fixture();
    limits.logical_bytes = 4096;
    let quota = temp.path().join("quota");
    let store = Store::create(&quota, limits.clone()).unwrap();
    store.attach_engine(source(), 4096, false).unwrap();
    let row = store.source(Uuid::from_u128(8)).unwrap();
    let snapshot = store.snapshot().unwrap();
    let result = store.collect_frame(
        Uuid::from_u128(8),
        &Frame {
            start: 0,
            bytes: support::row(1),
            oversized: false,
        },
        100,
    );
    assert!(result.is_err());
    drop(store);
    let store = Store::open(&quota, limits).unwrap();
    assert_eq!(store.source(Uuid::from_u128(8)).unwrap(), row);
    assert_eq!(store.snapshot().unwrap(), snapshot);
    store.audit().unwrap();
}
fn frames(first: u64, count: u64, start: u64) -> Vec<Frame> {
    let mut offset = start;
    let mut frames = vec![];
    for call in first..first + count {
        let bytes = support::row(call);
        frames.push(Frame {
            start: offset,
            bytes: bytes.clone(),
            oversized: false,
        });
        offset += bytes.len() as u64;
    }
    frames
}
#[test]
fn thirty_two_record_batch_commits_once_and_quota_commits_only_accepted_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach_engine(source(), 4096, false).unwrap();
    let input = frames(0, 32, 0);
    store.monitor().reset();
    let result = store
        .collect_batch(Uuid::from_u128(8), &input, 100)
        .unwrap();
    assert_eq!(result.facts_added, 32);
    assert!(!result.blocked);
    assert_eq!(
        store.monitor().stats().syncs,
        2,
        "one two-phase transaction for32records"
    );
    assert_eq!(
        store
            .source(Uuid::from_u128(8))
            .unwrap()
            .engine
            .unwrap()
            .revision,
        2
    );
    store.audit().unwrap();
    let mut cursor = store.source(Uuid::from_u128(8)).unwrap().cursor;
    for i in 1..7 {
        let input = frames(i * 32, 32, cursor);
        let accepted = store
            .collect_batch(Uuid::from_u128(8), &input, 100)
            .unwrap();
        assert_eq!(accepted.facts_added, 32);
        cursor += accepted.consumed_bytes;
    }
    let tail = store
        .collect_batch(Uuid::from_u128(8), &frames(224, 27, cursor), 100)
        .unwrap();
    cursor += tail.consumed_bytes;
    assert_eq!(store.snapshot().unwrap().queued, 251);
    let prefix = store
        .collect_batch(Uuid::from_u128(8), &frames(1000, 32, cursor), 100)
        .unwrap();
    assert_eq!(prefix.facts_added, 5);
    assert!(prefix.blocked);
    cursor += prefix.consumed_bytes;
    assert_eq!(store.snapshot().unwrap().queued, 256);
    let blocked = store
        .collect_batch(Uuid::from_u128(8), &frames(1005, 27, cursor), 100)
        .unwrap();
    assert_eq!(blocked.consumed_bytes, 0);
    assert!(blocked.blocked);
    let packet = store.prepare().unwrap().unwrap();
    store.acknowledge(&support::receipt(&packet)).unwrap();
    let accepted = store
        .collect_batch(Uuid::from_u128(8), &frames(1005, 27, cursor), 100)
        .unwrap();
    assert_eq!(accepted.facts_added, 27);
    assert!(!accepted.blocked);
    store.audit().unwrap();
}
#[test]
fn batch_retains_no_partial_source_state_when_foreign_record_follows_valid_record() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach_engine(source(), 4096, false).unwrap();
    let before = store.source(Uuid::from_u128(8)).unwrap();
    let mut input = frames(1, 2, 0);
    let mut foreign: serde_json::Value = serde_json::from_slice(&input[1].bytes).unwrap();
    foreign["sessionId"] = "foreign-session".into();
    input[1].bytes = serde_json::to_vec(&foreign).unwrap();
    assert!(
        store
            .collect_batch(Uuid::from_u128(8), &input, 100)
            .is_err()
    );
    assert_eq!(store.source(Uuid::from_u128(8)).unwrap(), before);
    assert_eq!(store.snapshot().unwrap().queued, 0);
}
#[test]
fn ordinary_quota_cannot_consume_reserved_gap_loss_is_atomic_and_fences_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach_engine(source(), 4096, false).unwrap();
    let mut cursor = 0;
    for i in 0..8 {
        let result = store
            .collect_batch(Uuid::from_u128(8), &frames(i * 32, 32, cursor), 100)
            .unwrap();
        cursor += result.consumed_bytes;
    }
    assert_eq!(store.snapshot().unwrap().queued, 256);
    let lost = frames(1000, 1, cursor);
    let result = store
        .collect_batch_policy(
            Uuid::from_u128(8),
            &lost,
            101,
            aq_observation::spool::OverflowPolicy::GapAndHold,
        )
        .unwrap();
    assert!(result.blocked);
    assert_eq!(result.facts_added, 1);
    assert_eq!(result.consumed_bytes, lost[0].bytes.len() as u64);
    let row = store.source(Uuid::from_u128(8)).unwrap();
    assert_eq!(
        row.hold,
        Some(aq_observation_redb_prototype::HoldReason::SpoolLoss)
    );
    assert!(
        store
            .collect_frame(Uuid::from_u128(8), &frames(1001, 1, row.cursor)[0], 102)
            .is_err()
    );
    store.audit().unwrap();
    drop(store);
    let store = Store::open(&root, Limits::fixture()).unwrap();
    let mut gap = None;
    while let Some(packet) = store.prepare().unwrap() {
        let aq_lifecycle_protocol::Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        for event in &batch.events {
            if matches!(
                event.body.kind,
                aq_lifecycle_protocol::ObservationKind::Gap {
                    reason: aq_lifecycle_protocol::GapReason::SpoolOverflow,
                    ..
                }
            ) {
                gap = Some(event.clone());
            }
        }
        store.acknowledge(&support::receipt(&packet)).unwrap();
    }
    let gap = gap.unwrap();
    assert_eq!(
        gap.body.provenance.cursor,
        aq_lifecycle_protocol::SourceCursor::Bytes {
            start: cursor,
            end: cursor + lost[0].bytes.len() as u64
        }
    );
    assert_eq!(store.snapshot().unwrap().acked, 257);
    store.audit().unwrap();
}
