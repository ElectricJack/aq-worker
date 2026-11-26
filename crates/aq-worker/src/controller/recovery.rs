//! Recovery never grants execution or repeats create/start. All cleanup effects
//! follow durable reason/identity records; uncertain proof keeps reservations.
use super::*;
mod grant;
mod handoff;
mod inventory;
mod preservation;
mod readoption;
mod sweep;

impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub(super) fn has_orphans(&self) -> bool {
        self.state().orphans.values().any(|o| !o.destroyed)
    }
    fn sweep_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.config.recovery.sweep_interval_ms)
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
}
