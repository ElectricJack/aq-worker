//! Durable result history, never a live channel, stopped proof or release permit.
use super::pins::ResultInstallationPins;
use crate::{
    native::journal::RuntimeIdentity,
    source::{Checkpoint, PendingGitOperation, Workspace},
    transfer_client::PushedSource,
};
use anyhow::{Context, Result, ensure};
use aq_admission::{result_record, transport::ResultReport};
use aq_agent_result_local as local;
use aq_session_protocol::{ArtifactRef, Digest, Validate};
use aq_worker_protocol::{
    AttemptRef, CentralRetentionReceipt, NodeIdentity, ObjectId, SourceBinding, Validate as _,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryPin {
    pub device: u64,
    pub inode: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoragePins {
    pub root: DirectoryPin,
    pub submissions: DirectoryPin,
}
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
    pub transfer: Uuid,
    pub pushed: Uuid,
}
impl SourceOperations {
    pub fn all(&self) -> [Uuid; 3] {
        [self.checkpoint, self.transfer, self.pushed]
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
    pub workspace: Workspace,
    pub managed_root: PathBuf,
    pub reserved_disk_bytes: u64,
    pub workspace_limit_bytes: u64,
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
pub struct PushRecord {
    pub identity: NodeIdentity,
    /// `git push` of the checkpoint to the attempt's remote ref succeeded.
    pub pushed: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceHistory {
    pub operations: BTreeMap<String, PendingGitOperation>,
    pub head: Option<ObjectId>,
    pub workspace_bytes: Option<u64>,
    pub checkpoint: Option<Checkpoint>,
    /// Journaled before the first push, so a retry is a replay of this intent.
    pub push: Option<PushRecord>,
    pub receipt: Option<CentralRetentionReceipt>,
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
    pub fn validate(&self) -> Result<()> {
        let s = &self.scope;
        s.binding.validate()?;
        s.installation.validate()?;
        s.source.validate()?;
        ensure!(
            self.schema_version == 1
                && s.contract == local::artifact()
                && !s.kernel_boot_id.is_nil()
                && s.created_ns < s.original_deadline_ns
                && s.original_deadline_ns <= s.original_hard_deadline_ns
                && s.recorded_revision > 0
                && s.git_pin.inode > 0
                && s.workspace_pin.inode > 0,
            "result scope identity/clocks"
        );
        ensure!(
            s.root.is_absolute()
                && s.root.as_os_str().len() <= 2048
                && s.workspace.path.is_absolute()
                && s.managed_root.is_absolute()
                && !s.workspace.workspace_id.is_nil()
                && s.workspace.generation > 0
                && !s.root.starts_with(&s.workspace.path)
                && !s.workspace.path.starts_with(&s.root),
            "result local/source path scope"
        );
        ensure!(
            s.workspace_limit_bytes > 0
                && s.workspace_limit_bytes
                    .checked_mul(2)
                    .is_some_and(|n| n <= s.reserved_disk_bytes),
            "result original source reservation"
        );
        let mut ids = BTreeSet::new();
        for id in [s.binding.operation_id, s.binding.report_id, s.binding.nonce]
            .into_iter()
            .chain(s.operations.all())
        {
            ensure!(
                !id.is_nil() && ids.insert(id),
                "result operation identity reused"
            );
        }
        if let StorageStage::Ready {
            pins,
            recorded_revision,
        } = &s.storage
        {
            ensure!(
                pins.root.inode > 0
                    && pins.submissions.inode > 0
                    && *recorded_revision > s.recorded_revision,
                "result directory pins absent"
            );
        }
        if let Some(claim) = &self.claim {
            let value = claim.submission()?;
            ensure!(
                value.binding == s.binding
                    && self.intake_closed
                    && matches!(s.storage, StorageStage::Ready { .. })
                    && matches!(s.storage, StorageStage::Ready { recorded_revision, .. } if claim.recorded_revision > recorded_revision),
                "result claimed scope changed"
            );
        }
        ensure!(
            self.salvaged.is_none() || (self.claim.is_some() && self.stop.is_some()),
            "result salvage without claim and stop"
        );
        if let Some(stop) = &self.stop {
            ensure!(
                self.intake_closed
                    && stop.native_stopped_revision > 0
                    && stop.recorded_revision >= stop.native_stopped_revision
                    && stop.recorded_revision > s.recorded_revision,
                "result stopped anchor order"
            );
        }
        ensure!(
            encoded(&self.scope)?.len() <= 16384
                && encoded(&self.source)?.len() <= MAX_SOURCE_HISTORY_BYTES,
            "result source envelope cannot spend stop/report reserve"
        );
        self.validate_source()?;
        if let Some(report) = &self.report {
            let claim = self
                .claim
                .as_ref()
                .context("result report without explicit claim")?
                .submission()?;
            let stop = self
                .stop
                .as_ref()
                .context("result report without actual stop")?;
            let source = self
                .source
                .receipt
                .as_ref()
                .context("result report without source retention")?;
            result_record::validate_report(&report.request)?;
            ensure!(
                report.bytes == serde_json::to_vec(&report.request)?
                    && report.digest == result_record::report_digest(&report.request)?
                    && report.bytes.len() <= MAX_REPORT_BYTES
                    && report.request.operation_id == s.binding.operation_id
                    && report.request.report_id == s.binding.report_id
                    && report.request.binding == s.binding.launch
                    && report.request.supersedes.is_none()
                    && report.request.conclusion.as_str() == claim.conclusion.as_str()
                    && report.request.source_receipt_ids == vec![source.receipt_id]
                    && serde_json::to_value(report.request.outcome)?
                        == serde_json::to_value(stop.outcome)?
                    && report.recorded_revision > stop.recorded_revision,
                "result exact retained report changed"
            );
        }
        if let Some(receipt) = &self.receipt {
            let report = self
                .report
                .as_ref()
                .context("result receipt without exact request")?;
            receipt.validate_for(&report.request)?;
            ensure!(
                receipt.expected_outcome == s.binding.expected_outcome
                    && receipt.stopped_receipt == self.stop.as_ref().unwrap().stopped_receipt,
                "result B1 receipt owner pins changed"
            );
        }
        ensure!(
            encoded(self)?.len() <= MAX_HISTORY_BYTES,
            "result history reserved envelope exhausted"
        );
        Ok(())
    }
    /// The worker identity of the original remote session.
    fn remote(&self) -> Result<(NodeIdentity, Uuid)> {
        match self.scope.binding.launch.session {
            aq_session_protocol::SessionIdentity::Remote {
                node_id,
                boot_id,
                session_id,
                ..
            } => Ok((NodeIdentity { node_id, boot_id }, session_id)),
            _ => anyhow::bail!("result requires actual remote session"),
        }
    }
    /// Journals the push intent; the checkpoint and every ID are the originals.
    pub(super) fn begin_push(&mut self) -> Result<()> {
        ensure!(self.source.push.is_none(), "result push intent raced");
        let (identity, _) = self.remote()?;
        self.source.push = Some(PushRecord {
            identity,
            pushed: false,
        });
        Ok(())
    }
    /// The admission request of the pushed checkpoint: only original scope facts.
    pub(crate) fn pushed_request(&self) -> Result<PushedSource> {
        let s = &self.scope;
        let b = &s.binding.launch;
        let push = self.source.push.as_ref();
        let checkpoint = self.source.checkpoint.as_ref();
        Ok(PushedSource {
            identity: push.context("result push intent absent")?.identity.clone(),
            operation_id: s.operations.pushed,
            transfer_id: s.operations.transfer,
            attempt: AttemptRef {
                attempt_id: b.attempt_id,
                session_id: self.remote()?.1,
                generation: b.assignment_generation,
            },
            resolved_spec_digest: b.execution_spec_digest.as_str().into(),
            source: s.source.clone(),
            checkpoint: checkpoint
                .context("result checkpoint absent")?
                .source
                .clone(),
        })
    }
    fn validate_source(&self) -> Result<()> {
        let h = &self.source;
        let s = &self.scope;
        let operation = h.operations.get("checkpoint");
        ensure!(
            h.operations.len() == usize::from(operation.is_some())
                && h.checkpoint.is_some() == operation.is_some_and(|op| op.result.is_some()),
            "result source history projection"
        );
        if let Some(op) = operation {
            ensure!(
                self.claim.is_some() && self.stop.is_some(),
                "result source action before claim/actual stop"
            );
            let action = crate::source::GitAction::Checkpoint {
                workspace_id: s.workspace.workspace_id,
                generation: s.workspace.generation,
                head: h.head.clone().context("result checkpoint head absent")?,
            };
            ensure!(
                op.operation_id == s.operations.checkpoint
                    && op.request == action.request(&s.source, s.operations.checkpoint),
                "result source original request changed"
            );
            if let Some(value) = &op.result {
                ensure!(
                    serde_json::to_vec(value)?.len() <= MAX_GIT_REPLY_BYTES,
                    "result source reply bound"
                );
            }
        }
        if let Some(head) = &h.head {
            head.validate()?;
        }
        if let Some(bytes) = h.workspace_bytes {
            ensure!(
                bytes <= s.workspace_limit_bytes,
                "result actual workspace budget"
            );
        }
        if let Some(c) = &h.checkpoint {
            c.source.validate()?;
            ensure!(
                c.workspace_id == s.workspace.workspace_id
                    && c.workspace_generation >= s.workspace.generation
                    && !c.retention_handle.is_nil()
                    && serde_json::to_value(c)?
                        == serde_json::to_value(serde_json::from_value::<Checkpoint>(
                            h.operations["checkpoint"].result.clone().unwrap()
                        )?)?,
                "result checkpoint original owner/result"
            );
        }
        if let Some(p) = &h.push {
            ensure!(
                h.checkpoint.is_some() && p.identity == self.remote()?.0,
                "result push before checkpoint or by another identity"
            );
        }
        if let Some(r) = &h.receipt {
            r.validate()?;
            let c = h.checkpoint.as_ref();
            ensure!(
                h.push.as_ref().is_some_and(|p| p.pushed)
                    && r.transfer_id == s.operations.transfer
                    && r.repository_id == s.source.repository_id
                    && r.binding_generation == s.source.binding_generation
                    && c.is_some_and(|c| r.source == c.source),
                "central source receipt differs from the pushed checkpoint"
            );
        }
        Ok(())
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
