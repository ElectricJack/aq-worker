//! Exact received execution history. Disk never reconstructs current authority.
use super::*;
use crate::native::http::preparation_v2::ExecutionObservation;
use aq_admission::preparation_v2::{ExecutionDelivery, PrepareRequest};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivedPhaseExecution {
    pub delivery: ExecutionDelivery,
    pub request_digest: Digest,
    pub accepted_digest: Digest,
    pub runtime: RuntimeIdentity,
    pub kernel_boot_id: Uuid,
    pub clock_source: String,
    pub sent_ns: u64,
    pub received_ns: u64,
    pub first_send_deadline_ns: u64,
    pub deadline_ns: u64,
    pub hard_deadline_ns: u64,
    /// Pre-serialization sample, never a durable post-fsync timestamp.
    pub observed_before_write_ns: u64,
    pub record_revision: u64,
}
impl ReceivedPhaseExecution {
    pub fn digest(&self) -> Result<Digest> {
        canonical_digest(self)
    }
}
pub(super) fn validate(a: &Attempt, kernel: Uuid, revision: u64) -> Result<()> {
    let Some(r) = &a.phase_execution else {
        return Ok(());
    };
    let p = a
        .phase_prepared
        .as_ref()
        .context("execution without phase Prepare")?;
    let accepted = a
        .phase_preparation
        .as_ref()
        .context("execution without accepted history")?;
    let intent = a
        .phase_acceptance
        .as_ref()
        .context("execution without phase intent")?;
    let sample = r.delivery.sample(
        &p.request,
        &accepted.delivery.receipt,
        &intent.request,
        &intent.proposal,
        r.delivery.delivery.nonce,
        r.sent_ns,
        r.received_ns,
        None,
    )?;
    ensure!(
        r.request_digest == p.request.digest()?
            && r.accepted_digest == accepted.receipt_digest
            && same(&r.runtime, &a.reservation.runtime)?
            && r.kernel_boot_id == kernel
            && r.clock_source == "linux_clock_boottime"
            && r.record_revision
                >= p.request
                    .original
                    .journal_revision
                    .checked_add(2)
                    .context("phase execution revision overflow")?
            && r.record_revision <= revision,
        "phase execution provenance"
    );
    ensure!(
        r.sent_ns >= p.prepared_at_ns
            && r.sent_ns < r.first_send_deadline_ns
            && r.first_send_deadline_ns <= accepted.deadline_ns
            && r.received_ns >= r.sent_ns
            && r.received_ns < r.deadline_ns
            && r.deadline_ns <= sample.deadline_ns()
            && r.deadline_ns <= r.hard_deadline_ns
            && r.hard_deadline_ns <= sample.hard_deadline_ns()
            && r.observed_before_write_ns >= r.received_ns,
        "phase execution historical clocks"
    );
    Ok(())
}
impl Journal {
    pub(crate) fn record_phase_execution(
        &mut self,
        id: Uuid,
        request: &PrepareRequest,
        observed: &ExecutionObservation,
    ) -> Result<ReceivedPhaseExecution> {
        let now = self.live()?;
        let a = self
            .state
            .attempts
            .get(&id)
            .context("phase execution attempt absent")?;
        let prepared = a.phase_prepared.as_ref().context("phase Prepare absent")?;
        let accepted = a
            .phase_preparation
            .as_ref()
            .context("accepted history absent")?;
        let intent = a.phase_acceptance.as_ref().context("phase intent absent")?;
        ensure!(
            same(request, &prepared.request)?
                && observed.request_digest() == &request.digest()?
                && observed.accepted_digest() == &accepted.receipt_digest,
            "phase execution differs from actual Prepared owner"
        );
        observed.delivery().validate_for(
            request,
            &accepted.delivery.receipt,
            &intent.request,
            &intent.proposal,
            observed.delivery().delivery.nonce,
        )?;
        if let Some(first) = &a.phase_execution {
            ensure!(
                first.delivery.receipt.digest()? == observed.delivery().receipt.digest()?,
                "phase execution replay changed"
            );
            return Ok(first.clone());
        }
        let next_revision = self
            .state
            .revision
            .checked_add(1)
            .context("phase execution revision overflow")?;
        let (delivery, sent_ns, received_ns, deadline_ns, hard_deadline_ns) =
            observed.first_transmission();
        let record = ReceivedPhaseExecution {
            delivery: delivery.clone(),
            request_digest: observed.request_digest().clone(),
            accepted_digest: observed.accepted_digest().clone(),
            runtime: a.reservation.runtime.clone(),
            kernel_boot_id: self.kernel_boot_id,
            clock_source: "linux_clock_boottime".into(),
            sent_ns,
            received_ns,
            first_send_deadline_ns: observed.first_send_deadline_ns(),
            deadline_ns,
            hard_deadline_ns,
            observed_before_write_ns: now,
            record_revision: next_revision,
        };
        let mut next = self.next()?;
        next.attempts.get_mut(&id).unwrap().phase_execution = Some(record.clone());
        self.persist(next)?;
        Ok(record)
    }
    pub(crate) fn check_phase_execution(
        &self,
        id: Uuid,
        digest: &Digest,
        deadline: u64,
        hard: u64,
    ) -> Result<()> {
        let now = self.live()?;
        let r = self
            .state
            .attempts
            .get(&id)
            .and_then(|a| a.phase_execution.as_ref())
            .context("phase execution history absent")?;
        ensure!(
            r.digest()? == *digest
                && r.kernel_boot_id == self.kernel_boot_id
                && deadline <= r.deadline_ns
                && hard <= r.hard_deadline_ns
                && deadline <= hard
                && now < deadline
                && now < hard,
            "phase execution expired or changed"
        );
        Ok(())
    }
}
