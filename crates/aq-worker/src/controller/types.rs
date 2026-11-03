//! Controller-owned local state, never an HTTP wire contract, except the
//! [`Inventory`] report.
use crate::backend::{
    Capability, CreateRequest, DiskLimitMode, PrivateMaterial, SandboxIdentity, StoppedWriterProof,
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
    #[serde(default)]
    pub preserve: PreserveConfig,
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
    NodePreservedCapacityReached,
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
    /// SBX22: present once a stopped sandbox failed handoff and kept its disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved: Option<Preservation>,
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
/// Node inventory report. B2 owns mapping this to its published transport.
/// Measured disk fields are optional so older reports still decode and an
/// unmeasured value is absent rather than 0. `available_disk_bytes` remains
/// the configured-capacity admission figure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub identity: WorkerIdentity,
    pub generation: u64,
    pub reserved_sandboxes: usize,
    pub preparing: usize,
    pub available_memory_bytes: u64,
    pub available_cpu_millis: u64,
    pub available_disk_bytes: u64,
    pub refusal: Option<RefusalReason>,
    /// SBX20 enforcement advertised as `disk-limit=watchdog|hard`.
    pub disk_limit: Option<DiskLimitMode>,
    /// SBX20: admitted onto a shared root filesystem by operator override.
    pub shared_root_fs: bool,
    /// statvfs of the sandbox filesystem (docker-local: the daemon GraphRoot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    /// Active plus preserved (stopped, undestroyed) disk reservations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_reserved_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_count: Option<u64>,
    /// Backend-measured size of preserved sandboxes (docker-local: SizeRw
    /// plus work volumes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_bytes: Option<u64>,
    /// boat only: sum of retained snapshots' `contentSizeBytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_snapshot_bytes: Option<u64>,
    /// boat only: saved snapshots beyond the per-sandbox allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_snapshots_over_allowance: Option<u64>,
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
    /// Deliver a non-secret node event (server F4 notification). Ok means
    /// delivered; errors are retried on a later sweep. Must be bounded.
    fn emit(&mut self, _event: &NodeEvent) -> impl Future<Output = Result<()>> + Send {
        async { anyhow::bail!("node event transport unavailable") }
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
    /// SBX20 typed failure: the attempt reached its per-attempt disk limit.
    #[serde(rename = "sandbox_disk_limit_exceeded")]
    SandboxDiskLimitExceeded,
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

pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// Boat expires stopped-sandbox data after 14 days (resume fails with 409
/// `snapshot_data_expired`), so a boat node may preserve for at most 7 days.
pub const BOAT_MAX_PRESERVE_AGE_MS: u64 = 7 * DAY_MS;
/// Upper bound on the handoff retry backoff for a preserved sandbox.
pub const MAX_HANDOFF_BACKOFF_MS: u64 = 5 * 60 * 1000;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreserveExpiryAction {
    /// Keep the sandbox and re-notify at most once per day.
    #[default]
    Notify,
    /// Final notice 24h before, then a journaled standing operator release.
    Destroy,
}
/// SBX22/SBX23 per-node bounds for preserved (stopped, unretained) sandboxes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreserveConfig {
    pub max_count: usize,
    pub max_age_ms: u64,
    #[serde(default)]
    pub expiry_action: PreserveExpiryAction,
    /// Template-declared reconstructible paths, keyed by canonical template
    /// digest (`sha256:<hex>`). Templates without an entry use the defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reconstructible_paths: BTreeMap<String, Vec<String>>,
}
impl Default for PreserveConfig {
    fn default() -> Self {
        Self::vps()
    }
}
impl PreserveConfig {
    pub const DEFAULT_MAX_AGE_MS: u64 = 3 * DAY_MS;
    pub const DEFAULT_RECONSTRUCTIBLE_PATHS: [&str; 3] =
        ["CARGO_TARGET_DIR", "~/.cache", "node_modules"];
    fn with_count(max_count: usize) -> Self {
        Self {
            max_count,
            max_age_ms: Self::DEFAULT_MAX_AGE_MS,
            expiry_action: PreserveExpiryAction::Notify,
            reconstructible_paths: BTreeMap::new(),
        }
    }
    pub fn vps() -> Self {
        Self::with_count(1)
    }
    pub fn desktop() -> Self {
        Self::with_count(4)
    }
    pub fn boat() -> Self {
        Self::with_count(10)
    }
    /// Declared paths only; nothing outside this list is ever deleted.
    pub fn paths_for(&self, template: &crate::backend::TemplateDigest) -> Vec<String> {
        self.reconstructible_paths
            .get(template.as_str())
            .cloned()
            .unwrap_or_else(|| {
                Self::DEFAULT_RECONSTRUCTIBLE_PATHS
                    .iter()
                    .map(|p| (*p).to_owned())
                    .collect()
            })
    }
    pub(super) fn validate(&self, backend: crate::backend::BackendKind) -> Result<()> {
        anyhow::ensure!(
            (1..=4096).contains(&self.max_count)
                && (1..=30 * DAY_MS).contains(&self.max_age_ms)
                && self.reconstructible_paths.values().flatten().all(|p| {
                    !p.trim().is_empty() && p != "/" && !p.split('/').any(|c| c == "..")
                }),
            "invalid preserve bounds"
        );
        anyhow::ensure!(
            backend != crate::backend::BackendKind::Boat
                || self.max_age_ms <= BOAT_MAX_PRESERVE_AGE_MS,
            "boat preserve.max_age must be at most 7 days (boat expires stopped data at 14 days)"
        );
        Ok(())
    }
}
/// Durable SBX22 bookkeeping; the disk reservation stays held meanwhile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preservation {
    pub preserved_at_ms: u64,
    pub expires_at_ms: u64,
    pub bytes_held: u64,
    pub handoff_failures: u32,
    pub next_handoff_ms: u64,
    /// Last delivered `sandbox.preserved` (initial or reminder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notified_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_notice_ms: Option<u64>,
    /// Journaled standing operator release (SBX5) for `expiry_action=destroy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_release: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreserveNotice {
    Preserved,
    Reminder,
    FinalNotice,
}
/// Non-secret node events for server notification (F4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum NodeEvent {
    #[serde(rename = "sandbox.preserved")]
    SandboxPreserved {
        attempt_id: Uuid,
        node: Uuid,
        bytes_held: u64,
        /// Unix milliseconds.
        expires_at: u64,
        notice: PreserveNotice,
    },
}
