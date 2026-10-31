//! Synthetic retained parser-error state; not evidence of any released parser.
#[allow(dead_code)]
mod support;
use anyhow::{Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation_engine::{self as engine, framing::Frame};
use aq_observation_redb_prototype::{
    Limits, ReplayAuthority, ReplayPlan, SourceRow, Store, VerifiedRetained,
};
use redb::{Durability, ReadableTable, TableDefinition};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
static REPARSE_TEST_OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const SOURCES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("sources");
const SEEN: TableDefinition<&[u8], &[u8]> = TableDefinition::new("seen");
const OUT: TableDefinition<u64, &[u8]> = TableDefinition::new("outbox");
const IDS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("event_ids");
fn fixture(temp: &tempfile::TempDir, unknown_history: bool) -> (Store, Vec<u8>, ReplayPlan) {
    let root = temp.path().join("rows");
    let raw = support::row(1);
    let source: engine::Source =
        serde_json::from_value(serde_json::to_value(support::source(8)).unwrap()).unwrap();
    let new_parser = source.parser();
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach_engine(source, 4096, false).unwrap();
    store
        .collect_frame(
            Uuid::from_u128(8),
            &Frame {
                start: 0,
                bytes: raw.clone(),
                oversized: false,
            },
            100,
        )
        .unwrap();
    drop(store);
    let old_parser = ArtifactRef {
        id: Text::new("synthetic-retained-parser-error").unwrap(),
        version: Text::new("0").unwrap(),
        digest: Digest::of(b"synthetic-input7-from-native12"),
    };
    let db = redb::Database::open(root.join("store.redb")).unwrap();
    let mut tx = db.begin_write().unwrap();
    tx.set_durability(Durability::Immediate).unwrap();
    tx.set_two_phase_commit(true);
    let mut sources = tx.open_table(SOURCES).unwrap();
    let old_source = sources
        .get(Uuid::from_u128(8).as_bytes().as_slice())
        .unwrap()
        .unwrap()
        .value()
        .to_vec();
    let mut row: SourceRow = serde_json::from_slice(&old_source).unwrap();
    let mut bytes = raw.clone();
    if unknown_history {
        bytes.extend(support::row(999));
    }
    row.cursor = bytes.len() as u64;
    row.engine.as_mut().unwrap().context.parser = old_parser.clone();
    row.parsers = vec![old_parser.clone()];
    let source_bytes = serde_json::to_vec(&row).unwrap();
    sources
        .insert(
            Uuid::from_u128(8).as_bytes().as_slice(),
            source_bytes.as_slice(),
        )
        .unwrap();
    drop(sources);
    let mut out = tx.open_table(OUT).unwrap();
    let old_event = out.get(1).unwrap().unwrap().value().to_vec();
    let mut event: ObservationRecord = serde_json::from_slice(&old_event).unwrap();
    let old_id = event.event_id;
    event.event_id = Uuid::from_u128(777);
    event.body.provenance.parser = old_parser.clone();
    let ObservationKind::Usage { usage } = &mut event.body.kind else {
        panic!()
    };
    usage.counters.input = Counter::Known { value: 7 };
    usage.category_semantics = CategorySemantics::MayOverlap {
        policy: old_parser.clone(),
    };
    event.body_digest = canonical_digest(&event.body).unwrap();
    let event_bytes = serde_json::to_vec(&event).unwrap();
    out.insert(1, event_bytes.as_slice()).unwrap();
    drop(out);
    let mut ids = tx.open_table(IDS).unwrap();
    ids.remove(old_id.as_bytes().as_slice()).unwrap();
    let digest = serde_json::to_vec(&event.body_digest).unwrap();
    ids.insert(event.event_id.as_bytes().as_slice(), digest.as_slice())
        .unwrap();
    drop(ids);
    let mut seen = tx.open_table(SEEN).unwrap();
    let mut key = Uuid::from_u128(8).as_bytes().to_vec();
    key.extend(b"claude-call:call-1");
    let old_seen = seen.get(key.as_slice()).unwrap().unwrap().value().to_vec();
    let mut seen_value: Value = serde_json::from_slice(&old_seen).unwrap();
    seen_value["normalized_digest"] = json!(canonical_digest(&event.body.kind).unwrap());
    let prior = EventRef {
        event_id: event.event_id,
        digest: event.body_digest.clone(),
    };
    seen_value["event"] = json!(prior);
    let seen_bytes = serde_json::to_vec(&seen_value).unwrap();
    seen.insert(key.as_slice(), seen_bytes.as_slice()).unwrap();
    drop(seen);
    let mut meta = tx.open_table(META).unwrap();
    let mut state: Value =
        serde_json::from_slice(meta.get("state").unwrap().unwrap().value()).unwrap();
    state["logical_bytes"] = json!(
        state["logical_bytes"].as_u64().unwrap()
            - (old_source.len() + old_event.len() + old_seen.len()) as u64
            + (source_bytes.len() + event_bytes.len() + seen_bytes.len()) as u64
    );
    state["ordinary_bytes"] = json!(event_bytes.len());
    let state_bytes = serde_json::to_vec(&state).unwrap();
    meta.insert("state", state_bytes.as_slice()).unwrap();
    drop(meta);
    tx.commit().unwrap();
    drop(db);
    let store = Store::open(&root, Limits::fixture()).unwrap();
    let plan = ReplayPlan {
        operation_id: Uuid::from_u128(900),
        source_id: Uuid::from_u128(8),
        source_generation: 1,
        binding: support::binding(),
        native_session_key: Text::new("native-fixture").unwrap(),
        old_parser,
        new_parser,
        retained_cursor: bytes.len() as u64,
        transcript: RetentionReceipt {
            receipt_id: Uuid::from_u128(901),
            owner: Text::new("synthetic-retention-owner").unwrap(),
            binding_digest: canonical_digest(&support::binding()).unwrap(),
            artifact: ArtifactRef {
                id: Text::new("synthetic-retained-prefix").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(&bytes),
            },
            role: ArtifactRole::Transcript,
            bytes: bytes.len() as u64,
            retained_at_ms: 10,
            retained_until_ms: 10000,
        },
        known: BTreeMap::from([(
            "claude-call:call-1".into(),
            engine::ReplayFact {
                prior,
                selected_record_digest: Digest::of(&raw),
            },
        )]),
    };
    (store, bytes, plan)
}
struct Owner(Digest);
impl ReplayAuthority for Owner {
    fn admit(&self, p: &ReplayPlan) -> Result<()> {
        ensure!(p.digest()? == self.0, "owner refused source/raw/plan scope");
        Ok(())
    }
}
fn verified(plan: ReplayPlan, bytes: Vec<u8>) -> VerifiedRetained {
    let owner = Owner(plan.digest().unwrap());
    VerifiedRetained::verify(plan, bytes, 100, &owner).unwrap()
}
#[test]
fn verified_known_prior_correction_retains_old_parser_and_live_cursor_then_finishes_once() {
    let _test_owner = REPARSE_TEST_OWNER.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (store, bytes, plan) = fixture(&temp, false);
    let cursor = store.source(plan.source_id).unwrap().cursor;
    let replay = verified(plan.clone(), bytes);
    store.begin_reparse(&replay).unwrap();
    store.begin_reparse(&replay).unwrap();
    store.replay_next(&replay, 101).unwrap();
    assert_eq!(store.source(plan.source_id).unwrap().cursor, cursor);
    let packet = store.prepare().unwrap().unwrap();
    let Message::Batch(batch) = packet.body.message else {
        panic!()
    };
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.events[0].body.provenance.parser, plan.old_parser);
    let ObservationKind::Usage { usage } = &batch.events[1].body.kind else {
        panic!()
    };
    assert_eq!(usage.counters.input, Counter::Known { value: 12 });
    assert_eq!(
        usage.correction_of,
        Some(plan.known.values().next().unwrap().prior.clone())
    );
    assert_eq!(batch.events[1].body.provenance.parser, plan.new_parser);
    store
        .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
        .unwrap();
    store.begin_reparse(&replay).unwrap();
    store
        .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
        .unwrap();
    store.audit().unwrap();
    drop(store);
    let store = Store::open(&temp.path().join("rows"), Limits::fixture()).unwrap();
    assert_eq!(store.snapshot().unwrap().queued, 2);
}
#[test]
fn unknown_historical_records_emit_gaps_and_never_new_usage_charge() {
    let _test_owner = REPARSE_TEST_OWNER.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (store, bytes, plan) = fixture(&temp, true);
    let replay = verified(plan.clone(), bytes);
    store.begin_reparse(&replay).unwrap();
    let result = store.replay_next(&replay, 101).unwrap();
    assert_eq!(result.facts_added, 2);
    store
        .finish_reparse(plan.source_id, plan.operation_id, &plan.digest().unwrap())
        .unwrap();
    let packet = store.prepare().unwrap().unwrap();
    let Message::Batch(batch) = packet.body.message else {
        panic!()
    };
    assert_eq!(
        batch
            .events
            .iter()
            .filter(|e| matches!(e.body.kind, ObservationKind::Usage { .. }))
            .count(),
        2
    );
    assert!(batch.events.iter().any(|e| matches!(
        e.body.kind,
        ObservationKind::Gap {
            reason: GapReason::ReplayUnavailable,
            ..
        }
    )));
    store.audit().unwrap();
}
#[test]
fn wrong_bytes_owner_prior_and_changed_intent_refuse_without_cursor_advance() {
    let _test_owner = REPARSE_TEST_OWNER.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let (store, bytes, plan) = fixture(&temp, false);
    let mut wrong = bytes.clone();
    wrong[0] = b'x';
    assert!(
        VerifiedRetained::verify(plan.clone(), wrong, 100, &Owner(plan.digest().unwrap())).is_err()
    );
    assert!(
        VerifiedRetained::verify(
            plan.clone(),
            bytes.clone(),
            100,
            &Owner(Digest::of(b"other owner plan"))
        )
        .is_err()
    );
    let other = tempfile::tempdir().unwrap();
    let (other_store, other_bytes, mut wrong_prior) = fixture(&other, false);
    wrong_prior
        .known
        .values_mut()
        .next()
        .unwrap()
        .prior
        .event_id = Uuid::from_u128(12345);
    let wrong_prior = verified(wrong_prior, other_bytes);
    assert!(other_store.begin_reparse(&wrong_prior).is_err());
    drop(other_store);
    let reopened = Store::open(&other.path().join("rows"), Limits::fixture()).unwrap();
    assert!(
        reopened
            .source(Uuid::from_u128(8))
            .unwrap()
            .engine
            .unwrap()
            .replay
            .is_none()
    );
    let replay = verified(plan.clone(), bytes.clone());
    store.begin_reparse(&replay).unwrap();
    let mut changed = plan.clone();
    changed.transcript.receipt_id = Uuid::from_u128(999);
    let changed = verified(changed, bytes);
    assert!(store.begin_reparse(&changed).is_err());
    drop(store);
    let store = Store::open(&temp.path().join("rows"), Limits::fixture()).unwrap();
    assert_eq!(
        store
            .source(plan.source_id)
            .unwrap()
            .engine
            .unwrap()
            .replay
            .unwrap()
            .offset,
        0
    );
    assert!(store.replay_next(&replay, 10000).is_err());
}
fn replay_phase(store: &Store, replay: &VerifiedRetained, phase: &str) -> Result<()> {
    match phase {
        "begin" => store.begin_reparse(replay),
        "record" => store.replay_next(replay, 101).map(|_| ()),
        "finish" => store.finish_reparse(
            replay.plan().source_id,
            replay.plan().operation_id,
            &replay.plan().digest()?,
        ),
        _ => panic!(),
    }
}
fn projection(store: &Store) -> (aq_observation_redb_prototype::Snapshot, SourceRow) {
    (
        store.snapshot().unwrap(),
        store.source(Uuid::from_u128(8)).unwrap(),
    )
}
fn copy_db(from: &std::path::Path, to: &std::path::Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(to).unwrap();
    std::fs::copy(from.join("store.redb"), to.join("store.redb")).unwrap();
    std::fs::copy(from.join("store.format"), to.join("store.format")).unwrap();
}
#[test]
fn replay_crash_child() {
    let _test_owner = REPARSE_TEST_OWNER.lock().unwrap();
    let Ok(root) = std::env::var("AQ_REPLAY_CRASH_ROOT") else {
        return;
    };
    let plan: ReplayPlan =
        serde_json::from_slice(&std::fs::read(std::env::var("AQ_REPLAY_PLAN").unwrap()).unwrap())
            .unwrap();
    let bytes = std::fs::read(std::env::var("AQ_REPLAY_BYTES").unwrap()).unwrap();
    let replay = verified(plan, bytes);
    let store = Store::open(std::path::Path::new(&root), Limits::fixture()).unwrap();
    store.monitor().reset();
    store
        .monitor()
        .fault(aq_observation_redb_prototype::backend::Fault {
            operation: std::env::var("AQ_REPLAY_CUT").unwrap().parse().unwrap(),
            after: std::env::var("AQ_REPLAY_AFTER").unwrap() == "true",
            action: aq_observation_redb_prototype::backend::Action::Exit,
        });
    replay_phase(&store, &replay, &std::env::var("AQ_REPLAY_PHASE").unwrap()).unwrap();
    panic!("replay crash cut not reached");
}
#[test]
fn replay_intent_cursor_corrections_and_finish_share_every_mutating_crash_boundary() {
    let _test_owner = REPARSE_TEST_OWNER.lock().unwrap();
    let mut io_cuts = 0;
    let mut process_cuts = 0;
    for phase in ["begin", "record", "finish"] {
        let temp = tempfile::tempdir().unwrap();
        let (store, bytes, plan) = fixture(&temp, false);
        let replay = verified(plan.clone(), bytes.clone());
        if phase != "begin" {
            store.begin_reparse(&replay).unwrap();
        }
        if phase == "finish" {
            store.replay_next(&replay, 101).unwrap();
        }
        let before = projection(&store);
        drop(store);
        let base = temp.path().join("rows");
        let plan_path = temp.path().join("plan.json");
        let bytes_path = temp.path().join("retained.jsonl");
        std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
        std::fs::write(&bytes_path, &bytes).unwrap();
        let successful = temp.path().join("successful");
        copy_db(&base, &successful);
        let store = Store::open(&successful, Limits::fixture()).unwrap();
        store.monitor().reset();
        replay_phase(&store, &replay, phase).unwrap();
        let operations = store.monitor().stats().operations;
        let after = projection(&store);
        drop(store);
        for process in [false, true] {
            for cut in 1..=operations {
                for is_after in [false, true] {
                    let path = temp.path().join(format!("cut-{process}-{cut}-{is_after}"));
                    copy_db(&base, &path);
                    if process {
                        let status = std::process::Command::new(std::env::current_exe().unwrap())
                            .args(["--exact", "replay_crash_child", "--nocapture"])
                            .env("AQ_REPLAY_CRASH_ROOT", &path)
                            .env("AQ_REPLAY_PLAN", &plan_path)
                            .env("AQ_REPLAY_BYTES", &bytes_path)
                            .env("AQ_REPLAY_CUT", cut.to_string())
                            .env("AQ_REPLAY_AFTER", is_after.to_string())
                            .env("AQ_REPLAY_PHASE", phase)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .unwrap();
                        assert_eq!(status.code(), Some(77));
                        process_cuts += 1;
                    } else {
                        let store = Store::open(&path, Limits::fixture()).unwrap();
                        store.monitor().reset();
                        store
                            .monitor()
                            .fault(aq_observation_redb_prototype::backend::Fault {
                                operation: cut,
                                after: is_after,
                                action: aq_observation_redb_prototype::backend::Action::Error,
                            });
                        assert!(replay_phase(&store, &replay, phase).is_err());
                        drop(store);
                        io_cuts += 1;
                    }
                    let store = Store::open(&path, Limits::fixture()).unwrap();
                    let actual = projection(&store);
                    assert!(
                        actual == before || actual == after,
                        "partial replay {phase}/{cut}/{is_after}"
                    );
                    store.audit().unwrap();
                    drop(store);
                    std::fs::remove_dir_all(path).unwrap();
                }
            }
        }
    }
    eprintln!("reparse IO-error cuts:{io_cuts};process-exit cuts:{process_cuts}");
}
