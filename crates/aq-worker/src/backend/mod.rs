//! Placement adapters and pure lifecycle policy. No controller persistence or
//! execution authority lives here. Backend confirmations cover only writers
//! inside the named sandbox; external writers need their own proof.
mod types;
pub use types::*;
mod lifecycle;
pub use lifecycle::*;
mod fake;
pub use fake::{FakeBackend, Fault};
pub mod boat_size;
#[cfg(feature = "backend-conformance")]
pub mod conformance;
pub mod docker_local;

use std::future::Future;
use uuid::Uuid;

/// Implementations must scope every operation to the full sandbox identity,
/// preserve disk on stop/archive, and never treat failed/partial API calls as
/// confirmation. Futures are Send for use by a concurrent node controller.
/// Real adapters must impose bounded control-channel timeouts themselves.
pub trait PlacementBackend: Send {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Vec<Capability>;
    /// Idempotent only for an identical request. Label node, boot and attempt
    /// where supported; otherwise use a dedicated scope and deterministic name.
    /// Inventory must discover successful creates whose response was lost.
    fn create(
        &mut self,
        request: &CreateRequest,
    ) -> impl Future<Output = Result<SandboxIdentity, BackendError>> + Send;
    fn deliver_private_material(
        &mut self,
        sandbox: &SandboxIdentity,
        material: PrivateMaterial,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
    /// Starts the fixed runner from the selected template, without secret argv/env.
    fn start_runner(
        &mut self,
        sandbox: &SandboxIdentity,
    ) -> impl Future<Output = Result<(), BackendError>> + Send;
    fn inspect(
        &mut self,
        sandbox: &SandboxIdentity,
    ) -> impl Future<Output = Result<InventoryEntry, BackendError>> + Send;
    /// Includes old boots and unrecorded creates, but never another node's scope.
    /// Partial/failed inventory is Err, never an apparently complete empty list.
    fn list_owned(
        &mut self,
        node_id: Uuid,
    ) -> impl Future<Output = Result<Vec<InventoryEntry>, BackendError>> + Send;
    fn stop(
        &mut self,
        sandbox: &SandboxIdentity,
    ) -> impl Future<Output = Confirmation<StoppedWriterProof>> + Send;
    fn archive(
        &mut self,
        _sandbox: &SandboxIdentity,
    ) -> impl Future<Output = Confirmation<StoppedWriterProof>> + Send {
        async { Confirmation::Unconfirmed(UnconfirmedReason::Unsupported) }
    }
    /// SBX23: through a reachable runner, delete exactly the given
    /// template-declared reconstructible paths before a preserving stop. Never
    /// deletes anything undeclared. Err (incl. unsupported) means unreachable;
    /// the controller then stops without deleting.
    fn drop_reconstructible(
        &mut self,
        _sandbox: &SandboxIdentity,
        _paths: &[String],
    ) -> impl Future<Output = Result<(), BackendError>> + Send {
        async { Err(BackendError::UnsupportedOperation) }
    }
    /// Optional resume/fork must scrub old credentials and require fresh material
    /// before starting any runner. They do not grant execution authority.
    fn resume(
        &mut self,
        _sandbox: &SandboxIdentity,
    ) -> impl Future<Output = Result<(), BackendError>> + Send {
        async { Err(BackendError::UnsupportedOperation) }
    }
    fn fork(
        &mut self,
        _sandbox: &SandboxIdentity,
        _request: &CreateRequest,
    ) -> impl Future<Output = Result<SandboxIdentity, BackendError>> + Send {
        async { Err(BackendError::UnsupportedOperation) }
    }
    /// SBX20: how this backend bounds per-attempt disk. Default: not at all.
    fn disk_limit(&self) -> DiskLimitStatus {
        DiskLimitStatus::default()
    }
    /// SBX20 watchdog sample of running sandboxes owned by `node_id`. A
    /// partial sample is Err, never a silently shorter list.
    fn disk_usage(
        &mut self,
        _node_id: Uuid,
    ) -> impl Future<Output = Result<Vec<DiskUsage>, BackendError>> + Send {
        async { Ok(Vec::new()) }
    }
    /// Verify authorization scope, stop writers, then delete. Only confirmed
    /// deletion yields Destroyed proof; failure leaves resources protected.
    fn destroy(
        &mut self,
        sandbox: &SandboxIdentity,
        authorization: &DestroyAuthorization,
    ) -> impl Future<Output = Confirmation<StoppedWriterProof>> + Send;
}

pub mod boat;
mod selected;
pub use selected::SelectedBackend;
