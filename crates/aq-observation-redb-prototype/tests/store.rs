#[allow(dead_code)]
mod support;
use aq_lifecycle_protocol::*;
use aq_observation::{
    Normalizer,
    source::SourceFile,
    spool::{Limits as ReferenceLimits, Node, OverflowPolicy, Spool},
};
use aq_observation_redb_prototype::{
    Limits, Store,
    backend::{CappedFile, Monitor},
};
use redb::StorageBackend;
use std::{fs::File, os::unix::fs::PermissionsExt};
use uuid::Uuid;
struct Owner;
impl aq_observation::TransportAuthority for Owner {
    fn admit_initial(&self, _: &aq_observation::InitialTransport) -> anyhow::Result<()> {
        Ok(())
    }
    fn admit_boot(&self, _: &aq_observation::BootTransition) -> anyhow::Result<()> {
        anyhow::bail!("not a boot prototype")
    }
}
#[test]
fn exact_delivery_send_unknown_restart_and_ack_match_reference_spool() {
    let t = tempfile::tempdir().unwrap();
    let source = support::source(8);
    let mut bytes = support::row(1);
    bytes.extend(support::row(2));
    std::fs::write(t.path().join("native.jsonl"), &bytes).unwrap();
    let permit = SourceFile::inspect(
        source.source_id,
        t.path(),
        "native.jsonl",
        source.native_session_key.clone(),
    )
    .unwrap();
    let file = SourceFile::open(permit.clone()).unwrap();
    let node = Node {
        node_id: Uuid::from_u128(5),
        boot_id: Uuid::from_u128(6),
    };
    let initial = aq_observation::InitialTransport {
        operation_id: Uuid::from_u128(400),
        node: node.clone(),
        transport_owner_id: Uuid::from_u128(401),
        initial_sequence: 1,
        owner_receipt: ArtifactRef {
            id: Text::new("synthetic-owner").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"owner"),
        },
    };
    let verified = aq_observation::VerifiedInitial::verify(initial, &Owner).unwrap();
    let mut reference = Spool::initialize(
        &t.path().join("reference"),
        ReferenceLimits::default(),
        verified,
    )
    .unwrap();
    reference
        .attach_file(Normalizer::new(source.clone(), 4096).unwrap(), permit)
        .unwrap();
    reference
        .collect(source.source_id, &file, 100, OverflowPolicy::Hold)
        .unwrap();
    let path = t.path().join("rows");
    let store = Store::create(&path, Limits::fixture()).unwrap();
    store.attach(source).unwrap();
    support::append(&store, 1);
    support::append(&store, 2);
    let packet = store.prepare().unwrap().unwrap();
    assert_eq!(Some(packet.clone()), reference.prepare_packet().unwrap());
    assert_eq!(
        store.begin_send(1000).unwrap(),
        reference.begin_send(1000).unwrap()
    );
    let sent = store.snapshot().unwrap();
    drop(store);
    let store = Store::open(&path, Limits::fixture()).unwrap();
    assert_eq!(store.snapshot().unwrap(), sent);
    assert_eq!(
        store.begin_send(1250).unwrap(),
        reference.begin_send(1250).unwrap()
    );
    let mut peer = support::peer();
    let receipt = peer.receive(&packet).unwrap();
    assert_eq!(peer.receive(&packet).unwrap(), receipt);
    store.acknowledge(&receipt).unwrap();
    reference.acknowledge(&receipt).unwrap();
    store.acknowledge(&receipt).unwrap();
    assert_eq!(
        store.snapshot().unwrap().acked,
        reference.health().unwrap().acked_through
    );
    assert_eq!(store.snapshot().unwrap().queued, 0);
    store.audit().unwrap();
    drop(store);
    let store = Store::open(&path, Limits::fixture()).unwrap();
    assert!(store.prepare().unwrap().is_none());
    assert_eq!(store.snapshot().unwrap().receipt, Some(receipt));
}
#[test]
fn scope_changed_identity_quota_and_invalid_receipt_refuse_without_new_effect() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("rows");
    let store = Store::create(&path, Limits::fixture()).unwrap();
    store.attach(support::source(8)).unwrap();
    let before = store.snapshot().unwrap();
    let (mut events, seen) = support::normalize(1, 1);
    events[0].body.binding.task_id = Uuid::from_u128(44);
    assert!(
        store
            .append(Uuid::from_u128(8), 0, 1, &events, &seen)
            .is_err()
    );
    drop(store);
    let store = Store::open(&path, Limits::fixture()).unwrap();
    assert_eq!(store.snapshot().unwrap(), before);
    support::append(&store, 1);
    let packet = store.prepare().unwrap().unwrap();
    let pending = store.snapshot().unwrap();
    let mut receipt = support::receipt(&packet);
    receipt.through_sequence = Some(999);
    assert!(store.acknowledge(&receipt).is_err());
    drop(store);
    let store = Store::open(&path, Limits::fixture()).unwrap();
    assert_eq!(store.snapshot().unwrap(), pending);
    let mut changed = support::source(8);
    changed.generation = 2;
    assert!(store.attach(changed).is_err());
    drop(store);
    let mut small = Limits::fixture();
    small.logical_bytes = 4096;
    let other = t.path().join("quota");
    let store = Store::create(&other, small.clone()).unwrap();
    store.attach(support::source(8)).unwrap();
    let before = store.snapshot().unwrap();
    let (mut events, mut seen) = support::normalize(1, 1);
    let (e, s) = support::normalize(2, 2);
    events.extend(e);
    seen.extend(s);
    assert!(
        store
            .append(Uuid::from_u128(8), 0, 2, &events, &seen)
            .is_err()
    );
    drop(store);
    assert_eq!(
        Store::open(&other, small).unwrap().snapshot().unwrap(),
        before
    );
}
#[test]
fn private_root_exclusivity_missing_corruption_and_physical_cap_fail_closed() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("rows");
    assert!(Store::open(&path, Limits::fixture()).is_err());
    let store = Store::create(&path, Limits::fixture()).unwrap();
    assert!(Store::open(&path, Limits::fixture()).is_err());
    assert!(store.file_bytes().unwrap() <= Limits::fixture().physical_bytes);
    drop(store);
    std::fs::set_permissions(
        path.join("store.redb"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(Store::open(&path, Limits::fixture()).is_err());
    std::fs::set_permissions(
        path.join("store.redb"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(path.join("pending.tmp"), b"unknown").unwrap();
    assert!(Store::open(&path, Limits::fixture()).is_err());
    assert!(path.join("pending.tmp").exists());
    let cap = t.path().join("capped");
    std::fs::create_dir(&cap).unwrap();
    std::fs::set_permissions(&cap, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = File::open(cap).unwrap();
    let backend = CappedFile::open(&directory, true, 4096, Monitor::default()).unwrap();
    assert!(backend.set_len(4097).is_err());
    assert!(backend.write(4095, b"ab").is_err());
    assert_eq!(backend.len().unwrap(), 0);
    backend.set_len(4096).unwrap();
    backend.write(4095, b"a").unwrap();
    assert_eq!(backend.len().unwrap(), 4096);
}
#[test]
fn retry_bound_keeps_exact_intent_and_late_ack_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rows");
    let store = Store::create(&path, Limits::fixture()).unwrap();
    store.attach(support::source(8)).unwrap();
    support::append(&store, 1);
    let packet = store.begin_send(1000).unwrap().unwrap();
    assert_eq!(store.begin_send(1250).unwrap().unwrap(), packet);
    assert_eq!(store.begin_send(1750).unwrap().unwrap(), packet);
    assert!(store.begin_send(3000).is_err());
    drop(store);
    let store = Store::open(&path, Limits::fixture()).unwrap();
    assert_eq!(store.prepare().unwrap().unwrap(), packet);
    assert_eq!(store.snapshot().unwrap().delivery.unwrap().attempts, 3);
    store.acknowledge(&support::receipt(&packet)).unwrap();
    assert_eq!(store.snapshot().unwrap().queued, 0);
}
#[test]
fn transaction_disk_cap_holds_cursor_and_reopens_without_reseed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rows");
    let mut limits = Limits::fixture();
    limits.physical_bytes = 2 * 1024 * 1024;
    limits.logical_bytes = 1024 * 1024;
    let store = Store::create(&path, limits.clone()).unwrap();
    store.attach(support::source(8)).unwrap();
    let (_, template) = support::normalize(1, 1);
    let mut hit = false;
    let mut prior = store.snapshot().unwrap();
    let mut prior_source = store.source(Uuid::from_u128(8)).unwrap();
    for chunk in 0..8 {
        let mut seen = vec![];
        for index in 0..128 {
            let mut row = template[0].1.clone();
            row.raw_record_digests = vec![Digest::of(b"synthetic-retained-raw"); 32];
            seen.push((format!("retained-{chunk}-{index}"), row));
        }
        let result = store.append(
            Uuid::from_u128(8),
            prior_source.cursor,
            prior_source.cursor + 1,
            &[],
            &seen,
        );
        if let Err(error) = result {
            ensure_physical_cap(&error);
            eprintln!("transaction cap refusal: {error:#}");
            hit = true;
            break;
        }
        prior = store.snapshot().unwrap();
        prior_source = store.source(Uuid::from_u128(8)).unwrap();
    }
    assert!(hit, "fixture must exercise retained capacity refusal");
    assert!(store.file_bytes().unwrap() <= limits.physical_bytes);
    assert!(store.prepare().is_err());
    drop(store);
    let store = Store::open(&path, limits.clone()).unwrap();
    assert_eq!(store.snapshot().unwrap(), prior);
    assert_eq!(store.source(Uuid::from_u128(8)).unwrap(), prior_source);
    assert!(store.file_bytes().unwrap() <= limits.physical_bytes);
    store.audit().unwrap();
}

fn ensure_physical_cap(error: &anyhow::Error) {
    assert!(
        format!("{error:#}").contains("physical database cap"),
        "fixture did not exercise physical backend cap: {error:#}"
    );
}

#[test]
fn empty_or_short_existing_database_is_rejected_before_engine_without_changing_bytes() {
    let temp = tempfile::tempdir().unwrap();
    for length in [0, 1, 511] {
        let root = temp.path().join(format!("short-{length}"));
        let store = Store::create(&root, Limits::fixture()).unwrap();
        store.attach(support::source(8)).unwrap();
        drop(store);
        let bytes = vec![0x5a; length];
        std::fs::write(root.join("store.redb"), &bytes).unwrap();
        let error = Store::open(&root, Limits::fixture())
            .err()
            .expect("must not return a Store or reseed");
        assert!(
            format!("{error:#}")
                .contains("existing database shorter than pinned512-byte super-header")
        );
        assert_eq!(std::fs::read(root.join("store.redb")).unwrap(), bytes);
    }
}
#[test]
fn database_basename_replacement_refuses_mutation_of_detached_inode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach(support::source(8)).unwrap();
    support::append(&store, 1);
    std::fs::rename(root.join("store.redb"), root.join("detached.redb")).unwrap();
    let original = std::fs::read(root.join("detached.redb")).unwrap();
    std::fs::copy(root.join("detached.redb"), root.join("store.redb")).unwrap();
    let replacement = std::fs::read(root.join("store.redb")).unwrap();
    assert!(store.prepare().is_err());
    assert_eq!(std::fs::read(root.join("detached.redb")).unwrap(), original);
    assert_eq!(std::fs::read(root.join("store.redb")).unwrap(), replacement);
}
#[test]
fn private_root_rename_and_direct_backend_replacement_are_fenced() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    store.attach(support::source(8)).unwrap();
    support::append(&store, 1);
    let moved = temp.path().join("detached-root");
    std::fs::rename(&root, &moved).unwrap();
    let bytes = std::fs::read(moved.join("store.redb")).unwrap();
    assert!(store.prepare().is_err());
    assert_eq!(std::fs::read(moved.join("store.redb")).unwrap(), bytes);
    let direct = temp.path().join("direct");
    std::fs::create_dir(&direct).unwrap();
    std::fs::set_permissions(&direct, std::fs::Permissions::from_mode(0o700)).unwrap();
    let dir = File::open(&direct).unwrap();
    let backend = CappedFile::open(&dir, true, 4096, Monitor::default()).unwrap();
    backend.set_len(4096).unwrap();
    std::fs::rename(direct.join("store.redb"), direct.join("old")).unwrap();
    std::fs::copy(direct.join("old"), direct.join("store.redb")).unwrap();
    assert!(backend.write(0, b"unreachable").is_err());
    assert!(backend.set_len(2048).is_err());
    assert!(backend.sync_data().is_err());
    assert_eq!(std::fs::read(direct.join("old")).unwrap(), vec![0; 4096]);
}
