//! Controller-owned local state, never an HTTP wire contract.
use crate::backend::{
    Capability, CreateRequest, PrivateMaterial, SandboxIdentity, StoppedWriterProof,
};
use anyhow::Result;
use aq_admission_sandbox::{
    sandbox::{DelegatedPermit, Money, Registration, TemplateAdvertisement},
    transport::{Feature, WorkerIdentity},
};
use aq_session_protocol::{ArtifactRef, LaunchManifest};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, future::Future};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityLimits {
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub disk_bytes: u64,
    pub max_preparing: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub runtime: ArtifactRef,
    pub capabilities: Vec<aq_session_protocol::Capability>,
    pub features: Vec<Feature>,
    pub templates: Vec<TemplateAdvertisement>,
    pub max_concurrent_sandboxes: u32,
    pub spend_ceiling: Option<Money>,
    pub capacity: CapacityLimits,
    #[serde(default)]
    pub recovery: RecoveryConfig,
}
/// Frozen local launch input. The caller owns the authenticated native offer and
/// its acceptance observation. A manifest alone is never execution authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub manifest: LaunchManifest,
    pub hard_deadline_ms: u64,
    pub disk_bytes: u64,
    pub required_capabilities: Vec<Capability>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    SandboxCeiling,
    SpendCeiling,
    CapacityChanged,
    DiskReserve,
    EnvironmentUnavailable,
    BackendUnavailable,
    GrantDenied,
    PreparationBacklog,
    Draining,
    PendingRetention,
}
impl std::fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for RefusalReason {}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Abandoned,
    Reserved,
    AcceptIntent,
    Accepted,
    CreateIntent,
    Created,
    IssueIntent,
    MaterialIntent,
    MaterialDelivered,
    StartIntent,
    Running,
    StopIntent,
    Stopped,
    DestroyIntent,
    Destroyed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub request: LaunchRequest,
    pub create: CreateRequest,
    pub accept_operation_id: Uuid,
    pub phase: Phase,
    pub sandbox: Option<SandboxIdentity>,
    pub stopped: Option<StoppedWriterProof>,
    pub permit_id: Option<Uuid>,
    pub logical_sandbox_id: Option<Uuid>,
    pub channel_id: Option<Uuid>,
    pub destroy_authorization: Option<crate::backend::DestroyAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<GrantMonitor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<Cleanup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<CancellationObservation>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub format: u32,
    pub identity: WorkerIdentity,
    pub revision: u64,
    pub registration_operation_id: Option<Uuid>,
    pub attempts: BTreeMap<Uuid, Attempt>,
    /// Conservative cumulative admission estimates, never refunded on stop.
    pub estimated_spend: BTreeMap<String, u64>,
    pub last_refusal: Option<RefusalReason>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub orphans: BTreeMap<String, Orphan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readoptions: Vec<aq_admission_sandbox::sandbox::ReadoptionRequest>,
}
/// In-process inventory. B2 owns mapping this to its published transport.
#[derive(Clone, Debug)]
pub struct Inventory {
    pub identity: WorkerIdentity,
    pub generation: u64,
    pub reserved_sandboxes: usize,
    pub preparing: usize,
    pub available_memory_bytes: u64,
    pub available_cpu_millis: u64,
    pub available_disk_bytes: u64,
    pub refusal: Option<RefusalReason>,
}
/// Authenticated provider result. Payload packaging is the provider/runner's
/// responsibility, not a new controller-authored wire envelope. No Debug/Serde.
pub struct IssuedMaterial {
    pub permit: DelegatedPermit,
    pub payload: PrivateMaterial,
}
/// B2 installs the authenticated registration and issuance transport here.
/// Requests are opaque to the controller and are never serialized into its
/// journal. Providers MUST bind issuance to the supplied backend identity and
/// exact manifest, and package the same permit/manifest for the fixed runner.
pub trait PermitProvider: Send {
    type Request: Send + Sync;
    fn register(&mut self, registration: &Registration) -> impl Future<Output = Result<()>> + Send;
    /// Authenticated B1 re-adoption; the provider validates against current server
    /// permits. No local response or fresh grant may be synthesized.
    fn readopt(
        &mut self,
        _request: &aq_admission_sandbox::sandbox::ReadoptionRequest,
    ) -> impl Future<Output = Result<aq_admission_sandbox::sandbox::ReadoptionResponse>> + Send
    {
        async { anyhow::bail!("re-adoption provider unavailable") }
    }
    /// Published node poll; None means no page, never absence of cancellation.
    fn poll(
        &mut self,
        _identity: &WorkerIdentity,
    ) -> impl Future<Output = Result<Option<aq_admission_sandbox::transport::PollPage>>> + Send
    {
        async { Ok(None) }
    }
    /// Try runner extraction of workspace AND evidence and central handoff.
    /// Return an authenticated retention receipt only after both are retained;
    /// None/error keeps disk and is retried. Implementations must be bounded.
    fn retain(
        &mut self,
        _sandbox: &SandboxIdentity,
    ) -> impl Future<Output = Result<Option<crate::backend::DestroyAuthorization>>> + Send {
        async { Ok(None) }
    }
    fn issue(
        &mut self,
        request: &Self::Request,
        sandbox: &SandboxIdentity,
        manifest: &LaunchManifest,
    ) -> impl Future<Output = Result<IssuedMaterial>> + Send;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConfig {
    pub orphan_grace_ms: u64,
    pub cancellation_grace_ms: u64,
    pub sweep_interval_ms: u64,
    pub deadline_margin_ms: u64,
    pub max_pending_results: usize,
}
impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            orphan_grace_ms: 30_000,
            cancellation_grace_ms: 5_000,
            sweep_interval_ms: 1_000,
            deadline_margin_ms: 5_000,
            max_pending_results: 128,
        }
    }
}
impl RecoveryConfig {
    pub(super) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=300_000).contains(&self.orphan_grace_ms)
                && (1..=60_000).contains(&self.cancellation_grace_ms)
                && (1..=30_000).contains(&self.sweep_interval_ms)
                && self.deadline_margin_ms <= 30_000
                && (1..=4096).contains(&self.max_pending_results),
            "invalid recovery bounds"
        );
        Ok(())
    }
}
/// Non-secret monitoring facts. Never independently authorizes execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantMonitor {
    pub boot_id: Uuid,
    pub permit_id: Uuid,
    pub sandbox_id: Uuid,
    pub channel_id: Uuid,
    pub generation: u64,
    pub renewal_deadline_ms: u64,
    pub execution: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanupReason {
    UnknownSandbox,
    NoCurrentGrant,
    HardDeadline,
    Cancellation,
    Stopped,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cleanup {
    pub reason: CleanupReason,
    pub destroy_after_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancellationObservation {
    pub cancellation_id: Uuid,
    pub authority_revision: u64,
    pub stop_after_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Orphan {
    pub entry: crate::backend::InventoryEntry,
    pub cleanup: Cleanup,
    pub stopped: Option<StoppedWriterProof>,
    pub authorization: Option<crate::backend::DestroyAuthorization>,
    pub destroyed: bool,
}
