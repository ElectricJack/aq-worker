//! Private FORMAT12 parking bridge. History/claims cannot construct these owners.
//! ```compile_fail
//! let _: aq_worker::native::parking::DurableStoppedWorkspace = serde_json::from_str("{}").unwrap();
//! ```
use super::*;
use crate::continuation::history::{ParkingHistory, ParkingIntent, ParkingOperations};
use aq_lifecycle_protocol::{Checkpoint as AgentCheckpoint, CheckpointClaim, SuspensionIntent};
use aq_session_protocol::ArtifactRef;
mod roots;
mod source;
use roots::WorkspaceRoots;
pub(crate) struct StoppedSourceObservation {
    pub head: aq_worker_protocol::ObjectId,
    pub workspace_bytes: u64,
}

struct FenceCustody {
    pipeline: Arc<InputPipeline>,
    entry: Arc<Entry>,
    scope: Arc<LaunchExecutionScope>,
    roots: WorkspaceRoots,
    intent: ParkingIntent,
    intent_digest: Digest,
}
pub struct DurableParkingFence {
    custody: Arc<FenceCustody>,
}
struct StoppedCustody {
    fence: Arc<FenceCustody>,
    proof: StoppedProof,
    anchor_digest: Digest,
    recorded_revision: u64,
}
pub struct DurableStoppedWorkspace {
    custody: Arc<StoppedCustody>,
}
impl DurableParkingFence {
    pub fn attempt_id(&self) -> Uuid {
        self.custody.scope.id
    }
    pub fn intent_digest(&self) -> &Digest {
        &self.custody.intent_digest
    }
}
impl DurableStoppedWorkspace {
    pub fn attempt_id(&self) -> Uuid {
        self.custody.fence.scope.id
    }
    pub fn intent_digest(&self) -> &Digest {
        &self.custody.fence.intent_digest
    }
    pub fn stopped_record_revision(&self) -> u64 {
        self.custody.recorded_revision
    }
}
impl FenceCustody {
    fn association(&self, pipeline: &InputPipeline, owner: &Owned) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&pipeline.owned, &self.pipeline.owned)
                && Arc::ptr_eq(&pipeline.retained, &self.pipeline.retained)
                && Arc::ptr_eq(&pipeline.launch.entry(self.scope.id)?, &self.entry)
                && self.entry.stop.load(Ordering::SeqCst),
            "parking belongs to another owner or is not fenced"
        );
        let attempt = owner
            .journal
            .state()
            .attempts
            .get(&self.scope.id)
            .context("parking attempt absent")?;
        let history = attempt
            .parking
            .as_ref()
            .context("parking durable intent absent")?;
        ensure!(
            history.intent.digest()? == self.intent_digest
                && attempt.phase_scope()?.digest == self.scope.digest
                && attempt
                    .native_host
                    .as_ref()
                    .context("parking native host absent")?
                    .intent
                    .digest()?
                    == self.intent.host_intent_digest,
            "parking original execution/intent association changed"
        );
        self.roots.verify(&self.intent.pins)?;
        self.scope
            .inputs
            .workspace()
            .parking_association(&owner.journal)
    }
}
impl StoppedCustody {
    fn check(&self, pipeline: &InputPipeline, owner: &Owned) -> Result<ParkingHistory> {
        self.fence.association(pipeline, owner)?;
        let attempt = &owner.journal.state().attempts[&self.fence.scope.id];
        let history = attempt.parking.as_ref().unwrap();
        let anchor = history
            .stop
            .as_ref()
            .context("parking stop anchor absent")?;
        let stopped = attempt
            .native_host
            .as_ref()
            .and_then(|host| host.stopped.as_ref())
            .context("native stopped history absent")?;
        ensure!(
            crate::native::journal::canonical_digest(anchor)? == self.anchor_digest
                && anchor.recorded_revision == self.recorded_revision
                && stopped.proof_digest == crate::native::journal::canonical_digest(&self.proof)?
                && anchor.proof_digest == stopped.proof_digest
                && anchor.host_stopped_revision == stopped.recorded_revision
                && self.proof.observation().kernel_wait_echild
                && self.proof.observation().phase == aq_session_host::HostPhase::Stopped
                && self.proof.observation().reaped_count > 0,
            "parking opaque stopped proof or synced anchor changed"
        );
        Ok(history.clone())
    }
}
fn new_operations() -> ParkingOperations {
    ParkingOperations {
        terminal_fence: Uuid::new_v4(),
        checkpoint: Uuid::new_v4(),
        export: Uuid::new_v4(),
        transfer: Uuid::new_v4(),
        upload_begin: Uuid::new_v4(),
        upload_finalize: Uuid::new_v4(),
        report: Uuid::new_v4(),
    }
}

impl InputPipeline {
    /// Synthetic delivery only. Actual execution/input owners are still required.
    /// The one admitted writer owns both fsync and the synchronous registry fence.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn begin_synthetic_parking(
        self: &Arc<Self>,
        execution: &DurablePhaseExecution,
        suspension: SuspensionIntent,
        claim: CheckpointClaim,
        fake_hold: ArtifactRef,
    ) -> Result<DurableParkingFence> {
        use aq_lifecycle_protocol::Validate as _;
        suspension.validate()?;
        claim.validate()?;
        let scope = Arc::new(self.launch_scope(execution)?);
        let entry = self.launch.entry(scope.id)?;
        let retained = entry
            .scope
            .as_ref()
            .context("parking cannot use recovered history as execution")?;
        ensure!(
            retained.id == scope.id
                && retained.digest == scope.digest
                && retained.inputs.same_owner(&scope.inputs),
            "parking foreign retained execution inputs"
        );
        ensure!(
            matches!(entry.profile, Some(Profile::Synthetic(_))),
            "synthetic parking requires actual synthetic host profile"
        );
        let pipeline = self.clone();
        self.storage(move |owner| {
            let attempt = owner
                .journal
                .state()
                .attempts
                .get(&scope.id)
                .context("parking attempt absent")?;
            let offer = &attempt.reservation.offer.offered.offer;
            ensure!(
                suspension.binding == aq_admission::transport::lifecycle_binding(offer)?
                    && !offer.manifest.terminal.input_enabled
                    && attempt.phase_scope()?.digest == scope.digest,
                "synthetic parking requires exact execution and disabled terminal input"
            );
            let native = attempt
                .native_host
                .as_ref()
                .context("parking host intent absent")?;
            ensure!(
                native.running.is_some() && native.intent.execution_digest == scope.digest,
                "parking requires actual launched execution"
            );
            let workspace = scope.inputs.workspace();
            workspace.parking_association(&owner.journal)?;
            let git = workspace.retained_git();
            ensure!(
                attempt.reservation.disk_bytes >= git.native_reservation_bytes()?,
                "parking original disk reservation too small"
            );
            let roots = WorkspaceRoots::open(git.root(), &workspace.workspace().path)?;
            let checkpoint = AgentCheckpoint {
                binding: suspension.binding.clone(),
                continuation: suspension.continuation.clone(),
                claim,
            };
            let intent = if let Some(existing) = &attempt.parking {
                ensure!(
                    crate::native::journal::canonical_digest(&existing.intent.suspension)?
                        == crate::native::journal::canonical_digest(&suspension)?
                        && crate::native::journal::canonical_digest(&existing.intent.checkpoint)?
                            == crate::native::journal::canonical_digest(&checkpoint)?
                        && existing.intent.synthetic_hold == fake_hold
                        && existing.intent.execution_record_digest == scope.digest
                        && existing.intent.host_intent_digest == native.intent.digest()?,
                    "synthetic parking immutable request changed"
                );
                roots.verify(&existing.intent.pins)?;
                existing.intent.clone()
            } else {
                ParkingIntent {
                    suspension,
                    checkpoint,
                    synthetic_hold: fake_hold,
                    execution_record_digest: scope.digest.clone(),
                    host_intent_digest: native.intent.digest()?,
                    execution_spec_digest: offer.binding.execution_spec_digest.clone(),
                    source: workspace.manifest().source.clone(),
                    workspace: workspace.workspace().clone(),
                    managed_root: git.root().into(),
                    pins: roots.pins(),
                    reserved_disk_bytes: attempt.reservation.disk_bytes,
                    workspace_limit_bytes: git.workspace_byte_limit(),
                    artifact_limit_bytes: git.transfer_byte_limit(),
                    operations: new_operations(),
                    recorded_revision: owner
                        .journal
                        .state()
                        .revision
                        .checked_add(1)
                        .context("parking revision overflow")?,
                }
            };
            let history = match owner.journal.begin_parking(scope.id, intent) {
                Ok(history) => history,
                Err(error) => {
                    // A failed/ambiguous fsync yields no token or source action.
                    // Fencing remains a safety action even if no intent survived.
                    let _ = pipeline.launch.fence(scope.id);
                    return Err(error);
                }
            };
            // This effect stays inside the admitted storage job even if its
            // async waiter disappears after fsync. No launch permission is minted.
            let fenced = pipeline
                .launch
                .fence(scope.id)?
                .context("parking host disappeared")?;
            ensure!(
                Arc::ptr_eq(&fenced, &entry),
                "parking registry entry changed"
            );
            let custody = FenceCustody {
                pipeline: pipeline.clone(),
                entry,
                scope,
                roots,
                intent_digest: history.intent.digest()?,
                intent: history.intent,
            };
            custody.association(&pipeline, owner)?;
            Ok(DurableParkingFence {
                custody: Arc::new(custody),
            })
        })
        .await
    }
    pub async fn stop_parking(
        self: &Arc<Self>,
        fence: DurableParkingFence,
    ) -> Result<DurableStoppedWorkspace> {
        ensure!(
            Arc::ptr_eq(&self.owned, &fence.custody.pipeline.owned)
                && Arc::ptr_eq(&self.retained, &fence.custody.pipeline.retained),
            "parking stop foreign pipeline"
        );
        let id = fence.attempt_id();
        if !matches!(
            self.stop_phase_host(id).await?,
            NativeStopOutcome::StoppedPendingJournal(_)
        ) {
            let _ = self.persist_phase_host_uncertain(id).await;
            anyhow::bail!("parking physical stop uncertain; all resources retained");
        }
        let proof = lock(&fence.custody.entry.physical)?
            .stopped
            .clone()
            .context("parking actual stopped proof absent")?;
        let pipeline = self.clone();
        self.storage(move |owner| {
            fence.custody.association(&pipeline, owner)?;
            let history =
                owner
                    .journal
                    .record_parking_stopped(id, &fence.custody.intent_digest, &proof)?;
            fence.custody.entry.snapshot(&owner.journal)?;
            let anchor = history
                .stop
                .as_ref()
                .context("parking synced stopped anchor absent")?;
            let custody = StoppedCustody {
                fence: fence.custody,
                proof,
                anchor_digest: crate::native::journal::canonical_digest(anchor)?,
                recorded_revision: anchor.recorded_revision,
            };
            custody.check(&pipeline, owner)?;
            Ok(DurableStoppedWorkspace {
                custody: Arc::new(custody),
            })
        })
        .await
    }
    pub(crate) async fn parking_history(
        self: &Arc<Self>,
        stopped: &DurableStoppedWorkspace,
    ) -> Result<ParkingHistory> {
        let custody = stopped.custody.clone();
        let pipeline = self.clone();
        self.storage(move |owner| custody.check(&pipeline, owner))
            .await
    }
    pub(crate) async fn parking_commit(
        self: &Arc<Self>,
        stopped: &DurableStoppedWorkspace,
        expected: Digest,
        proposed: ParkingHistory,
    ) -> Result<ParkingHistory> {
        let custody = stopped.custody.clone();
        let pipeline = self.clone();
        self.storage(move |owner| {
            custody.check(&pipeline, owner)?;
            let history = owner.journal.parking_progress(
                custody.fence.scope.id,
                &custody.proof,
                &expected,
                proposed,
            )?;
            custody.check(&pipeline, owner)?;
            Ok(history)
        })
        .await
    }
}
