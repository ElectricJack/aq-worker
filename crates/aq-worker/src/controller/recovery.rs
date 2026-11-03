//! Recovery never grants execution or repeats create/start. All cleanup effects
//! follow durable reason/identity records; uncertain proof keeps reservations.
use super::*;
use aq_admission_sandbox::{
    sandbox::{LiveSandbox, PermitScope, ReadoptionRequest, ReadoptionResult},
    transport::{PollPage, TransportDocument},
};
use std::collections::{BTreeMap, BTreeSet};

impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub(super) fn has_orphans(&self) -> bool {
        self.state().orphans.values().any(|o| !o.destroyed)
    }
    fn cleanup(&self, reason: CleanupReason, now: u64) -> Cleanup {
        Cleanup {
            reason,
            destroy_after_ms: now.saturating_add(self.config.recovery.orphan_grace_ms),
        }
    }
    fn mark_cleanup(&mut self, id: Uuid, reason: CleanupReason, now: u64) -> Result<()> {
        let mut a = self.attempt(id)?;
        if a.cleanup.is_none() {
            a.cleanup = Some(self.cleanup(reason, now));
            self.save(id, a)?;
            tracing::warn!(attempt=%id, ?reason, "sandbox cleanup scheduled; retention required");
        }
        Ok(())
    }
    /// Authenticated runner/provider observation, not caller-authored authority.
    /// Credentials are deliberately excluded from the controller snapshot.
    pub fn observe_grant(&mut self, permit: &sandbox::DelegatedPermit, now: u64) -> Result<()> {
        permit.validate()?;
        let id = permit.binding.attempt_id;
        let mut a = self.attempt(id)?;
        ensure!(
            permit.binding.node == *self.identity()
                && permit.binding.grant_generation
                    == a.request.manifest.bindings.assignment_generation
                && Some(permit.binding.sandbox_id) == a.logical_sandbox_id
                && permit.hard_deadline_ms == a.request.hard_deadline_ms
                && permit.issued_at_ms <= now
                && now < permit.expires_at_ms
                && a.stopped.is_none()
                && a.cleanup.is_none()
                && a.cancellation.is_none()
                && matches!(a.phase, Phase::Running | Phase::StartIntent),
            "grant observation mismatch or terminal attempt"
        );
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
        a.permit_id = Some(permit.permit_id);
        a.channel_id = Some(permit.binding.channel_id);
        a.grant = Some(GrantMonitor {
            boot_id: permit.binding.node.boot_id,
            permit_id: permit.permit_id,
            sandbox_id: permit.binding.sandbox_id,
            channel_id: permit.binding.channel_id,
            generation: permit.binding.grant_generation,
            renewal_deadline_ms: permit.expires_at_ms,
            execution: permit.scope == PermitScope::Execution,
        });
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
            let bindings = &a.request.manifest.bindings;
            let mut expected_session = bindings.session.clone();
            if let SessionIdentity::Remote { boot_id, .. } = &mut expected_session {
                *boot_id = page.identity.boot_id;
            }
            ensure!(
                cancellation.binding.session == expected_session
                    && cancellation.binding.assignment_generation == bindings.assignment_generation
                    && cancellation.binding.launch_manifest_digest
                        == aq_session_protocol::canonical_digest(&a.request.manifest)?,
                "cancellation binding mismatch"
            );
            if a.cancellation.is_none() {
                a.cancellation = Some(CancellationObservation {
                    cancellation_id: cancellation.cancellation_id,
                    authority_revision: cancellation.authority_revision,
                    stop_after_ms: now.saturating_add(self.config.recovery.cancellation_grace_ms),
                });
            }
        }
        self.journal.persist(state)
    }
    fn matches_entry(&self, a: &Attempt, entry: &InventoryEntry) -> bool {
        entry.sandbox.owner == a.create.owner
            && entry.sandbox.kind == self.backend.kind()
            && a.sandbox.as_ref().is_none_or(|s| *s == entry.sandbox)
            && entry.name == a.create.name
            && entry.template_digest == a.create.template_digest
            && entry.hard_deadline_ms == a.create.hard_deadline_ms
    }
    async fn orphan(&mut self, entry: InventoryEntry, now: u64) -> Result<()> {
        let key = entry.sandbox.backend_id.clone();
        let mut state = self.state().clone();
        if let Some(old) = state.orphans.get(&key) {
            ensure!(
                old.entry.sandbox == entry.sandbox && !old.destroyed,
                "orphan identity reused or destroyed sandbox revived"
            );
            if old.stopped.is_some() && !entry.writer_proof().is_confirmed() {
                self.journal.poisoned = true;
                anyhow::bail!("inventory contradicts orphan stopped proof");
            }
        } else {
            ensure!(
                state.orphans.len() < journal::MAX_ATTEMPTS,
                "orphan journal bound reached"
            );
            state.orphans.insert(
                key.clone(),
                Orphan {
                    entry: entry.clone(),
                    cleanup: self.cleanup(CleanupReason::UnknownSandbox, now),
                    stopped: None,
                    authorization: None,
                    destroyed: false,
                },
            );
            self.journal.persist(state)?;
            tracing::warn!(sandbox=%key, "unknown owned sandbox; stopping and preserving");
        }
        self.stop_orphan(&key).await
    }
    async fn stop_orphan(&mut self, key: &str) -> Result<()> {
        self.journal.check()?;
        let o = self.state().orphans[key].clone();
        if o.stopped.is_some() {
            return Ok(());
        }
        if let Confirmation::Confirmed(proof) = self.backend.stop(&o.entry.sandbox).await {
            ensure!(
                *proof.sandbox() == o.entry.sandbox
                    && proof.kind() == WriterProofKind::StoppedNoProcesses,
                "orphan stop did not confirm preserved disk"
            );
            let mut state = self.state().clone();
            state.orphans.get_mut(key).unwrap().stopped = Some(proof);
            self.journal.persist(state)?;
        }
        Ok(())
    }
    /// Complete owned inventory is required; absence alone never releases writers.
    pub async fn reconcile_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        self.recovery_required = true;
        let entries = self.backend.list_owned(self.identity().node_id).await?;
        let mut seen = BTreeSet::new();
        let mut unresolved = false;
        for entry in entries {
            if entry.sandbox.owner.node_id != self.identity().node_id {
                continue;
            }
            ensure!(
                entry.sandbox.kind == self.backend.kind() && !entry.sandbox.backend_id.is_empty(),
                "invalid backend inventory identity"
            );
            let id = entry.sandbox.owner.attempt_id;
            if let Some(mut a) = self.state().attempts.get(&id).cloned() {
                if !self.matches_entry(&a, &entry) {
                    unresolved = true;
                    if entry.sandbox.owner != a.create.owner
                        || a.sandbox.as_ref().is_some_and(|s| *s != entry.sandbox)
                    {
                        self.orphan(entry, now).await?;
                    }
                    continue;
                }
                ensure!(seen.insert(id), "duplicate owned inventory identity");
                if a.sandbox.is_none() {
                    ensure!(
                        a.phase == Phase::CreateIntent,
                        "sandbox without preceding create intent"
                    );
                    a.sandbox = Some(entry.sandbox.clone());
                    self.save(id, a.clone())?;
                }
                if let Confirmation::Confirmed(proof) = entry.writer_proof() {
                    self.record_proof(id, a.clone(), &proof)?;
                    if proof.kind() != WriterProofKind::Destroyed {
                        self.mark_cleanup(id, CleanupReason::Stopped, now)?;
                    }
                    continue;
                }
                ensure!(a.stopped.is_none(), "inventory contradicts stopped proof");
                if a.cleanup.is_some()
                    || a.phase == Phase::StopIntent
                    || a.grant.as_ref().is_none_or(|g| {
                        !g.execution && g.boot_id != self.identity().boot_id
                            || g.boot_id == self.identity().boot_id && now >= g.renewal_deadline_ms
                    })
                {
                    self.mark_cleanup(id, CleanupReason::NoCurrentGrant, now)?;
                    self.stop(id).await?;
                }
                // Old-boot execution grants wait for re-adoption. On expiry the
                // runner owns renewal-loss stopping; the independent hard sweep
                // remains a backstop, never a relaunch.
            } else {
                self.orphan(entry, now).await?;
            }
        }
        let missing: Vec<_> = self
            .state()
            .attempts
            .iter()
            .filter(|(id, a)| {
                !seen.contains(id) && !matches!(a.phase, Phase::Destroyed | Phase::Abandoned)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in missing {
            let mut a = self.attempt(id)?;
            if matches!(
                a.phase,
                Phase::Reserved | Phase::AcceptIntent | Phase::Accepted
            ) && a.sandbox.is_none()
            {
                if a.create.owner.boot_id == self.identity().boot_id
                    && now < a.request.hard_deadline_ms
                    && a.cancellation.is_none()
                {
                    continue;
                }
                // No create intent was committed, so this invocation could not
                // have created writers. Acceptance is not execution authority.
                a.phase = Phase::Abandoned;
                a.cleanup = Some(self.cleanup(
                    if a.cancellation.is_some() {
                        CleanupReason::Cancellation
                    } else {
                        CleanupReason::NoCurrentGrant
                    },
                    now,
                ));
                self.save(id, a)?;
            } else if a.sandbox.is_some() {
                if self.inspect(id).await.is_err() {
                    unresolved = true;
                }
            } else {
                unresolved = true;
            }
        }
        if self.registered {
            self.readopt_at(now, &seen).await?;
        }
        self.recovery_required = unresolved;
        Ok(())
    }
    async fn readopt_at(&mut self, now: u64, seen: &BTreeSet<Uuid>) -> Result<()> {
        let mut by_boot: BTreeMap<Uuid, Vec<LiveSandbox>> = BTreeMap::new();
        for (id, a) in &self.state().attempts {
            if !seen.contains(id)
                || a.stopped.is_some()
                || a.cleanup.is_some()
                || a.cancellation.is_some()
            {
                continue;
            }
            if let Some(g) = &a.grant
                && g.execution
                && g.boot_id != self.identity().boot_id
                && now < g.renewal_deadline_ms
                && now < a.request.hard_deadline_ms
            {
                by_boot.entry(g.boot_id).or_default().push(LiveSandbox {
                    permit_id: g.permit_id,
                    attempt_id: *id,
                    grant_generation: g.generation,
                    sandbox_id: g.sandbox_id,
                });
            }
        }
        let mut requests = Vec::new();
        for (boot, live) in by_boot {
            for chunk in live.chunks(128) {
                let prior = self
                    .state()
                    .readoptions
                    .iter()
                    .find(|r| r.previous.boot_id == boot && r.sandboxes == chunk);
                requests.push(prior.cloned().unwrap_or_else(|| ReadoptionRequest {
                    schema_version: 1,
                    operation_id: Uuid::new_v4(),
                    previous: WorkerIdentity {
                        node_id: self.identity().node_id,
                        boot_id: boot,
                    },
                    current: self.identity().clone(),
                    sandboxes: chunk.to_vec(),
                }));
            }
        }
        let mut state = self.state().clone();
        state.readoptions = requests.clone();
        self.journal.persist(state)?;
        for request in requests {
            let timeout = std::time::Duration::from_millis(self.config.recovery.sweep_interval_ms);
            let response = match tokio::time::timeout(timeout, self.provider.readopt(&request))
                .await
            {
                Ok(Ok(r)) => r,
                _ => {
                    tracing::warn!(operation=%request.operation_id,"re-adoption unavailable; runner deadline unchanged");
                    continue;
                }
            };
            response.validate()?;
            ensure!(
                response.operation_id == request.operation_id
                    && response.previous == request.previous
                    && response.current == request.current
                    && response.results.len() == request.sandboxes.len(),
                "re-adoption response mismatch"
            );
            // Validate the complete page before applying any observation.
            for result in &response.results {
                let sandbox_id = match result {
                    ReadoptionResult::Rebound { permit } => permit.binding.sandbox_id,
                    ReadoptionResult::Refused { sandbox_id, .. } => *sandbox_id,
                };
                let listed = request
                    .sandboxes
                    .iter()
                    .find(|s| s.sandbox_id == sandbox_id)
                    .ok_or_else(|| anyhow::anyhow!("unrequested sandbox rebound"))?;
                if let ReadoptionResult::Rebound { permit } = result {
                    let a = self.attempt(listed.attempt_id)?;
                    let g = a.grant.as_ref().unwrap();
                    ensure!(
                        permit.binding.attempt_id == listed.attempt_id
                            && permit.binding.grant_generation == listed.grant_generation
                            && permit.permit_id == listed.permit_id
                            && permit.binding.channel_id == g.channel_id
                            && permit.scope == PermitScope::Execution
                            && permit.expires_at_ms == g.renewal_deadline_ms
                            && permit.hard_deadline_ms == a.request.hard_deadline_ms
                            && permit.issued_at_ms <= now
                            && now < permit.expires_at_ms,
                        "re-adoption changed grant or deadline"
                    );
                }
            }
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
                            let id = request
                                .sandboxes
                                .iter()
                                .find(|s| s.sandbox_id == sandbox_id)
                                .unwrap()
                                .attempt_id;
                            self.mark_cleanup(id, CleanupReason::NoCurrentGrant, now)?;
                            self.stop(id).await?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
    /// Local backstop: only stops. It never calls destroy, even with retention.
    pub async fn sweep_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        let entries = self.backend.list_owned(self.identity().node_id).await?;
        for entry in entries {
            if entry.sandbox.owner.node_id != self.identity().node_id {
                continue;
            }
            let id = entry.sandbox.owner.attempt_id;
            if let Some(a) = self.state().attempts.get(&id).cloned() {
                if !self.matches_entry(&a, &entry) {
                    self.recovery_required = true;
                    continue;
                }
                let cancelled = a
                    .cancellation
                    .as_ref()
                    .is_some_and(|c| now >= c.stop_after_ms);
                if now >= a.create.hard_deadline_ms || cancelled {
                    if a.sandbox.is_none() {
                        let mut a = a.clone();
                        a.sandbox = Some(entry.sandbox.clone());
                        self.save(id, a)?;
                    }
                    self.mark_cleanup(
                        id,
                        if cancelled {
                            CleanupReason::Cancellation
                        } else {
                            CleanupReason::HardDeadline
                        },
                        now,
                    )?;
                    self.stop(id).await?;
                }
            } else {
                // Unknown owned work has no current known grant, regardless of TTL.
                self.orphan(entry, now).await?;
            }
        }
        Ok(())
    }
    /// Separate from sweep so a deadline can never itself authorize deletion.
    pub async fn retry_handoffs_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        let ids: Vec<_> = self
            .state()
            .attempts
            .iter()
            .filter(|(_, a)| {
                a.sandbox.is_some()
                    && a.phase != Phase::Destroyed
                    && (a.cleanup.is_some() || a.stopped.is_some())
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.mark_cleanup(id, CleanupReason::Stopped, now)?;
            // A receipt must cover the final workspace. Do not extract while
            // an uncertain writer can still change it after retention.
            if !self.stop(id).await?.is_confirmed() {
                continue;
            }
            let mut a = self.attempt(id)?;
            let s = a.sandbox.clone().unwrap();
            if a.destroy_authorization.is_none()
                && let Ok(Ok(Some(auth))) = tokio::time::timeout(
                    std::time::Duration::from_millis(self.config.recovery.sweep_interval_ms),
                    self.provider.retain(&s),
                )
                .await
            {
                ensure!(
                    auth.authorizes(&s),
                    "retention receipt belongs to another sandbox"
                );
                a.destroy_authorization = Some(auth);
                self.save(id, a.clone())?;
            }
            if now >= a.cleanup.as_ref().unwrap().destroy_after_ms
                && let Some(auth) = a.destroy_authorization
            {
                tracing::info!(attempt=%id,reason=?a.cleanup.as_ref().unwrap().reason,"destroying retained sandbox");
                self.destroy_at(id, &auth).await?;
            }
        }
        let keys: Vec<_> = self
            .state()
            .orphans
            .iter()
            .filter(|(_, o)| !o.destroyed)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.stop_orphan(&key).await?;
            let mut o = self.state().orphans[&key].clone();
            if o.stopped.is_none() {
                continue;
            }
            if o.authorization.is_none()
                && let Ok(Ok(Some(auth))) = tokio::time::timeout(
                    std::time::Duration::from_millis(self.config.recovery.sweep_interval_ms),
                    self.provider.retain(&o.entry.sandbox),
                )
                .await
            {
                ensure!(
                    auth.authorizes(&o.entry.sandbox),
                    "orphan retention receipt mismatch"
                );
                o.authorization = Some(auth);
                let mut state = self.state().clone();
                state.orphans.insert(key.clone(), o.clone());
                self.journal.persist(state)?;
            }
            if now >= o.cleanup.destroy_after_ms
                && self.state().orphans[&key].stopped.is_some()
                && let Some(auth) = o.authorization
                && let Confirmation::Confirmed(proof) =
                    self.backend.destroy(&o.entry.sandbox, &auth).await
            {
                ensure!(
                    *proof.sandbox() == o.entry.sandbox
                        && proof.kind() == WriterProofKind::Destroyed,
                    "orphan destruction not confirmed"
                );
                let mut state = self.state().clone();
                let row = state.orphans.get_mut(&key).unwrap();
                row.destroyed = true;
                row.stopped = Some(proof);
                self.journal.persist(state)?;
                tracing::info!(sandbox=%key,reason=?o.cleanup.reason,"destroyed retained orphan");
            }
        }
        Ok(())
    }
}
