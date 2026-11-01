//! Transfer metadata proposals. These functions perform no network or journal
//! effects and cannot establish that a coordinator retained any artifact.
use crate::{
    continuation::history::ParkingHistory,
    source_transfer::{UploadAction, UploadState, steps},
};
use anyhow::{Context, Result, ensure};
use aq_worker_protocol::{AttemptRef, NodeIdentity, SourceRole, TransferManifest, TransferStatus};

pub fn plan_transfer(old: &ParkingHistory) -> Result<ParkingHistory> {
    old.validate()?;
    ensure!(
        old.settled_source_claim()?,
        "parking transfer requires both completed source operations"
    );
    if old.source.transfer.is_some() {
        ensure!(
            old.source.upload.is_some(),
            "parking transfer lacks original upload owner"
        );
        return Ok(old.clone());
    }
    let exported = old
        .source
        .exported
        .as_ref()
        .context("parking export absent")?;
    let binding = &old.intent.suspension.binding;
    let aq_session_protocol::SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        anyhow::bail!("parking upload requires original remote identity")
    };
    let operations = &old.intent.operations;
    let manifest = TransferManifest {
        transfer_id: operations.transfer,
        role: SourceRole::Checkpoint,
        checkpoint: exported.manifest.source.clone(),
        attempt: AttemptRef {
            attempt_id: binding.attempt_id,
            session_id: binding.session_id(),
            generation: binding.attempt_generation,
        },
        resolved_spec_digest: old.intent.execution_spec_digest.as_str().into(),
        source: old.intent.source.clone(),
        artifact_id: exported.artifact,
        manifest_digest: exported.raw_manifest_digest.clone(),
        bundle_digest: exported.manifest.bundle_sha256.clone(),
        manifest_bytes: exported.manifest_bytes,
        bundle_bytes: exported.manifest.bundle_size,
    };
    let upload = UploadState {
        identity: NodeIdentity { node_id, boot_id },
        begin_operation: operations.upload_begin,
        finalize_operation: operations.upload_finalize,
        begun: false,
        manifest_offset: 0,
        bundle_offset: 0,
        pending: None,
    };
    let mut next = old.clone();
    next.source.transfer = Some(manifest);
    next.source.upload = Some(upload);
    next.validate()?;
    Ok(next)
}
/// Call only with the exact previously journaled action and its actual authenticated
/// exchange result. This validates association; it does not authenticate a DTO.
pub fn record_upload_progress(
    old: &ParkingHistory,
    action: &UploadAction,
    status: TransferStatus,
) -> Result<ParkingHistory> {
    old.validate()?;
    ensure!(
        old.settled_source_claim()?,
        "parking upload requires known source completion"
    );
    let transfer = old
        .source
        .transfer
        .as_ref()
        .context("parking transfer absent")?;
    let upload = old
        .source
        .upload
        .as_ref()
        .context("parking upload absent")?;
    let mut next = old.clone();
    match steps::acknowledge_upload(upload, transfer, action, status, upload.identity.node_id)? {
        steps::UploadAcknowledgment::Progress(progress) => next.source.upload = Some(progress),
        steps::UploadAcknowledgment::RetentionClaim(receipt) => {
            let export = old
                .source
                .exported
                .as_ref()
                .context("parking export absent")?;
            steps::validate_retention_claim(
                &receipt,
                transfer,
                &export.manifest,
                &old.intent.source,
                &export.manifest.source,
            )?;
            if let Some(previous) = &old.source.receipt {
                ensure!(
                    previous == &receipt,
                    "parking source retention receipt changed"
                );
            }
            next.source.receipt = Some(receipt);
        }
    }
    next.validate()?;
    Ok(next)
}
