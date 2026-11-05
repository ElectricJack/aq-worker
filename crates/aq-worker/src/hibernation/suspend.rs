//! Physical suspension (spec 0004 §3). Every step journals its result before
//! the next effect, so a crash at any boundary resumes the same local
//! operation: no repeated stop after a durable stop record, uploads resume at
//! the journaled offset under the same operation id, and the parking report
//! reuses one message id.
//!
//! Agent-side mediation (design review rev-agile-falcon, Gap 4b):
//! - `request_checkpoint` asks the agent to retain its [`SummaryClaim`] JSON
//!   through `POST /v1/execution/artifacts/*` with role `checkpoint`; the
//!   Suspender fetches and verifies those retained bytes before it trusts the
//!   claim ([`agent_checkpoint_claim`]).
//! - `close_input` is a worker-local [`InputGate`] fence recording the last
//!   accepted operation id. AQ observes it only through the effects report
//!   (`POST /v1/worker/lifecycle/effects`), which `reconcile_effects` carries.
//! - `ReportedStopped` names a retained `result` artifact whose bytes are the
//!   canonical [`StopProof`] JSON; the Suspender retains it right after the
//!   stop is journaled so every parking report carries its receipt.
use super::archive::{SourceArchive, Unsupported};
use super::credentials::{self, CredentialLayout, ScrubbedSecrets};
use super::journal::{Bound, Journal};
use super::{Boundaries, CentralRetention, LifecycleApi, artifact_ref, require, send_exact, text};
use anyhow::{Context, Result, bail};
use aq_lifecycle_protocol::{
    ArtifactRef, ArtifactRole, Binding, Checkpoint, CheckpointClaim, ContinuationRef, Digest,
    EffectsObservation, Message, Packet, ParkingReport, Receipt, RetentionReceipt, StopObservation,
    StopProof, SummaryClaim, SuspensionDisposition, SuspensionIntent, Text, Validate,
    canonical_digest,
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
    "upload_journaled:result",
    "upload_chunk:result",
    "receipt_recorded:result",
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

/// Upper bound on the agent's retained checkpoint artifact (a [`SummaryClaim`]
/// JSON body: 16 KiB of summary text plus bounded references and ids).
pub const AGENT_CHECKPOINT_MAX_BYTES: u64 = 64 * 1024;

/// What the agent is asked to retain for a checkpoint. The artifact id is
/// deterministic per continuation generation so a repeated request after a
/// crash names the same artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointRequest {
    pub binding: Binding,
    pub continuation: ContinuationRef,
    pub artifact_id: Text,
    pub role: ArtifactRole,
    pub deadline_ms: u64,
    pub max_summary_bytes: u32,
}
impl CheckpointRequest {
    pub fn for_intent(intent: &SuspensionIntent) -> Result<Self> {
        let c = &intent.continuation;
        Ok(Self {
            binding: intent.binding.clone(),
            continuation: c.clone(),
            artifact_id: text(format!(
                "hibernation-agent-checkpoint-{}-g{}",
                c.continuation_id, c.generation
            ))?,
            role: ArtifactRole::Checkpoint,
            deadline_ms: intent.deadline_ms,
            max_summary_bytes: intent.max_summary_bytes,
        })
    }
}

/// The worker-local input fence: the last agent operation accepted before
/// input closed (`None` if none was accepted). AQ sees it only through the
/// effects report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputFence {
    pub last_accepted_operation_id: Option<Uuid>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateState {
    last_accepted_operation_id: Option<Uuid>,
    closed: bool,
}

/// Durable worker-local admission gate for agent operations. `admit` records
/// each accepted operation id; `close` fences input and returns the fence.
/// Closing is idempotent, and no operation is admitted after it.
pub struct InputGate {
    journal: Journal,
}
impl InputGate {
    pub fn new(dir: impl AsRef<std::path::Path>) -> Result<Self> {
        Ok(Self {
            journal: Journal::new(dir.as_ref(), "input-fence")?,
        })
    }
    fn state(&self) -> Result<GateState> {
        Ok(self.journal.load()?.unwrap_or_default())
    }
    /// Admit one agent operation, journaled before the caller acts on it.
    pub fn admit(&self, operation_id: Uuid) -> Result<()> {
        let mut state = self.state()?;
        require(!state.closed, "agent input is closed for suspension")?;
        state.last_accepted_operation_id = Some(operation_id);
        self.journal.store(&state)
    }
    /// Close input and return the fence. Repeats return the same fence.
    pub fn close(&self) -> Result<InputFence> {
        let mut state = self.state()?;
        if !state.closed {
            state.closed = true;
            self.journal.store(&state)?;
        }
        Ok(InputFence {
            last_accepted_operation_id: state.last_accepted_operation_id,
        })
    }
    /// The recorded fence, if input has closed.
    pub fn fence(&self) -> Result<Option<InputFence>> {
        let state = self.state()?;
        Ok(state.closed.then_some(InputFence {
            last_accepted_operation_id: state.last_accepted_operation_id,
        }))
    }
}

/// The guardian's answer to a writer stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriterStop {
    /// Stop could not be proven; suspension holds.
    Unknown { reason: String },
    /// Every owned writer stopped; `proof` becomes the retained `result` artifact.
    Stopped {
        proof: StopProof,
        writer_count: u32,
        observed_at_ms: u64,
    },
}

/// Node effects driven by an admitted suspension directive.
pub trait SuspensionPorts {
    /// Ask the agent, through the scoped AQ API, to retain its `SummaryClaim`
    /// JSON as `request.artifact_id` with role `checkpoint`. Returns the
    /// observed retention receipt, or `None` if the agent did not answer by
    /// the deadline. A claim, not stop proof.
    fn request_checkpoint(
        &mut self,
        request: &CheckpointRequest,
    ) -> Result<Option<RetentionReceipt>>;
    /// Close new input and ordinary agent operations ([`InputGate::close`]).
    /// Must be idempotent and return the same fence on repeat.
    fn close_input(&mut self) -> Result<InputFence>;
    /// Stop every owned writer through the guardian.
    fn stop_writers(&mut self) -> Result<WriterStop>;
    /// Hand pending domain effects and the input fence to central
    /// reconciliation (`POST /v1/worker/lifecycle/effects`).
    fn reconcile_effects(
        &mut self,
        fence: &InputFence,
        pending: &[Uuid],
    ) -> Result<EffectsObservation>;
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
    stop_observation_id: Uuid,
    checkpoint: Option<Checkpoint>,
    checkpoint_recorded: bool,
    input_fence: Option<InputFence>,
    stop: Option<StopObservation>,
    stop_proof: Option<StopProof>,
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
                stop_observation_id: Uuid::new_v4(),
                checkpoint: None,
                checkpoint_recorded: false,
                input_fence: None,
                stop: None,
                stop_proof: None,
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
            let request = CheckpointRequest::for_intent(intent)?;
            let retained = ports.request_checkpoint(&request)?;
            let claim = agent_checkpoint_claim(&request, retained.as_ref(), retention)?;
            let claim = bounded_claim(claim, intent)?;
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
        if state.input_fence.is_none() {
            state.input_fence = Some(ports.close_input()?);
            self.save(&journal, &state, "input_closed")?;
        }
        let fence = state.input_fence.clone().context("journaled input fence")?;
        if state.stop.is_none() {
            let (stop, proof) = match ports.stop_writers()? {
                WriterStop::Unknown { reason } => (
                    StopObservation::Unknown {
                        reason: text(reason)?,
                    },
                    None,
                ),
                WriterStop::Stopped {
                    proof,
                    writer_count,
                    observed_at_ms,
                } => {
                    let c = &intent.continuation;
                    let id = format!(
                        "hibernation-{}-{}-g{}",
                        role_name(&ArtifactRole::Result),
                        c.continuation_id,
                        c.generation
                    );
                    let stop = proof.observation(
                        state.stop_observation_id,
                        writer_count,
                        observed_at_ms,
                        text(id)?,
                    )?;
                    (stop, Some(proof))
                }
            };
            stop.validate()?;
            state.stop = Some(stop);
            state.stop_proof = proof;
            self.save(&journal, &state, "stop_recorded")?;
        }
        let stop = state.stop.clone().context("journaled stop")?;
        if let StopObservation::Unknown { reason } = &stop {
            let reason = format!("writer stop not proven: {}", reason.as_str());
            return self.park(&journal, &mut state, Some(reason), aq);
        }
        // Retain the stop proof (role `result`) before anything relies on it.
        let proof = state.stop_proof.clone().context("journaled stop proof")?;
        let proof_bytes = proof.to_bytes()?;
        self.upload(
            &journal,
            &mut state,
            ArtifactRole::Result,
            &proof_bytes,
            retention,
        )?;
        require(
            aq_lifecycle_protocol::stop_proof_receipt(&stop, &self.receipts(&state)).is_some(),
            "retained stop proof does not match the reported observation",
        )?;

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
            let effects = ports.reconcile_effects(&fence, &pending)?;
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

    fn receipts(&self, state: &State) -> Vec<RetentionReceipt> {
        state
            .uploads
            .iter()
            .filter_map(|u| u.receipt.clone())
            .collect()
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
            central_receipts: self.receipts(state),
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

/// Turn the agent's retained checkpoint artifact into a claim. Anything the
/// agent got wrong (no answer, wrong role, artifact or attempt, digest or byte
/// mismatch, undecodable body) is an explicit missing claim, never an invented
/// one. Infrastructure failures (retention fetch) are errors so the step retries.
pub fn agent_checkpoint_claim(
    request: &CheckpointRequest,
    retained: Option<&RetentionReceipt>,
    retention: &impl CentralRetention,
) -> Result<CheckpointClaim> {
    let missing = |why: &str| -> Result<CheckpointClaim> {
        Ok(CheckpointClaim::Missing { reason: text(why)? })
    };
    let Some(receipt) = retained else {
        return missing("agent did not checkpoint before the directive deadline");
    };
    if receipt.validate().is_err()
        || receipt.role != request.role
        || receipt.artifact.id != request.artifact_id
        || receipt.binding_digest != canonical_digest(&request.binding)?
    {
        return missing("agent checkpoint receipt does not match the requested artifact");
    }
    if receipt.bytes > AGENT_CHECKPOINT_MAX_BYTES {
        return missing("agent checkpoint exceeded the retained byte bound");
    }
    let bytes = retention
        .fetch(receipt)
        .context("fetch agent checkpoint artifact")?;
    if bytes.len() as u64 != receipt.bytes || Digest::of(&bytes) != receipt.artifact.digest {
        return missing("agent checkpoint bytes do not match the retention receipt");
    }
    let Ok(summary) = serde_json::from_slice::<SummaryClaim>(&bytes) else {
        return missing("agent checkpoint is not a summary claim");
    };
    if summary.validate().is_err() {
        return missing("agent checkpoint is not a valid summary claim");
    }
    Ok(CheckpointClaim::Supplied {
        summary_digest: canonical_digest(&summary)?,
        summary,
    })
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
