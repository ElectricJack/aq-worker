//! Shared finite upload planning/acknowledgment. These are claims and proposals,
//! never durable ownership, authenticated retention, or permission to send.
use super::*;

pub(crate) struct ProposedUploadStep {
    pub state: UploadState,
    pub action: UploadAction,
}
pub(crate) enum UploadAcknowledgment {
    Progress(UploadState),
    RetentionClaim(CentralRetentionReceipt),
}
/// Caller must durably retain a changed state before executing the returned action.
/// Existing pending requests reconstruct the exact original bytes/UUID.
pub(crate) fn propose_upload(
    upload: &UploadState,
    manifest: &TransferManifest,
    root: Option<&Path>,
    node: Uuid,
) -> Result<ProposedUploadStep> {
    upload.validate(manifest, node)?;
    let mut next = upload.clone();
    let action = if !upload.begun {
        UploadAction::Begin {
            identity: upload.identity.clone(),
            operation_id: upload.begin_operation,
            manifest: manifest.clone(),
        }
    } else if let Some(request) = &upload.pending {
        let size = if request.file == TransferFile::Manifest {
            manifest.manifest_bytes
        } else {
            manifest.bundle_bytes
        };
        let bytes = read_range(
            root.context("source root missing")?,
            manifest.artifact_id,
            &request.file,
            request.offset,
            request.length as usize,
            size,
        )?;
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == request.digest,
            "pending artifact bytes changed; preserved without replay"
        );
        UploadAction::Chunk {
            request: request.clone(),
            bytes,
        }
    } else {
        let remaining = if upload.manifest_offset < manifest.manifest_bytes {
            Some((
                TransferFile::Manifest,
                upload.manifest_offset,
                manifest.manifest_bytes,
            ))
        } else if upload.bundle_offset < manifest.bundle_bytes {
            Some((
                TransferFile::Bundle,
                upload.bundle_offset,
                manifest.bundle_bytes,
            ))
        } else {
            None
        };
        if let Some((file, offset, size)) = remaining {
            let length = (size - offset).min(MAX_CHUNK_BYTES as u64) as usize;
            let bytes = read_range(
                root.context("source root missing")?,
                manifest.artifact_id,
                &file,
                offset,
                length,
                size,
            )?;
            let request = TransferChunkRequest {
                identity: upload.identity.clone(),
                operation_id: Uuid::new_v4(),
                transfer_id: manifest.transfer_id,
                file,
                offset,
                length: length as u32,
                digest: format!("{:x}", Sha256::digest(&bytes)),
            };
            next.pending = Some(request.clone());
            UploadAction::Chunk { request, bytes }
        } else {
            UploadAction::Finalize(TransferStatusRequest {
                identity: upload.identity.clone(),
                operation_id: upload.finalize_operation,
                transfer_id: manifest.transfer_id,
            })
        }
    };
    Ok(ProposedUploadStep {
        state: next,
        action,
    })
}
/// Validates finite transfer progress only. A returned retention claim must still
/// be checked against the actual exported Git manifest and current owner.
pub(crate) fn acknowledge_upload(
    upload: &UploadState,
    manifest: &TransferManifest,
    action: &UploadAction,
    status: TransferStatus,
    node: Uuid,
) -> Result<UploadAcknowledgment> {
    status.validate()?;
    upload.validate(manifest, node)?;
    ensure!(
        status.transfer_id == manifest.transfer_id,
        "upload response transfer mismatch"
    );
    let mut next = upload.clone();
    match action {
        UploadAction::Begin {
            identity,
            operation_id,
            manifest: sent,
        } => {
            ensure!(
                !upload.begun
                    && identity == &upload.identity
                    && operation_id == &upload.begin_operation
                    && sent == manifest
                    && status.manifest_offset == 0
                    && status.bundle_offset == 0
                    && status.receipt.is_none(),
                "unexpected begin receipt"
            );
            next.begun = true;
        }
        UploadAction::Chunk { request, bytes } => {
            ensure!(
                upload.pending.as_ref() == Some(request)
                    && bytes.len() == request.length as usize
                    && format!("{:x}", Sha256::digest(bytes)) == request.digest
                    && status.receipt.is_none(),
                "unexpected chunk receipt"
            );
            match request.file {
                TransferFile::Manifest => next.manifest_offset += u64::from(request.length),
                TransferFile::Bundle => next.bundle_offset += u64::from(request.length),
            }
            ensure!(
                status.manifest_offset == next.manifest_offset
                    && status.bundle_offset == next.bundle_offset,
                "unexpected chunk progress"
            );
            next.pending = None;
        }
        UploadAction::Finalize(request) => {
            ensure!(
                request.identity == upload.identity
                    && request.operation_id == upload.finalize_operation
                    && request.transfer_id == manifest.transfer_id
                    && upload.begun
                    && upload.pending.is_none()
                    && upload.manifest_offset == manifest.manifest_bytes
                    && upload.bundle_offset == manifest.bundle_bytes
                    && status.manifest_offset == manifest.manifest_bytes
                    && status.bundle_offset == manifest.bundle_bytes,
                "unexpected finalize receipt"
            );
            return Ok(UploadAcknowledgment::RetentionClaim(
                status.receipt.context("central retention unconfirmed")?,
            ));
        }
    }
    next.validate(manifest, node)?;
    Ok(UploadAcknowledgment::Progress(next))
}

/// Association validation only: the caller must own the authenticated central
/// exchange and actual retained export. A deserialized receipt is not authority.
pub(crate) fn validate_retention_claim(
    receipt: &CentralRetentionReceipt,
    transfer: &TransferManifest,
    native: &GitManifest,
    binding: &SourceBinding,
    checkpoint: &ObjectId,
) -> Result<()> {
    receipt.validate()?;
    ensure!(
        receipt.transfer_id == transfer.transfer_id
            && receipt.retention_handle == native.transfer_id
            && receipt.manifest_digest == native.canonical_digest()?,
        "central receipt manifest/transfer mismatch"
    );
    ensure!(
        receipt.repository_id == binding.repository_id
            && receipt.binding_generation == binding.binding_generation
            && receipt.source == *checkpoint,
        "central source receipt binding mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    fn fixture() -> (UploadState, TransferManifest, GitManifest) {
        let binding = SourceBinding {
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
            logical_identity: "synthetic-parking-source".into(),
            base: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
        };
        let native = GitManifest {
            format_version: 1,
            repository_id: binding.repository_id,
            repository_identity: binding.logical_identity.clone(),
            binding_generation: 1,
            transfer_id: Uuid::new_v4(),
            object_format: "sha1".into(),
            base: binding.base.clone(),
            source: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "b".repeat(40),
            },
            prerequisites: vec![],
            bundle_size: 0,
            bundle_sha256: None,
            producer_installation_id: Uuid::new_v4(),
            reference_only: true,
            caller_correlation: Value::Null,
        };
        let manifest = TransferManifest {
            transfer_id: Uuid::new_v4(),
            role: SourceRole::Checkpoint,
            checkpoint: native.source.clone(),
            attempt: AttemptRef {
                attempt_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            },
            resolved_spec_digest: "c".repeat(64),
            source: binding,
            artifact_id: native.transfer_id,
            manifest_digest: "d".repeat(64),
            bundle_digest: None,
            manifest_bytes: 3,
            bundle_bytes: 0,
        };
        let state = UploadState {
            identity: NodeIdentity {
                node_id: Uuid::new_v4(),
                boot_id: Uuid::new_v4(),
            },
            begin_operation: Uuid::new_v4(),
            finalize_operation: Uuid::new_v4(),
            begun: false,
            manifest_offset: 0,
            bundle_offset: 0,
            pending: None,
        };
        (state, manifest, native)
    }
    #[test]
    fn planning_does_not_commit_a_chunk_and_reopened_proposal_reuses_exact_identity() {
        let (mut state, manifest, _) = fixture();
        state.begun = true;
        let root = tempfile::tempdir().unwrap();
        let directory = root
            .path()
            .join("artifacts")
            .join(manifest.artifact_id.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("manifest.json"), b"abc").unwrap();
        let proposed =
            propose_upload(&state, &manifest, Some(root.path()), state.identity.node_id).unwrap();
        assert!(
            state.pending.is_none(),
            "pure planning cannot pretend journal durability"
        );
        let recovered: UploadState =
            serde_json::from_slice(&serde_json::to_vec(&proposed.state).unwrap()).unwrap();
        let replay = propose_upload(
            &recovered,
            &manifest,
            Some(root.path()),
            state.identity.node_id,
        )
        .unwrap();
        let (
            UploadAction::Chunk {
                request: first,
                bytes: first_bytes,
            },
            UploadAction::Chunk {
                request: second,
                bytes: second_bytes,
            },
        ) = (proposed.action, replay.action)
        else {
            panic!("expected exact chunk proposal")
        };
        assert_eq!(first, second);
        assert_eq!(first_bytes, second_bytes);
    }
    #[test]
    fn source_http_progress_is_not_retention_and_changed_ids_cannot_advance() {
        let (state, manifest, _) = fixture();
        let proposed = propose_upload(&state, &manifest, None, state.identity.node_id).unwrap();
        let mut status = TransferStatus {
            transfer_id: manifest.transfer_id,
            manifest_offset: 0,
            bundle_offset: 0,
            receipt: None,
        };
        assert!(matches!(
            acknowledge_upload(
                &state,
                &manifest,
                &proposed.action,
                status.clone(),
                state.identity.node_id
            )
            .unwrap(),
            UploadAcknowledgment::Progress(_)
        ));
        status.transfer_id = Uuid::new_v4();
        assert!(
            acknowledge_upload(
                &state,
                &manifest,
                &proposed.action,
                status,
                state.identity.node_id
            )
            .is_err()
        );
        let complete = UploadState {
            begun: true,
            manifest_offset: manifest.manifest_bytes,
            ..state.clone()
        };
        let final_step =
            propose_upload(&complete, &manifest, None, state.identity.node_id).unwrap();
        let status = TransferStatus {
            transfer_id: manifest.transfer_id,
            manifest_offset: manifest.manifest_bytes,
            bundle_offset: 0,
            receipt: None,
        };
        assert!(
            acknowledge_upload(
                &complete,
                &manifest,
                &final_step.action,
                status,
                state.identity.node_id
            )
            .is_err()
        );
    }
    #[test]
    fn central_claim_binds_both_distinct_transport_and_artifact_and_canonical_manifest() {
        let (_, manifest, native) = fixture();
        let receipt = CentralRetentionReceipt {
            receipt_id: Uuid::new_v4(),
            transfer_id: manifest.transfer_id,
            repository_id: manifest.source.repository_id,
            binding_generation: 1,
            source: native.source.clone(),
            retention_handle: native.transfer_id,
            manifest_digest: native.canonical_digest().unwrap(),
        };
        assert_ne!(receipt.transfer_id, receipt.retention_handle);
        validate_retention_claim(
            &receipt,
            &manifest,
            &native,
            &manifest.source,
            &native.source,
        )
        .unwrap();
        for mismatch in ["transfer", "artifact", "digest", "source"] {
            let mut changed = receipt.clone();
            match mismatch {
                "transfer" => changed.transfer_id = native.transfer_id,
                "artifact" => changed.retention_handle = manifest.transfer_id,
                "digest" => changed.manifest_digest = manifest.manifest_digest.clone(),
                _ => changed.source = manifest.source.base.clone(),
            }
            assert!(
                validate_retention_claim(
                    &changed,
                    &manifest,
                    &native,
                    &manifest.source,
                    &native.source
                )
                .is_err(),
                "accepted changed {mismatch}"
            );
        }
    }
}
