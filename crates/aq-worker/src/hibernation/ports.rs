//! `SuspensionPorts` over the local node: writer stop through the subreaper
//! guardian (`crate::guardian`) and compute/disk release through the placement
//! backend. Every effect is idempotent under replay, because the suspender
//! repeats a port effect whose journal write was lost to a crash.
//!
//! The agent-side effects (`request_checkpoint`, `close_input`,
//! `reconcile_effects`) have no AQ or harness surface yet. They go through the
//! `AgentLifecycle` port until the platform follow-up defines one
//! (docs/implementation/H5.md, "Port-level TODOs").
use anyhow::{Result, bail};
use aq_lifecycle_protocol::{
    ArtifactRef, CheckpointClaim, Digest, EffectsObservation, StopObservation, SuspensionIntent,
};
use aq_worker_protocol::ProcessOutcome;
use sha2::{Digest as _, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::{SuspensionPorts, artifact_ref, text};
use crate::backend::{Confirmation, DestroyAuthorization, PlacementBackend, SandboxIdentity};
use crate::guardian::{self, GuardianIdentity, GuardianPhase, GuardianState, Operation};

/// TODO(platform follow-up): agent-side suspension surface. No AQ route or
/// harness API exists for these yet; do not invent one in this adapter.
pub trait AgentLifecycle {
    /// Ask the agent for a bounded checkpoint summary (`intent.max_summary_bytes`).
    fn request_checkpoint(&mut self, intent: &SuspensionIntent) -> Result<CheckpointClaim>;
    /// Close new input and ordinary agent operations. Idempotent.
    fn close_input(&mut self) -> Result<()>;
    /// Hand pending domain effects to central reconciliation. Idempotent.
    fn reconcile_effects(&mut self, pending: &[Uuid]) -> Result<EffectsObservation>;
}

/// Stop every owned writer and report stopped-writer evidence.
pub trait WriterGuardian {
    fn stop_writers(&mut self) -> Result<StopObservation>;
}

/// The subreaper guardian for one launch directory.
pub struct LocalGuardian {
    directory: PathBuf,
    stop_wait: Duration,
}
impl LocalGuardian {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            // The guardian bounds TERM grace plus KILL at 5s.
            stop_wait: Duration::from_secs(6),
        }
    }
    fn state(&self, identity: &GuardianIdentity) -> Result<(GuardianState, Vec<u8>)> {
        let state = guardian::read_state(&self.directory, identity)?;
        let bytes = std::fs::read(self.directory.join("guardian-state.json"))?;
        Ok((state, bytes))
    }
}
impl WriterGuardian for LocalGuardian {
    /// A guardian already `Stopped` is not contacted again, so a replay after a
    /// lost journal write yields the same observation. `StopUncertain` is
    /// reported as `Unknown` (the suspender then holds). A guardian that has
    /// not reached a terminal phase within the bound is an error, which the
    /// suspender retries without journaling a stop.
    fn stop_writers(&mut self) -> Result<StopObservation> {
        let identity = guardian::read_identity(&self.directory)?;
        let (state, _) = self.state(&identity)?;
        if matches!(state.phase, GuardianPhase::Waiting | GuardianPhase::Running) {
            let sent = guardian::control(
                &self.directory,
                &identity,
                Operation::Stop {
                    reason: ProcessOutcome::Cancelled,
                },
                || Ok(()),
            );
            if let Err(error) = sent {
                // The guardian may have stopped and exited after saving proof.
                let (state, _) = self.state(&identity)?;
                if !matches!(
                    state.phase,
                    GuardianPhase::Stopped | GuardianPhase::StopUncertain
                ) {
                    return Err(error);
                }
            }
        }
        let deadline = Instant::now() + self.stop_wait;
        loop {
            let (state, bytes) = self.state(&identity)?;
            match state.phase {
                GuardianPhase::Stopped => return stopped_observation(&identity, &state, &bytes),
                GuardianPhase::StopUncertain => {
                    return Ok(StopObservation::Unknown {
                        reason: text("guardian lost; stopped writers unproven")?,
                    });
                }
                _ if Instant::now() >= deadline => {
                    bail!("guardian did not reach a stopped phase within the bound")
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

/// Map an exact guardian `Stopped` receipt (kernel ECHILD proof) to the wire
/// observation. Deterministic in the receipt bytes, so replays are identical:
/// - `observation_id`: first 16 bytes of sha256(receipt bytes);
/// - `guardian_instance_id`: the guardian launch id;
/// - `writer_set_digest`: digest of the guardian identity JSON;
/// - `writer_count`: processes the guardian reaped;
/// - `proof`: `guardian-stop:<launch_id>` over the receipt bytes.
pub fn stopped_observation(
    identity: &GuardianIdentity,
    state: &GuardianState,
    receipt_bytes: &[u8],
) -> Result<StopObservation> {
    let (true, Some(observed_at_ms)) = (
        state.phase == GuardianPhase::Stopped && state.kernel_wait_echild,
        state.stopped_at_ms,
    ) else {
        bail!("guardian receipt carries no kernel stopped proof");
    };
    let Ok(writer_count) = u32::try_from(state.reaped_count) else {
        return Ok(StopObservation::Unknown {
            reason: text("guardian writer count out of range")?,
        });
    };
    if writer_count > 65536 {
        return Ok(StopObservation::Unknown {
            reason: text("guardian writer count out of range")?,
        });
    }
    let hash = Sha256::digest(receipt_bytes);
    let mut id = [0u8; 16];
    id.copy_from_slice(&hash[..16]);
    let proof: ArtifactRef = artifact_ref(
        format!("guardian-stop:{}", identity.launch_id),
        receipt_bytes,
    )?;
    Ok(StopObservation::ReportedStopped {
        observation_id: Uuid::from_bytes(id),
        guardian_instance_id: identity.launch_id,
        writer_set_digest: Digest::of(&serde_json::to_vec(identity)?),
        writer_count,
        observed_at_ms,
        proof,
    })
}

/// Production `SuspensionPorts` for one sandbox on this node. Owns a
/// current-thread runtime for the async backend, so call it from a plain
/// thread, never from inside an async task.
pub struct NodePorts<B, G, A> {
    backend: B,
    sandbox: SandboxIdentity,
    guardian: G,
    agent: A,
    runtime: tokio::runtime::Runtime,
}
impl<B: PlacementBackend, G: WriterGuardian, A: AgentLifecycle> NodePorts<B, G, A> {
    pub fn new(backend: B, sandbox: SandboxIdentity, guardian: G, agent: A) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            backend,
            sandbox,
            guardian,
            agent,
            runtime,
        })
    }
    pub fn backend(&self) -> &B {
        &self.backend
    }
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }
}

/// The non-secret, durable release reference for a disk-release directive.
pub fn release_reference(directive: &ArtifactRef) -> String {
    format!(
        "aq-disk-release:{}@{}:{}",
        directive.id.as_str(),
        directive.version.as_str(),
        directive.digest.as_str()
    )
}

fn confirmed<T>(what: &str, result: Confirmation<T>) -> Result<T> {
    match result {
        Confirmation::Confirmed(proof) => Ok(proof),
        Confirmation::Unconfirmed(reason) => bail!("{what} unconfirmed: {reason:?}"),
    }
}

impl<B: PlacementBackend, G: WriterGuardian, A: AgentLifecycle> SuspensionPorts
    for NodePorts<B, G, A>
{
    fn request_checkpoint(&mut self, intent: &SuspensionIntent) -> Result<CheckpointClaim> {
        self.agent.request_checkpoint(intent)
    }
    fn close_input(&mut self) -> Result<()> {
        self.agent.close_input()
    }
    fn stop_writers(&mut self) -> Result<StopObservation> {
        self.guardian.stop_writers()
    }
    fn reconcile_effects(&mut self, pending: &[Uuid]) -> Result<EffectsObservation> {
        self.agent.reconcile_effects(pending)
    }
    /// A disk-preserving backend stop; an already stopped sandbox confirms again.
    fn release_compute(&mut self, stop: &StopObservation) -> Result<()> {
        if !matches!(stop, StopObservation::ReportedStopped { .. }) {
            bail!("compute is released only after stopped-writer proof");
        }
        let result = self.runtime.block_on(self.backend.stop(&self.sandbox));
        confirmed("compute release", result).map(drop)
    }
    /// Destroy under the explicit AQ directive; destroying an already destroyed
    /// sandbox confirms again with the same proof.
    fn release_disk(&mut self, directive: &ArtifactRef) -> Result<()> {
        let authorization = DestroyAuthorization::operator_released(
            self.sandbox.clone(),
            release_reference(directive),
        )
        .map_err(|e| anyhow::anyhow!("disk release authorization: {e:?}"))?;
        let result = self
            .runtime
            .block_on(self.backend.destroy(&self.sandbox, &authorization));
        confirmed("protected disk release", result).map(drop)
    }
}
