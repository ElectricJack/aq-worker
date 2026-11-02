//! History-only negative checks; these vectors never construct an opaque proof.
use super::*;
use crate::continuation::history::{DirectoryPin, ParkingIntent, ParkingOperations, WorkspacePins};

#[test]
fn parking_claim_cannot_be_journaled_as_an_actual_host_or_release_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let mut journal = create(&root);
    let id = reserve(&mut journal);
    let before = canonical_bytes(journal.state()).unwrap();
    let free = journal.available().unwrap();
    let fixtures: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../aq-lifecycle-protocol/artifacts/fixtures.json"
    ))
    .unwrap();
    let packet: aq_lifecycle_protocol::Packet =
        serde_json::from_value(fixtures["valid_packets"][1].clone()).unwrap();
    let aq_lifecycle_protocol::Message::Checkpoint(mut checkpoint) = packet.body.message else {
        panic!("checkpoint fixture")
    };
    let packet: aq_lifecycle_protocol::Packet =
        serde_json::from_value(fixtures["valid_packets"][2].clone()).unwrap();
    let aq_lifecycle_protocol::Message::Suspend(mut suspension) = packet.body.message else {
        panic!("suspension fixture")
    };
    let binding = aq_admission::transport::lifecycle_binding(
        &journal.state().attempts[&id]
            .reservation
            .offer
            .offered
            .offer,
    )
    .unwrap();
    suspension.binding = binding.clone();
    checkpoint.binding = binding;
    checkpoint.continuation = suspension.continuation.clone();
    let reference = qualification().runtime;
    let intent = ParkingIntent {
        suspension,
        checkpoint,
        synthetic_hold: reference,
        execution_record_digest: Digest::of(b"not a live execution"),
        host_intent_digest: Digest::of(b"not a host"),
        execution_spec_digest: journal.state().attempts[&id]
            .reservation
            .offer
            .offered
            .offer
            .binding
            .execution_spec_digest
            .clone(),
        source: aq_worker_protocol::SourceBinding {
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
            logical_identity: "synthetic history only".into(),
            base: aq_worker_protocol::ObjectId {
                algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
        },
        workspace: crate::source::Workspace {
            workspace_id: Uuid::new_v4(),
            generation: 1,
            path: "/synthetic/workspaces/exact".into(),
        },
        managed_root: "/synthetic".into(),
        pins: WorkspacePins {
            source_root: DirectoryPin {
                device: 1,
                inode: 2,
            },
            workspace: DirectoryPin {
                device: 1,
                inode: 3,
            },
        },
        reserved_disk_bytes: 1024,
        workspace_limit_bytes: 256,
        artifact_limit_bytes: 512,
        operations: ParkingOperations {
            terminal_fence: Uuid::new_v4(),
            checkpoint: Uuid::new_v4(),
            export: Uuid::new_v4(),
            transfer: Uuid::new_v4(),
            upload_begin: Uuid::new_v4(),
            upload_finalize: Uuid::new_v4(),
            report: Uuid::new_v4(),
        },
        recorded_revision: journal.state().revision + 1,
    };
    intent.validate().unwrap();
    assert!(journal.begin_parking(id, intent).is_err());
    assert_eq!(before, canonical_bytes(journal.state()).unwrap());
    assert_eq!(
        canonical_bytes(&free).unwrap(),
        canonical_bytes(&journal.available().unwrap()).unwrap()
    );
    assert!(journal.state().attempts[&id].parking.is_none());
}

#[test]
fn format12_candidate_refuses_frozen11_root_without_rewriting_its_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("native");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |state| state.format = 11);
    let saved = fs::read(root.join("journal.json")).unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(saved, fs::read(root.join("journal.json")).unwrap());
}
