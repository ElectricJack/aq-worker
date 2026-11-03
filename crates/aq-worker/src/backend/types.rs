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
        }
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
        if self.name != self.owner.deterministic_name()
            || self.hard_deadline_ms == 0
            || self.owner.node_id.is_nil()
            || self.owner.boot_id.is_nil()
            || self.owner.attempt_id.is_nil()
        {
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

/// CPU, memory and PID limits copied from the node reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub pids: u32,
}
impl ResourceLimits {
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.cpu_millis == 0 || self.pids == 0 || self.memory_bytes < MIN_MEMORY_BYTES {
            return Err(BackendError::InvalidRequest("invalid reservation limits"));
        }
        Ok(())
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
    fn new(sandbox: SandboxIdentity, evidence: RetentionEvidence) -> Result<Self, BackendError> {
        let value = Self { sandbox, evidence };
        if !value.authorizes(&value.sandbox) {
            return Err(BackendError::InvalidRequest("empty retention evidence"));
        }
        Ok(value)
    }
    pub fn authorizes(&self, sandbox: &SandboxIdentity) -> bool {
        let (RetentionEvidence::CentralReceipt(reference)
        | RetentionEvidence::OperatorRelease(reference)) = &self.evidence;
        &self.sandbox == sandbox && !reference.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CreateRequest {
        CreateRequest::new(
            Ownership {
                node_id: Uuid::from_u128(1),
                boot_id: Uuid::from_u128(2),
                attempt_id: Uuid::from_u128(3),
            },
            TemplateDigest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            100,
            vec![],
        )
    }

    #[test]
    fn create_request_without_resources_deserializes() {
        let mut value = serde_json::to_value(request()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("resources");
        object.remove("mount_aliases");
        let parsed: CreateRequest = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.resources, None);
        assert!(parsed.mount_aliases.is_empty());
        assert_eq!(parsed, request());
    }

    #[test]
    fn resource_limits_reject_zero() {
        let ok = ResourceLimits {
            cpu_millis: 500,
            memory_bytes: 64 * 1024 * 1024,
            pids: 32,
        };
        assert!(request().with_resources(ok).validate().is_ok());
        for bad in [
            ResourceLimits {
                cpu_millis: 0,
                ..ok
            },
            ResourceLimits { pids: 0, ..ok },
            ResourceLimits {
                memory_bytes: 6 * 1024 * 1024 - 1,
                ..ok
            },
        ] {
            assert!(matches!(
                request().with_resources(bad).validate(),
                Err(BackendError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn mount_alias_names_are_validated() {
        for bad in ["", "../x", "a/b", "-x", "Upper"] {
            assert!(
                matches!(
                    request()
                        .with_mount_aliases(vec![bad.to_owned()])
                        .validate(),
                    Err(BackendError::InvalidRequest(_))
                ),
                "{bad:?} must be refused"
            );
        }
        assert!(
            request()
                .with_mount_aliases(vec!["repo-cache".to_owned()])
                .validate()
                .is_ok()
        );
    }
}
