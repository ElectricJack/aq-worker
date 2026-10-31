#[allow(dead_code)]
mod support;
use anyhow::{Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation::{BootTransition, InitialTransport, TransportAuthority, spool::Node};
use aq_observation_engine::{self as engine, framing::Frame};
use aq_observation_redb_prototype::{
    HoldReason, Limits, Store, VerifiedBootAdmission, VerifiedNewStore,
};
use uuid::Uuid;
struct Owner;
impl TransportAuthority for Owner {
    fn admit_initial(&self, p: &InitialTransport) -> Result<()> {
        ensure!(p.transport_owner_id == Uuid::from_u128(400), "owner");
        Ok(())
    }
    fn admit_boot(&self, p: &BootTransition) -> Result<()> {
        ensure!(p.transport_owner_id == Uuid::from_u128(400), "owner");
        Ok(())
    }
}
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
fn initial() -> InitialTransport {
    InitialTransport {
        operation_id: Uuid::from_u128(401),
        node: Node {
            node_id: Uuid::from_u128(5),
            boot_id: Uuid::from_u128(6),
        },
        transport_owner_id: Uuid::from_u128(400),
        initial_sequence: 1,
        owner_receipt: reference("synthetic-initial-owner"),
    }
}
fn transition(next: u64) -> BootTransition {
    BootTransition {
        operation_id: Uuid::from_u128(402),
        node_id: Uuid::from_u128(5),
        transport_owner_id: Uuid::from_u128(400),
        expected_boot_id: Uuid::from_u128(6),
        new_boot_id: Uuid::from_u128(66),
        expected_next_sequence: next,
        owner_receipt: reference("synthetic-new-boot-owner"),
    }
}
fn source(id: u128, boot: u128) -> engine::Source {
    let mut s = support::source(id);
    let SessionIdentity::Remote { boot_id, .. } = &mut s.binding.session else {
        panic!()
    };
    *boot_id = Uuid::from_u128(boot);
    serde_json::from_value(serde_json::to_value(s).unwrap()).unwrap()
}
#[test]
fn retained_boot_fences_live_sources_preserves_packet_and_drains_same_boot_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("rows");
    let verified = VerifiedNewStore::verify(initial(), Limits::fixture(), &Owner).unwrap();
    let store = Store::create_admitted(&root, verified).unwrap();
    store.attach_engine(source(8, 6), 4096, false).unwrap();
    let raw = support::row(1);
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
    let frozen = store.begin_send(1000).unwrap().unwrap();
    let plan = transition(store.snapshot().unwrap().next);
    let verified = VerifiedBootAdmission::verify(plan.clone(), &Owner).unwrap();
    store.adopt_boot(&verified, 1001).unwrap();
    let snapshot = store.snapshot().unwrap();
    store.adopt_boot(&verified, 1001).unwrap();
    assert_eq!(store.snapshot().unwrap(), snapshot);
    assert_eq!(store.prepare().unwrap().unwrap(), frozen);
    assert_eq!(
        store.source(Uuid::from_u128(8)).unwrap().hold,
        Some(HoldReason::BootFenced)
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
                1002
            )
            .is_err()
    );
    store.attach_engine(source(9, 66), 4096, false).unwrap();
    store
        .collect_frame(
            Uuid::from_u128(9),
            &Frame {
                start: 0,
                bytes: raw,
                oversized: false,
            },
            1003,
        )
        .unwrap();
    store.audit().unwrap();
    drop(store);
    let store = Store::open_admitted(&root, Limits::fixture(), Uuid::from_u128(400)).unwrap();
    assert_eq!(store.snapshot().unwrap().active_boot, Uuid::from_u128(66));
    let mut boots = vec![];
    let mut ends = vec![];
    while let Some(packet) = store.prepare().unwrap() {
        let Message::Batch(batch) = &packet.body.message else {
            panic!()
        };
        boots.push(batch.boot_id);
        ends.push(batch.last_sequence);
        store.acknowledge(&support::receipt(&packet)).unwrap();
    }
    assert_eq!(
        boots,
        vec![Uuid::from_u128(6), Uuid::from_u128(6), Uuid::from_u128(66)]
    );
    assert_eq!(ends, vec![1, 2, 3]);
    assert_eq!(store.snapshot().unwrap().next, 4);
    assert_eq!(store.snapshot().unwrap().acked, 3);
    store.audit().unwrap();
    let mut changed = plan;
    changed.new_boot_id = Uuid::from_u128(67);
    let changed = VerifiedBootAdmission::verify(changed, &Owner).unwrap();
    assert!(store.adopt_boot(&changed, 1004).is_err());
    drop(store);
    assert_eq!(
        Store::open_admitted(&root, Limits::fixture(), Uuid::from_u128(400))
            .unwrap()
            .snapshot()
            .unwrap()
            .active_boot,
        Uuid::from_u128(66)
    );
}
#[test]
fn boot_transition_requires_retained_admitted_owner_and_current_sequence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("anonymous");
    let store = Store::create(&root, Limits::fixture()).unwrap();
    let proof = VerifiedBootAdmission::verify(transition(1), &Owner).unwrap();
    assert!(store.adopt_boot(&proof, 100).is_err());
    drop(store);
    let root = temp.path().join("admitted");
    let proof = VerifiedNewStore::verify(initial(), Limits::fixture(), &Owner).unwrap();
    let store = Store::create_admitted(&root, proof).unwrap();
    let wrong = VerifiedBootAdmission::verify(transition(2), &Owner).unwrap();
    assert!(store.adopt_boot(&wrong, 100).is_err());
    drop(store);
    let store = Store::open_admitted(&root, Limits::fixture(), Uuid::from_u128(400)).unwrap();
    assert_eq!(store.snapshot().unwrap().active_boot, Uuid::from_u128(6));
}
