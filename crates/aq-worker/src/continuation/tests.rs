// Pure synthetic history vectors: no actual stop, Git, hold or retention authority.
use super::{history::*, source::*, transfer::*};
use aq_lifecycle_protocol::{Message, Packet};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use aq_worker_protocol::{ObjectAlgorithm, ObjectId, SourceBinding};
use serde_json::{Value, json};
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn object(c: char) -> ObjectId {
    ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: c.to_string().repeat(40),
    }
}
fn reference(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
fn initial() -> ParkingHistory {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../aq-lifecycle-protocol/artifacts/fixtures.json"
    ))
    .unwrap();
    let packet: Packet = serde_json::from_value(fixtures["valid_packets"][1].clone()).unwrap();
    let Message::Checkpoint(checkpoint) = packet.body.message else {
        panic!("checkpoint fixture")
    };
    let packet: Packet = serde_json::from_value(fixtures["valid_packets"][2].clone()).unwrap();
    let Message::Suspend(suspension) = packet.body.message else {
        panic!("suspension fixture")
    };
    ParkingHistory {
        schema_version: 1,
        intent: ParkingIntent {
            suspension,
            checkpoint,
            synthetic_hold: reference("synthetic-hold-marker-not-production-authority"),
            execution_record_digest: Digest::of(b"synthetic history not execution"),
            host_intent_digest: Digest::of(b"synthetic history not a host"),
            execution_spec_digest: Digest::of(b"synthetic spec"),
            source: SourceBinding {
                repository_id: id(100),
                binding_generation: 1,
                logical_identity: "synthetic-source-only".into(),
                base: object('a'),
            },
            workspace: crate::source::Workspace {
                workspace_id: id(101),
                generation: 1,
                path: "/synthetic/workspaces/101".into(),
            },
            managed_root: "/synthetic".into(),
            reserved_disk_bytes: 1048576,
            workspace_limit_bytes: 524288,
            artifact_limit_bytes: 524288,
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
            operations: ParkingOperations {
                terminal_fence: id(200),
                checkpoint: id(201),
                export: id(202),
                transfer: id(203),
                upload_begin: id(204),
                upload_finalize: id(205),
                report: id(206),
            },
            recorded_revision: 1,
        },
        stop: None,
        source: ParkingSourceHistory::default(),
        retention: None,
    }
}
fn stopped_history() -> ParkingHistory {
    let mut h = initial();
    h.stop = Some(ParkingStopAnchor {
        proof_digest: Digest::of(b"synthetic history not physical proof"),
        host_stopped_revision: 2,
        recorded_revision: 3,
    });
    h
}
fn complete() -> ParkingHistory {
    let h = plan_checkpoint(&stopped_history(), object('a'), 4096).unwrap();
    let h = record_checkpoint(&h, json!({"workspace_id":h.intent.workspace.workspace_id,"workspace_generation":2,"source":object('b'),"retention_handle":id(300),"unchanged":false})).unwrap();
    let h = plan_export(&h).unwrap();
    let manifest = crate::source::GitManifest {
        format_version: 1,
        repository_id: h.intent.source.repository_id,
        repository_identity: h.intent.source.logical_identity.clone(),
        binding_generation: 1,
        transfer_id: id(301),
        object_format: "sha1".into(),
        base: object('a'),
        source: object('b'),
        prerequisites: vec![],
        bundle_size: 0,
        bundle_sha256: None,
        producer_installation_id: id(302),
        reference_only: true,
        caller_correlation: Value::Null,
    };
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let exported = ParkingExport {
        artifact: manifest.transfer_id,
        manifest: manifest.clone(),
        raw_manifest_digest: Digest::of(&bytes).as_str().into(),
        manifest_bytes: bytes.len() as u64,
        total_bytes: bytes.len() as u64,
    };
    record_export(
        &h,
        json!({"artifact_handle":manifest.transfer_id,"manifest":manifest}),
        exported,
    )
    .unwrap()
}
#[test]
fn empty_or_single_source_result_never_counts_as_complete_and_no_checkpoint_precedes_stop() {
    let h = initial();
    h.validate().unwrap();
    assert!(!h.completed_source_claim().unwrap());
    assert!(plan_checkpoint(&h, object('a'), 4096).is_err());
    let h = plan_checkpoint(&stopped_history(), object('a'), 4096).unwrap();
    assert!(!h.completed_source_claim().unwrap());
    let h = record_checkpoint(&h, json!({"workspace_id":h.intent.workspace.workspace_id,"workspace_generation":2,"source":object('b'),"retention_handle":id(300),"unchanged":false})).unwrap();
    assert!(!h.completed_source_claim().unwrap());
    assert!(!plan_export(&h).unwrap().completed_source_claim().unwrap());
    assert!(complete().completed_source_claim().unwrap());
}
#[test]
fn lost_checkpoint_reuses_original_head_and_id_but_changed_operation_recipe_refuses() {
    let h = plan_checkpoint(&stopped_history(), object('a'), 4096).unwrap();
    let retry = plan_checkpoint(&h, object('b'), 4096).unwrap();
    assert_eq!(
        retry.source.operations["checkpoint"].request,
        h.source.operations["checkpoint"].request
    );
    let mut changed = retry;
    changed
        .source
        .operations
        .get_mut("checkpoint")
        .unwrap()
        .request["operation"] = json!("workspace.retire");
    assert!(changed.validate().is_err());
}
#[test]
fn only_checkpoint_transfer_is_proposed_and_predecessor_base_is_never_relabelled() {
    let h = plan_transfer(&complete()).unwrap();
    let manifest = h.source.transfer.as_ref().unwrap();
    assert_eq!(manifest.role, aq_worker_protocol::SourceRole::Checkpoint);
    assert_eq!(manifest.source.base, object('a'));
    assert_eq!(manifest.checkpoint, object('b'));
    assert_eq!(
        manifest.attempt.attempt_id,
        h.intent.suspension.binding.attempt_id
    );
    assert_ne!(manifest.transfer_id, manifest.artifact_id);
    let mut changed = h.clone();
    changed.source.transfer.as_mut().unwrap().source.base = object('b');
    assert!(changed.validate().is_err());
    let mut changed = h;
    changed.source.upload.as_mut().unwrap().identity.boot_id = id(999);
    assert!(changed.validate().is_err());
}
#[test]
fn missing_summary_allows_settled_source_but_never_migration_completion() {
    let mut h = stopped_history();
    h.intent.checkpoint.claim = aq_lifecycle_protocol::CheckpointClaim::Missing {
        reason: Text::new("not supplied").unwrap(),
    };
    h.validate().unwrap();
    assert!(plan_checkpoint(&h, object('a'), 4096).is_ok());
    assert!(!h.completed_source_claim().unwrap());
    let mut settled = complete();
    settled.intent.checkpoint.claim = h.intent.checkpoint.claim;
    settled.validate().unwrap();
    assert!(settled.settled_source_claim().unwrap());
    assert!(!settled.completed_source_claim().unwrap());
    assert!(plan_transfer(&settled).is_ok());
    let mut h = stopped_history();
    h.intent.suspension.max_summary_bytes = 1;
    assert!(h.validate().is_err());
}

#[test]
fn source_budget_and_origin_projection_cannot_be_omitted_or_overstated() {
    let h = stopped_history();
    assert!(plan_checkpoint(&h, object('a'), h.intent.workspace_limit_bytes + 1).is_err());
    let complete = complete();
    let mut changed = complete.clone();
    changed.source.observed_workspace_bytes = None;
    assert!(!changed.completed_source_claim().unwrap());
    let mut changed = complete.clone();
    changed.source.operations.remove("export");
    assert!(changed.validate().is_err());
    let mut changed = complete.clone();
    changed.source.exported.as_mut().unwrap().total_bytes = changed.intent.artifact_limit_bytes + 1;
    assert!(changed.validate().is_err());
    let mut changed = complete;
    changed.source.observed_workspace_bytes = Some(changed.intent.reserved_disk_bytes);
    assert!(changed.completed_source_claim().is_err());
}
