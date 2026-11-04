//! Physical suspension (spec 0004 §3). Every step journals its result before
//! the next effect, so a crash at any boundary resumes the same local
//! operation: no repeated stop after a durable stop record, uploads resume at
//! the journaled offset under the same operation id, and the parking report
//! reuses one message id.
use super::archive::{SourceArchive, Unsupported};
use super::credentials::{self, CredentialLayout, ScrubbedSecrets};
use super::journal::{Bound, Journal};
use super::{Boundaries, CentralRetention, LifecycleApi, artifact_ref, require, send_exact, text};
use anyhow::{Context, Result, bail};
use aq_lifecycle_protocol::{
    ArtifactRef, ArtifactRole, Binding, Checkpoint, CheckpointClaim, Digest, EffectsObservation,
    Message, Packet, ParkingReport, Receipt, RetentionReceipt, StopObservation,
    SuspensionDisposition, SuspensionIntent, Validate, canonical_digest,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Every durable boundary, in order, at which a crash is recoverable.
pub const SUSPENSION_BOUNDARIES: &[&str] = &[
    "intent_journaled",
    "checkpoint_journaled",
    "checkpoint_recorded",
    "input_closed",
    "stop_recorded",
    "snapshot_recorded",
    "credentials_scrubbed",
    "upload_journaled:source",
    "upload_chunk:source",
    "receipt_recorded:source",
    "upload_journaled:checkpoint",
    "upload_chunk:checkpoint",
    "receipt_recorded:checkpoint",
    "effects_recorded",
    "compute_released",
    "parking_reported",
];

const SOURCE_FILE: &str = "source.archive";

/// Node effects driven by an admitted suspension directive.
pub trait SuspensionPorts {
    /// Bounded agent checkpoint through the scoped AQ API. A claim, not stop proof.
    fn request_checkpoint(&mut self, intent: &SuspensionIntent) -> Result<CheckpointClaim>;
    /// Close new input and ordinary agent operations.
    fn close_input(&mut self) -> Result<()>;
    /// Stop every owned writer through the guardian.
    fn stop_writers(&mut self) -> Result<StopObservation>;
    /// Hand pending domain effects to central reconciliation.
    fn reconcile_effects(&mut self, pending: &[Uuid]) -> Result<EffectsObservation>;
    /// Release compute after stopped-writer proof. Must be idempotent.
    fn release_compute(&mut self, stop: &StopObservation) -> Result<()>;
    /// Release protected disk under an explicit directive. Must be idempotent.
    fn release_disk(&mut self, directive: &ArtifactRef) -> Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    role: ArtifactRole,
    operation_id: Uuid,
    artifact: ArtifactRef,
    total: u64,
    offset: u64,
    receipt: Option<RetentionReceipt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    intent_digest: Digest,
    intent: SuspensionIntent,
    checkpoint_message_id: Uuid,
    parking_message_id: Uuid,
    disk_message_id: Uuid,
    checkpoint: Option<Checkpoint>,
    checkpoint_recorded: bool,
    input_closed: bool,
    stop: Option<StopObservation>,
    hold: Option<String>,
    source: Option<(Digest, u64)>,
    credential_fingerprints: Vec<String>,
    scrubbed: bool,
    uploads: Vec<Upload>,
    effects: Option<EffectsObservation>,
    compute_released: bool,
    parking: Option<(ParkingReport, Receipt)>,
    disk: Option<(ParkingReport, Receipt)>,
}
impl Bound for State {
    fn bound_digest(&self) -> &Digest {
        &self.intent_digest
    }
}

/// The reported parking disposition and AQ's exact receipt for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspended {
    pub report: ParkingReport,
    pub receipt: Receipt,
}

pub struct Suspender {
    dir: PathBuf,
    local: Binding,
    layout: CredentialLayout,
    chunk_bytes: usize,
    boundaries: Boundaries,
}

impl Suspender {
    /// `local` is this node's current binding (session, attempt, node, boot,
    /// assignment generation); a directive for any other binding is stale.
    pub fn new(dir: impl Into<PathBuf>, local: Binding, layout: CredentialLayout) -> Self {
        Self {
            dir: dir.into(),
            local,
            layout,
            chunk_bytes: 1024 * 1024,
            boundaries: Boundaries::default(),
        }
    }
    pub fn with_chunk_bytes(mut self, bytes: usize) -> Self {
        self.chunk_bytes = bytes.max(1);
        self
    }
    pub fn with_boundaries(mut self, boundaries: Boundaries) -> Self {
        self.boundaries = boundaries;
        self
    }

    fn journal(&self) -> Result<Journal> {
        Journal::new(&self.dir, "suspension")
    }

    fn save(&mut self, journal: &Journal, state: &State, boundary: &str) -> Result<()> {
        journal.store(state)?;
        self.boundaries.mark(boundary)
    }

    /// Run (or recover) the suspension named by an AQ `Suspend` directive.
    pub fn suspend(
        &mut self,
        directive: &Packet,
        ports: &mut impl SuspensionPorts,
        aq: &mut impl LifecycleApi,
        retention: &mut impl CentralRetention,
    ) -> Result<Suspended> {
        directive.validate()?;
        let Message::Suspend(intent) = &directive.body.message else {
            bail!("not a suspension directive");
        };
        require(
            intent.binding == self.local,
            "stale suspension directive: binding does not match this session, node, boot and assignment generation",
        )?;
        // 1. Journal the directive digest before any effect.
        let journal = self.journal()?;
        let digest = canonical_digest(intent)?;
        let (mut state, created) = journal.open_bound(&digest, || {
            Ok(State {
                intent_digest: digest.clone(),
                intent: intent.clone(),
                checkpoint_message_id: Uuid::new_v4(),
                parking_message_id: Uuid::new_v4(),
                disk_message_id: Uuid::new_v4(),
                checkpoint: None,
                checkpoint_recorded: false,
                input_closed: false,
                stop: None,
                hold: None,
                source: None,
                credential_fingerprints: Vec::new(),
                scrubbed: false,
                uploads: Vec::new(),
                effects: None,
                compute_released: false,
                parking: None,
                disk: None,
            })
        })?;
        if created {
            self.boundaries.mark("intent_journaled")?;
        }
        if let Some((report, receipt)) = &state.parking {
            return Ok(Suspended {
                report: state.disk.as_ref().map_or(report, |d| &d.0).clone(),
                receipt: state.disk.as_ref().map_or(receipt, |d| &d.1).clone(),
            });
        }

        // 2. Bounded checkpoint summary, journaled before it is sent.
        if state.checkpoint.is_none() {
            let claim = bounded_claim(ports.request_checkpoint(intent)?, intent)?;
            let checkpoint = Checkpoint {
                binding: intent.binding.clone(),
                continuation: intent.continuation.clone(),
                claim,
            };
            checkpoint.validate()?;
            state.checkpoint = Some(checkpoint);
            self.save(&journal, &state, "checkpoint_journaled")?;
        }
        let checkpoint = state.checkpoint.clone().context("journaled checkpoint")?;
        if !state.checkpoint_recorded {
            let packet = Packet::seal(
                state.checkpoint_message_id,
                Message::Checkpoint(checkpoint.clone()),
            )?;
            send_exact(aq, &packet)?;
            state.checkpoint_recorded = true;
            self.save(&journal, &state, "checkpoint_recorded")?;
        }

        // 3. Close input, then stop every writer through the guardian.
        if !state.input_closed {
            ports.close_input()?;
            state.input_closed = true;
            self.save(&journal, &state, "input_closed")?;
        }
        if state.stop.is_none() {
            let stop = ports.stop_writers()?;
            stop.validate()?;
            state.stop = Some(stop);
            self.save(&journal, &state, "stop_recorded")?;
        }
        let stop = state.stop.clone().context("journaled stop")?;
        if let StopObservation::Unknown { reason } = &stop {
            let reason = format!("writer stop not proven: {}", reason.as_str());
            return self.park(&journal, &mut state, Some(reason), aq);
        }

        // 4. After confirmed stop: snapshot, scan for credentials, then scrub.
        let checkpoint_bytes = serde_json::to_vec(&checkpoint)?;
        if state.source.is_none() && state.hold.is_none() {
            let secrets = ScrubbedSecrets::capture(&self.layout)?;
            state.credential_fingerprints = secrets.fingerprints.clone();
            match SourceArchive::capture(&self.layout.workspace())? {
                Err(Unsupported(why)) => state.hold = Some(why.to_owned()),
                Ok(archive) => {
                    let leak = archive
                        .decoded()?
                        .iter()
                        .find_map(|(path, bytes)| secrets.scan(path, bytes))
                        .or_else(|| secrets.scan("checkpoint", &checkpoint_bytes));
                    if let Some(why) = leak {
                        state.hold = Some(why.to_owned());
                    } else {
                        let bytes = archive.to_bytes()?;
                        crate::guardian::durable_replace(&self.dir.join(SOURCE_FILE), &bytes)?;
                        state.source = Some((Digest::of(&bytes), bytes.len() as u64));
                    }
                }
            }
            self.save(&journal, &state, "snapshot_recorded")?;
        }
        if !state.scrubbed {
            credentials::scrub(&self.layout)?;
            state.scrubbed = true;
            self.save(&journal, &state, "credentials_scrubbed")?;
        }
        if let Some(hold) = state.hold.clone() {
            return self.park(&journal, &mut state, Some(hold), aq);
        }

        // 5. Resumable upload to central retention with exact receipts.
        let source = std::fs::read(self.dir.join(SOURCE_FILE)).context("retained snapshot")?;
        let (source_digest, source_len) = state.source.clone().context("journaled snapshot")?;
        require(
            Digest::of(&source) == source_digest && source.len() as u64 == source_len,
            "local snapshot changed after it was journaled",
        )?;
        for (role, bytes) in [
            (ArtifactRole::Source, source.as_slice()),
            (ArtifactRole::Checkpoint, checkpoint_bytes.as_slice()),
        ] {
            self.upload(&journal, &mut state, role, bytes, retention)?;
        }

        // Pending domain effects go to central reconciliation, never cancelled here.
        if state.effects.is_none() {
            let pending = match &checkpoint.claim {
                CheckpointClaim::Supplied { summary, .. } => summary.pending_operation_ids.clone(),
                CheckpointClaim::Missing { .. } => Vec::new(),
            };
            let effects = ports.reconcile_effects(&pending)?;
            effects.validate()?;
            state.effects = Some(effects);
            self.save(&journal, &state, "effects_recorded")?;
        }

        // 6. Release compute after stopped-writer proof; disk stays protected.
        if !state.compute_released {
            ports.release_compute(&stop)?;
            state.compute_released = true;
            self.save(&journal, &state, "compute_released")?;
        }
        self.park(&journal, &mut state, None, aq)
    }

    fn upload(
        &mut self,
        journal: &Journal,
        state: &mut State,
        role: ArtifactRole,
        bytes: &[u8],
        retention: &mut impl CentralRetention,
    ) -> Result<()> {
        let name = role_name(&role);
        let index = match state.uploads.iter().position(|u| u.role == role) {
            Some(i) => i,
            None => {
                let c = &state.intent.continuation;
                state.uploads.push(Upload {
                    role: role.clone(),
                    operation_id: Uuid::new_v4(),
                    artifact: artifact_ref(
                        format!("hibernation-{name}-{}-g{}", c.continuation_id, c.generation),
                        bytes,
                    )?,
                    total: bytes.len() as u64,
                    offset: 0,
                    receipt: None,
                });
                self.save(journal, state, &format!("upload_journaled:{name}"))?;
                state.uploads.len() - 1
            }
        };
        require(
            Digest::of(bytes) == state.uploads[index].artifact.digest,
            "retained bytes differ from the journaled artifact",
        )?;
        while state.uploads[index].offset < state.uploads[index].total {
            let upload = &state.uploads[index];
            let start = upload.offset as usize;
            let end = (start + self.chunk_bytes).min(bytes.len());
            let committed =
                retention.upload_chunk(upload.operation_id, upload.offset, &bytes[start..end])?;
            require(
                committed == end as u64,
                "retention acknowledged an unexpected offset",
            )?;
            state.uploads[index].offset = committed;
            self.save(journal, state, &format!("upload_chunk:{name}"))?;
        }
        if state.uploads[index].receipt.is_none() {
            let upload = &state.uploads[index];
            let receipt = retention.commit(
                upload.operation_id,
                &upload.artifact,
                role.clone(),
                &self.local,
            )?;
            receipt.validate()?;
            require(
                receipt.artifact == upload.artifact
                    && receipt.role == role
                    && receipt.bytes == upload.total
                    && receipt.binding_digest == canonical_digest(&self.local)?,
                "retention receipt does not match the uploaded artifact",
            )?;
            state.uploads[index].receipt = Some(receipt);
            self.save(journal, state, &format!("receipt_recorded:{name}"))?;
        }
        Ok(())
    }

    fn report(&self, state: &State, hold: Option<String>) -> Result<ParkingReport> {
        let stop = state
            .stop
            .clone()
            .context("parking needs a stop observation")?;
        let effects = state
            .effects
            .clone()
            .unwrap_or_else(|| EffectsObservation::Pending {
                operation_ids: match state.checkpoint.as_ref().map(|c| &c.claim) {
                    Some(CheckpointClaim::Supplied { summary, .. }) => {
                        summary.pending_operation_ids.clone()
                    }
                    _ => Vec::new(),
                },
            });
        let disposition = match hold {
            Some(reason) => SuspensionDisposition::Held {
                reason: text(reason)?,
            },
            None if matches!(effects, EffectsObservation::Reconciled { .. }) => {
                SuspensionDisposition::RetainedSuspended
            }
            None => SuspensionDisposition::StoppedRetaining,
        };
        let local = std::fs::metadata(self.dir.join(SOURCE_FILE)).map_or(0, |m| m.len());
        let report = ParkingReport {
            binding: state.intent.binding.clone(),
            continuation: state.intent.continuation.clone(),
            disposition,
            stop,
            local_retained_bytes: local,
            central_receipts: state
                .uploads
                .iter()
                .filter_map(|u| u.receipt.clone())
                .collect(),
            effects,
            compute_released: state.compute_released,
            protected_disk_released: false,
            disk_release_directive: None,
        };
        report.validate()?;
        Ok(report)
    }

    fn park(
        &mut self,
        journal: &Journal,
        state: &mut State,
        hold: Option<String>,
        aq: &mut impl LifecycleApi,
    ) -> Result<Suspended> {
        let report = self.report(state, hold)?;
        let packet = Packet::seal(state.parking_message_id, Message::Parking(report.clone()))?;
        let receipt = send_exact(aq, &packet)?;
        state.parking = Some((report.clone(), receipt.clone()));
        self.save(journal, state, "parking_reported")?;
        Ok(Suspended { report, receipt })
    }

    /// Release protected disk. Only an explicit directive after a retained
    /// suspension can do this; compute release never implies it.
    pub fn release_disk(
        &mut self,
        directive: &ArtifactRef,
        ports: &mut impl SuspensionPorts,
        aq: &mut impl LifecycleApi,
    ) -> Result<Suspended> {
        let journal = self.journal()?;
        let mut state: State = journal.load()?.context("no suspension to release")?;
        if let Some((report, receipt)) = &state.disk {
            require(
                report.disk_release_directive.as_ref() == Some(directive),
                "protected disk already released under another directive",
            )?;
            return Ok(Suspended {
                report: report.clone(),
                receipt: receipt.clone(),
            });
        }
        let (parked, _) = state.parking.clone().context("suspension has not parked")?;
        require(
            parked.disposition == SuspensionDisposition::RetainedSuspended,
            "protected disk is released only after a retained suspension",
        )?;
        ports.release_disk(directive)?;
        match std::fs::remove_file(self.dir.join(SOURCE_FILE)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let report = ParkingReport {
            local_retained_bytes: 0,
            protected_disk_released: true,
            disk_release_directive: Some(directive.clone()),
            ..parked
        };
        report.validate()?;
        let packet = Packet::seal(state.disk_message_id, Message::Parking(report.clone()))?;
        let receipt = send_exact(aq, &packet)?;
        state.disk = Some((report.clone(), receipt.clone()));
        self.save(&journal, &state, "disk_released")?;
        Ok(Suspended { report, receipt })
    }
}

/// Enforce the directive's summary budget. An over-budget summary becomes an
/// explicit missing claim rather than a truncated (invented) one.
fn bounded_claim(claim: CheckpointClaim, intent: &SuspensionIntent) -> Result<CheckpointClaim> {
    claim.validate()?;
    if let CheckpointClaim::Supplied { summary, .. } = &claim
        && summary.text.as_str().len() > intent.max_summary_bytes as usize
    {
        return Ok(CheckpointClaim::Missing {
            reason: text("checkpoint summary exceeded the directive budget")?,
        });
    }
    Ok(claim)
}

fn role_name(role: &ArtifactRole) -> &'static str {
    match role {
        ArtifactRole::Source => "source",
        ArtifactRole::Checkpoint => "checkpoint",
        ArtifactRole::Transcript => "transcript",
        ArtifactRole::Result => "result",
    }
}
