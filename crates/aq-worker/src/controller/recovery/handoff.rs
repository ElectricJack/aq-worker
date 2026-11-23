use super::*;
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    /// Separate from sweep so a deadline can never itself authorize deletion.
    pub async fn retry_handoffs_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        let ids: Vec<_> = self
            .state()
            .attempts
            .iter()
            .filter(|(_, a)| a.needs_handoff())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.handoff_attempt(id, now).await?;
        }
        let keys: Vec<_> = self
            .state()
            .orphans
            .iter()
            .filter(|(_, o)| !o.destroyed)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.handoff_orphan(&key, now).await?;
        }
        Ok(())
    }
    async fn handoff_attempt(&mut self, id: Uuid, now: u64) -> Result<()> {
        self.mark_cleanup(id, CleanupReason::Stopped, now)?;
        // A receipt must cover the final workspace, after the writer stops.
        if !self.stop(id).await?.is_confirmed() {
            return Ok(());
        }
        let mut a = self.attempt(id)?;
        if a.handoff_due(now) {
            a = self.retain_attempt(id, a, now).await?;
        }
        let cleanup = a.cleanup.as_ref().unwrap();
        // A preserved sandbox is already past its grace on a successful handoff.
        if let Some(auth) = a
            .destroy_authorization
            .as_ref()
            .filter(|_| a.preserved.is_some() || now >= cleanup.destroy_after_ms)
        {
            tracing::info!(attempt=%id,reason=?cleanup.reason,"destroying retained sandbox");
            self.destroy_at(id, auth).await?;
        } else if a.preserved.is_some() {
            self.tend_preserved(id, now).await?;
        }
        Ok(())
    }
    async fn retain(&mut self, sandbox: &SandboxIdentity) -> Option<DestroyAuthorization> {
        tokio::time::timeout(self.sweep_interval(), self.provider.retain(sandbox))
            .await
            .ok()?
            .ok()?
    }
    async fn retain_attempt(&mut self, id: Uuid, mut a: Attempt, now: u64) -> Result<Attempt> {
        let s = a.sandbox.as_ref().unwrap();
        if let Some(auth) = self.retain(s).await {
            ensure!(
                auth.authorizes(s),
                "retention receipt belongs to another sandbox"
            );
            a.destroy_authorization = Some(auth);
            self.save(id, a.clone())?;
            Ok(a)
        } else {
            self.preserve_failed_handoff(id, now)?;
            self.attempt(id)
        }
    }
    async fn handoff_orphan(&mut self, key: &str, now: u64) -> Result<()> {
        self.stop_orphan(key).await?;
        let mut o = self.state().orphans[key].clone();
        if o.stopped.is_none() {
            return Ok(());
        }
        if o.authorization.is_none()
            && let Some(auth) = self.retain(&o.entry.sandbox).await
        {
            ensure!(
                auth.authorizes(&o.entry.sandbox),
                "orphan retention receipt mismatch"
            );
            o.authorization = Some(auth);
            let mut state = self.state().clone();
            state.orphans.insert(key.to_owned(), o.clone());
            self.journal.persist(state)?;
        }
        if now >= o.cleanup.destroy_after_ms && self.state().orphans[key].stopped.is_some() {
            self.destroy_retained_orphan(key, &o).await?;
        }
        Ok(())
    }
    async fn destroy_retained_orphan(&mut self, key: &str, o: &Orphan) -> Result<()> {
        let Some(auth) = &o.authorization else {
            return Ok(());
        };
        if let Confirmation::Confirmed(proof) = self.backend.destroy(&o.entry.sandbox, auth).await {
            ensure!(
                *proof.sandbox() == o.entry.sandbox && proof.kind() == WriterProofKind::Destroyed,
                "orphan destruction not confirmed"
            );
            let mut state = self.state().clone();
            let row = state.orphans.get_mut(key).unwrap();
            row.destroyed = true;
            row.stopped = Some(proof);
            self.journal.persist(state)?;
            tracing::info!(sandbox=%key,reason=?o.cleanup.reason,"destroyed retained orphan");
        }
        Ok(())
    }
}
impl Attempt {
    fn needs_handoff(&self) -> bool {
        self.sandbox.is_some()
            && self.phase != Phase::Destroyed
            && (self.cleanup.is_some() || self.stopped.is_some())
    }
    fn handoff_due(&self, now: u64) -> bool {
        self.destroy_authorization.is_none()
            && self
                .preserved
                .as_ref()
                .is_none_or(|p| now >= p.next_handoff_ms)
    }
}
