use super::*;
use aq_admission_sandbox::sandbox::Money;
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub fn reserve(&mut self, request: LaunchRequest) -> Result<Uuid> {
        self.journal.check()?;
        if self.draining {
            return self.refuse(RefusalReason::Draining);
        }
        let id = request.manifest.bindings.attempt_id;
        if self.reservation_replay(&request)? {
            return Ok(id);
        }
        self.check_admission(&request)?;
        let cost = self.template_cost(&request)?;
        let inventory = self.inventory();
        if inventory.reserved_sandboxes >= self.config.max_concurrent_sandboxes as usize {
            return self.refuse(RefusalReason::SandboxCeiling);
        }
        let spend = self.reserved_spend(&cost)?;
        self.check_resources(&request, &inventory)?;
        let create = self.create_reservation(&request)?;
        self.commit_reservation(Attempt::reserved(request, create), cost.currency, spend)
    }
    fn commit_reservation(
        &mut self,
        attempt: Attempt,
        currency: String,
        spend: u64,
    ) -> Result<Uuid> {
        let id = attempt.request.manifest.bindings.attempt_id;
        let mut state = self.state().clone();
        state.estimated_spend.insert(currency, spend);
        state.last_refusal = None;
        state.attempts.insert(id, attempt);
        self.journal.persist(state)?;
        Ok(id)
    }
    pub(super) fn admission_blocked(&self) -> bool {
        !self.registered || self.recovery_required || self.has_orphans()
    }
    fn reservation_replay(&self, request: &LaunchRequest) -> Result<bool> {
        if self.admission_blocked() {
            return Ok(false);
        }
        let previous = self
            .state()
            .attempts
            .get(&request.manifest.bindings.attempt_id);
        Ok(
            previous.is_some_and(|a| a.phase == Phase::Reserved && a.request == *request)
                && request.hard_deadline_ms > (self.clock)()?,
        )
    }
    fn check_admission(&mut self, request: &LaunchRequest) -> Result<()> {
        if self.has_orphans() {
            return self.refuse(RefusalReason::GrantDenied);
        }
        if self.preserved_count() >= self.config.preserve.max_count {
            return self.refuse(RefusalReason::NodePreservedCapacityReached);
        }
        let pending = self
            .state()
            .attempts
            .values()
            .filter(|a| a.stopped.is_some() && a.phase != Phase::Destroyed)
            .count();
        if pending >= self.config.recovery.max_pending_results {
            return self.refuse(RefusalReason::PendingRetention);
        }
        if !self.accepts_request(request)? {
            return self.refuse(RefusalReason::GrantDenied);
        }
        Ok(())
    }
    fn accepts_request(&self, request: &LaunchRequest) -> Result<bool> {
        let attempts = &self.state().attempts;
        if !self.registered || self.recovery_required {
            return Ok(false);
        }
        if attempts.contains_key(&request.manifest.bindings.attempt_id)
            || attempts.len() >= journal::MAX_ATTEMPTS
        {
            return Ok(false);
        }
        Ok(request.manifest.validate().is_ok()
            && request.hard_deadline_ms > (self.clock)()?
            && request.matches_node(self.identity()))
    }
    fn template_cost(&mut self, request: &LaunchRequest) -> Result<Money> {
        let m = &request.manifest;
        let Some(template) = self.config.templates.iter().find(|t| {
            t.digest == m.harness.environment.digest
                && t.harnesses.iter().any(|h| {
                    h.harness == m.harness.harness.as_str()
                        && h.version == m.harness.cli_version.as_str()
                })
        }) else {
            return self.refuse(RefusalReason::EnvironmentUnavailable);
        };
        let cost = template.estimated_cold_start_cost.clone();
        if request
            .required_capabilities
            .iter()
            .any(|c| !self.backend.capabilities().contains(c))
        {
            return self.refuse(RefusalReason::EnvironmentUnavailable);
        }
        Ok(cost)
    }
    fn reserved_spend(&mut self, cost: &Money) -> Result<u64> {
        let previous_spend = self
            .state()
            .estimated_spend
            .get(&cost.currency)
            .copied()
            .unwrap_or(0);
        let Some(spend) = previous_spend.checked_add(cost.micros) else {
            return self.refuse(RefusalReason::SpendCeiling);
        };
        if self.config.spend_ceiling.as_ref().is_some_and(|c| {
            c.currency != cost.currency || previous_spend >= c.micros || spend > c.micros
        }) {
            return self.refuse(RefusalReason::SpendCeiling);
        }
        Ok(spend)
    }
    fn check_resources(&mut self, request: &LaunchRequest, inventory: &Inventory) -> Result<()> {
        let m = &request.manifest;
        if inventory.preparing >= self.config.capacity.max_preparing as usize {
            return self.refuse(RefusalReason::PreparationBacklog);
        }
        if m.resources.cpu_millis > inventory.available_cpu_millis
            || m.resources.memory_bytes > inventory.available_memory_bytes
        {
            return self.refuse(RefusalReason::CapacityChanged);
        }
        let reserved_disk = self
            .config
            .capacity
            .disk_bytes
            .saturating_sub(inventory.available_disk_bytes);
        if !(1..=inventory.available_disk_bytes).contains(&request.disk_bytes)
            || request.disk_bytes
                > self
                    .journal
                    .storage
                    .free_bytes()?
                    .saturating_sub(reserved_disk)
        {
            return self.refuse(RefusalReason::DiskReserve);
        }
        Ok(())
    }
    fn create_reservation(&mut self, request: &LaunchRequest) -> Result<CreateRequest> {
        let m = &request.manifest;
        let id = m.bindings.attempt_id;
        let Ok(cpu_millis) = u32::try_from(m.resources.cpu_millis) else {
            return self.refuse(RefusalReason::CapacityChanged);
        };
        let create = CreateRequest::new(
            Ownership {
                node_id: self.identity().node_id,
                boot_id: self.identity().boot_id,
                attempt_id: id,
            },
            TemplateDigest::parse(format!("sha256:{}", m.harness.environment.digest.as_str()))?,
            request
                .hard_deadline_ms
                .checked_add(self.config.recovery.deadline_margin_ms)
                .ok_or_else(|| anyhow::anyhow!("sandbox deadline overflow"))?,
            request.required_capabilities.clone(),
        )
        .with_resources(ResourceLimits {
            cpu_millis,
            memory_bytes: m.resources.memory_bytes,
            pids: m.resources.process_limit,
            // SBX20: the reservation's disk bytes bound the attempt; zero
            // leaves the backend's node default in force.
            disk_bytes: Some(request.disk_bytes).filter(|bytes| *bytes > 0),
        });
        if create.validate().is_err() {
            return self.refuse(RefusalReason::CapacityChanged);
        }
        Ok(create)
    }
}
impl LaunchRequest {
    pub(super) fn matches_node(&self, node: &WorkerIdentity) -> bool {
        matches!(self.manifest.bindings.session, SessionIdentity::Remote {node_id,boot_id,..} if node_id==node.node_id && boot_id==node.boot_id)
    }
}
impl Attempt {
    fn reserved(request: LaunchRequest, create: CreateRequest) -> Self {
        Self {
            request,
            create,
            accept_operation_id: Uuid::new_v4(),
            phase: Phase::Reserved,
            sandbox: None,
            stopped: None,
            permit_id: None,
            logical_sandbox_id: None,
            channel_id: None,
            destroy_authorization: None,
            grant: None,
            cleanup: None,
            cancellation: None,
            preserved: None,
        }
    }
}
