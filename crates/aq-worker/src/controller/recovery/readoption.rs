use super::*;
use aq_admission_sandbox::sandbox::{
    DelegatedPermit, LiveSandbox, PermitScope, ReadoptionRequest, ReadoptionResponse,
    ReadoptionResult,
};
use std::collections::{BTreeMap, BTreeSet};
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub(super) async fn readopt_at(&mut self, now: u64, seen: &BTreeSet<Uuid>) -> Result<()> {
        let mut requests = Vec::new();
        for (boot, live) in self.live_previous_grants(now, seen) {
            requests.extend(
                live.chunks(128)
                    .map(|chunk| self.readoption_request(boot, chunk)),
            );
        }
        let mut state = self.state().clone();
        state.readoptions = requests.clone();
        self.journal.persist(state)?;
        for request in requests {
            self.readopt_request(&request, now).await?;
        }
        Ok(())
    }
    fn live_previous_grants(
        &self,
        now: u64,
        seen: &BTreeSet<Uuid>,
    ) -> BTreeMap<Uuid, Vec<LiveSandbox>> {
        let mut groups = BTreeMap::<_, Vec<_>>::new();
        for (id, a) in self
            .state()
            .attempts
            .iter()
            .filter(|(id, _)| seen.contains(id))
        {
            if let Some(g) = a.previous_execution(self.identity().boot_id, now) {
                groups.entry(g.boot_id).or_default().push(LiveSandbox {
                    permit_id: g.permit_id,
                    attempt_id: *id,
                    grant_generation: g.generation,
                    sandbox_id: g.sandbox_id,
                });
            }
        }
        groups
    }
    fn readoption_request(&self, boot: Uuid, chunk: &[LiveSandbox]) -> ReadoptionRequest {
        let prior = self
            .state()
            .readoptions
            .iter()
            .find(|r| r.previous.boot_id == boot && r.sandboxes == chunk);
        prior.cloned().unwrap_or_else(|| ReadoptionRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            previous: WorkerIdentity {
                node_id: self.identity().node_id,
                boot_id: boot,
            },
            current: self.identity().clone(),
            sandboxes: chunk.to_vec(),
        })
    }
    async fn readopt_request(&mut self, req: &ReadoptionRequest, now: u64) -> Result<()> {
        let timeout = self.sweep_interval();
        let Ok(Ok(response)) = tokio::time::timeout(timeout, self.provider.readopt(req)).await
        else {
            tracing::warn!(operation=%req.operation_id,"re-adoption unavailable; runner deadline unchanged");
            return Ok(());
        };
        self.validate_readoption(req, &response, now)?;
        for result in response.results {
            match result {
                ReadoptionResult::Rebound { permit } => self.observe_grant(&permit, now)?,
                ReadoptionResult::Refused { sandbox_id, code } => {
                    tracing::warn!(%sandbox_id,?code,"re-adoption refused; runner must stop by renewal deadline");
                    if matches!(
                        code,
                        sandbox::ReadoptionRefusal::NoCurrentGrant
                            | sandbox::ReadoptionRefusal::UnknownSandbox
                            | sandbox::ReadoptionRefusal::Revoked
                    ) {
                        let id = listed_sandbox(req, sandbox_id)?.attempt_id;
                        self.mark_cleanup(id, CleanupReason::NoCurrentGrant, now)?;
                        self.stop(id).await?;
                    }
                }
            }
        }
        Ok(())
    }
    /// Validate the whole response before applying the first rebound or refusal.
    fn validate_readoption(
        &self,
        req: &ReadoptionRequest,
        reply: &ReadoptionResponse,
        now: u64,
    ) -> Result<()> {
        reply.validate()?;
        ensure!(
            reply.operation_id == req.operation_id
                && reply.previous == req.previous
                && reply.current == req.current
                && reply.results.len() == req.sandboxes.len(),
            "re-adoption response mismatch"
        );
        for result in &reply.results {
            match result {
                ReadoptionResult::Rebound { permit } => {
                    let listed = listed_sandbox(req, permit.binding.sandbox_id)?;
                    self.attempt(listed.attempt_id)?
                        .validate_rebound(permit, listed, now)?;
                }
                ReadoptionResult::Refused { sandbox_id, .. } => {
                    listed_sandbox(req, *sandbox_id)?;
                }
            }
        }
        Ok(())
    }
}
fn listed_sandbox(request: &ReadoptionRequest, id: Uuid) -> Result<&LiveSandbox> {
    request
        .sandboxes
        .iter()
        .find(|s| s.sandbox_id == id)
        .ok_or_else(|| anyhow::anyhow!("unrequested sandbox rebound"))
}
impl Attempt {
    fn previous_execution(&self, boot: Uuid, now: u64) -> Option<&GrantMonitor> {
        if self.observation_closed() || now >= self.request.hard_deadline_ms {
            return None;
        }
        self.grant
            .as_ref()
            .filter(|g| g.execution && g.boot_id != boot && now < g.renewal_deadline_ms)
    }
    fn validate_rebound(
        &self,
        permit: &DelegatedPermit,
        listed: &LiveSandbox,
        now: u64,
    ) -> Result<()> {
        let grant = self.grant.as_ref().unwrap();
        ensure!(
            permit.binding.attempt_id == listed.attempt_id
                && permit.binding.grant_generation == listed.grant_generation
                && permit.permit_id == listed.permit_id
                && permit.binding.channel_id == grant.channel_id,
            "re-adoption changed grant or deadline"
        );
        ensure!(
            permit.scope == PermitScope::Execution
                && permit.expires_at_ms == grant.renewal_deadline_ms
                && permit.hard_deadline_ms == self.request.hard_deadline_ms
                && (permit.issued_at_ms..permit.expires_at_ms).contains(&now),
            "re-adoption changed grant or deadline"
        );
        Ok(())
    }
}
