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
    let mut h = stopped_history();
    let aq_lifecycle_protocol::CheckpointClaim::Supplied { summary, .. } =
        &h.intent.checkpoint.claim
    else {
        panic!("supplied fixture")
    };
    h.intent.suspension.max_summary_bytes = summary.text.as_str().len() as u32;
    assert!(
        h.validate().is_err(),
        "the budget covers canonical summary/context bytes, not only text"
    );
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

#[test]
fn finite_advance_requires_each_frozen_git_step_and_immutable_stop_intent() {
    let h = stopped_history();
    let planned = plan_checkpoint(&h, object('a'), 4096).unwrap();
    h.validate_advance(&planned).unwrap();
    planned.validate_advance(&planned).unwrap();
    assert_ne!(h.digest().unwrap(), planned.digest().unwrap());
    let checkpoint = record_checkpoint(&planned, json!({"workspace_id":h.intent.workspace.workspace_id,"workspace_generation":2,"source":object('b'),"retention_handle":id(300),"unchanged":false})).unwrap();
    planned.validate_advance(&checkpoint).unwrap();
    assert!(
        h.validate_advance(&checkpoint).is_err(),
        "cannot bypass durable checkpoint intent"
    );
    let exported_intent = plan_export(&checkpoint).unwrap();
    checkpoint.validate_advance(&exported_intent).unwrap();
    let completed = complete();
    exported_intent.validate_advance(&completed).unwrap();
    let transferred = plan_transfer(&completed).unwrap();
    completed.validate_advance(&transferred).unwrap();
    let mut changed = planned.clone();
    changed.intent.pins.workspace.inode += 1;
    assert!(h.validate_advance(&changed).is_err());
    let mut changed = planned;
    changed.stop.as_mut().unwrap().proof_digest = Digest::of(b"changed proof");
    assert!(h.validate_advance(&changed).is_err());
}

#[test]
fn finite_upload_advances_preserve_pending_recipe_and_reject_unearned_offsets() {
    use aq_worker_protocol::{TransferChunkRequest, TransferFile};
    let initial = plan_transfer(&complete()).unwrap();
    let mut begun = initial.clone();
    begun.source.upload.as_mut().unwrap().begun = true;
    initial.validate_advance(&begun).unwrap();
    let size = begun.source.transfer.as_ref().unwrap().manifest_bytes;
    let mut jumped = begun.clone();
    jumped.source.upload.as_mut().unwrap().manifest_offset = size;
    assert!(
        begun.validate_advance(&jumped).is_err(),
        "offsets need an exact pending request"
    );
    let mut pending = begun.clone();
    let upload = pending.source.upload.as_mut().unwrap();
    upload.pending = Some(TransferChunkRequest {
        identity: upload.identity.clone(),
        operation_id: id(400),
        transfer_id: pending.intent.operations.transfer,
        file: TransferFile::Manifest,
        offset: 0,
        length: size as u32,
        digest: "d".repeat(64),
    });
    begun.validate_advance(&pending).unwrap();
    let mut short = pending.clone();
    short
        .source
        .upload
        .as_mut()
        .unwrap()
        .pending
        .as_mut()
        .unwrap()
        .length -= 1;
    assert!(
        begun.validate_advance(&short).is_err(),
        "chunk recipe uses the full bounded next range"
    );
    let mut changed = pending.clone();
    changed
        .source
        .upload
        .as_mut()
        .unwrap()
        .pending
        .as_mut()
        .unwrap()
        .operation_id = id(401);
    assert!(
        pending.validate_advance(&changed).is_err(),
        "pending operation cannot be replaced"
    );
    let mut wrong = pending.clone();
    wrong.source.upload.as_mut().unwrap().manifest_offset = size - 1;
    wrong.source.upload.as_mut().unwrap().pending = None;
    assert!(pending.validate_advance(&wrong).is_err());
    pending.validate_advance(&jumped).unwrap();
    let mut changed = jumped.clone();
    changed.source.upload.as_mut().unwrap().begun = false;
    assert!(jumped.validate_advance(&changed).is_err());
}

#[test]
fn capacity_predicate_rejects_malformed_history_even_without_prior_caller_validation() {
    let mut h = complete();
    h.source.exported.as_mut().unwrap().manifest.base = object('c');
    assert!(h.settled_source_claim().is_err());
    assert!(h.completed_source_claim().is_err());
    let mut h = complete();
    h.source
        .operations
        .get_mut("checkpoint")
        .unwrap()
        .operation_id = id(999);
    assert!(h.settled_source_claim().is_err());
}

fn retained_fixture_source() -> (
    ParkingHistory,
    super::synthetic_retention::SyntheticRetentionOwner,
) {
    use crate::source_transfer::UploadAction;
    use aq_worker_protocol::{TransferChunkRequest, TransferFile, TransferStatusRequest};
    let mut h = plan_transfer(&complete()).unwrap();
    let mut owner =
        super::synthetic_retention::SyntheticRetentionOwner::new(&h, 1100, 1200).unwrap();
    let upload = h.source.upload.as_ref().unwrap();
    let begin = UploadAction::Begin {
        identity: upload.identity.clone(),
        operation_id: upload.begin_operation,
        manifest: h.source.transfer.clone().unwrap(),
    };
    let status = owner.accept(&begin, 1101).unwrap();
    h = record_upload_progress(&h, &begin, status).unwrap();
    let bytes = serde_json::to_vec(&h.source.exported.as_ref().unwrap().manifest).unwrap();
    let upload = h.source.upload.as_mut().unwrap();
    let chunk = TransferChunkRequest {
        identity: upload.identity.clone(),
        operation_id: id(700),
        transfer_id: h.intent.operations.transfer,
        file: TransferFile::Manifest,
        offset: 0,
        length: bytes.len() as u32,
        digest: Digest::of(&bytes).as_str().into(),
    };
    upload.pending = Some(chunk.clone());
    let action = UploadAction::Chunk {
        request: chunk,
        bytes,
    };
    let status = owner.accept(&action, 1102).unwrap();
    h = record_upload_progress(&h, &action, status).unwrap();
    let upload = h.source.upload.as_ref().unwrap();
    let finalize = UploadAction::Finalize(TransferStatusRequest {
        identity: upload.identity.clone(),
        operation_id: upload.finalize_operation,
        transfer_id: h.intent.operations.transfer,
    });
    let status = owner.accept(&finalize, 1103).unwrap();
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        serde_json::to_value(owner.accept(&finalize, 1104).unwrap()).unwrap()
    );
    h = record_upload_progress(&h, &finalize, status).unwrap();
    (h, owner)
}
#[test]
fn synthetic_retention_profile_requires_actual_bytes_summary_and_explicit_effects_ledger() {
    let (mut h, mut owner) = retained_fixture_source();
    let identity = h.source.upload.as_ref().unwrap().identity.clone();
    assert!(owner.finish(&h, &identity, 1105).is_err());
    owner.retain_summary(&h.intent.checkpoint).unwrap();
    assert!(
        owner.finish(&h, &identity, 1105).is_err(),
        "empty/missing ledger is not reconciliation"
    );
    let aq_lifecycle_protocol::CheckpointClaim::Supplied {
        summary,
        summary_digest,
    } = &h.intent.checkpoint.claim
    else {
        panic!("supplied fixture")
    };
    for id in &summary.pending_operation_ids {
        owner.resolve_fixture_effect(*id);
    }
    let summary_bytes = aq_session_protocol::canonical_bytes(summary).unwrap();
    let expected_summary_digest = summary_digest.clone();
    owner
        .reconcile_fixture_effects(reference("explicit-synthetic-owner-ledger-decision"))
        .unwrap();
    let retained = owner.finish(&h, &identity, 1106).unwrap();
    assert_eq!(retained.checkpoint.artifact.digest, expected_summary_digest);
    assert_eq!(retained.checkpoint.bytes, summary_bytes.len() as u64);
    assert_eq!(
        retained.source.bytes,
        h.source.exported.as_ref().unwrap().total_bytes
    );
    assert_ne!(
        retained.source.bytes,
        h.source.observed_workspace_bytes.unwrap()
    );
    assert_eq!(retained.checkpoint.retained_until_ms, 1200);
    let again = owner.finish(&h, &identity, 1107).unwrap();
    assert_eq!(
        serde_json::to_value(&retained).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
    h.retention = Some(retained);
    h.validate().unwrap();
}
#[test]
fn synthetic_retention_refuses_wrong_summary_source_actor_and_expired_interval() {
    let (h, mut owner) = retained_fixture_source();
    let identity = h.source.upload.as_ref().unwrap().identity.clone();
    let mut wrong = h.intent.checkpoint.clone();
    wrong.claim = aq_lifecycle_protocol::CheckpointClaim::Missing {
        reason: Text::new("missing").unwrap(),
    };
    assert!(owner.retain_summary(&wrong).is_err());
    let mut wrong = h.intent.checkpoint.clone();
    if let aq_lifecycle_protocol::CheckpointClaim::Supplied {
        summary,
        summary_digest,
    } = &mut wrong.claim
    {
        summary.text =
            aq_lifecycle_protocol::SummaryText::new("different but internally valid summary")
                .unwrap();
        *summary_digest = aq_session_protocol::canonical_digest(summary).unwrap();
    }
    assert!(
        owner.retain_summary(&wrong).is_err(),
        "a valid new digest cannot replace the original summary"
    );
    owner.retain_summary(&h.intent.checkpoint).unwrap();
    let aq_lifecycle_protocol::CheckpointClaim::Supplied { summary, .. } =
        &h.intent.checkpoint.claim
    else {
        panic!("supplied fixture")
    };
    for id in &summary.pending_operation_ids {
        owner.resolve_fixture_effect(*id);
    }
    owner
        .reconcile_fixture_effects(reference("explicit-synthetic-owner-ledger-decision"))
        .unwrap();
    let mut other_actor = identity.clone();
    other_actor.boot_id = id(999);
    assert!(owner.finish(&h, &other_actor, 1106).is_err());
    assert!(owner.finish(&h, &identity, 1200).is_err());
    let mut other_source = h.clone();
    other_source
        .source
        .transfer
        .as_mut()
        .unwrap()
        .manifest_digest = "e".repeat(64);
    assert!(owner.finish(&other_source, &identity, 1106).is_err());
    let mut other_continuation = h;
    other_continuation
        .intent
        .suspension
        .continuation
        .continuation_id = id(999);
    assert!(owner.finish(&other_continuation, &identity, 1106).is_err());
}

#[test]
fn synthetic_owner_never_claims_retention_for_wrong_complete_raw_source_bytes() {
    use crate::source_transfer::UploadAction;
    use aq_worker_protocol::{TransferChunkRequest, TransferFile, TransferStatusRequest};
    let h = plan_transfer(&complete()).unwrap();
    let mut owner =
        super::synthetic_retention::SyntheticRetentionOwner::new(&h, 1100, 1200).unwrap();
    let upload = h.source.upload.as_ref().unwrap();
    let begin = UploadAction::Begin {
        identity: upload.identity.clone(),
        operation_id: upload.begin_operation,
        manifest: h.source.transfer.clone().unwrap(),
    };
    owner.accept(&begin, 1101).unwrap();
    let mut bytes = serde_json::to_vec(&h.source.exported.as_ref().unwrap().manifest).unwrap();
    bytes[0] = b'x';
    let chunk = UploadAction::Chunk {
        request: TransferChunkRequest {
            identity: upload.identity.clone(),
            operation_id: id(700),
            transfer_id: h.intent.operations.transfer,
            file: TransferFile::Manifest,
            offset: 0,
            length: bytes.len() as u32,
            digest: Digest::of(&bytes).as_str().into(),
        },
        bytes,
    };
    owner.accept(&chunk, 1102).unwrap();
    let finalize = UploadAction::Finalize(TransferStatusRequest {
        identity: upload.identity.clone(),
        operation_id: upload.finalize_operation,
        transfer_id: h.intent.operations.transfer,
    });
    assert!(
        owner.accept(&finalize, 1103).is_err(),
        "actual raw manifest SHA must match original transfer before retention"
    );
}
