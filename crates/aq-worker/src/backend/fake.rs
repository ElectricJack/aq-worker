//! Deterministic in-memory model, not a container or isolation implementation.
use super::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    CrashAfterCreate,
    DestroyFailure,
    StopFailure,
    UnknownState,
    UnknownSandbox,
    ForeignSandbox,
    DestroyedOnInspect,
    DestroyedOnStop,
    /// The runner cannot be reached to drop reconstructible paths.
    RunnerUnreachable,
}

struct Sandbox {
    request: CreateRequest,
    inventory: InventoryEntry,
    material: Option<PrivateMaterial>,
    fixture_output: Option<Vec<u8>>,
    exit: Option<RunnerExit>,
    /// Modelled workspace paths (no real files); SBX23 deletes declared ones.
    paths: BTreeSet<String>,
}

#[derive(Default)]
pub struct FakeBackend {
    sandboxes: BTreeMap<String, Sandbox>,
    faults: VecDeque<Fault>,
    disk_used: BTreeMap<String, u64>,
    measured_disk: BackendDiskUsage,
    disk_queries: Vec<Vec<SandboxIdentity>>,
    runner_exit_on_start: Option<RunnerExit>,
}
impl FakeBackend {
    /// Reported SBX20 watchdog usage for one sandbox; no bytes are written.
    pub fn set_disk_usage(&mut self, identity: &SandboxIdentity, used_bytes: u64) {
        self.disk_used
            .insert(identity.backend_id.clone(), used_bytes);
    }
    /// Fake node measurements returned by every later `measured_disk_usage`.
    pub fn set_measured_disk_usage(&mut self, usage: BackendDiskUsage) {
        self.measured_disk = usage;
    }
    /// The preserved sets the controller asked to size, in call order.
    pub fn disk_queries(&self) -> &[Vec<SandboxIdentity>] {
        &self.disk_queries
    }
    /// Later started runners end at once with this exit (default: keep running).
    pub fn exit_runners_on_start(&mut self, exit: RunnerExit) {
        self.runner_exit_on_start = Some(exit);
    }
    /// The private material delivered to a sandbox, if any.
    pub fn delivered_material(&self, identity: &SandboxIdentity) -> Option<&[u8]> {
        self.get(identity)
            .ok()?
            .material
            .as_ref()
            .map(PrivateMaterial::expose_for_control_channel)
    }
    /// A one-shot fault is consumed only by its corresponding operation.
    pub fn inject(&mut self, fault: Fault) {
        self.faults.push_back(fault);
    }
    fn take_fault(&mut self, fault: Fault) -> bool {
        if let Some(index) = self.faults.iter().position(|pending| *pending == fault) {
            self.faults.remove(index);
            true
        } else {
            false
        }
    }
    fn get(&self, identity: &SandboxIdentity) -> Result<&Sandbox, BackendError> {
        self.sandboxes
            .get(&identity.backend_id)
            .filter(|sandbox| sandbox.inventory.sandbox == *identity)
            .ok_or(BackendError::NotFound)
    }
    fn get_mut(&mut self, identity: &SandboxIdentity) -> Result<&mut Sandbox, BackendError> {
        self.sandboxes
            .get_mut(&identity.backend_id)
            .filter(|sandbox| sandbox.inventory.sandbox == *identity)
            .ok_or(BackendError::NotFound)
    }
    /// Observable fixture artifact; no private material is copied into it.
    pub fn fixture_output(&self, identity: &SandboxIdentity) -> Result<&[u8], BackendError> {
        self.get(identity)?
            .fixture_output
            .as_deref()
            .ok_or(BackendError::InvalidState)
    }
    /// Test hook: model paths present inside the sandbox.
    pub fn seed_paths<'a>(
        &mut self,
        identity: &SandboxIdentity,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), BackendError> {
        self.get_mut(identity)?
            .paths
            .extend(paths.into_iter().map(str::to_owned));
        Ok(())
    }
    pub fn paths(&self, identity: &SandboxIdentity) -> Result<&BTreeSet<String>, BackendError> {
        Ok(&self.get(identity)?.paths)
    }
    fn insert(&mut self, request: &CreateRequest) -> SandboxIdentity {
        let identity = SandboxIdentity {
            kind: self.kind(),
            backend_id: request.name.clone(),
            owner: request.owner.clone(),
        };
        self.sandboxes.insert(
            identity.backend_id.clone(),
            Sandbox {
                request: request.clone(),
                inventory: InventoryEntry {
                    sandbox: identity.clone(),
                    name: request.name.clone(),
                    template_digest: request.template_digest.clone(),
                    hard_deadline_ms: request.hard_deadline_ms,
                    state: SandboxState::Created,
                    running_processes: Some(0),
                    disk_present: true,
                },
                material: None,
                fixture_output: None,
                exit: None,
                paths: BTreeSet::new(),
            },
        );
        identity
    }
    fn inject_inventory(&mut self, node_id: Uuid, foreign: bool) {
        let owner = Ownership {
            node_id: if foreign {
                Uuid::from_u128(node_id.as_u128() ^ u128::MAX)
            } else {
                node_id
            },
            boot_id: Uuid::from_u128(101),
            attempt_id: Uuid::from_u128(if foreign { 102 } else { 103 }),
        };
        let request = CreateRequest::new(
            owner,
            TemplateDigest::parse(format!("sha256:{}", "f".repeat(64))).expect("fixed digest"),
            100,
            vec![],
        );
        if !self.sandboxes.contains_key(&request.name) {
            self.insert(&request);
        }
    }
}
impl PlacementBackend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::DockerLocal
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::PersistentDiskDuringAttempt]
    }
    fn disk_limit(&self) -> DiskLimitStatus {
        DiskLimitStatus {
            mode: Some(DiskLimitMode::Watchdog),
            shared_root_fs: false,
            watch_interval_ms: 10_000,
        }
    }
    async fn disk_usage(&mut self, node_id: Uuid) -> Result<Vec<DiskUsage>, BackendError> {
        Ok(self
            .sandboxes
            .values()
            .filter(|s| {
                s.inventory.sandbox.owner.node_id == node_id
                    && s.inventory.state == SandboxState::Running
            })
            .filter_map(|s| {
                Some(DiskUsage {
                    sandbox: s.inventory.sandbox.clone(),
                    used_bytes: self.disk_used.get(&s.request.name).copied().unwrap_or(0),
                    limit_bytes: s.request.resources?.disk_bytes?,
                })
            })
            .collect())
    }
    async fn create(&mut self, request: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        request.validate()?;
        for capability in &request.required_capabilities {
            if !self.capabilities().contains(capability) {
                return Err(BackendError::UnsupportedCapability(*capability));
            }
        }
        if let Some(existing) = self.sandboxes.get(&request.name) {
            return if existing.request == *request
                && existing.inventory.state != SandboxState::Destroyed
            {
                Ok(existing.inventory.sandbox.clone())
            } else {
                Err(BackendError::Conflict)
            };
        }
        let identity = self.insert(request);
        if self.take_fault(Fault::CrashAfterCreate) {
            return Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure));
        }
        Ok(identity)
    }
    async fn deliver_private_material(
        &mut self,
        identity: &SandboxIdentity,
        material: PrivateMaterial,
    ) -> Result<(), BackendError> {
        let sandbox = self.get_mut(identity)?;
        if sandbox.inventory.state == SandboxState::Destroyed
            || material.expose_for_control_channel().is_empty()
        {
            return Err(BackendError::InvalidState);
        }
        sandbox.material = Some(material);
        Ok(())
    }
    async fn start_runner(&mut self, identity: &SandboxIdentity) -> Result<(), BackendError> {
        let sandbox = self.get_mut(identity)?;
        if sandbox.material.is_none()
            || !matches!(
                sandbox.inventory.state,
                SandboxState::Created | SandboxState::Running
            )
        {
            return Err(BackendError::InvalidState);
        }
        sandbox.inventory.state = SandboxState::Running;
        sandbox.inventory.running_processes = Some(1);
        sandbox.fixture_output = Some(b"fixture-complete\n".to_vec());
        if let Some(exit) = self.runner_exit_on_start {
            let sandbox = self.get_mut(identity)?;
            sandbox.inventory.state = SandboxState::Stopped;
            sandbox.inventory.running_processes = Some(0);
            sandbox.exit = Some(exit);
        }
        Ok(())
    }
    async fn inspect(
        &mut self,
        identity: &SandboxIdentity,
    ) -> Result<InventoryEntry, BackendError> {
        let mut inventory = self.get(identity)?.inventory.clone();
        if self.take_fault(Fault::DestroyedOnInspect) {
            inventory.state = SandboxState::Destroyed;
            inventory.running_processes = Some(0);
            inventory.disk_present = false;
        }
        if self.take_fault(Fault::UnknownState) {
            inventory.state = SandboxState::Unknown;
            inventory.running_processes = None;
        }
        Ok(inventory)
    }
    async fn runner_exit(
        &mut self,
        identity: &SandboxIdentity,
    ) -> Result<RunnerExit, BackendError> {
        let sandbox = self.get(identity)?;
        match (sandbox.inventory.state, sandbox.exit) {
            (SandboxState::Stopped, Some(exit)) => Ok(exit),
            _ => Err(BackendError::InvalidState),
        }
    }
    async fn list_owned(&mut self, node_id: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        if self.take_fault(Fault::UnknownSandbox) {
            self.inject_inventory(node_id, false);
        }
        if self.take_fault(Fault::ForeignSandbox) {
            self.inject_inventory(node_id, true);
        }
        Ok(self
            .sandboxes
            .values()
            .filter(|sandbox| {
                sandbox.inventory.sandbox.owner.node_id == node_id
                    && sandbox.inventory.state != SandboxState::Destroyed
            })
            .map(|sandbox| sandbox.inventory.clone())
            .collect())
    }
    async fn stop(&mut self, identity: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        if self.take_fault(Fault::DestroyedOnStop) {
            return Confirmation::Confirmed(StoppedWriterProof::destroyed(identity.clone()));
        }
        if self.take_fault(Fault::StopFailure) {
            return Confirmation::Unconfirmed(UnconfirmedReason::BackendFailure);
        }
        let Ok(sandbox) = self.get_mut(identity) else {
            return Confirmation::Unconfirmed(UnconfirmedReason::NotFound);
        };
        if sandbox.inventory.state != SandboxState::Destroyed {
            sandbox.inventory.state = SandboxState::Stopped;
            sandbox.inventory.running_processes = Some(0);
        }
        sandbox.inventory.writer_proof()
    }
    async fn drop_reconstructible(
        &mut self,
        identity: &SandboxIdentity,
        paths: &[String],
    ) -> Result<(), BackendError> {
        if self.take_fault(Fault::RunnerUnreachable) {
            return Err(BackendError::Uncertain(UnconfirmedReason::BackendFailure));
        }
        let sandbox = self.get_mut(identity)?;
        if sandbox.inventory.state != SandboxState::Running {
            return Err(BackendError::InvalidState);
        }
        for path in paths {
            sandbox.paths.remove(path);
        }
        Ok(())
    }
    async fn destroy(
        &mut self,
        identity: &SandboxIdentity,
        authorization: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        if !authorization.authorizes(identity) {
            return Confirmation::Unconfirmed(UnconfirmedReason::Unauthorized);
        }
        if self.take_fault(Fault::DestroyFailure) {
            return Confirmation::Unconfirmed(UnconfirmedReason::BackendFailure);
        }
        let Ok(sandbox) = self.get_mut(identity) else {
            return Confirmation::Unconfirmed(UnconfirmedReason::NotFound);
        };
        if !sandbox.inventory.writer_proof().is_confirmed() {
            return Confirmation::Unconfirmed(UnconfirmedReason::StillRunning);
        }
        sandbox.inventory.state = SandboxState::Destroyed;
        sandbox.inventory.running_processes = Some(0);
        sandbox.inventory.disk_present = false;
        sandbox.material = None;
        sandbox.fixture_output = None;
        sandbox.inventory.writer_proof()
    }
    async fn measured_disk_usage(&mut self, preserved: &[SandboxIdentity]) -> BackendDiskUsage {
        self.disk_queries.push(preserved.to_vec());
        self.measured_disk.clone()
    }
}
