//! Shared contracts for placed sandboxes, their resources, and retention evidence.
use aq_lifecycle_protocol::{ArtifactRole, Binding, RetentionReceipt, validate_retention};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    DockerLocal,
    Boat,
    CloudflareContainers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    VmIsolation,
    NestedDocker,
    SuspendSnapshot,
    PersistentDiskDuringAttempt,
}

/// Immutable OCI digest, exactly as selected by the launch manifest. Never a tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TemplateDigest(String);
impl TemplateDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self, BackendError> {
        let value = value.into();
        let valid = value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        if !valid {
            return Err(BackendError::InvalidRequest(
                "expected canonical sha256 digest",
            ));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for TemplateDigest {
    type Error = BackendError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<TemplateDigest> for String {
    fn from(value: TemplateDigest) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Ownership {
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub attempt_id: Uuid,
}
impl Ownership {
    fn has_ids(&self) -> bool {
        ![self.node_id, self.boot_id, self.attempt_id].contains(&Uuid::nil())
    }
    /// Boot is a label, not part of the name: a new boot must discover old work.
    pub fn deterministic_name(&self) -> String {
        format!("aq-{}-{}", self.node_id.simple(), self.attempt_id.simple())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRequest {
    pub owner: Ownership,
    pub name: String,
    pub template_digest: TemplateDigest,
    pub hard_deadline_ms: u64,
    pub required_capabilities: Vec<Capability>,
    /// Reservation limits. Backends that enforce limits refuse a request without them.
    #[serde(default)]
    pub resources: Option<ResourceLimits>,
    /// Names of node-local mount aliases selected by the assignment, never host paths.
    #[serde(default)]
    pub mount_aliases: Vec<String>,
    /// Mount the attempt's agent-channel directory at `/aq/channel` (0006
    /// §5.1). The source is fixed by the node, never named by the request.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_channel: bool,
    /// A disposable task-local PostgreSQL cluster in DockerLocal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub postgres: bool,
}
impl CreateRequest {
    pub fn new(
        owner: Ownership,
        template_digest: TemplateDigest,
        hard_deadline_ms: u64,
        required_capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            name: owner.deterministic_name(),
            owner,
            template_digest,
            hard_deadline_ms,
            required_capabilities,
            resources: None,
            mount_aliases: Vec::new(),
            agent_channel: false,
            postgres: false,
        }
    }
    pub fn with_agent_channel(mut self) -> Self {
        self.agent_channel = true;
        self
    }
    pub fn with_resources(mut self, resources: ResourceLimits) -> Self {
        self.resources = Some(resources);
        self
    }
    pub fn with_mount_aliases(mut self, aliases: Vec<String>) -> Self {
        self.mount_aliases = aliases;
        self
    }
    pub fn validate(&self) -> Result<(), BackendError> {
        let valid_owner = self.owner.has_ids() && self.name == self.owner.deterministic_name();
        if !valid_owner || self.hard_deadline_ms == 0 {
            return Err(BackendError::InvalidRequest(
                "invalid identity, name or deadline",
            ));
        }
        if let Some(resources) = &self.resources {
            resources.validate()?;
        }
        if !self
            .mount_aliases
            .iter()
            .all(|alias| valid_alias_name(alias))
        {
            return Err(BackendError::InvalidRequest("invalid mount alias name"));
        }
        Ok(())
    }
}

/// Smallest memory limit Docker-compatible runtimes accept.
pub const MIN_MEMORY_BYTES: u64 = 6 * 1024 * 1024;

/// CPU, memory, PID and disk limits copied from the node reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub pids: u32,
    /// SBX20 reservation disk bytes. Omitted when absent so journaled
    /// requests from before SBX20 keep their request hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}
impl ResourceLimits {
    pub fn validate(&self) -> Result<(), BackendError> {
        let has_compute = self.cpu_millis > 0 && self.pids > 0;
        let has_memory_and_disk =
            self.memory_bytes >= MIN_MEMORY_BYTES && self.disk_bytes != Some(0);
        if !has_compute || !has_memory_and_disk {
            return Err(BackendError::InvalidRequest("invalid reservation limits"));
        }
        Ok(())
    }
}

/// SBX20 enforcement a backend provides for the per-attempt disk limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskLimitMode {
    /// Sampled usage; breaching attempts are stopped and preserved.
    Watchdog,
    /// Storage-driver quota in addition to the always-on watchdog.
    Hard,
}
impl DiskLimitMode {
    /// Advertised capability label, e.g. `disk-limit=watchdog`.
    pub fn capability(self) -> &'static str {
        match self {
            Self::Watchdog => "disk-limit=watchdog",
            Self::Hard => "disk-limit=hard",
        }
    }
}

/// What a backend enforces for disk, reported in node inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskLimitStatus {
    /// `None`: the backend does not bound disk and reports no usage.
    pub mode: Option<DiskLimitMode>,
    /// Operator override: sandbox storage shares the root filesystem.
    pub shared_root_fs: bool,
    pub watch_interval_ms: u64,
}

/// One sampled running sandbox: writable layer plus its work volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskUsage {
    pub sandbox: SandboxIdentity,
    pub used_bytes: u64,
    pub limit_bytes: u64,
}
impl DiskUsage {
    pub fn exceeded(&self) -> bool {
        self.used_bytes >= self.limit_bytes
    }
}

/// `[a-z0-9][a-z0-9-]{0,62}`: an alias name, never a path.
pub fn valid_alias_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Harness filesystem confinement mode (0005 SBX19). `container` means the
/// harness's own sandbox is off and the container is the only boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfinementMode {
    Native,
    Container,
}

/// Opaque runner-only payload. No Serialize, Clone, argv or environment surface.
/// Adapters must use their authenticated control channel and runner-only storage.
pub struct PrivateMaterial(Zeroizing<Vec<u8>>);
impl PrivateMaterial {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }
    pub fn expose_for_control_channel(&self) -> &[u8] {
        &self.0
    }
}
impl fmt::Debug for PrivateMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateMaterial([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SandboxIdentity {
    pub kind: BackendKind,
    pub backend_id: String,
    pub owner: Ownership,
}

/// How a stopped runner process ended, as the backend observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerExit {
    Exited(i32),
    /// Killed by the runtime (e.g. the OOM killer); no trustworthy exit code.
    Signalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxState {
    Created,
    Running,
    Stopped,
    Archived,
    Destroyed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryEntry {
    pub sandbox: SandboxIdentity,
    pub name: String,
    pub template_digest: TemplateDigest,
    pub hard_deadline_ms: u64,
    pub state: SandboxState,
    /// None means unknown, not zero. Includes *all* processes in the sandbox.
    pub running_processes: Option<u64>,
    pub disk_present: bool,
}
impl InventoryEntry {
    /// Inventory states must be authoritative backend observations. Absence from
    /// a list, an HTTP timeout or a missing local mapping is never Destroyed.
    pub fn writer_proof(&self) -> Confirmation<StoppedWriterProof> {
        match self.state {
            SandboxState::Destroyed if !self.disk_present && self.running_processes == Some(0) => {
                Confirmation::Confirmed(StoppedWriterProof::destroyed(self.sandbox.clone()))
            }
            SandboxState::Stopped | SandboxState::Archived if self.running_processes == Some(0) => {
                Confirmation::Confirmed(StoppedWriterProof::stopped(self.sandbox.clone()))
            }
            _ => Confirmation::Unconfirmed(UnconfirmedReason::UnknownState),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnconfirmedReason {
    Timeout,
    BackendFailure,
    UnknownState,
    NotFound,
    StillRunning,
    Unauthorized,
    Unsupported,
}

/// Consumers must retain reservations on every Unconfirmed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confirmation<T> {
    Confirmed(T),
    Unconfirmed(UnconfirmedReason),
}
impl<T> Confirmation<T> {
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriterProofKind {
    StoppedNoProcesses,
    Destroyed,
}

/// Constructed only after the backend confirms the named fact. These are
/// trusted adapter attestations, not cryptographic evidence or grants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedWriterProof {
    sandbox: SandboxIdentity,
    kind: WriterProofKind,
}
impl StoppedWriterProof {
    pub fn stopped(sandbox: SandboxIdentity) -> Self {
        Self {
            sandbox,
            kind: WriterProofKind::StoppedNoProcesses,
        }
    }
    pub fn destroyed(sandbox: SandboxIdentity) -> Self {
        Self {
            sandbox,
            kind: WriterProofKind::Destroyed,
        }
    }
    pub fn sandbox(&self) -> &SandboxIdentity {
        &self.sandbox
    }
    pub fn kind(&self) -> WriterProofKind {
        self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionEvidence {
    CentralReceipt(String),
    OperatorRelease(String),
    /// CON-03 central receipts already checked by `validate_retention`
    /// against the attempt binding, including at least one source receipt.
    Lifecycle(Vec<RetentionReceipt>),
}

/// The controller validates retention/operator authority before constructing
/// this value. References must be non-secret, durable receipt/release IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestroyAuthorization {
    sandbox: SandboxIdentity,
    evidence: RetentionEvidence,
}
impl DestroyAuthorization {
    pub fn retained(
        sandbox: SandboxIdentity,
        receipt: impl Into<String>,
    ) -> Result<Self, BackendError> {
        Self::new(sandbox, RetentionEvidence::CentralReceipt(receipt.into()))
    }
    pub fn operator_released(
        sandbox: SandboxIdentity,
        release: impl Into<String>,
    ) -> Result<Self, BackendError> {
        Self::new(sandbox, RetentionEvidence::OperatorRelease(release.into()))
    }
    /// Typed central retention: every receipt must bind `binding` (the
    /// sandbox owner's attempt) and one must retain the source.
    pub fn retained_lifecycle(
        sandbox: SandboxIdentity,
        binding: &Binding,
        receipts: Vec<RetentionReceipt>,
    ) -> Result<Self, BackendError> {
        if binding.attempt_id != sandbox.owner.attempt_id
            || validate_retention(&receipts, binding).is_err()
        {
            return Err(BackendError::InvalidRequest(
                "retention receipts do not bind this attempt",
            ));
        }
        Self::new(sandbox, RetentionEvidence::Lifecycle(receipts))
    }
    /// Receipts carried by [`Self::retained_lifecycle`]; empty otherwise.
    pub fn lifecycle_receipts(&self) -> &[RetentionReceipt] {
        match &self.evidence {
            RetentionEvidence::Lifecycle(receipts) => receipts,
            _ => &[],
        }
    }
    fn new(sandbox: SandboxIdentity, evidence: RetentionEvidence) -> Result<Self, BackendError> {
        let value = Self { sandbox, evidence };
        if !value.authorizes(&value.sandbox) {
            return Err(BackendError::InvalidRequest("empty retention evidence"));
        }
        Ok(value)
    }
    pub fn authorizes(&self, sandbox: &SandboxIdentity) -> bool {
        let evidence = match &self.evidence {
            RetentionEvidence::CentralReceipt(reference)
            | RetentionEvidence::OperatorRelease(reference) => !reference.trim().is_empty(),
            RetentionEvidence::Lifecycle(receipts) => {
                receipts.iter().any(|r| r.role == ArtifactRole::Source)
            }
        };
        &self.sandbox == sandbox && evidence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendUnavailable {
    Api,
    RateLimited,
    Capacity,
    Transport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// Temporary remote outage/capacity refusal; intent may already exist.
    Unavailable(BackendUnavailable),
    InvalidRequest(&'static str),
    UnsupportedCapability(Capability),
    UnsupportedOperation,
    Conflict,
    NotFound,
    InvalidState,
    /// A side effect may already exist; reconcile inventory before retrying.
    Uncertain(UnconfirmedReason),
}
impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for BackendError {}

impl From<crate::docker_engine::CallError> for BackendError {
    fn from(error: crate::docker_engine::CallError) -> Self {
        BackendError::Uncertain(match error {
            crate::docker_engine::CallError::Timeout => UnconfirmedReason::Timeout,
            crate::docker_engine::CallError::Transport
            | crate::docker_engine::CallError::TooLarge => UnconfirmedReason::BackendFailure,
        })
    }
}

impl From<crate::docker_engine::ApiError> for BackendError {
    fn from(error: crate::docker_engine::ApiError) -> Self {
        match error {
            crate::docker_engine::ApiError::Transport(error) => error.into(),
            crate::docker_engine::ApiError::UnexpectedStatus => {
                Self::Uncertain(UnconfirmedReason::BackendFailure)
            }
            crate::docker_engine::ApiError::InvalidBody => {
                Self::Uncertain(UnconfirmedReason::UnknownState)
            }
        }
    }
}
