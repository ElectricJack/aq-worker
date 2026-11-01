//! Fsync-gated observation over the original retained phase physical owners.
use super::*;
use crate::native::http::preparation_v2::ExecutionObservation;
use aq_session_protocol::Digest;
pub struct RetainedPhaseExecutionHistory {
    attempt: Uuid,
    record_digest: Digest,
    observed_after_sync_ns: u64,
    owner: Arc<Mutex<Owned>>,
    resources: Arc<Mutex<granted::RetainedOwners>>,
    inputs: Arc<PreparedPhaseInputs>,
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
    /// Keep actual Prepared owners synchronously before the first HTTP poll.
    /// A lost Grant response leaves the original inputs held as history rather
    /// than dropping their workspace/material ownership or retrying admission.
    pub fn request_phase_execution<'a>(
        &'a self,
        inputs: PreparedPhaseInputs,
        preparation: &'a super::PhasePreparation,
    ) -> impl std::future::Future<Output = Result<PhaseExecutionCommit>> + Send + 'a {
        let retained = self
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("retained phase resources poisoned"))
            .and_then(|mut owners| owners.retain_phase(inputs));
        async move {
            let inputs = retained?;
            let observed = self
                .client
                .prepare_execution(
                    &preparation.proposed,
                    &preparation.request,
                    &preparation.observed,
                    inputs.request(),
                    None,
                )
                .await?;
            self.commit_phase_execution(inputs, Arc::new(observed))
                .await
        }
    }

    /// Synchronous ownership handoff precedes the first poll or any admission wait.
    /// Pass an Arc clone to retain the actual opaque network observation for
    /// exact wire replay after Accepted expiry. JSON history cannot replace it.
    pub fn record_phase_execution(
        &self,
        inputs: PreparedPhaseInputs,
        observed: impl Into<Arc<ExecutionObservation>>,
    ) -> impl std::future::Future<Output = Result<PhaseExecutionCommit>> + Send + '_ {
        let observed = observed.into();
        let retained = self
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("retained phase resources poisoned"))
            .and_then(|mut map| map.retain_phase(inputs));
        async move { self.commit_phase_execution(retained?, observed).await }
    }
    pub fn retry_phase_execution(
        &self,
        id: Uuid,
        observed: impl Into<Arc<ExecutionObservation>>,
    ) -> impl std::future::Future<Output = Result<PhaseExecutionCommit>> + Send + '_ {
        let observed = observed.into();
        let retained = self
            .retained
            .lock()
            .map_err(|_| anyhow::anyhow!("retained phase resources poisoned"))
            .and_then(|map| map.phase(id));
        async move { self.commit_phase_execution(retained?, observed).await }
    }
    async fn commit_phase_execution(
        &self,
        inputs: Arc<PreparedPhaseInputs>,
        observed: Arc<ExecutionObservation>,
    ) -> Result<PhaseExecutionCommit> {
        let owned = self.owned.clone();
        let resources = self.retained.clone();
        self.storage(move |owner| {
            let id = inputs.request().original.binding.attempt_id;
            let record = owner
                .journal
                .record_phase_execution(id, inputs.request(), &observed)?;
            let deadline = record.deadline_ns.min(observed.deadline_ns());
            let hard = record.hard_deadline_ns.min(observed.hard_deadline_ns());
            let history = RetainedPhaseExecutionHistory {
                attempt: id,
                record_digest: record.digest()?,
                observed_after_sync_ns: boot_ns()?,
                owner: owned,
                resources,
                inputs,
            };
            if owner
                .journal
                .check_phase_execution(id, &history.record_digest, deadline, hard)
                .is_err()
                || history
                    .inputs
                    .verify_execution_inputs(&owner.journal, &owner.materials, deadline.min(hard))
                    .is_err()
                || owner
                    .journal
                    .check_phase_execution(id, &history.record_digest, deadline, hard)
                    .is_err()
            {
                return Ok(PhaseExecutionCommit::HistoryOnly(history));
            }
            Ok(PhaseExecutionCommit::Usable(DurablePhaseExecution {
                history,
                deadline_ns: deadline,
                hard_deadline_ns: hard,
            }))
        })
        .await
    }
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
            owner
                .journal
                .check_phase_execution(id, &digest, deadline, hard)?;
            inputs.verify_execution_inputs(&owner.journal, &owner.materials, deadline.min(hard))?;
            owner
                .journal
                .check_phase_execution(id, &digest, deadline, hard)
        })
        .await
    }
}

pub(super) struct LaunchExecutionScope {
    pub(super) id: Uuid,
    pub(super) digest: Digest,
    pub(super) deadline: u64,
    pub(super) hard: u64,
    pub(super) inputs: Arc<PreparedPhaseInputs>,
}
impl LaunchExecutionScope {
    pub(super) fn check(&self, owner: &Owned) -> Result<()> {
        owner
            .journal
            .check_phase_execution(self.id, &self.digest, self.deadline, self.hard)?;
        self.inputs.verify_execution_inputs(
            &owner.journal,
            &owner.materials,
            self.deadline.min(self.hard),
        )?;
        owner
            .journal
            .check_phase_execution(self.id, &self.digest, self.deadline, self.hard)
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
