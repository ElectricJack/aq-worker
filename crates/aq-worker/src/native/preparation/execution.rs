//! Fsync-gated observation over the original retained phase physical owners.
use super::*;
use aq_session_protocol::Digest;
#[derive(Clone)]
pub(super) enum RetainedPhaseInputs {
    Frozen(Arc<PreparedPhaseInputs>),
    Refresh(Arc<PreparedRefreshInputs>),
}
impl RetainedPhaseInputs {
    pub(super) fn artifacts(&self) -> &[super::super::VerifiedArtifact] {
        match self {
            Self::Frozen(i) => i.artifacts(),
            Self::Refresh(i) => i.artifacts(),
        }
    }
    pub(super) fn same_owner(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Frozen(a), Self::Frozen(b)) => Arc::ptr_eq(a, b),
            (Self::Refresh(a), Self::Refresh(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    pub(super) fn startup(&self) -> &aq_session_host::RenderedStartup {
        match self {
            Self::Frozen(i) => i.startup(),
            Self::Refresh(i) => i.startup(),
        }
    }
    pub(super) fn workspace(&self) -> &HydratedWorkspace {
        match self {
            Self::Frozen(i) => i.workspace(),
            Self::Refresh(i) => i.workspace(),
        }
    }
    pub(super) fn materials(&self) -> &[InstalledMaterial] {
        match self {
            Self::Frozen(i) => i.materials(),
            Self::Refresh(i) => i.materials(),
        }
    }
    fn verify_execution_inputs(&self, j: &Journal, s: &MaterialStore, d: u64) -> Result<()> {
        match self {
            Self::Frozen(i) => i.verify_execution_inputs(j, s, d),
            Self::Refresh(i) => i.verify_execution_inputs(j, s, d),
        }
    }
    fn check(
        &self,
        j: &Journal,
        id: Uuid,
        digest: &Digest,
        deadline: u64,
        hard: u64,
    ) -> Result<()> {
        match self {
            Self::Frozen(_) => j.check_phase_execution(id, digest, deadline, hard),
            Self::Refresh(_) => j.check_refresh_execution(id, digest, deadline, hard),
        }
    }
}

pub struct RetainedPhaseExecutionHistory {
    attempt: Uuid,
    record_digest: Digest,
    observed_after_sync_ns: u64,
    owner: Arc<Mutex<Owned>>,
    resources: Arc<Mutex<granted::RetainedOwners>>,
    inputs: RetainedPhaseInputs,
}
impl RetainedPhaseExecutionHistory {
    pub fn attempt_id(&self) -> Uuid {
        self.attempt
    }
    pub fn record_digest(&self) -> &Digest {
        &self.record_digest
    }
    pub fn observed_after_sync_ns(&self) -> u64 {
        self.observed_after_sync_ns
    }
}
/// No host or renewal interface. Future effects require their own durable intent.
pub struct DurablePhaseExecution {
    history: RetainedPhaseExecutionHistory,
    deadline_ns: u64,
    hard_deadline_ns: u64,
}
impl DurablePhaseExecution {
    pub fn history(&self) -> &RetainedPhaseExecutionHistory {
        &self.history
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn hard_deadline_ns(&self) -> u64 {
        self.hard_deadline_ns
    }
}
pub enum PhaseExecutionCommit {
    Usable(DurablePhaseExecution),
    HistoryOnly(RetainedPhaseExecutionHistory),
}
impl InputPipeline {
    pub async fn check_phase_execution(&self, grant: &DurablePhaseExecution) -> Result<()> {
        ensure!(
            Arc::ptr_eq(&self.owned, &grant.history.owner)
                && Arc::ptr_eq(&self.retained, &grant.history.resources),
            "phase execution belongs to another owner"
        );
        let id = grant.history.attempt;
        let digest = grant.history.record_digest.clone();
        let deadline = grant.deadline_ns;
        let hard = grant.hard_deadline_ns;
        let inputs = grant.history.inputs.clone();
        self.storage(move |owner| {
            inputs.check(&owner.journal, id, &digest, deadline, hard)?;
            inputs.verify_execution_inputs(&owner.journal, &owner.materials, deadline.min(hard))?;
            inputs.check(&owner.journal, id, &digest, deadline, hard)
        })
        .await
    }
}

pub(super) struct LaunchExecutionScope {
    pub(super) id: Uuid,
    pub(super) digest: Digest,
    pub(super) deadline: u64,
    pub(super) hard: u64,
    pub(super) inputs: RetainedPhaseInputs,
}
impl LaunchExecutionScope {
    pub(super) fn check(&self, owner: &Owned) -> Result<()> {
        self.inputs.check(
            &owner.journal,
            self.id,
            &self.digest,
            self.deadline,
            self.hard,
        )?;
        self.inputs.verify_execution_inputs(
            &owner.journal,
            &owner.materials,
            self.deadline.min(self.hard),
        )?;
        self.inputs.check(
            &owner.journal,
            self.id,
            &self.digest,
            self.deadline,
            self.hard,
        )
    }
}
impl InputPipeline {
    pub(super) fn launch_scope(
        &self,
        grant: &DurablePhaseExecution,
    ) -> Result<LaunchExecutionScope> {
        ensure!(
            Arc::ptr_eq(&self.owned, &grant.history.owner)
                && Arc::ptr_eq(&self.retained, &grant.history.resources),
            "phase launch foreign execution owner"
        );
        Ok(LaunchExecutionScope {
            id: grant.history.attempt,
            digest: grant.history.record_digest.clone(),
            deadline: grant.deadline_ns,
            hard: grant.hard_deadline_ns,
            inputs: grant.history.inputs.clone(),
        })
    }
}

mod frozen_commit {
    use super::*;
    crate::native::phase_profile_shared::execution_commit! { PreparedPhaseInputs, super::super::PhasePreparation, crate::native::http::preparation_v2::ExecutionObservation, Frozen, request_phase_execution, prepare_execution, record_phase_execution, retry_phase_execution, commit_phase_execution, retain_phase, phase, record_phase_execution, check_phase_execution }
}
mod refresh_commit {
    use super::*;
    crate::native::phase_profile_shared::execution_commit! { PreparedRefreshInputs, super::super::RefreshPreparation, crate::native::http::preparation_refresh_v1::ExecutionObservation, Refresh, request_refresh_execution, prepare_refresh_execution, record_refresh_execution, retry_refresh_execution, commit_refresh_execution, retain_refresh, refresh, record_refresh_execution, check_refresh_execution }
}
