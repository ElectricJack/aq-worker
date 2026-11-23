//! Durable node controller. All agent execution remains inside the backend.
pub mod allowlist;
mod inventory;
mod journal;
mod reservation;
mod service;
pub use service::{ProviderUnavailable, RunConfig, UnavailableProvider};
mod lifecycle;
pub use lifecycle::RetainedDestroy;
mod recovery;
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
    clock: Box<dyn Fn() -> Result<u64> + Send + Sync>,
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
        config.recovery.validate()?;
        config.preserve.validate(backend.kind())?;
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
            clock: Box::new(now_ms),
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
    /// Preserved sandboxes still holding disk (SBX22 `preserve.max_count`).
    pub fn preserved_count(&self) -> usize {
        self.state()
            .attempts
            .values()
            .filter(|a| a.preserved.is_some() && a.phase != Phase::Destroyed)
            .count()
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
                && !self.has_orphans()
                && a.cancellation.is_none()
                && a.cleanup.is_none()
                && a.create.owner.boot_id == self.identity().boot_id
                && a.request.hard_deadline_ms > (self.clock)()?,
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
        self.admit_effect(&a)?;
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
mod preserve_tests;
#[cfg(test)]
mod recovery_ports;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
