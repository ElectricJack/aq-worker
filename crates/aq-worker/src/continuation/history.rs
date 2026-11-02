//! Proposed nested worker-owned history; native schema insertion belongs to its
//! version owner. None of these records is a live stopped-workspace capability.
use crate::{
    source::{Checkpoint as GitCheckpoint, GitAction, GitManifest, PendingGitOperation, Workspace},
    source_transfer::UploadState,
};
use anyhow::{Context, Result, ensure};
use aq_lifecycle_protocol::{Checkpoint as AgentCheckpoint, CheckpointClaim, SuspensionIntent};
use aq_session_protocol::{ArtifactRef, Digest};
use aq_worker_protocol::{CentralRetentionReceipt, SourceBinding, TransferManifest, Validate as _};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

pub const MAX_HISTORY_BYTES: usize = 262144;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryPin {
    pub device: u64,
    pub inode: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePins {
    pub source_root: DirectoryPin,
    pub workspace: DirectoryPin,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingOperations {
    pub terminal_fence: Uuid,
    pub checkpoint: Uuid,
    pub export: Uuid,
    pub transfer: Uuid,
    pub upload_begin: Uuid,
    pub upload_finalize: Uuid,
    pub report: Uuid,
}
impl ParkingOperations {
    pub fn validate(&self) -> Result<()> {
        let ids = [
            self.terminal_fence,
            self.checkpoint,
            self.export,
            self.transfer,
            self.upload_begin,
            self.upload_finalize,
            self.report,
        ];
        ensure!(
            ids.iter().all(|id| !id.is_nil())
                && ids
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == ids.len(),
            "parking operation identities"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingIntent {
    pub suspension: SuspensionIntent,
    pub checkpoint: AgentCheckpoint,
    /// An explicit synthetic coordinator hold marker, never a production hold.
    pub synthetic_hold: ArtifactRef,
    pub execution_record_digest: Digest,
    /// Existing native launch intent digest pins admitted profile/incarnation.
    pub host_intent_digest: Digest,
    pub execution_spec_digest: Digest,
    pub source: SourceBinding,
    pub workspace: Workspace,
    pub managed_root: PathBuf,
    pub pins: WorkspacePins,
    pub reserved_disk_bytes: u64,
    pub workspace_limit_bytes: u64,
    pub artifact_limit_bytes: u64,
    pub operations: ParkingOperations,
    pub recorded_revision: u64,
}
impl ParkingIntent {
    pub fn validate(&self) -> Result<()> {
        use aq_lifecycle_protocol::Validate as _;
        self.suspension.validate()?;
        self.checkpoint.validate()?;
        self.source.validate()?;
        self.operations.validate()?;
        ensure!(
            self.checkpoint.binding == self.suspension.binding
                && self.checkpoint.continuation == self.suspension.continuation,
            "parking checkpoint binding"
        );
        self.synthetic_hold.id.validate()?;
        self.synthetic_hold.version.validate()?;
        ensure!(
            self.workspace.path.is_absolute()
                && self.managed_root.is_absolute()
                && !self.workspace.workspace_id.is_nil()
                && self.workspace.generation > 0
                && self.pins.source_root.inode > 0
                && self.pins.workspace.inode > 0
                && self.recorded_revision > 0
                && self.reserved_disk_bytes > 0
                && self.workspace_limit_bytes > 0
                && self.artifact_limit_bytes > 0
                && self.workspace_limit_bytes <= self.reserved_disk_bytes
                && self.artifact_limit_bytes <= self.reserved_disk_bytes,
            "parking original workspace pins"
        );
        if let CheckpointClaim::Supplied { summary, .. } = &self.checkpoint.claim {
            ensure!(
                aq_session_protocol::canonical_bytes(summary)?.len()
                    <= self.suspension.max_summary_bytes as usize,
                "candidate parking canonical summary budget"
            );
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        Ok(aq_session_protocol::canonical_digest(self)?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingStopAnchor {
    pub proof_digest: Digest,
    pub host_stopped_revision: u64,
    pub recorded_revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingExport {
    pub artifact: Uuid,
    pub manifest: GitManifest,
    pub raw_manifest_digest: String,
    pub manifest_bytes: u64,
    pub total_bytes: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingSourceHistory {
    pub operations: BTreeMap<String, PendingGitOperation>,
    pub checkpoint_head: Option<aq_worker_protocol::ObjectId>,
    pub observed_workspace_bytes: Option<u64>,
    pub checkpoint: Option<GitCheckpoint>,
    pub exported: Option<ParkingExport>,
    pub transfer: Option<TransferManifest>,
    pub upload: Option<UploadState>,
    pub receipt: Option<CentralRetentionReceipt>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingRetentionHistory {
    /// Receipt facts remain claims until the authenticated coordinator owner verifies them.
    pub source: aq_lifecycle_protocol::RetentionReceipt,
    pub checkpoint: aq_lifecycle_protocol::RetentionReceipt,
    pub effects: aq_lifecycle_protocol::EffectsObservation,
    pub report_ack: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkingHistory {
    pub schema_version: u32,
    pub intent: ParkingIntent,
    pub stop: Option<ParkingStopAnchor>,
    pub source: ParkingSourceHistory,
    pub retention: Option<ParkingRetentionHistory>,
}
impl ParkingHistory {
    pub fn digest(&self) -> Result<Digest> {
        Ok(aq_session_protocol::canonical_digest(self)?)
    }
    /// Finite data association only; the native owner still supplies live proof,
    /// response authentication, exact CAS and actual fsync.
    pub fn validate_advance(&self, next: &Self) -> Result<()> {
        super::advance::validate_advance(self, next)
    }
    /// Shape/association checks only. The native owner must additionally compare
    /// actual retained execution, host proof, filesystem pins and synced revision.
    pub fn validate(&self) -> Result<()> {
        self.intent.validate()?;
        ensure!(
            self.schema_version == 1 && self.source.operations.len() <= 2,
            "parking history bounds"
        );
        if let Some(stop) = &self.stop {
            ensure!(
                stop.host_stopped_revision > 0
                    && stop.recorded_revision >= self.intent.recorded_revision
                    && stop.recorded_revision >= stop.host_stopped_revision,
                "parking stopped checkpoint order"
            );
        }
        let ids = &self.intent.operations;
        let checkpoint_result = self
            .source
            .operations
            .get("checkpoint")
            .and_then(|op| op.result.as_ref());
        let export_result = self
            .source
            .operations
            .get("export")
            .and_then(|op| op.result.as_ref());
        ensure!(
            self.source.checkpoint.is_some() == checkpoint_result.is_some()
                && self.source.exported.is_some() == export_result.is_some()
                && self.source.transfer.is_some() == self.source.upload.is_some(),
            "parking source projections must retain their originating result/owner"
        );
        for (name, op) in &self.source.operations {
            let expected = match name.as_str() {
                "checkpoint" => ids.checkpoint,
                "export" => ids.export,
                _ => anyhow::bail!("unknown parking source operation"),
            };
            ensure!(
                self.stop.is_some() && op.operation_id == expected,
                "parking Git before stopped anchor or changed ID"
            );
            let action = match name.as_str() {
                "checkpoint" => {
                    let head = self
                        .source
                        .checkpoint_head
                        .as_ref()
                        .context("parking checkpoint head absent")?;
                    head.validate()?;
                    GitAction::Checkpoint {
                        workspace_id: self.intent.workspace.workspace_id,
                        generation: self.intent.workspace.generation,
                        head: head.clone(),
                    }
                }
                "export" => {
                    let checkpoint = self
                        .source
                        .checkpoint
                        .as_ref()
                        .context("parking export without checkpoint")?;
                    GitAction::Export {
                        source: checkpoint.source.clone(),
                        retention_handle: checkpoint.retention_handle,
                    }
                }
                _ => unreachable!(),
            };
            ensure!(
                op.request == action.request(&self.intent.source, expected),
                "parking original Git request changed"
            );
            if let Some(result) = &op.result {
                ensure!(
                    serde_json::to_vec(result)?.len() <= 32768,
                    "parking Git reply bound"
                );
            }
        }
        if let Some(bytes) = self.source.observed_workspace_bytes {
            ensure!(
                bytes <= self.intent.workspace_limit_bytes
                    && bytes <= self.intent.reserved_disk_bytes,
                "parking stopped workspace budget exceeded"
            );
        }
        if let Some(checkpoint) = &self.source.checkpoint {
            ensure!(
                checkpoint.workspace_id == self.intent.workspace.workspace_id
                    && checkpoint.workspace_generation >= self.intent.workspace.generation
                    && !checkpoint.retention_handle.is_nil(),
                "parking checkpoint workspace"
            );
            checkpoint.source.validate()?;
        }
        if let Some(export) = &self.source.exported {
            let checkpoint = self
                .source
                .checkpoint
                .as_ref()
                .context("parking export without checkpoint")?;
            for prerequisite in &export.manifest.prerequisites {
                prerequisite.validate()?;
            }
            ensure!(
                !export.artifact.is_nil()
                    && export.manifest.format_version == 1
                    && export.manifest.object_format == "sha1"
                    && export.manifest.prerequisites.len() <= 64
                    && export.manifest.reference_only == (export.manifest.bundle_size == 0)
                    && export.manifest.bundle_sha256.is_none()
                        == (export.manifest.bundle_size == 0)
                    && export
                        .manifest
                        .bundle_sha256
                        .as_ref()
                        .is_none_or(|digest| digest.len() == 64
                            && digest
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                    && !export.manifest.producer_installation_id.is_nil()
                    && export.raw_manifest_digest.len() == 64
                    && export
                        .raw_manifest_digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && export.total_bytes <= self.intent.artifact_limit_bytes
                    && export.total_bytes <= self.intent.reserved_disk_bytes
                    && export.artifact == export.manifest.transfer_id
                    && export.manifest.repository_id == self.intent.source.repository_id
                    && export.manifest.repository_identity == self.intent.source.logical_identity
                    && export.manifest.binding_generation == self.intent.source.binding_generation
                    && export.manifest.base == self.intent.source.base
                    && export.manifest.source == checkpoint.source
                    && export.manifest_bytes > 0
                    && export.manifest_bytes <= 1048576
                    && export
                        .manifest_bytes
                        .checked_add(export.manifest.bundle_size)
                        == Some(export.total_bytes),
                "parking exact exported source"
            );
        }
        if let Some(transfer) = &self.source.transfer {
            transfer.validate()?;
            let export = self
                .source
                .exported
                .as_ref()
                .context("parking transfer without export")?;
            ensure!(
                transfer.resolved_spec_digest == self.intent.execution_spec_digest.as_str()
                    && transfer.transfer_id == ids.transfer
                    && transfer.artifact_id == export.artifact
                    && transfer.source == self.intent.source
                    && transfer.checkpoint == export.manifest.source
                    && transfer.manifest_bytes == export.manifest_bytes
                    && transfer.bundle_bytes == export.manifest.bundle_size
                    && transfer.manifest_digest == export.raw_manifest_digest
                    && transfer.bundle_digest == export.manifest.bundle_sha256
                    && transfer.role == aq_worker_protocol::SourceRole::Checkpoint,
                "parking exact transfer"
            );
            ensure!(
                transfer.attempt.attempt_id == self.intent.suspension.binding.attempt_id
                    && transfer.attempt.session_id == self.intent.suspension.binding.session_id()
                    && transfer.attempt.generation
                        == self.intent.suspension.binding.attempt_generation,
                "parking transfer predecessor"
            );
        }
        if let Some(upload) = &self.source.upload {
            let transfer = self
                .source
                .transfer
                .as_ref()
                .context("parking upload without transfer")?;
            upload.validate(transfer, upload.identity.node_id)?;
            ensure!(
                upload.begin_operation == ids.upload_begin
                    && upload.finalize_operation == ids.upload_finalize,
                "parking upload operation changed"
            );
            let aq_session_protocol::SessionIdentity::Remote {
                node_id, boot_id, ..
            } = self.intent.suspension.binding.session
            else {
                anyhow::bail!("worker parking requires remote identity")
            };
            ensure!(
                upload.identity.node_id == node_id && upload.identity.boot_id == boot_id,
                "parking original uploader identity"
            );
        }
        if let Some(receipt) = &self.source.receipt {
            let transfer = self
                .source
                .transfer
                .as_ref()
                .context("parking receipt without transfer")?;
            let export = self
                .source
                .exported
                .as_ref()
                .context("parking receipt without actual export")?;
            let upload = self
                .source
                .upload
                .as_ref()
                .context("parking receipt without uploader")?;
            ensure!(
                upload.begun
                    && upload.pending.is_none()
                    && upload.manifest_offset == transfer.manifest_bytes
                    && upload.bundle_offset == transfer.bundle_bytes,
                "parking receipt before completed upload"
            );
            crate::source_transfer::steps::validate_retention_claim(
                receipt,
                transfer,
                &export.manifest,
                &self.intent.source,
                &export.manifest.source,
            )?;
        }
        if let Some(retained) = &self.retention {
            aq_lifecycle_protocol::validate_retention(
                &[retained.source.clone(), retained.checkpoint.clone()],
                &self.intent.suspension.binding,
            )?;
            use aq_lifecycle_protocol::Validate as _;
            retained.effects.validate()?;
            ensure!(
                retained.source.role == aq_lifecycle_protocol::ArtifactRole::Source
                    && retained.checkpoint.role == aq_lifecycle_protocol::ArtifactRole::Checkpoint
                    && matches!(
                        retained.effects,
                        aq_lifecycle_protocol::EffectsObservation::Reconciled { .. }
                    )
                    && self.source.receipt.is_some()
                    && matches!(
                        self.intent.checkpoint.claim,
                        CheckpointClaim::Supplied { .. }
                    )
                    && self.settled_source_facts()?,
                "parking migration claims incomplete"
            );
            retained.report_ack.id.validate()?;
            retained.report_ack.version.validate()?;
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_HISTORY_BYTES,
            "parking history exceeds reserved byte bound"
        );
        Ok(())
    }
    /// Conservative capacity HISTORY predicate, not a constructor of physical proof.
    /// The journal owner must also verify the exact actual stopped anchor.
    pub fn settled_source_claim(&self) -> Result<bool> {
        self.validate()?;
        self.settled_source_facts()
    }
    fn settled_source_facts(&self) -> Result<bool> {
        if self.stop.is_none() {
            return Ok(false);
        }
        let Some(observed_bytes) = self.source.observed_workspace_bytes else {
            return Ok(false);
        };
        let (Some(checkpoint), Some(export)) = (&self.source.checkpoint, &self.source.exported)
        else {
            return Ok(false);
        };
        ensure!(
            observed_bytes
                .checked_add(export.total_bytes)
                .is_some_and(|total| total <= self.intent.reserved_disk_bytes),
            "parking observed disk exceeds reservation"
        );
        let Some(cp) = self
            .source
            .operations
            .get("checkpoint")
            .and_then(|op| op.result.as_ref())
        else {
            return Ok(false);
        };
        let Some(ex) = self
            .source
            .operations
            .get("export")
            .and_then(|op| op.result.as_ref())
        else {
            return Ok(false);
        };
        ensure!(
            serde_json::to_value(checkpoint)?
                == serde_json::to_value(serde_json::from_value::<GitCheckpoint>(cp.clone())?)?
                && ex["artifact_handle"] == export.artifact.to_string()
                && ex["manifest"] == serde_json::to_value(&export.manifest)?,
            "parking operation results do not match recorded source"
        );
        Ok(true)
    }
    /// Summary-bearing source completion is necessary for migration, but does
    /// not itself prove summary/source retention, effects reconciliation or ack.
    pub fn completed_source_claim(&self) -> Result<bool> {
        self.validate()?;
        Ok(matches!(
            self.intent.checkpoint.claim,
            CheckpointClaim::Supplied { .. }
        ) && self.settled_source_facts()?)
    }
}

impl aq_session_protocol::Validate for ParkingIntent {
    fn validate(&self) -> std::result::Result<(), aq_session_protocol::ContractError> {
        ParkingIntent::validate(self)
            .map_err(|_| aq_session_protocol::ContractError::Invalid("worker parking intent"))
    }
}

impl aq_session_protocol::Validate for ParkingHistory {
    fn validate(&self) -> std::result::Result<(), aq_session_protocol::ContractError> {
        ParkingHistory::validate(self)
            .map_err(|_| aq_session_protocol::ContractError::Invalid("worker parking history"))
    }
}
