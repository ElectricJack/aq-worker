//! Durable node controller. All agent execution remains inside the backend.
mod journal;
mod service;
pub use service::{RunConfig, UnavailableProvider};
mod lifecycle;
mod storage;
mod types;
use crate::backend::*;
use anyhow::{Result, ensure};
use aq_admission_sandbox::{
    sandbox::{self, Registration},
    transport::{Register, WorkerIdentity},
};
use aq_session_protocol::{SessionIdentity, Validate};
use std::{future::Future, path::Path};
pub use types::*;
use uuid::Uuid;

pub struct Controller<B: PlacementBackend, P: PermitProvider> {
    backend: B,
    provider: P,
    journal: journal::Journal,
    config: NodeConfig,
    registration_operation: Uuid,
    registered: bool,
    draining: bool,
    recovery_required: bool,
}
impl<B: PlacementBackend, P: PermitProvider> Controller<B, P> {
    pub fn open(
        root: &Path,
        node: Uuid,
        config: NodeConfig,
        backend: B,
        provider: P,
    ) -> Result<Self> {
        ensure!(
            config.capacity.memory_bytes > 0
                && config.capacity.cpu_millis > 0
                && config.capacity.disk_bytes > 0
                && config.capacity.max_preparing > 0,
            "invalid node capacity"
        );
        let journal = journal::Journal::open(root, node)?;
        let recovery_required = journal.state.attempts.values().any(|a| a.stopped.is_none());
        let controller = Self {
            backend,
            provider,
            journal,
            config,
            registration_operation: Uuid::new_v4(),
            registered: false,
            draining: false,
            recovery_required,
        };
        controller.registration()?.validate()?;
        Ok(controller)
    }
    pub fn identity(&self) -> &WorkerIdentity {
        &self.journal.state.identity
    }
    pub fn state(&self) -> &State {
        &self.journal.state
    }
    pub fn inventory(&self) -> Inventory {
        let mut memory = 0u64;
        let mut cpu = 0u64;
        let mut disk = 0u64;
        let mut count = 0;
        let mut preparing = 0;
        for a in self.state().attempts.values() {
            if a.stopped.is_none() {
                count += 1;
                memory = memory.saturating_add(a.request.manifest.resources.memory_bytes);
                cpu = cpu.saturating_add(a.request.manifest.resources.cpu_millis);
                if a.phase != Phase::Running {
                    preparing += 1;
                }
            }
            if a.phase != Phase::Destroyed {
                disk = disk.saturating_add(a.request.disk_bytes);
            }
        }
        Inventory {
            identity: self.identity().clone(),
            generation: self.state().revision,
            reserved_sandboxes: count,
            preparing,
            available_memory_bytes: self.config.capacity.memory_bytes.saturating_sub(memory),
            available_cpu_millis: self.config.capacity.cpu_millis.saturating_sub(cpu),
            available_disk_bytes: self.config.capacity.disk_bytes.saturating_sub(disk),
            refusal: if self.draining {
                Some(RefusalReason::Draining)
            } else if self.journal.check().is_err() || self.recovery_required || !self.registered {
                Some(RefusalReason::GrantDenied)
            } else {
                self.state().last_refusal
            },
        }
    }
    pub fn registration(&self) -> Result<Registration> {
        let kind = match self.backend.kind() {
            BackendKind::DockerLocal => sandbox::BackendKind::DockerLocal,
            BackendKind::Boat => sandbox::BackendKind::Boat,
            BackendKind::CloudflareContainers => sandbox::BackendKind::CloudflareContainers,
        };
        let capabilities = self
            .backend
            .capabilities()
            .iter()
            .map(|c| match c {
                Capability::VmIsolation => sandbox::BackendCapability::VmIsolation,
                Capability::NestedDocker => sandbox::BackendCapability::NestedDocker,
                Capability::SuspendSnapshot => sandbox::BackendCapability::SuspendSnapshot,
                Capability::PersistentDiskDuringAttempt => {
                    sandbox::BackendCapability::PersistentDiskDuringAttempt
                }
            })
            .collect();
        let r = Registration {
            schema_version: 2,
            registration: Register {
                schema_version: 1,
                operation_id: self.registration_operation,
                identity: self.identity().clone(),
                runtime: self.config.runtime.clone(),
                capabilities: self.config.capabilities.clone(),
                features: self.config.features.clone(),
            },
            backend: sandbox::BackendAdvertisement {
                kind,
                capabilities,
                max_concurrent_sandboxes: self.config.max_concurrent_sandboxes,
                spend_ceiling: self.config.spend_ceiling.clone(),
                templates: self.config.templates.clone(),
            },
        };
        r.validate()?;
        Ok(r)
    }
    pub async fn register(&mut self) -> Result<()> {
        self.journal.check()?;
        self.registered = false;
        let r = self.registration()?;
        let mut state = self.state().clone();
        state.registration_operation_id = Some(r.registration.operation_id);
        self.journal.persist(state)?;
        self.provider.register(&r).await?;
        self.journal.check()?;
        self.registered = true;
        Ok(())
    }
    fn refuse<T>(&mut self, reason: RefusalReason) -> Result<T> {
        let mut state = self.state().clone();
        state.last_refusal = Some(reason);
        self.journal.persist(state)?;
        Err(reason.into())
    }
    pub fn reserve(&mut self, request: LaunchRequest) -> Result<Uuid> {
        self.journal.check()?;
        if self.draining {
            return self.refuse(RefusalReason::Draining);
        }
        let id = request.manifest.bindings.attempt_id;
        if !self.registered
            || self.recovery_required
            || self.state().attempts.contains_key(&id)
            || self.state().attempts.len() >= journal::MAX_ATTEMPTS
            || request.manifest.validate().is_err()
            || request.hard_deadline_ms <= now_ms()?
            || !matches!(request.manifest.bindings.session, SessionIdentity::Remote {node_id,boot_id,..} if node_id==self.identity().node_id && boot_id==self.identity().boot_id)
        {
            return self.refuse(RefusalReason::GrantDenied);
        }
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
        let inventory = self.inventory();
        if inventory.reserved_sandboxes >= self.config.max_concurrent_sandboxes as usize {
            return self.refuse(RefusalReason::SandboxCeiling);
        }
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
        if request.disk_bytes == 0
            || request.disk_bytes > inventory.available_disk_bytes
            || request.disk_bytes
                > self
                    .journal
                    .storage
                    .free_bytes()?
                    .saturating_sub(reserved_disk)
        {
            return self.refuse(RefusalReason::DiskReserve);
        }
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
            request.hard_deadline_ms,
            request.required_capabilities.clone(),
        )
        .with_resources(ResourceLimits {
            cpu_millis,
            memory_bytes: m.resources.memory_bytes,
            pids: m.resources.process_limit,
        });
        if create.validate().is_err() {
            return self.refuse(RefusalReason::CapacityChanged);
        }
        let mut state = self.state().clone();
        state.estimated_spend.insert(cost.currency, spend);
        state.last_refusal = None;
        state.attempts.insert(
            id,
            Attempt {
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
            },
        );
        self.journal.persist(state)?;
        Ok(id)
    }
    fn attempt(&self, id: Uuid) -> Result<Attempt> {
        self.journal.check()?;
        self.state()
            .attempts
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown controller attempt"))
    }
    fn save(&mut self, id: Uuid, attempt: Attempt) -> Result<()> {
        let mut state = self.state().clone();
        state.attempts.insert(id, attempt);
        self.journal.persist(state)
    }
    fn admit_effect(&self, a: &Attempt) -> Result<()> {
        ensure!(
            self.registered
                && !self.draining
                && !self.recovery_required
                && a.create.owner.boot_id == self.identity().boot_id
                && a.request.hard_deadline_ms > now_ms()?,
            "controller admission fenced"
        );
        Ok(())
    }
    pub async fn accept_with<T, F, Fut>(&mut self, id: Uuid, accept: F) -> Result<T>
    where
        F: FnOnce(Uuid) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut a = self.attempt(id)?;
        self.admit_effect(&a)?;
        ensure!(a.phase == Phase::Reserved, "acceptance already attempted");
        a.phase = Phase::AcceptIntent;
        self.save(id, a.clone())?;
        let result = accept(a.accept_operation_id).await?;
        a.phase = Phase::Accepted;
        self.save(id, a)?;
        Ok(result)
    }
    pub fn set_draining(&mut self, draining: bool) -> Result<()> {
        self.journal.check()?;
        let mut state = self.state().clone();
        state.last_refusal = draining.then_some(RefusalReason::Draining);
        self.journal.persist(state)?;
        self.draining = draining;
        Ok(())
    }
}
fn now_ms() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
