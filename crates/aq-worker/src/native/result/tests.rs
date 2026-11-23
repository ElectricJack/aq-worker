use super::*;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
#[path = "../../../tests/support/result_history.rs"]
mod history_fixture;
use crate::native::journal::RuntimeIdentity;
use history_fixture::binding;
pub(crate) use history_fixture::history;
fn directories() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .unwrap();
    let channel = temp.path().join("result");
    (temp, workspace, channel)
}
fn publish(storage: &ChannelStorage) -> wire::Binding {
    let b = binding();
    let presence = wire::Presence {
        schema_version: 1,
        binding_digest: b.digest().unwrap(),
        kernel_boot_id: Uuid::from_u128(99),
        original_execution_deadline_boot_ns: 100,
        original_hard_deadline_boot_ns: 200,
    };
    storage.publish(&b, &presence).unwrap();
    storage.verify_control(&b, &presence).unwrap();
    b
}
#[test]
fn result_storage_control_custody_and_first_intake_lock_are_actual() {
    let (_temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    let b = publish(&storage);
    assert!(storage.intake_open().unwrap());
    assert!(storage.read_submission().unwrap().is_none());
    let submission = wire::Submission::new(
        b,
        wire::SubmissionInput {
            schema_version: 1,
            conclusion: wire::Conclusion::new("Change made; tests passed; limits unknown").unwrap(),
        },
    )
    .unwrap();
    let bytes = wire::encode(&submission).unwrap();
    let p = storage.submissions().join("submission.json");
    std::fs::write(&p, &bytes).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(storage.read_submission().unwrap().unwrap(), bytes);
    storage.close_intake();
    assert!(!storage.intake_open().unwrap());
    let presence = std::fs::File::open(path.join("owner.lock")).unwrap();
    assert!(
        fs2::FileExt::try_lock_shared(&presence).is_err(),
        "completion owner remains present after closing new intake"
    );
    storage.release_presence();
    fs2::FileExt::try_lock_shared(&presence).unwrap();
    assert!(ChannelStorage::create(&path, &workspace).is_err()); // Never adopt a previous directory.
}
#[test]
fn result_storage_refuses_route_replacement_and_workspace_overlap() {
    let (temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    std::fs::rename(&path, temp.path().join("retained-original")).unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    assert!(storage.verify().is_err());
    assert!(ChannelStorage::create(&workspace.join("result"), &workspace).is_err());
}
#[test]
fn result_storage_refuses_links_special_members_and_wrong_modes() {
    let (temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    let p = storage.submissions().join("submission.json");
    let foreign = temp.path().join("foreign");
    std::fs::write(&foreign, b"{}").unwrap();
    std::os::unix::fs::symlink(&foreign, &p).unwrap();
    assert!(storage.read_submission().is_err());
    std::fs::remove_file(&p).unwrap();
    std::fs::hard_link(&foreign, &p).unwrap();
    assert!(storage.read_submission().is_err());
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, b"{}").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(storage.read_submission().is_err());
}
#[test]
fn result_ack_is_projection_and_unknown_pending_is_preserved() {
    let (_temp, workspace, path) = directories();
    let storage = ChannelStorage::create(&path, &workspace).unwrap();
    let b = publish(&storage);
    let ack = wire::Acknowledgment {
        schema_version: 1,
        operation_id: b.operation_id,
        report_id: b.report_id,
        binding_digest: b.digest().unwrap(),
        submission_digest: Digest::of(b"example"),
        phase: wire::Phase::Accepted,
    };
    storage.write_ack(&ack).unwrap();
    assert_eq!(
        std::fs::read(path.join("ack.json")).unwrap(),
        wire::encode(&ack).unwrap()
    );
    std::fs::write(path.join("ack.pending"), b"unknown prior write").unwrap();
    assert!(storage.write_ack(&ack).is_err());
    assert_eq!(
        std::fs::read(path.join("ack.pending")).unwrap(),
        b"unknown prior write"
    );
}
#[test]
fn result_module_alone_does_not_prove_counted_startup_delivery() {
    let value:serde_json::Value=serde_json::from_slice(include_bytes!("../../../../../vendor/aq-phase-terminal-protocol-0.1.1/tests/data/preparation-v2.fixture.json")).unwrap();
    let mut offer: aq_admission::startup::NativeOffer =
        serde_json::from_value(value["proposal"]["original"].clone()).unwrap();
    let startup = wire::startup_artifact();
    offer.offered.offer.manifest.modules.push(startup.clone());
    assert!(!counted_startup(&offer));
    let mut context = offer.offered.offer.manifest.context.references[0].clone();
    context.artifact = startup.clone();
    context.bytes = wire::STARTUP_TEXT.len() as u64;
    context.required = true;
    context.delivery = aq_session_protocol::ContextDelivery::Startup;
    offer
        .offered
        .offer
        .manifest
        .context
        .references
        .push(context);
    assert!(!counted_startup(&offer));
    offer
        .startup
        .startup_inputs
        .push(aq_admission::startup::InputSpan {
            artifact: startup,
            offset: 0,
            bytes: wire::STARTUP_TEXT.len() as u64,
        });
    assert!(counted_startup(&offer));
}
#[test]
fn result_namespace_is_owned_and_allocates_disjoint_attempt_children() {
    let (temp, _, _) = directories();
    let path = temp.path().join("results");
    let ns = storage::Namespace::create(&path).unwrap();
    let first = ns.attempt(&path, Uuid::from_u128(1)).unwrap();
    let second = ns.attempt(&path, Uuid::from_u128(2)).unwrap();
    assert_ne!(first, second);
    assert_eq!(first.parent(), Some(path.as_path()));
    assert!(
        storage::Namespace::create(&path).is_err(),
        "a new owner cannot adopt a retained namespace"
    );
    std::fs::rename(&path, temp.path().join("retained-results")).unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    assert!(ns.attempt(&path, Uuid::from_u128(3)).is_err());
}
