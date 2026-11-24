//! Independent v1 contracts only; no process, transport or tmux qualification.
mod conformance;
mod keyboard;
mod launch;
mod manifest;
mod schema;
mod terminal;
pub use keyboard::complete_terminal_input;
pub use schema::contract_schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_INPUT_BYTES: usize = 4096;
pub const MAX_PENDING_INPUT_BYTES: usize = 32768;
pub const MAX_FRAME_BYTES: usize = 65536;
pub const MAX_REPLAY_BYTES: usize = 1048576;
/// Numeric JSON byte arrays have up to four wire bytes per byte.
pub const MAX_SNAPSHOT_BYTES: usize = 196608;
pub const MAX_BATCH_BYTES: usize = 1048576;
pub const PROFILED_LAUNCH_CAPABILITY: &str = "aq.profiled-launch";
pub const TERMINAL_CAPABILITY: &str = "aq.terminal";
/// Harness-native confinement: the harness's own sandbox confines the agent to the worktree.
pub const NATIVE_CONFINEMENT_CAPABILITY: &str = "aq.harness.confined-worktree";
/// Container confinement (0005 SBX19): the harness sandbox is disabled and the backend
/// sandbox is the boundary. Admission still requires explicit profile permission.
pub const CONTAINER_CONFINEMENT_CAPABILITY: &str = "aq.harness.container-confinement";
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("invalid contract: {0}")]
    Invalid(&'static str),
    #[error("identity or digest conflict")]
    Conflict,
    #[error("stale authority or identity")]
    Stale,
    #[error("authority or command expired")]
    Expired,
    #[error("delivery is unknown; explicit resolution required")]
    DeliveryUnknown,
    #[error("unsupported required capability")]
    Unsupported,
    #[error("JSON: {0}")]
    Json(String),
}
pub trait Validate {
    fn validate(&self) -> Result<(), ContractError>;
}
fn ensure(ok: bool, why: &'static str) -> Result<(), ContractError> {
    if ok {
        Ok(())
    } else {
        Err(ContractError::Invalid(why))
    }
}
/// Nonempty UTF-8 text, at most 512 bytes; no control characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Text(#[schemars(length(min = 1, max = 512))] String);
impl Text {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let s = Self(value.into());
        s.validate()?;
        Ok(s)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Validate for Text {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            !self.0.is_empty() && self.0.len() <= 512 && !self.0.chars().any(char::is_control),
            "text bounds",
        )
    }
}
impl TryFrom<&str> for Text {
    type Error = ContractError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Digest(#[schemars(regex(pattern = "^[0-9a-f]{64}$"))] String);
impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom("lowercase SHA256 required"))
        }
    }
}
/// Typed canonical JSON: UTF-8, compact, recursively sorted object keys, integer units.
pub fn canonical_bytes<T: Serialize + Validate>(value: &T) -> Result<Vec<u8>, ContractError> {
    value.validate()?;
    {
        let mut json =
            serde_json::to_value(value).map_err(|e| ContractError::Json(e.to_string()))?;
        json.sort_all_objects();
        serde_json::to_vec(&json).map_err(|e| ContractError::Json(e.to_string()))
    }
}
pub fn canonical_digest<T: Serialize + Validate>(value: &T) -> Result<Digest, ContractError> {
    Ok(Digest::of(&canonical_bytes(value)?))
}
pub fn decode<T: serde::de::DeserializeOwned + Validate>(bytes: &[u8]) -> Result<T, ContractError> {
    ensure(bytes.len() <= MAX_BATCH_BYTES, "document bounds")?;
    let v: T = serde_json::from_slice(bytes).map_err(|e| ContractError::Json(e.to_string()))?;
    v.validate()?;
    Ok(v)
}
macro_rules! record {($name:ident {$($field:ident:$ty:ty),* $(,)?})=>{#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize,JsonSchema)] #[serde(deny_unknown_fields)] pub struct $name {$(pub $field:$ty),*}}}
record!(ArtifactRef {
    id: Text,
    version: Text,
    digest: Digest
});
record!(Capability {
    id: Text,
    version: u32
});
fn validate_capability_ids(list: &[Capability]) -> Result<(), ContractError> {
    let mut ids = BTreeSet::new();
    for c in list {
        c.validate()?;
        ensure(ids.insert(&c.id), "duplicate capability")?;
    }
    Ok(())
}
impl Capability {
    fn is_supported(&self) -> bool {
        self.version == 1
            && matches!(
                self.id.as_str(),
                "aq.profiled-launch"
                    | "aq.terminal"
                    | "aq.harness.startup-context"
                    | "aq.harness.confined-worktree"
                    | "aq.harness.container-confinement"
                    | "aq.harness.stop"
                    | "aq.harness.native-message"
                    | "aq.harness.resume"
                    | "aq.harness.structured-usage"
                    | "aq.harness.transcript"
                    | "aq.budget.task-cost"
            )
    }
}
impl Validate for Capability {
    fn validate(&self) -> Result<(), ContractError> {
        self.id.validate()?;
        ensure(self.version > 0, "capability version")
    }
}
/// Required capability versions must match exactly. Unknown advertised extensions confer no support.
pub fn require_capabilities(
    required: &[Capability],
    advertised: &[Capability],
) -> Result<(), ContractError> {
    ensure(
        required.len() <= 64 && advertised.len() <= 64,
        "capability count",
    )?;
    validate_capability_ids(required)?;
    validate_capability_ids(advertised)?;
    for c in required {
        if !c.is_supported() || !advertised.contains(c) {
            return Err(ContractError::Unsupported);
        }
    }
    Ok(())
}
record!(SpendingBudget {
    currency: Text,
    basis: BudgetBasis,
    limit_microunits: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetBasis {
    TaskCumulativeEstimatedCost,
    TaskCumulativeEnforcedCost,
}
record!(ModelSettings {
    provider: Text,
    model: Text,
    reasoning: Text,
    max_output_tokens: u64,
    spending_budget: Option<SpendingBudget>
});
record!(HarnessSettings { harness:Text, cli_version:Text, adapter:ArtifactRef, environment:ArtifactRef, required_capabilities:Vec<Capability> });
record!(SecretRef {
    credential_profile: ArtifactRef,
    purpose: Text
});
record!(Scope {
    operation: Text,
    resource: Text
});
record!(PermissionPlan { requested:Vec<Scope>, effective:Vec<Scope>, required:Vec<Scope>, omissions:Vec<Text> });
record!(ContextRef {
    artifact: ArtifactRef,
    provenance: Text,
    delivery: ContextDelivery,
    bytes: u64,
    tokens: u64,
    required: bool
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextDelivery {
    Startup,
    OnDemand,
}
record!(ContextPlan { references:Vec<ContextRef>, startup_token_budget:u64, startup_byte_budget:u64, reserved_output_tokens:u64, context_window_tokens:u64 });
record!(LaunchBindings {
    session: SessionIdentity,
    project_id: Uuid,
    project_revision: u64,
    graph_id: Uuid,
    task_id: Uuid,
    task_revision: u64,
    attempt_id: Uuid,
    assignment_generation: u64,
    source: ArtifactRef,
    outcome: ArtifactRef,
    graph_access_profile: ArtifactRef
});
record!(ResourceLimits {
    memory_bytes: u64,
    cpu_millis: u64,
    process_limit: u32,
    wall_time_ms: u64,
    stop_grace_ms: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalHost {
    Tmux,
}
record!(TerminalPolicy {
    host: TerminalHost,
    input_enabled: bool,
    max_input_bytes: u32,
    max_pending_input_bytes: u32,
    max_frame_bytes: u32,
    replay_bytes: u32
});
record!(LaunchManifest { schema_version:u32, catalog:ArtifactRef, modules:Vec<ArtifactRef>, agent_profile:ArtifactRef, intelligence_profile:ArtifactRef, execution_profile:ArtifactRef, bindings:LaunchBindings, model:ModelSettings, harness:HarnessSettings, credentials:Vec<SecretRef>, permissions:PermissionPlan, context:ContextPlan, resources:ResourceLimits, terminal:TerminalPolicy, selection_explanation:ArtifactRef });
// A new profiled execution extension, never an old unbound execution envelope.
record!(LaunchBinding {
    session: SessionIdentity,
    schema_version: u32,
    offer_id: Uuid,
    attempt_id: Uuid,
    assignment_generation: u64,
    execution_spec_digest: Digest,
    launch_manifest_digest: Digest,
    graph_access_profile_digest: Digest
});
record!(ProfiledOffer { binding:LaunchBinding, manifest:LaunchManifest, required_capabilities:Vec<Capability> });
record!(ProfiledAcceptance { binding:LaunchBinding, advertised_capabilities:Vec<Capability> });
record!(ProfiledGrant {
    binding: LaunchBinding,
    grant_id: Uuid,
    expires_at_ms: u64
});
record!(ProfiledJournalRecord {
    binding: LaunchBinding,
    grant_id: Uuid
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionIdentity {
    Remote {
        node_id: Uuid,
        boot_id: Uuid,
        attempt_id: Uuid,
        assignment_generation: u64,
        session_id: Uuid,
    },
    Central {
        authority_id: Uuid,
        session_id: Uuid,
        generation: u64,
    },
}
impl Validate for SessionIdentity {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            match self {
                Self::Remote {
                    node_id,
                    boot_id,
                    attempt_id,
                    assignment_generation,
                    session_id,
                } => {
                    *assignment_generation > 0
                        && [node_id, boot_id, attempt_id, session_id]
                            .iter()
                            .all(|id| !id.is_nil())
                }
                Self::Central {
                    authority_id,
                    session_id,
                    generation,
                } => *generation > 0 && !authority_id.is_nil() && !session_id.is_nil(),
            },
            "session generation",
        )
    }
}
record!(TerminalIdentity {
    session: SessionIdentity,
    terminal_id: Uuid,
    epoch: u64
});
record!(ControllerLease {
    terminal: TerminalIdentity,
    principal_id: Text,
    lease_id: Uuid,
    generation: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
    execution_expires_at_ms: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputCommand {
    pub operation_id: Uuid,
    pub lease: ControllerLease,
    pub issued_at_ms: u64,
    pub deadline_ms: u64,
    pub bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escape_key: bool,
}

record!(ResizeCommand {
    operation_id: Uuid,
    lease: ControllerLease,
    sequence: u64,
    issued_at_ms: u64,
    deadline_ms: u64,
    columns: u16,
    rows: u16
});
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputDisposition {
    Queued,
    Received,
    Injecting,
    Applied,
    DeliveryUnknown,
    Expired,
    Rejected,
}
record!(InputReceipt {
    operation_id: Uuid,
    command_digest: Digest,
    terminal: TerminalIdentity,
    disposition: InputDisposition
});
record!(OutputFrame { terminal:TerminalIdentity, sequence:u64, bytes:Vec<u8>, byte_length:u32, digest:Digest });
// Opaque renderer-specific state. This contract does not qualify any renderer.
record!(Snapshot { snapshot_id:Uuid,capture_request_id:Uuid,terminal:TerminalIdentity, through_sequence:u64, columns:u16, rows:u16, renderer:ArtifactRef, state:Vec<u8>, digest:Digest });
record!(OutputGap {
    terminal: TerminalIdentity,
    first_missing: u64,
    last_missing: u64
});
record!(WatchSubscription {
    subscription_id: Uuid,
    terminal: TerminalIdentity,
    principal_id: Text,
    expires_at_ms: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TerminalCommand {
    Input(InputCommand),
    Resize(ResizeCommand),
    Controller(ControllerLease),
    Watch(WatchSubscription),
}
record!(NodeTerminalPoll {
    schema_version: u32,
    node_id: Uuid,
    boot_id: Uuid,
    cursor: u64,
    hold_ms: u32,
    active_subscriptions: u32
});
record!(NodeCommandBatch { schema_version:u32, node_id:Uuid, boot_id:Uuid, batch_id:Uuid, cursor:u64, commands:Vec<TerminalCommand> });
record!(NodeOutputBatch { schema_version:u32, node_id:Uuid, boot_id:Uuid, batch_id:Uuid, cursor:u64, frames:Vec<OutputFrame>, snapshots:Vec<Snapshot>, gaps:Vec<OutputGap>, receipts:Vec<InputReceipt> });
/// Deterministic conformance oracle, not a runtime journal or authority service.
pub struct InputConformance {
    pub terminal: TerminalIdentity,
    pub controller_generation: u64,
    records: BTreeMap<Uuid, (Digest, InputDisposition, usize)>,
    pending: usize,
    uncertain: bool,
    resolution_capture: Option<(Uuid, u64)>,
    used_capture_ids: BTreeSet<Uuid>,
    controller_lease: Option<(Uuid, Text)>,
}
/// Snapshot covers all frames through its watermark; only strictly later contiguous frames apply.
pub struct ReplayConformance {
    terminal: TerminalIdentity,
    next: u64,
    seen: BTreeMap<u64, Digest>,
    retained: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContractDocument {
    Launch(Box<ProfiledOffer>),
    Acceptance(ProfiledAcceptance),
    Grant(ProfiledGrant),
    Journal(ProfiledJournalRecord),
    Poll(NodeTerminalPoll),
    Commands(NodeCommandBatch),
    Output(NodeOutputBatch),
}
impl Validate for ContractDocument {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Launch(v) => v.validate(),
            Self::Acceptance(v) => v.validate(),
            Self::Grant(v) => v.validate(),
            Self::Journal(v) => v.validate(),
            Self::Poll(v) => v.validate(),
            Self::Commands(v) => v.validate(),
            Self::Output(v) => v.validate(),
        }
    }
}
/// Resize retries are independent of literal-input uncertainty and ordered by sequence.
pub struct ResizeConformance {
    pub terminal: TerminalIdentity,
    pub controller_generation: u64,
    last_sequence: u64,
    history: BTreeMap<Uuid, Digest>,
    controller_lease: Option<(Uuid, Text)>,
}
/// Bounded batch-id conflict oracle; production transports own durable retention.
#[derive(Default)]
pub struct BatchConformance {
    seen: BTreeMap<Uuid, Digest>,
}
