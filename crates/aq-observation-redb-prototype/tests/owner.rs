#[allow(dead_code)]
mod support;
use anyhow::{Result, ensure};
use aq_lifecycle_protocol::*;
use aq_observation::{BootTransition, InitialTransport, TransportAuthority, spool::Node};
use aq_observation_redb_prototype::{Limits, Store, VerifiedNewStore};
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
struct Owner {
    seen: AtomicBool,
}
impl TransportAuthority for Owner {
    fn admit_initial(&self, plan: &InitialTransport) -> Result<()> {
        ensure!(
            plan.transport_owner_id == Uuid::from_u128(400),
            "wrong owner"
        );
        ensure!(
            !self.seen.swap(true, Ordering::SeqCst),
            "node already has history; explicit recovery needed"
        );
        Ok(())
    }
    fn admit_boot(&self, _: &BootTransition) -> Result<()> {
        anyhow::bail!("not part of initial admission fixture")
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
        owner_receipt: ArtifactRef {
            id: Text::new("synthetic-central-owner-admission").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"owner receipt"),
        },
    }
}
#[test]
fn only_explicit_new_owner_admission_creates_store_and_missing_data_never_reseeds() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rows");
    let owner = Owner {
        seen: AtomicBool::new(false),
    };
    let verified = VerifiedNewStore::verify(initial(), Limits::fixture(), &owner).unwrap();
    let store = Store::create_admitted(&path, verified).unwrap();
    assert_eq!(store.snapshot().unwrap().next, 1);
    drop(store);
    assert!(Store::open_admitted(&path, Limits::fixture(), Uuid::from_u128(999)).is_err());
    let store = Store::open_admitted(&path, Limits::fixture(), Uuid::from_u128(400)).unwrap();
    drop(store);
    std::fs::remove_file(path.join("store.redb")).unwrap();
    assert!(Store::open_admitted(&path, Limits::fixture(), Uuid::from_u128(400)).is_err());
    assert!(!path.join("store.redb").exists());
    assert!(VerifiedNewStore::verify(initial(), Limits::fixture(), &owner).is_err());
}
#[test]
fn anonymous_prototype_or_unknown_existing_format_cannot_become_admitted_store() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("anonymous");
    drop(Store::create(&path, Limits::fixture()).unwrap());
    assert!(Store::open_admitted(&path, Limits::fixture(), Uuid::from_u128(400)).is_err());
    let unknown = temp.path().join("unknown");
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&unknown)
        .unwrap();
    std::fs::write(unknown.join("legacy.json"), b"unknown retained state").unwrap();
    assert!(Store::open_admitted(&unknown, Limits::fixture(), Uuid::from_u128(400)).is_err());
    assert_eq!(
        std::fs::read(unknown.join("legacy.json")).unwrap(),
        b"unknown retained state"
    );
    assert!(!unknown.join("store.redb").exists());
}
