use super::*;
use aq_admission_sandbox::{
    sandbox::PermitScope,
    transport::{Cancellation, PollPage, TransportDocument},
};
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    /// Authenticated runner/provider observation, not caller-authored authority.
    /// Credentials are deliberately excluded from the controller snapshot.
    pub fn observe_grant(&mut self, permit: &sandbox::DelegatedPermit, now: u64) -> Result<()> {
        permit.validate()?;
        let id = permit.binding.attempt_id;
        let mut a = self.attempt(id)?;
        a.check_grant_observation(permit, self.identity(), now)?;
        ensure!(
            !self
                .state()
                .attempts
                .iter()
                .any(|(other_id, other)| *other_id != id
                    && (other.permit_id == Some(permit.permit_id)
                        || other.channel_id == Some(permit.binding.channel_id))),
            "grant identifiers reused"
        );
        if let Some(old) = &a.grant {
            ensure!(
                !(old.execution && permit.scope != PermitScope::Execution)
                    && permit.expires_at_ms >= old.renewal_deadline_ms,
                "grant observation regression"
            );
        }
        a.bind_permit(permit);
        self.save(id, a)
    }
    /// Consume cancellations from the authenticated node poll. A replay or newer
    /// revision can never postpone the first observed stop deadline.
    pub fn observe_poll(&mut self, page: &PollPage, now: u64) -> Result<()> {
        self.journal.check()?;
        TransportDocument::PollPage(page.clone()).validate()?;
        ensure!(
            page.identity == *self.identity(),
            "poll belongs to another node incarnation"
        );
        let mut state = self.state().clone();
        for cancellation in &page.cancellations {
            let id = cancellation.binding.attempt_id;
            let Some(a) = state.attempts.get_mut(&id) else {
                continue;
            };
            a.observe_cancellation(
                cancellation,
                page.identity.boot_id,
                now.saturating_add(self.config.recovery.cancellation_grace_ms),
            )?;
        }
        self.journal.persist(state)
    }
}
impl Attempt {
    pub(super) fn observation_closed(&self) -> bool {
        self.stopped.is_some() || self.cleanup.is_some() || self.cancellation.is_some()
    }
    fn check_grant_observation(
        &self,
        permit: &sandbox::DelegatedPermit,
        node: &WorkerIdentity,
        now: u64,
    ) -> Result<()> {
        ensure!(
            permit.binding.node == *node
                && permit.binding.grant_generation
                    == self.request.manifest.bindings.assignment_generation
                && Some(permit.binding.sandbox_id) == self.logical_sandbox_id,
            "grant observation mismatch or terminal attempt"
        );
        ensure!(
            permit.hard_deadline_ms == self.request.hard_deadline_ms
                && (permit.issued_at_ms..permit.expires_at_ms).contains(&now)
                && !self.observation_closed()
                && matches!(self.phase, Phase::Running | Phase::StartIntent),
            "grant observation mismatch or terminal attempt"
        );
        Ok(())
    }
    fn observe_cancellation(
        &mut self,
        cancellation: &Cancellation,
        boot: Uuid,
        stop_after_ms: u64,
    ) -> Result<()> {
        let bindings = &self.request.manifest.bindings;
        let mut expected_session = bindings.session.clone();
        if let SessionIdentity::Remote { boot_id, .. } = &mut expected_session {
            *boot_id = boot;
        }
        ensure!(
            cancellation.binding.session == expected_session
                && cancellation.binding.assignment_generation == bindings.assignment_generation
                && cancellation.binding.launch_manifest_digest
                    == aq_session_protocol::canonical_digest(&self.request.manifest)?,
            "cancellation binding mismatch"
        );
        if self.cancellation.is_none() {
            self.cancellation = Some(CancellationObservation {
                cancellation_id: cancellation.cancellation_id,
                authority_revision: cancellation.authority_revision,
                stop_after_ms,
            });
        }
        Ok(())
    }
}
