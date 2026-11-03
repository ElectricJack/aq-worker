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
}
/// Frozen local launch input. The caller owns the authenticated native offer and
/// its acceptance observation. A manifest alone is never execution authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    GrantDenied,
    PreparationBacklog,
    Draining,
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
    fn issue(
        &mut self,
        request: &Self::Request,
        sandbox: &SandboxIdentity,
        manifest: &LaunchManifest,
    ) -> impl Future<Output = Result<IssuedMaterial>> + Send;
}
