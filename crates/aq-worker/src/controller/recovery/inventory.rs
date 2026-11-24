use super::*;
use std::collections::BTreeSet;

#[derive(Default)]
struct RecoveryInventory {
    observed: BTreeSet<Uuid>,
    matched: BTreeSet<Uuid>,
    unresolved: bool,
}
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    /// Complete owned inventory is required; absence alone never releases writers.
    pub async fn reconcile_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        self.recovery_required = true;
        let node = self.identity().node_id;
        let entries = self.backend.list_owned(node).await?;
        let mut inventory = RecoveryInventory::default();
        for entry in entries
            .into_iter()
            .filter(|e| e.sandbox.owner.node_id == node)
        {
            inventory.observed.insert(entry.sandbox.owner.attempt_id);
            self.reconcile_entry(entry, now, &mut inventory).await?;
        }
        self.reconcile_missing(now, &mut inventory).await?;
        self.record_abandoned(now, &inventory.observed).await?;
        if self.registered {
            self.readopt_at(now, &inventory.matched).await?;
        }
        self.recovery_required = inventory.unresolved;
        Ok(())
    }
    async fn reconcile_entry(
        &mut self,
        entry: InventoryEntry,
        now: u64,
        inventory: &mut RecoveryInventory,
    ) -> Result<()> {
        ensure!(
            entry.sandbox.kind == self.backend.kind() && !entry.sandbox.backend_id.is_empty(),
            "invalid backend inventory identity"
        );
        let id = entry.sandbox.owner.attempt_id;
        let Some(mut a) = self.state().attempts.get(&id).cloned() else {
            return self.orphan(entry, now).await;
        };
        if !self.matches_entry(&a, &entry) {
            inventory.unresolved = true;
            if a.foreign_sandbox(&entry.sandbox) {
                self.orphan(entry, now).await?;
            }
            return Ok(());
        }
        ensure!(
            inventory.matched.insert(id),
            "duplicate owned inventory identity"
        );
        if a.sandbox.is_none() {
            ensure!(
                a.phase == Phase::CreateIntent,
                "sandbox without preceding create intent"
            );
            self.attach_sandbox(id, &mut a, &entry.sandbox)?;
        }
        self.reconcile_writers(a, &entry, now).await
    }
    async fn reconcile_writers(
        &mut self,
        a: Attempt,
        entry: &InventoryEntry,
        now: u64,
    ) -> Result<()> {
        let id = entry.sandbox.owner.attempt_id;
        if let Confirmation::Confirmed(proof) = entry.writer_proof() {
            self.record_proof(id, a, &proof)?;
            if proof.kind() != WriterProofKind::Destroyed {
                self.mark_cleanup(id, CleanupReason::Stopped, now)?;
            }
            return Ok(());
        }
        ensure!(a.stopped.is_none(), "inventory contradicts stopped proof");
        // Old-boot execution grants wait for re-adoption. The runner owns
        // renewal-loss stopping, with the hard deadline sweep as a backstop.
        if a.must_stop_writers(self.identity().boot_id, now) {
            self.mark_cleanup(id, CleanupReason::NoCurrentGrant, now)?;
            self.stop(id).await?;
        }
        Ok(())
    }
    async fn reconcile_missing(
        &mut self,
        now: u64,
        inventory: &mut RecoveryInventory,
    ) -> Result<()> {
        let missing: Vec<_> = self
            .state()
            .attempts
            .iter()
            .filter(|(id, a)| {
                !inventory.matched.contains(id)
                    && !matches!(a.phase, Phase::Destroyed | Phase::Abandoned)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in missing {
            let a = self.attempt(id)?;
            if a.before_create() {
                self.abandon_if_expired(id, a, now)?;
            } else if a.sandbox.is_none() || self.inspect(id).await.is_err() {
                inventory.unresolved = true;
            }
        }
        Ok(())
    }
    fn abandon_if_expired(&mut self, id: Uuid, mut a: Attempt, now: u64) -> Result<()> {
        if a.pending_reservation(self.identity().boot_id, now) {
            return Ok(());
        }
        // No create intent was committed, so this invocation could not have created writers.
        let reason = if a.cancellation.is_some() {
            CleanupReason::Cancellation
        } else {
            CleanupReason::NoCurrentGrant
        };
        a.phase = Phase::Abandoned;
        a.cleanup = Some(self.cleanup(reason, now));
        self.save(id, a)
    }
    async fn record_abandoned(&mut self, now: u64, observed: &BTreeSet<Uuid>) -> Result<()> {
        // A crash can separate saving abandonment from journaling the backend fact.
        let never_created: Vec<_> = self
            .state()
            .attempts
            .values()
            .filter(|a| {
                a.phase == Phase::Abandoned && !observed.contains(&a.create.owner.attempt_id)
            })
            .map(|a| a.create.clone())
            .collect();
        for create in never_created {
            self.backend.record_not_created(&create, now).await?;
        }
        Ok(())
    }
    pub(super) fn attach_sandbox(
        &mut self,
        id: Uuid,
        a: &mut Attempt,
        sandbox: &SandboxIdentity,
    ) -> Result<()> {
        if a.sandbox.is_none() {
            a.sandbox = Some(sandbox.clone());
            self.save(id, a.clone())?;
        }
        Ok(())
    }
}
impl Attempt {
    pub(super) fn foreign_sandbox(&self, sandbox: &SandboxIdentity) -> bool {
        sandbox.owner != self.create.owner || self.sandbox.as_ref().is_some_and(|s| s != sandbox)
    }
    fn pending_reservation(&self, boot: Uuid, now: u64) -> bool {
        self.create.owner.boot_id == boot
            && now < self.request.hard_deadline_ms
            && self.cancellation.is_none()
    }
    fn before_create(&self) -> bool {
        self.sandbox.is_none()
            && matches!(
                self.phase,
                Phase::Reserved | Phase::AcceptIntent | Phase::Accepted
            )
    }
    fn must_stop_writers(&self, boot: Uuid, now: u64) -> bool {
        self.cleanup.is_some()
            || self.phase == Phase::StopIntent
            || self
                .grant
                .as_ref()
                .is_none_or(|g| g.invalid_after_restart(boot, now))
    }
}
impl GrantMonitor {
    fn invalid_after_restart(&self, boot: Uuid, now: u64) -> bool {
        if self.boot_id == boot {
            now >= self.renewal_deadline_ms
        } else {
            !self.execution
        }
    }
}
