#[allow(dead_code)]
mod support;
use aq_lifecycle_protocol::*;
use aq_observation::{source::SourceFile, spool::OverflowPolicy};
use aq_observation_engine::{self as engine, framing::Frame};
use aq_observation_redb_prototype::{Limits, Store};
use std::io::Write;
use uuid::Uuid;
fn source() -> engine::Source {
    serde_json::from_value(serde_json::to_value(support::source(8)).unwrap()).unwrap()
}
fn fixture(temp: &tempfile::TempDir, bytes: &[u8]) -> (Store, SourceFile, std::path::PathBuf) {
    let native = temp.path().join("native");
    std::fs::create_dir(&native).unwrap();
    let path = native.join("native.jsonl");
    std::fs::write(&path, bytes).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        &native,
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    let file = SourceFile::open(permit.clone()).unwrap();
    let store = Store::create(&temp.path().join("rows"), Limits::fixture()).unwrap();
    store.attach_file_engine(source(), 4096, permit).unwrap();
    (store, file, path)
}
#[test]
fn file_partial_idle_poll_restart_and_oversized_source_keep_explicit_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let raw = support::row(1);
    let (partial, tail) = raw.split_at(raw.len() - 2);
    let (store, file, path) = fixture(&temp, partial);
    store.monitor().reset();
    let result = store
        .collect_file(Uuid::from_u128(8), &file, 100, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(result.consumed_bytes, 0);
    assert_eq!(store.monitor().stats().syncs, 0);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail)
        .unwrap();
    let result = store
        .collect_file(Uuid::from_u128(8), &file, 101, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(result.consumed_bytes, raw.len() as u64);
    assert_eq!(result.facts_added, 1);
    assert!(
        store
            .source(Uuid::from_u128(8))
            .unwrap()
            .reader
            .unwrap()
            .cursor
            .anchor
            .is_some()
    );
    drop(store);
    let store = Store::open(&temp.path().join("rows"), Limits::fixture()).unwrap();
    assert_eq!(
        store
            .collect_file(Uuid::from_u128(8), &file, 102, OverflowPolicy::Hold)
            .unwrap()
            .consumed_bytes,
        0
    );
    let block = vec![b'x'; engine::framing::MAX_RECORD_BYTES];
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&block)
        .unwrap();
    let result = store
        .collect_file(Uuid::from_u128(8), &file, 103, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(result.facts_added, 1);
    assert!(
        store
            .source(Uuid::from_u128(8))
            .unwrap()
            .reader
            .unwrap()
            .discarding
    );
    store.audit().unwrap();
}
#[test]
fn file_generation_identity_and_bounded_anchor_changes_report_durable_gaps_without_retarget() {
    let temp = tempfile::tempdir().unwrap();
    let raw = support::row(1);
    let (store, file, path) = fixture(&temp, &raw);
    store
        .collect_file(Uuid::from_u128(8), &file, 100, OverflowPolicy::Hold)
        .unwrap();
    std::fs::write(&path, b"short").unwrap();
    let change = store
        .collect_file(Uuid::from_u128(8), &file, 101, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(change.consumed_bytes, 0);
    assert_eq!(change.facts_added, 1);
    assert!(change.blocked);
    assert_eq!(
        store
            .collect_file(Uuid::from_u128(8), &file, 102, OverflowPolicy::Hold)
            .unwrap()
            .facts_added,
        0
    );
    std::fs::rename(&path, temp.path().join("old.jsonl")).unwrap();
    std::fs::write(&path, &raw).unwrap();
    assert_eq!(
        store
            .collect_file(Uuid::from_u128(8), &file, 103, OverflowPolicy::Hold)
            .unwrap()
            .facts_added,
        1
    );
    assert!(
        store
            .collect_frame(
                Uuid::from_u128(8),
                &Frame {
                    start: raw.len() as u64,
                    bytes: raw.clone(),
                    oversized: false
                },
                104
            )
            .is_err()
    );
    let new = SourceFile::inspect(
        Uuid::from_u128(8),
        path.parent().unwrap(),
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    assert!(store.attach_file_engine(source(), 4096, new).is_err());
    drop(store);
    let store = Store::open(&temp.path().join("rows"), Limits::fixture()).unwrap();
    assert_eq!(store.snapshot().unwrap().queued, 3);
    store.audit().unwrap();
}
#[test]
fn foreign_handles_symlinks_and_root_replacement_refuse_original_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let raw = support::row(1);
    let (store, file, path) = fixture(&temp, &raw);
    let mut foreign = file.permit().clone();
    foreign.native_session_key = Text::new("foreign-key").unwrap();
    let other = SourceFile::open(foreign).unwrap();
    assert!(
        store
            .collect_file(Uuid::from_u128(8), &other, 100, OverflowPolicy::Hold)
            .is_err()
    );
    std::fs::rename(&path, temp.path().join("held.jsonl")).unwrap();
    std::os::unix::fs::symlink(temp.path().join("held.jsonl"), &path).unwrap();
    assert!(
        store
            .collect_file(Uuid::from_u128(8), &file, 101, OverflowPolicy::Hold)
            .is_err()
    );
    assert_eq!(store.source(Uuid::from_u128(8)).unwrap().cursor, 0);
    let root = path.parent().unwrap();
    std::fs::rename(root, temp.path().join("old-native-root")).unwrap();
    std::fs::create_dir(root).unwrap();
    std::fs::write(root.join("native.jsonl"), raw).unwrap();
    let changed = store
        .collect_file(Uuid::from_u128(8), &file, 102, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(changed.facts_added, 1);
    assert_eq!(changed.consumed_bytes, 0);
    store.audit().unwrap();
}
struct Owner;
impl aq_observation_redb_prototype::SourceAuthority for Owner {
    fn admit_generation(
        &self,
        plan: &aq_observation_redb_prototype::FileGenerationPlan,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            plan.source_id == Uuid::from_u128(8) && plan.binding == support::binding(),
            "fixture owner refused source"
        );
        Ok(())
    }
    fn admit_history(
        &self,
        plan: &aq_observation_redb_prototype::HistoryBoundary,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            plan.source_id == Uuid::from_u128(8) && plan.binding == support::binding(),
            "fixture owner refused history"
        );
        Ok(())
    }
}
fn checkpoint(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
#[test]
fn explicit_generation_retains_native_identity_and_requires_owner_history_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let raw = support::row(1);
    let (store, file, path) = fixture(&temp, &raw);
    store
        .collect_file(Uuid::from_u128(8), &file, 100, OverflowPolicy::Hold)
        .unwrap();
    std::fs::rename(&path, temp.path().join("old-native.jsonl")).unwrap();
    std::fs::write(&path, &raw).unwrap();
    let permit = SourceFile::inspect(
        Uuid::from_u128(8),
        path.parent().unwrap(),
        "native.jsonl",
        Text::new("native-fixture").unwrap(),
    )
    .unwrap();
    let plan = aq_observation_redb_prototype::FileGenerationPlan {
        operation_id: Uuid::from_u128(500),
        source_id: Uuid::from_u128(8),
        binding: support::binding(),
        expected_generation: 1,
        new_generation: 2,
        parser: source().parser(),
        new_permit: permit.clone(),
        owner_checkpoint: checkpoint("synthetic-current-host-source-generation"),
    };
    let verified =
        aq_observation_redb_prototype::VerifiedFileGeneration::verify(plan.clone(), &Owner)
            .unwrap();
    store.adopt_file_generation(&verified, 101).unwrap();
    let after = store.snapshot().unwrap();
    store.adopt_file_generation(&verified, 101).unwrap();
    assert_eq!(store.snapshot().unwrap(), after);
    let new = SourceFile::open(permit).unwrap();
    let replay = store
        .collect_file(Uuid::from_u128(8), &new, 102, OverflowPolicy::Hold)
        .unwrap();
    assert_eq!(replay.facts_added, 0);
    assert!(
        store
            .source(Uuid::from_u128(8))
            .unwrap()
            .engine
            .unwrap()
            .context
            .history_replay
    );
    let boundary = aq_observation_redb_prototype::HistoryBoundary {
        operation_id: Uuid::from_u128(501),
        source_id: Uuid::from_u128(8),
        binding: support::binding(),
        generation: 2,
        cursor: raw.len() as u64,
        checkpoint: checkpoint("synthetic-authenticated-native-history-end"),
    };
    let verified =
        aq_observation_redb_prototype::VerifiedHistoryBoundary::verify(boundary, &Owner).unwrap();
    store.finish_file_history(&verified).unwrap();
    store.finish_file_history(&verified).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&support::row(2))
        .unwrap();
    assert_eq!(
        store
            .collect_file(Uuid::from_u128(8), &new, 103, OverflowPolicy::Hold)
            .unwrap()
            .facts_added,
        1
    );
    store.audit().unwrap();
    drop(store);
    let store = Store::open(&temp.path().join("rows"), Limits::fixture()).unwrap();
    assert_eq!(
        store.source(Uuid::from_u128(8)).unwrap().source.generation,
        2
    );
    assert_eq!(store.snapshot().unwrap().queued, 3);
}
