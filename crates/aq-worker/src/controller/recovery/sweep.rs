use super::*;
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    /// Local backstop: only stops. It never calls destroy, even with retention.
    pub async fn sweep_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        let node = self.identity().node_id;
        let entries = self.backend.list_owned(node).await?;
        for entry in entries
            .into_iter()
            .filter(|e| e.sandbox.owner.node_id == node)
        {
            self.sweep_entry(entry, now).await?;
        }
        Ok(())
    }
    async fn sweep_entry(&mut self, entry: InventoryEntry, now: u64) -> Result<()> {
        let id = entry.sandbox.owner.attempt_id;
        let Some(mut a) = self.state().attempts.get(&id).cloned() else {
            // Unknown owned work has no current known grant, regardless of TTL.
            return self.orphan(entry, now).await;
        };
        if !self.matches_entry(&a, &entry) {
            self.recovery_required = true;
        } else if let Some(reason) = a.expired_reason(now) {
            self.attach_sandbox(id, &mut a, &entry.sandbox)?;
            self.mark_cleanup(id, reason, now)?;
            self.stop(id).await?;
        }
        Ok(())
    }
    /// SBX20 watchdog: stop each known attempt whose sampled disk usage reached
    /// its limit. The work volume is preserved; the orphan sweep handles unknowns.
    pub async fn watch_disk_at(&mut self, now: u64) -> Result<()> {
        self.journal.check()?;
        if self.backend.disk_limit().mode.is_none() {
            return Ok(());
        }
        let usage = self.backend.disk_usage(self.identity().node_id).await?;
        for sample in usage.into_iter().filter(DiskUsage::exceeded) {
            self.stop_over_limit(sample, now).await?;
        }
        Ok(())
    }
    async fn stop_over_limit(&mut self, sample: DiskUsage, now: u64) -> Result<()> {
        let id = sample.sandbox.owner.attempt_id;
        let Some(mut a) = self.state().attempts.get(&id).cloned() else {
            return Ok(());
        };
        if !a.owns_disk_usage(&sample.sandbox, self.backend.kind()) {
            return Ok(());
        }
        tracing::warn!(attempt = %id, used_bytes = sample.used_bytes, limit_bytes = sample.limit_bytes, "sandbox_disk_limit_exceeded; stopping and preserving the attempt");
        self.attach_sandbox(id, &mut a, &sample.sandbox)?;
        self.mark_cleanup(id, CleanupReason::SandboxDiskLimitExceeded, now)?;
        self.stop(id).await?;
        Ok(())
    }
}
impl Attempt {
    fn expired_reason(&self, now: u64) -> Option<CleanupReason> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|c| now >= c.stop_after_ms)
        {
            Some(CleanupReason::Cancellation)
        } else {
            (now >= self.create.hard_deadline_ms).then_some(CleanupReason::HardDeadline)
        }
    }
    fn owns_disk_usage(&self, sandbox: &SandboxIdentity, kind: BackendKind) -> bool {
        self.stopped.is_none()
            && !self.foreign_sandbox(sandbox)
            && sandbox.kind == kind
            && sandbox.backend_id == self.create.name
    }
}
