//! Borrowed shared physical-host facts from one exact typed preparation profile.
use super::*;
pub(crate) struct PhaseScope<'a> {
    pub grant: &'a aq_session_protocol::ProfiledGrant,
    pub digest: Digest,
    pub record_revision: u64,
    pub observed_before_write_ns: u64,
    pub deadline_ns: u64,
    pub hard_deadline_ns: u64,
    pub kernel_boot_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
}
impl Attempt {
    pub(crate) fn phase_scope(&self) -> Result<PhaseScope<'_>> {
        match (&self.phase_execution, &self.refresh_execution) {
            (Some(e), None) => {
                let a = &self
                    .phase_preparation
                    .as_ref()
                    .context("Frozen Accepted absent")?
                    .delivery
                    .receipt
                    .lease;
                Ok(PhaseScope {
                    grant: &e.delivery.receipt.grant,
                    digest: e.digest()?,
                    record_revision: e.record_revision,
                    observed_before_write_ns: e.observed_before_write_ns,
                    deadline_ns: e.deadline_ns,
                    hard_deadline_ns: e.hard_deadline_ns,
                    kernel_boot_id: e.kernel_boot_id,
                    guardian_id: a.guardian_id,
                    launch_id: a.launch_id,
                    reservation_id: a.hold.reservation.reservation_id,
                })
            }
            (None, Some(e)) => {
                let a = &self
                    .refresh_preparation
                    .as_ref()
                    .context("Refresh Accepted absent")?
                    .delivery
                    .receipt
                    .lease;
                Ok(PhaseScope {
                    grant: &e.delivery.receipt.grant,
                    digest: e.digest()?,
                    record_revision: e.record_revision,
                    observed_before_write_ns: e.observed_before_write_ns,
                    deadline_ns: e.deadline_ns,
                    hard_deadline_ns: e.hard_deadline_ns,
                    kernel_boot_id: e.kernel_boot_id,
                    guardian_id: a.guardian_id,
                    launch_id: a.launch_id,
                    reservation_id: a.hold.reservation.reservation_id,
                })
            }
            _ => anyhow::bail!("missing or mixed exact phase execution"),
        }
    }
}
impl Journal {
    pub(crate) fn check_native_execution(
        &self,
        id: Uuid,
        digest: &Digest,
        deadline: u64,
        hard: u64,
    ) -> Result<()> {
        let a = self
            .state
            .attempts
            .get(&id)
            .context("phase attempt absent")?;
        match (a.phase_execution.is_some(), a.refresh_execution.is_some()) {
            (true, false) => self.check_phase_execution(id, digest, deadline, hard),
            (false, true) => self.check_refresh_execution(id, digest, deadline, hard),
            _ => anyhow::bail!("missing or mixed exact phase execution"),
        }
    }
}
