//! `SuspensionPorts` over the local node: writer stop through the subreaper
//! guardian (`crate::guardian`) and compute/disk release through the placement
//! backend. Every effect is idempotent under replay, because the suspender
//! repeats a port effect whose journal write was lost to a crash.
//!
//! The agent-side effects (`request_checkpoint`, `close_input`,
//! `reconcile_effects`) go through the `AgentLifecycle` port. Their mediation
//! is defined in `suspend.rs` (eager-horizon-82); the production adapter over
//! the agent artifacts and lifecycle effects routes is a follow-up
//! (docs/implementation/H5.md, "Port-level TODOs").
use anyhow::{Result, bail};
use aq_lifecycle_protocol::{
    ArtifactRef, Digest, EffectsObservation, RetentionReceipt, StopObservation, StopProof,
};
use aq_worker_protocol::ProcessOutcome;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::{CheckpointRequest, InputFence, SuspensionPorts, WriterStop};
use crate::backend::{Confirmation, DestroyAuthorization, PlacementBackend, SandboxIdentity};
use crate::guardian::{self, GuardianIdentity, GuardianPhase, GuardianState, Operation};

/// Agent-side suspension surface; the contract is `SuspensionPorts`'
/// (see `suspend.rs`). TODO(platform follow-up): production adapter.
pub trait AgentLifecycle {
    /// Ask the agent to retain its checkpoint summary under `request`; the
    /// receipt it observed, if any. The Suspender verifies it.
    fn request_checkpoint(
        &mut self,
        request: &CheckpointRequest,
    ) -> Result<Option<RetentionReceipt>>;
    /// Close the worker-local input gate. Idempotent.
    fn close_input(&mut self) -> Result<InputFence>;
    /// Hand pending domain effects, under `fence`, to central reconciliation.
    fn reconcile_effects(
        &mut self,
        fence: &InputFence,
        pending: &[Uuid],
    ) -> Result<EffectsObservation>;
}

/// Stop every owned writer and report stopped-writer evidence.
pub trait WriterGuardian {
    fn stop_writers(&mut self) -> Result<WriterStop>;
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
    fn state(&self, identity: &GuardianIdentity) -> Result<GuardianState> {
        guardian::read_state(&self.directory, identity)
    }
}
impl WriterGuardian for LocalGuardian {
    /// A guardian already `Stopped` is not contacted again, so a replay after a
    /// lost journal write yields the same observation. `StopUncertain` is
    /// reported as `Unknown` (the suspender then holds). A guardian that has
    /// not reached a terminal phase within the bound is an error, which the
    /// suspender retries without journaling a stop.
    fn stop_writers(&mut self) -> Result<WriterStop> {
        let identity = guardian::read_identity(&self.directory)?;
        let state = self.state(&identity)?;
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
                let state = self.state(&identity)?;
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
            let state = self.state(&identity)?;
            match state.phase {
                GuardianPhase::Stopped => return stopped_writers(&identity, &state),
                GuardianPhase::StopUncertain => {
                    return Ok(WriterStop::Unknown {
                        reason: "guardian lost; stopped writers unproven".into(),
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

/// Map an exact guardian `Stopped` receipt (kernel ECHILD proof) to the
/// stop proof. Deterministic in the receipt, so replays are identical:
/// - `guardian_instance_id`: the guardian launch id;
/// - `writer_set_digest`: digest of the guardian identity JSON;
/// - `stopped_at_boot_ms` / `observed_at_ms`: the receipt's `stopped_at_ms`;
/// - `writer_count`: processes the guardian reaped.
///
/// The Suspender builds `ReportedStopped` and retains the proof bytes.
pub fn stopped_writers(identity: &GuardianIdentity, state: &GuardianState) -> Result<WriterStop> {
    let (true, Some(stopped_at_ms)) = (
        state.phase == GuardianPhase::Stopped && state.kernel_wait_echild,
        state.stopped_at_ms,
    ) else {
        bail!("guardian receipt carries no kernel stopped proof");
    };
    let writer_count = match u32::try_from(state.reaped_count) {
        Ok(count) if count <= 65536 => count,
        _ => {
            return Ok(WriterStop::Unknown {
                reason: "guardian writer count out of range".into(),
            });
        }
    };
    Ok(WriterStop::Stopped {
        proof: StopProof {
            guardian_instance_id: identity.launch_id,
            writer_set_digest: Digest::of(&serde_json::to_vec(identity)?),
            stopped_at_boot_ms: stopped_at_ms,
        },
        writer_count,
        observed_at_ms: stopped_at_ms,
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
    fn request_checkpoint(
        &mut self,
        request: &CheckpointRequest,
    ) -> Result<Option<RetentionReceipt>> {
        self.agent.request_checkpoint(request)
    }
    fn close_input(&mut self) -> Result<InputFence> {
        self.agent.close_input()
    }
    fn stop_writers(&mut self) -> Result<WriterStop> {
        self.guardian.stop_writers()
    }
    fn reconcile_effects(
        &mut self,
        fence: &InputFence,
        pending: &[Uuid],
    ) -> Result<EffectsObservation> {
        self.agent.reconcile_effects(fence, pending)
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
