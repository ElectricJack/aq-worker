#[allow(dead_code)]
mod support;
use aq_observation_redb_prototype::{Limits, Store};
#[test]
fn cold_fingerprint_is_stable_after_reopen_and_changes_with_atomic_collection() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("rows");
    let store = Store::create(&p, Limits::fixture()).unwrap();
    store.attach(support::source(8)).unwrap();
    let before = store.audit_fingerprint().unwrap();
    support::append(&store, 1);
    let after = store.audit_fingerprint().unwrap();
    assert_ne!(before, after);
    drop(store);
    let store = Store::open(&p, Limits::fixture()).unwrap();
    assert_eq!(after, store.audit_fingerprint().unwrap());
    let packet = store.prepare().unwrap().unwrap();
    let pending = store.audit_fingerprint().unwrap();
    assert_ne!(after, pending);
    drop(store);
    let store = Store::open(&p, Limits::fixture()).unwrap();
    assert_eq!(pending, store.audit_fingerprint().unwrap());
    assert_eq!(packet, store.prepare().unwrap().unwrap());
}
#[test]
fn unknown_format_and_impossible_limits_refuse_without_rewriting_existing_bytes() {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let t = tempfile::tempdir().unwrap();
    let old = t.path().join("old-layout");
    std::fs::DirBuilder::new().mode(0o700).create(&old).unwrap();
    let data = vec![0x5a; 2048];
    std::fs::write(old.join("store.redb"), &data).unwrap();
    std::fs::set_permissions(
        old.join("store.redb"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(Store::open(&old, Limits::fixture()).is_err());
    assert_eq!(std::fs::read(old.join("store.redb")).unwrap(), data);
    assert!(!old.join("store.format").exists());
    assert!(!old.join("owner.lock").exists());
    let valid = t.path().join("valid");
    drop(Store::create(&valid, Limits::fixture()).unwrap());
    let before = std::fs::read(valid.join("store.redb")).unwrap();
    std::fs::write(valid.join("store.format"), b"unknown codec").unwrap();
    assert!(Store::open(&valid, Limits::fixture()).is_err());
    assert_eq!(std::fs::read(valid.join("store.redb")).unwrap(), before);
    let impossible = t.path().join("impossible");
    let mut limits = Limits::fixture();
    limits.logical_bytes = 4096;
    assert!(Store::create(&impossible, limits).is_err());
    assert!(!impossible.exists());
}
#[test]
fn history_diagnostic_cannot_hide_caller_or_ordinary_records() {
    let t = tempfile::tempdir().unwrap();
    let s = Store::create(&t.path().join("rows"), Limits::fixture()).unwrap();
    s.attach(support::source(8)).unwrap();
    support::append(&s, 1);
    let packet = s.prepare().unwrap().unwrap();
    let aq_lifecycle_protocol::Message::Batch(batch) = packet.body.message else {
        panic!()
    };
    assert!(
        s.audit_retained_fingerprint(&[batch.events[0].event_id])
            .is_err()
    );
}
