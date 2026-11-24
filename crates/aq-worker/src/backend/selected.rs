//! Static dispatch for the operator-selected node placement adapter.
use super::*;
pub enum SelectedBackend {
    Docker(Box<docker_local::DockerLocalBackend>),
    Boat(Box<boat::BoatBackend>),
}
// Expand each call in its concrete branch so the async methods retain static dispatch.
macro_rules! dispatch {
    ($selected:expr, $backend:ident => $operation:expr) => {
        match $selected {
            SelectedBackend::Docker($backend) => $operation,
            SelectedBackend::Boat($backend) => $operation,
        }
    };
}

impl PlacementBackend for SelectedBackend {
    fn kind(&self) -> BackendKind {
        dispatch!(self, backend => backend.kind())
    }
    fn capabilities(&self) -> Vec<Capability> {
        dispatch!(self, backend => backend.capabilities())
    }
    async fn create(&mut self, r: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        dispatch!(self, backend => backend.create(r).await)
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        m: PrivateMaterial,
    ) -> Result<(), BackendError> {
        dispatch!(self, backend => backend.deliver_private_material(s, m).await)
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        dispatch!(self, backend => backend.start_runner(s).await)
    }
    async fn inspect(&mut self, s: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        dispatch!(self, backend => backend.inspect(s).await)
    }
    async fn list_owned(&mut self, node: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        dispatch!(self, backend => backend.list_owned(node).await)
    }
    async fn stop(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        dispatch!(self, backend => backend.stop(s).await)
    }
    async fn record_not_created(&mut self, r: &CreateRequest, at: u64) -> Result<(), BackendError> {
        dispatch!(self, backend => backend.record_not_created(r, at).await)
    }
    async fn archive(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        dispatch!(self, backend => backend.archive(s).await)
    }
    async fn resume(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        dispatch!(self, backend => backend.resume(s).await)
    }
    async fn fork(
        &mut self,
        s: &SandboxIdentity,
        r: &CreateRequest,
    ) -> Result<SandboxIdentity, BackendError> {
        dispatch!(self, backend => backend.fork(s, r).await)
    }
    async fn runner_exit(&mut self, s: &SandboxIdentity) -> Result<RunnerExit, BackendError> {
        dispatch!(self, backend => backend.runner_exit(s).await)
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        dispatch!(self, backend => backend.destroy(s, a).await)
    }
}
