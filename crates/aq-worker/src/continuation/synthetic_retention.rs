//! Explicit synthetic-retention-profile-v1 fixture. Not a production coordinator,
//! hold/summary/effects owner or new interpretation of the common CON03 contract.
use super::history::{ParkingHistory, ParkingRetentionHistory};
use crate::source_transfer::UploadAction;
use anyhow::{Context, Result, ensure};
use aq_lifecycle_protocol::{ArtifactRole, CheckpointClaim, EffectsObservation, RetentionReceipt};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use aq_worker_protocol::{CentralRetentionReceipt, NodeIdentity, TransferFile, TransferStatus};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};
use uuid::Uuid;

const MAX_FIXTURE_BYTES: u64 = 8 * 1024 * 1024;
fn reference(id: Uuid, digest: Digest) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(id.to_string())?,
        version: Text::new("1")?,
        digest,
    })
}
fn persist(root: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(name))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(root)?.sync_all()?;
    Ok(())
}
/// Non-serializable fixture owner. IDs/retention interval are allocated once;
/// replay never extends the interval or creates replacement artifact identities.
pub(crate) struct SyntheticRetentionOwner {
    root: tempfile::TempDir,
    original: ParkingHistory,
    identity: NodeIdentity,
    retained_at: u64,
    retained_until: u64,
    begun: bool,
    manifest: Vec<u8>,
    bundle: Vec<u8>,
    operations: BTreeMap<Uuid, (Digest, TransferStatus)>,
    source_receipt: Option<CentralRetentionReceipt>,
    source_artifact_id: Uuid,
    source_receipt_id: Uuid,
    checkpoint_artifact_id: Uuid,
    checkpoint_receipt_id: Uuid,
    summary: Option<Vec<u8>>,
    resolved_effects: BTreeSet<Uuid>,
    effects_marker: Option<ArtifactRef>,
    effects: Option<ArtifactRef>,
    report: Option<ParkingRetentionHistory>,
}
impl SyntheticRetentionOwner {
    pub(crate) fn new(
        original: &ParkingHistory,
        retained_at: u64,
        retained_until: u64,
    ) -> Result<Self> {
        original.validate()?;
        ensure!(
            original.settled_source_claim()? && original.source.transfer.is_some(),
            "fixture needs a completed predecessor source and transfer"
        );
        ensure!(
            retained_at >= original.intent.suspension.issued_at_ms
                && retained_until > retained_at
                && retained_until - retained_at <= 300_000,
            "fixture original retention interval"
        );
        let transfer = original.source.transfer.as_ref().unwrap();
        ensure!(
            transfer
                .manifest_bytes
                .checked_add(transfer.bundle_bytes)
                .is_some_and(|n| n <= MAX_FIXTURE_BYTES),
            "fixture byte cap"
        );
        let identity = original
            .source
            .upload
            .as_ref()
            .context("fixture upload identity absent")?
            .identity
            .clone();
        Ok(Self {
            root: tempfile::tempdir()?,
            original: original.clone(),
            identity,
            retained_at,
            retained_until,
            begun: false,
            manifest: vec![],
            bundle: vec![],
            operations: BTreeMap::new(),
            source_receipt: None,
            source_artifact_id: Uuid::new_v4(),
            source_receipt_id: Uuid::new_v4(),
            checkpoint_artifact_id: Uuid::new_v4(),
            checkpoint_receipt_id: Uuid::new_v4(),
            summary: None,
            resolved_effects: BTreeSet::new(),
            effects_marker: None,
            effects: None,
            report: None,
        })
    }
    fn current(&self, identity: &NodeIdentity, now: u64) -> Result<()> {
        ensure!(
            identity == &self.identity && now >= self.retained_at && now < self.retained_until,
            "synthetic owner identity/retention interval"
        );
        Ok(())
    }
    fn status(&self) -> TransferStatus {
        TransferStatus {
            transfer_id: self.original.intent.operations.transfer,
            manifest_offset: self.manifest.len() as u64,
            bundle_offset: self.bundle.len() as u64,
            receipt: self.source_receipt.clone(),
        }
    }
    pub(crate) fn accept(&mut self, action: &UploadAction, now: u64) -> Result<TransferStatus> {
        let transfer = self.original.source.transfer.as_ref().unwrap();
        let (operation, identity, encoded): (Uuid, &NodeIdentity, Value) = match action {
            UploadAction::Begin {
                identity,
                operation_id,
                manifest,
            } => (
                *operation_id,
                identity,
                json!(["begin", identity, operation_id, manifest]),
            ),
            UploadAction::Chunk { request, bytes } => {
                ensure!(
                    Digest::of(bytes).as_str() == request.digest
                        && bytes.len() == request.length as usize,
                    "fixture actual chunk bytes"
                );
                (
                    request.operation_id,
                    &request.identity,
                    json!(["chunk", request]),
                )
            }
            UploadAction::Finalize(request) => (
                request.operation_id,
                &request.identity,
                json!(["finalize", request]),
            ),
        };
        self.current(identity, now)?;
        let fingerprint = Digest::of(&serde_json::to_vec(&encoded)?);
        if let Some((prior, status)) = self.operations.get(&operation) {
            ensure!(prior == &fingerprint, "fixture operation recipe changed");
            return Ok(status.clone());
        }
        match action {
            UploadAction::Begin {
                operation_id,
                manifest,
                ..
            } => {
                ensure!(
                    !self.begun
                        && *operation_id == self.original.intent.operations.upload_begin
                        && manifest == transfer,
                    "fixture exact begin"
                );
                self.begun = true;
            }
            UploadAction::Chunk { request, bytes } => {
                use aq_worker_protocol::Validate as _;
                request.validate()?;
                ensure!(
                    self.begun
                        && self.source_receipt.is_none()
                        && request.transfer_id == transfer.transfer_id,
                    "fixture chunk phase"
                );
                let (target, size) = match request.file {
                    TransferFile::Manifest => (&mut self.manifest, transfer.manifest_bytes),
                    TransferFile::Bundle => {
                        ensure!(
                            self.manifest.len() as u64 == transfer.manifest_bytes,
                            "manifest precedes bundle"
                        );
                        (&mut self.bundle, transfer.bundle_bytes)
                    }
                };
                ensure!(
                    request.offset == target.len() as u64
                        && (target.len() as u64)
                            .checked_add(bytes.len() as u64)
                            .is_some_and(|n| n <= size),
                    "fixture exact bounded range"
                );
                target.extend_from_slice(bytes);
            }
            UploadAction::Finalize(request) => {
                ensure!(
                    self.begun
                        && request.operation_id == self.original.intent.operations.upload_finalize
                        && request.transfer_id == transfer.transfer_id
                        && self.manifest.len() as u64 == transfer.manifest_bytes
                        && self.bundle.len() as u64 == transfer.bundle_bytes,
                    "fixture finalize before complete bytes"
                );
                ensure!(
                    Digest::of(&self.manifest).as_str() == transfer.manifest_digest
                        && transfer
                            .bundle_digest
                            .as_ref()
                            .is_none_or(|digest| Digest::of(&self.bundle).as_str() == digest),
                    "fixture full actual source SHA"
                );
                let actual: crate::source::GitManifest = serde_json::from_slice(&self.manifest)?;
                let exported = self.original.source.exported.as_ref().unwrap();
                ensure!(
                    serde_json::to_value(&actual)? == serde_json::to_value(&exported.manifest)?,
                    "fixture actual source descriptor"
                );
                persist(self.root.path(), "manifest.json", &self.manifest)?;
                persist(self.root.path(), "objects.bundle", &self.bundle)?;
                self.source_receipt = Some(CentralRetentionReceipt {
                    receipt_id: self.source_receipt_id,
                    transfer_id: transfer.transfer_id,
                    repository_id: transfer.source.repository_id,
                    binding_generation: transfer.source.binding_generation,
                    source: transfer.checkpoint.clone(),
                    retention_handle: transfer.artifact_id,
                    manifest_digest: actual.canonical_digest()?,
                });
                crate::source_transfer::steps::validate_retention_claim(
                    self.source_receipt.as_ref().unwrap(),
                    transfer,
                    &actual,
                    &transfer.source,
                    &transfer.checkpoint,
                )?;
            }
        }
        let status = self.status();
        self.operations
            .insert(operation, (fingerprint, status.clone()));
        Ok(status)
    }
    pub(crate) fn retain_summary(
        &mut self,
        checkpoint: &aq_lifecycle_protocol::Checkpoint,
    ) -> Result<()> {
        ensure!(
            checkpoint == &self.original.intent.checkpoint,
            "fixture exact summary/checkpoint association"
        );
        let CheckpointClaim::Supplied {
            summary,
            summary_digest,
        } = &checkpoint.claim
        else {
            anyhow::bail!("missing summary cannot be retained");
        };
        let bytes = aq_session_protocol::canonical_bytes(summary)?;
        ensure!(
            Digest::of(&bytes) == *summary_digest
                && bytes.len() <= self.original.intent.suspension.max_summary_bytes as usize,
            "fixture summary canonical byte digest/budget"
        );
        if let Some(prior) = &self.summary {
            ensure!(prior == &bytes, "fixture immutable summary bytes");
            return Ok(());
        }
        persist(self.root.path(), "summary.json", &bytes)?;
        self.summary = Some(bytes);
        Ok(())
    }
    /// This is an explicit synthetic owner ledger operation, not an inference
    /// that an empty list means effects were reconciled by a real business owner.
    pub(crate) fn resolve_fixture_effect(&mut self, operation: Uuid) {
        self.resolved_effects.insert(operation);
    }
    pub(crate) fn reconcile_fixture_effects(
        &mut self,
        explicit_decision: ArtifactRef,
    ) -> Result<()> {
        let CheckpointClaim::Supplied { summary, .. } = &self.original.intent.checkpoint.claim
        else {
            anyhow::bail!("fixture summary missing");
        };
        ensure!(
            self.source_receipt.is_some()
                && summary
                    .pending_operation_ids
                    .iter()
                    .all(|id| self.resolved_effects.contains(id)),
            "fixture effects still pending"
        );
        if let Some(prior) = &self.effects_marker {
            ensure!(
                prior == &explicit_decision,
                "fixture original ledger decision changed"
            );
            return Ok(());
        }
        let bytes = serde_json::to_vec(
            &json!({"profile":"synthetic-retention-profile-v1","decision":explicit_decision,"binding":self.original.intent.suspension.binding,"continuation":self.original.intent.suspension.continuation,"report_operation":self.original.intent.operations.report,"stop":self.original.stop,"source_receipt":self.source_receipt,"hold":self.original.intent.synthetic_hold,"resolved":self.resolved_effects}),
        )?;
        persist(self.root.path(), "effects-ledger.json", &bytes)?;
        self.effects_marker = Some(explicit_decision);
        self.effects = Some(reference(Uuid::new_v4(), Digest::of(&bytes))?);
        Ok(())
    }
    pub(crate) fn finish(
        &mut self,
        history: &ParkingHistory,
        identity: &NodeIdentity,
        now: u64,
    ) -> Result<ParkingRetentionHistory> {
        self.current(identity, now)?;
        history.validate()?;
        ensure!(
            history.intent.digest()? == self.original.intent.digest()?
                && serde_json::to_value(&history.stop)?
                    == serde_json::to_value(&self.original.stop)?
                && history.source.transfer == self.original.source.transfer
                && serde_json::to_value(&history.source.exported)?
                    == serde_json::to_value(&self.original.source.exported)?
                && history.source.receipt == self.source_receipt
                && self.source_receipt.is_some(),
            "fixture exact predecessor/continuation/report/source association"
        );
        let summary = self
            .summary
            .as_ref()
            .context("fixture summary not retained")?;
        let effects = self
            .effects
            .as_ref()
            .context("fixture explicit reconciliation absent")?;
        let CheckpointClaim::Supplied { summary_digest, .. } = &history.intent.checkpoint.claim
        else {
            anyhow::bail!("fixture summary missing");
        };
        if let Some(report) = &self.report {
            return Ok(report.clone());
        }
        let binding_digest =
            aq_session_protocol::canonical_digest(&history.intent.suspension.binding)?;
        let owner = Text::new("synthetic-retention-profile-v1-owner-not-production")?;
        let source_digest: Digest =
            serde_json::from_value(json!(self.source_receipt.as_ref().unwrap().manifest_digest))?;
        let source = RetentionReceipt {
            receipt_id: self.source_receipt_id,
            owner: owner.clone(),
            binding_digest: binding_digest.clone(),
            artifact: reference(self.source_artifact_id, source_digest)?,
            role: ArtifactRole::Source,
            bytes: (self.manifest.len() + self.bundle.len()) as u64,
            retained_at_ms: self.retained_at,
            retained_until_ms: self.retained_until,
        };
        let checkpoint = RetentionReceipt {
            receipt_id: self.checkpoint_receipt_id,
            owner,
            binding_digest,
            artifact: reference(self.checkpoint_artifact_id, summary_digest.clone())?,
            role: ArtifactRole::Checkpoint,
            bytes: summary.len() as u64,
            retained_at_ms: self.retained_at,
            retained_until_ms: self.retained_until,
        };
        let bytes = serde_json::to_vec(
            &json!({"profile":"synthetic-retention-profile-v1","source":source,"checkpoint":checkpoint,"effects":effects,"continuation":history.intent.suspension.continuation,"report_operation":history.intent.operations.report}),
        )?;
        persist(self.root.path(), "report.json", &bytes)?;
        let report = ParkingRetentionHistory {
            source,
            checkpoint,
            effects: EffectsObservation::Reconciled {
                receipt: effects.clone(),
            },
            report_ack: reference(Uuid::new_v4(), Digest::of(&bytes))?,
        };
        self.report = Some(report.clone());
        Ok(report)
    }
}
