use super::*;
#[derive(Default)]
struct UsedCapacity {
    memory: u64,
    cpu: u64,
    disk: u64,
    active: usize,
    preparing: usize,
    preserved: u64,
}
impl UsedCapacity {
    fn include_attempt(&mut self, a: &Attempt) {
        if a.preserves_disk() {
            self.preserved += 1;
        }
        if a.stopped.is_none() && a.phase != Phase::Abandoned {
            self.active += 1;
            self.memory = self
                .memory
                .saturating_add(a.request.manifest.resources.memory_bytes);
            self.cpu = self
                .cpu
                .saturating_add(a.request.manifest.resources.cpu_millis);
            self.preparing += usize::from(a.phase != Phase::Running);
        }
        if a.occupies_disk() {
            self.disk = self.disk.saturating_add(a.request.disk_bytes);
        }
    }
    fn include_orphan(&mut self, o: &Orphan, cap: &CapacityLimits) {
        // Unknown resource requirements cannot be subtracted optimistically.
        self.disk = cap.disk_bytes;
        if o.stopped.is_none() {
            self.active += 1;
            self.preparing += 1;
            self.memory = cap.memory_bytes;
            self.cpu = cap.cpu_millis;
        }
    }
}
impl Attempt {
    fn occupies_disk(&self) -> bool {
        !matches!(self.phase, Phase::Destroyed | Phase::Abandoned)
    }
    fn preserves_disk(&self) -> bool {
        self.stopped.is_some() && self.occupies_disk()
    }
}
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub fn inventory(&self) -> Inventory {
        let mut used = UsedCapacity::default();
        let cap = &self.config.capacity;
        for a in self.state().attempts.values() {
            used.include_attempt(a);
        }
        let reserved = used.disk;
        for o in self.state().orphans.values().filter(|o| !o.destroyed) {
            used.include_orphan(o, cap);
        }
        let disk_limit = self.backend.disk_limit();
        Inventory {
            disk_limit: disk_limit.mode,
            shared_root_fs: disk_limit.shared_root_fs,
            identity: self.identity().clone(),
            generation: self.state().revision,
            reserved_sandboxes: used.active,
            preparing: used.preparing,
            available_memory_bytes: cap.memory_bytes.saturating_sub(used.memory),
            available_cpu_millis: cap.cpu_millis.saturating_sub(used.cpu),
            available_disk_bytes: cap.disk_bytes.saturating_sub(used.disk),
            refusal: self.inventory_refusal(),
            disk_total_bytes: None,
            disk_used_bytes: None,
            disk_free_bytes: None,
            disk_reserved_bytes: Some(reserved),
            preserved_count: Some(used.preserved),
            preserved_bytes: None,
            retained_snapshot_bytes: None,
            saved_snapshots_over_allowance: None,
        }
    }
    fn inventory_refusal(&self) -> Option<RefusalReason> {
        if self.draining {
            Some(RefusalReason::Draining)
        } else if self.journal.check().is_err() || self.admission_blocked() {
            Some(RefusalReason::GrantDenied)
        } else {
            self.state().last_refusal
        }
    }
    /// [`Self::inventory`] plus backend disk measurement, for reporting only.
    pub async fn measured_inventory(&mut self) -> Inventory {
        let preserved: Vec<SandboxIdentity> = self
            .state()
            .attempts
            .values()
            .filter(|a| a.preserves_disk())
            .filter_map(|a| a.sandbox.clone())
            .collect();
        let mut inventory = self.inventory();
        let usage = self.backend.measured_disk_usage(&preserved).await;
        if let Some(fs) = usage.filesystem {
            inventory.disk_total_bytes = Some(fs.total_bytes);
            inventory.disk_used_bytes = Some(fs.used_bytes);
            inventory.disk_free_bytes = Some(fs.free_bytes);
        }
        inventory.preserved_bytes = usage.preserved_bytes;
        inventory.retained_snapshot_bytes = usage.retained_snapshot_bytes;
        inventory.saved_snapshots_over_allowance = usage.saved_snapshots_over_allowance;
        inventory
    }
}
