//! Static dispatch for the operator-selected node placement adapter.
use super::*;
pub enum SelectedBackend {
    Docker(Box<docker_local::DockerLocalBackend>),
    Boat(Box<boat::BoatBackend>),
}
impl PlacementBackend for SelectedBackend {
    fn kind(&self) -> BackendKind {
        match self {
            Self::Docker(b) => b.kind(),
            Self::Boat(b) => b.kind(),
        }
    }
    fn capabilities(&self) -> Vec<Capability> {
        match self {
            Self::Docker(b) => b.capabilities(),
            Self::Boat(b) => b.capabilities(),
        }
    }
    async fn create(&mut self, r: &CreateRequest) -> Result<SandboxIdentity, BackendError> {
        match self {
            Self::Docker(b) => b.create(r).await,
            Self::Boat(b) => b.create(r).await,
        }
    }
    async fn deliver_private_material(
        &mut self,
        s: &SandboxIdentity,
        m: PrivateMaterial,
    ) -> Result<(), BackendError> {
        match self {
            Self::Docker(b) => b.deliver_private_material(s, m).await,
            Self::Boat(b) => b.deliver_private_material(s, m).await,
        }
    }
    async fn start_runner(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        match self {
            Self::Docker(b) => b.start_runner(s).await,
            Self::Boat(b) => b.start_runner(s).await,
        }
    }
    async fn inspect(&mut self, s: &SandboxIdentity) -> Result<InventoryEntry, BackendError> {
        match self {
            Self::Docker(b) => b.inspect(s).await,
            Self::Boat(b) => b.inspect(s).await,
        }
    }
    async fn list_owned(&mut self, node: Uuid) -> Result<Vec<InventoryEntry>, BackendError> {
        match self {
            Self::Docker(b) => b.list_owned(node).await,
            Self::Boat(b) => b.list_owned(node).await,
        }
    }
    async fn stop(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        match self {
            Self::Docker(b) => b.stop(s).await,
            Self::Boat(b) => b.stop(s).await,
        }
    }
    async fn record_not_created(&mut self, r: &CreateRequest, at: u64) -> Result<(), BackendError> {
        match self {
            Self::Docker(b) => b.record_not_created(r, at).await,
            Self::Boat(b) => b.record_not_created(r, at).await,
        }
    }
    async fn archive(&mut self, s: &SandboxIdentity) -> Confirmation<StoppedWriterProof> {
        match self {
            Self::Docker(b) => b.archive(s).await,
            Self::Boat(b) => b.archive(s).await,
        }
    }
    async fn resume(&mut self, s: &SandboxIdentity) -> Result<(), BackendError> {
        match self {
            Self::Docker(b) => b.resume(s).await,
            Self::Boat(b) => b.resume(s).await,
        }
    }
    async fn fork(
        &mut self,
        s: &SandboxIdentity,
        r: &CreateRequest,
    ) -> Result<SandboxIdentity, BackendError> {
        match self {
            Self::Docker(b) => b.fork(s, r).await,
            Self::Boat(b) => b.fork(s, r).await,
        }
    }
    async fn runner_exit(&mut self, s: &SandboxIdentity) -> Result<RunnerExit, BackendError> {
        match self {
            Self::Docker(b) => b.runner_exit(s).await,
            Self::Boat(b) => b.runner_exit(s).await,
        }
    }
    async fn destroy(
        &mut self,
        s: &SandboxIdentity,
        a: &DestroyAuthorization,
    ) -> Confirmation<StoppedWriterProof> {
        match self {
            Self::Docker(b) => b.destroy(s, a).await,
            Self::Boat(b) => b.destroy(s, a).await,
        }
    }
}
