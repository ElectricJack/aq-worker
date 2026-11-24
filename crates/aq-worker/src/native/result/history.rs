//! Durable result history, never a live channel, stopped proof or release permit.
use super::pins::ResultInstallationPins;
use crate::{
    native::journal::RuntimeIdentity,
    source::{Checkpoint, GitManifest, PendingGitOperation, Workspace},
    source_transfer::UploadState,
};
use anyhow::{Context, Result, ensure};
use aq_admission::{result_record, transport::ResultReport};
use aq_agent_result_local as local;
use aq_session_protocol::{ArtifactRef, Digest, Validate};
use aq_worker_protocol::{
    CentralRetentionReceipt, ObjectId, SourceBinding, TransferManifest, Validate as _,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
use uuid::Uuid;

pub const MAX_HISTORY_BYTES: usize = 512 * 1024;
pub const MAX_GIT_REPLY_BYTES: usize = 32768;
pub const MAX_SOURCE_HISTORY_BYTES: usize = 256 * 1024;
pub const MAX_REPORT_BYTES: usize = 16384;
pub use aq_agent_client::completion::storage::{DirectoryPin, StoragePins};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageStage {
    Creating,
    Ready {
        pins: StoragePins,
        recorded_revision: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceOperations {
    pub checkpoint: Uuid,
    pub export: Uuid,
    pub transfer: Uuid,
    pub upload_begin: Uuid,
    pub upload_finalize: Uuid,
}
impl SourceOperations {
    pub fn all(&self) -> [Uuid; 5] {
        [
            self.checkpoint,
            self.export,
            self.transfer,
            self.upload_begin,
            self.upload_finalize,
        ]
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRecord {
    pub binding: local::Binding,
    pub contract: ArtifactRef,
    pub installation: ResultInstallationPins,
    pub root: PathBuf,
    pub storage: StorageStage,
    pub git_pin: DirectoryPin,
    pub workspace_pin: DirectoryPin,
    pub runtime: RuntimeIdentity,
    pub kernel_boot_id: Uuid,
    pub original_deadline_ns: u64,
    pub original_hard_deadline_ns: u64,
    pub created_ns: u64,
    pub recorded_revision: u64,
    pub source: SourceBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_author: Option<aq_worker_protocol::git_author::GitAuthor>,
    pub workspace: Workspace,
    pub managed_root: PathBuf,
    pub reserved_disk_bytes: u64,
    pub workspace_limit_bytes: u64,
    pub artifact_limit_bytes: u64,
    pub operations: SourceOperations,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRecord {
    pub bytes: Vec<u8>,
    pub bytes_digest: Digest,
    pub submission_digest: Digest,
    pub recorded_revision: u64,
}
impl ClaimRecord {
    pub fn submission(&self) -> Result<local::Submission> {
        ensure!(
            self.bytes.len() <= local::MAX_BYTES && Digest::of(&self.bytes) == self.bytes_digest,
            "result original claim bytes changed"
        );
        let submission: local::Submission = local::decode(&self.bytes)?;
        ensure!(
            local::encode(&submission)? == self.bytes
                && submission.digest()? == self.submission_digest,
            "result claim must retain exact canonical original bytes"
        );
        Ok(submission)
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopAnchor {
    pub proof_digest: Digest,
    pub native_stopped_revision: u64,
    pub recorded_revision: u64,
    pub stopped_receipt: ArtifactRef,
    pub outcome: aq_admission::transport::ProcessOutcome,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRecord {
    pub artifact: Uuid,
    pub manifest: GitManifest,
    pub manifest_digest: String,
    pub manifest_bytes: u64,
    pub total_bytes: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceHistory {
    pub operations: BTreeMap<String, PendingGitOperation>,
    pub head: Option<ObjectId>,
    pub workspace_bytes: Option<u64>,
    pub checkpoint: Option<Checkpoint>,
    pub export: Option<ExportRecord>,
    pub transfer: Option<TransferManifest>,
    pub upload: Option<UploadState>,
    pub receipt: Option<CentralRetentionReceipt>,
    /// What the push to central aq-git did: the branch
    /// ref is the record of the result, no retention receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<aq_node_git::Pushed>,
}
impl SourceHistory {
    /// What the report names as the result's source.
    pub fn receipt_ids(&self) -> Result<Vec<Uuid>> {
        if self.pushed.is_some() {
            return Ok(vec![]);
        }
        let retained = self.receipt.as_ref();
        Ok(vec![
            retained
                .context("central source retention required")?
                .receipt_id,
        ])
    }
    /// The result request's headers for the push (S6); none when nothing was pushed.
    pub fn pushed_headers(&self) -> Vec<(&'static str, String)> {
        if let Some(pushed) = &self.pushed {
            return aq_node_git::transport::result_headers(pushed);
        }
        vec![]
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportIntent {
    pub request: ResultReport,
    pub bytes: Vec<u8>,
    pub digest: Digest,
    pub recorded_revision: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultHistory {
    pub schema_version: u32,
    pub scope: ScopeRecord,
    pub intake_closed: bool,
    pub claim: Option<ClaimRecord>,
    pub stop: Option<StopAnchor>,
    pub source: SourceHistory,
    pub report: Option<ReportIntent>,
    pub receipt: Option<result_record::Receipt>,
    pub failure: Option<local::RecoveryCode>,
    /// Revision of the restart salvage (`Journal::salvage_result`). Absent in
    /// every history written before W4, so their digests are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salvaged: Option<u64>,
}
impl ResultHistory {
    pub fn digest(&self) -> Result<Digest> {
        crate::native::journal::canonical_digest(self)
    }
    pub fn acknowledgment(&self) -> Result<Option<local::Acknowledgment>> {
        self.validate()?;
        let Some(claim) = &self.claim else {
            return Ok(None);
        };
        let submission = claim.submission()?;
        let phase = if let Some(receipt) = &self.receipt {
            local::Phase::Recorded {
                receipt: receipt.artifact(&self.report.as_ref().unwrap().request)?,
            }
        } else if let Some(code) = &self.failure {
            local::Phase::NeedsReconciliation { code: code.clone() }
        } else if self.stop.is_some() {
            local::Phase::Finalizing
        } else {
            local::Phase::Accepted
        };
        let ack = local::Acknowledgment {
            schema_version: 1,
            operation_id: self.scope.binding.operation_id,
            report_id: self.scope.binding.report_id,
            binding_digest: self.scope.binding.digest()?,
            submission_digest: claim.submission_digest.clone(),
            phase,
        };
        ack.validate_for(&submission)?;
        Ok(Some(ack))
    }
}

pub(super) fn encoded(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    value.sort_all_objects();
    Ok(serde_json::to_vec(&value)?)
}

mod source_validation;
mod validation;
