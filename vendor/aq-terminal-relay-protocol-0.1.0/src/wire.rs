//! Portable AQ-owned terminal relay draft; no daemon/human API/runtime dependency.
use aq_session_protocol::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const CAPABILITY: &str = "aq.terminal-relay";
pub const VERSION: u32 = 1;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Peer {
    Remote { node_id: Uuid, boot_id: Uuid },
    Central { authority_id: Uuid },
}

impl Peer {
    pub fn matches(&self, t: &TerminalIdentity) -> bool {
        match (self, &t.session) {
            (
                Self::Remote {
                    node_id: a,
                    boot_id: b,
                },
                SessionIdentity::Remote {
                    node_id: c,
                    boot_id: d,
                    ..
                },
            ) => a == c && b == d,
            (
                Self::Central { authority_id: a },
                SessionIdentity::Central {
                    authority_id: b, ..
                },
            ) => a == b,
            _ => false,
        }
    }
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        if match self {
            Self::Remote { node_id, boot_id } => !node_id.is_nil() && !boot_id.is_nil(),
            Self::Central { authority_id } => !authority_id.is_nil(),
        } {
            Ok(())
        } else {
            Err(ContractError::Stale)
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fence {
    pub terminal: TerminalIdentity,
    pub fence_id: Uuid,
    pub generation: u64,
    /// None invalidates all older grants without minting another input principal.
    pub controller: Option<ControllerLease>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FenceAck {
    pub terminal: TerminalIdentity,
    pub fence_id: Uuid,
    pub generation: u64,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub terminal: TerminalIdentity,
    pub capture_request_id: Uuid,
    pub uncertain_operation_id: Uuid,
    pub lease_id: Uuid,
    pub controller_generation: u64,
    pub after_ledger_revision: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Effect {
    Fence(Fence),
    Capture(Capture),
    Input(InputCommand),
    Resize(ResizeCommand),
    Watch(WatchSubscription),
    EpochInput(crate::epoch::Command),
}

impl Effect {
    pub fn terminal(&self) -> &TerminalIdentity {
        match self {
            Self::Fence(v) => &v.terminal,
            Self::Capture(v) => &v.terminal,
            Self::Input(v) => &v.lease.terminal,
            Self::Resize(v) => &v.lease.terminal,
            Self::Watch(v) => &v.terminal,
            Self::EpochInput(v) => &v.permit.terminal,
        }
    }
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        self.terminal().validate()?;
        match self {
            Self::Fence(v) => {
                if v.fence_id.is_nil() || v.generation == 0 {
                    return Err(ContractError::Stale);
                }
                if let Some(lease) = &v.controller {
                    lease.validate()?;
                    if lease.terminal != v.terminal || lease.generation != v.generation {
                        return Err(ContractError::Stale);
                    }
                }
                Ok(())
            }
            Self::Capture(v) => {
                if v.capture_request_id.is_nil()
                    || v.uncertain_operation_id.is_nil()
                    || v.lease_id.is_nil()
                    || v.controller_generation == 0
                    || v.after_ledger_revision == 0
                    || v.expires_at_ms == 0
                {
                    Err(ContractError::Stale)
                } else {
                    Ok(())
                }
            }
            Self::Input(v) => v.validate(),
            Self::Resize(v) => v.validate(),
            Self::Watch(v) => TerminalCommand::Watch(v.clone()).validate(),
            Self::EpochInput(v) => v
                .validate()
                .map_err(|_| ContractError::Invalid("epoch command")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub capability: Text,
    pub peer: Peer,
    pub exchange_id: Uuid,
    pub cursor: u64,
    pub effects: Vec<Effect>,
}

impl Envelope {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        self.peer.validate()?;
        if self.version != VERSION
            || ![CAPABILITY, crate::epoch::CAPABILITY].contains(&self.capability.as_str())
            || self.exchange_id.is_nil()
            || self.effects.len() > 64
        {
            return Err(ContractError::Unsupported);
        }
        if self
            .effects
            .iter()
            .any(|e| matches!(e, Effect::EpochInput(_)))
            && self.capability.as_str() != crate::epoch::CAPABILITY
        {
            return Err(ContractError::Unsupported);
        }
        for effect in &self.effects {
            effect.validate()?;
            if !self.peer.matches(effect.terminal()) {
                return Err(ContractError::Stale);
            }
        }
        wire_size(self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResizeReceipt {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub command_digest: Digest,
    pub sequence: u64,
    pub applied: bool,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerUpload {
    pub version: u32,
    pub capability: Text,
    pub peer: Peer,
    pub exchange_id: Uuid,
    pub cursor: u64,
    pub receipts: Vec<InputReceipt>,
    pub epoch_receipts: Vec<crate::epoch::Receipt>,
    pub resize_receipts: Vec<ResizeReceipt>,
    pub fence_acks: Vec<FenceAck>,
    pub frames: Vec<OutputFrame>,
    pub snapshots: Vec<Snapshot>,
    pub gaps: Vec<OutputGap>,
}

impl PeerUpload {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        self.peer.validate()?;
        if self.version != VERSION
            || ![CAPABILITY, crate::epoch::CAPABILITY].contains(&self.capability.as_str())
            || self.exchange_id.is_nil()
            || self.receipts.len() > 64
            || self.epoch_receipts.len() > 64
            || self.resize_receipts.len() > 64
            || self.fence_acks.len() > 64
            || self.frames.len() > 64
            || self.snapshots.len() > 32
            || self.gaps.len() > 64
        {
            return Err(ContractError::Unsupported);
        }
        if !self.epoch_receipts.is_empty() && self.capability.as_str() != crate::epoch::CAPABILITY {
            return Err(ContractError::Unsupported);
        }
        for v in &self.epoch_receipts {
            v.receipt.validate()?;
            if v.epoch_id.is_nil() || v.sequence == 0 || !self.peer.matches(&v.receipt.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for v in &self.receipts {
            v.validate()?;
            if !self.peer.matches(&v.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for v in &self.frames {
            v.validate()?;
            if !self.peer.matches(&v.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for v in &self.snapshots {
            v.validate()?;
            if !self.peer.matches(&v.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for v in &self.gaps {
            v.validate()?;
            if !self.peer.matches(&v.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for v in &self.fence_acks {
            v.terminal.validate()?;
            if v.fence_id.is_nil() || v.generation == 0 || !self.peer.matches(&v.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for v in &self.resize_receipts {
            v.terminal.validate()?;
            if v.operation_id.is_nil() || v.sequence == 0 || !self.peer.matches(&v.terminal) {
                return Err(ContractError::Stale);
            }
        }
        wire_size(self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayPoll {
    pub version: u32,
    pub capability: Text,
    pub peer: Peer,
    pub cursor: u64,
    pub hold_ms: u32,
    pub active_subscriptions: u32,
    pub active_controllers: u32,
    pub advertised_capabilities: Vec<Capability>,
}

impl RelayPoll {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        self.peer.validate()?;
        if self.hold_ms > 20_000
            || self.active_subscriptions > 32
            || self.active_controllers > 32
            || (self.hold_ms > 0 && self.active_subscriptions == 0 && self.active_controllers == 0)
        {
            return Err(ContractError::Invalid("relay poll bound"));
        }
        if self.version != VERSION {
            return Err(ContractError::Unsupported);
        }
        match self.capability.as_str() {
            CAPABILITY => negotiate(&self.advertised_capabilities)?,
            crate::epoch::CAPABILITY => negotiate_epoch(&self.advertised_capabilities)?,
            _ => return Err(ContractError::Unsupported),
        }
        wire_size(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadAck {
    pub version: u32,
    pub capability: Text,
    pub peer: Peer,
    pub exchange_id: Uuid,
    pub cursor: u64,
    pub retirements: Vec<crate::epoch::Retirement>,
    pub dropped_output: Vec<OutputGap>,
}

impl UploadAck {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        self.peer.validate()?;
        if self.version != VERSION
            || ![CAPABILITY, crate::epoch::CAPABILITY].contains(&self.capability.as_str())
            || self.exchange_id.is_nil()
            || self.retirements.len() > 64
            || self.dropped_output.len() > 64
        {
            return Err(ContractError::Unsupported);
        }
        if !self.retirements.is_empty() && self.capability.as_str() != crate::epoch::CAPABILITY {
            return Err(ContractError::Unsupported);
        }
        for gap in &self.dropped_output {
            gap.validate()?;
            if !self.peer.matches(&gap.terminal) {
                return Err(ContractError::Stale);
            }
        }
        for retirement in &self.retirements {
            retirement
                .permit
                .validate()
                .map_err(|_| ContractError::Invalid("epoch retirement"))?;
            if !self.peer.matches(&retirement.permit.terminal) {
                return Err(ContractError::Stale);
            }
        }
        wire_size(self)
    }
}

pub fn decode(bytes: &[u8]) -> std::result::Result<Envelope, ContractError> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(ContractError::Invalid("draft envelope bytes"));
    }
    let v: Envelope = serde_json::from_slice(bytes)
        .map_err(|_| ContractError::Invalid("relay JSON encoding or shape"))?;
    v.validate()?;
    Ok(v)
}

pub fn negotiate(capabilities: &[Capability]) -> std::result::Result<(), ContractError> {
    let mut ids = std::collections::BTreeSet::new();
    if capabilities.len() > 64 {
        return Err(ContractError::Unsupported);
    }
    for c in capabilities {
        c.validate()?;
        if !ids.insert(c.id.clone()) {
            return Err(ContractError::Unsupported);
        }
    }
    if capabilities
        .iter()
        .any(|c| c.id.as_str() == CAPABILITY && c.version == VERSION)
    {
        Ok(())
    } else {
        Err(ContractError::Unsupported)
    }
}

fn wire_size(value: &impl Serialize) -> std::result::Result<(), ContractError> {
    if serde_json::to_vec(value)
        .map_err(|_| ContractError::Invalid("relay JSON encoding or shape"))?
        .len()
        > MAX_BATCH_BYTES
    {
        Err(ContractError::Invalid("draft envelope bytes"))
    } else {
        Ok(())
    }
}

pub fn negotiate_epoch(capabilities: &[Capability]) -> std::result::Result<(), ContractError> {
    negotiate(capabilities)?;
    if capabilities
        .iter()
        .any(|c| c.id.as_str() == crate::epoch::CAPABILITY && c.version == crate::epoch::VERSION)
    {
        Ok(())
    } else {
        Err(ContractError::Unsupported)
    }
}
impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Input(_) => "Input",
            Self::EpochInput(_) => "EpochInput",
            Self::Resize(_) => "Resize",
            Self::Fence(_) => "Fence",
            Self::Capture(_) => "Capture",
            Self::Watch(_) => "Watch",
        };
        f.debug_struct(kind)
            .field("terminal", self.terminal())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for PeerUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerUpload")
            .field("peer", &self.peer)
            .field("exchange_id", &self.exchange_id)
            .field("frames", &self.frames.len())
            .field("snapshots", &self.snapshots.len())
            .field("receipts", &self.receipts.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    FiniteV1,
    EpochV1,
}
impl InputMode {
    pub fn key(self) -> &'static str {
        match self {
            Self::FiniteV1 => "finite_v1",
            Self::EpochV1 => "epoch_v1",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub version: u32,
    pub capability: Text,
    pub operation_id: Uuid,
    pub peer: Peer,
    pub terminal: TerminalIdentity,
    pub expected_authority_revision: u64,
    pub input_mode: InputMode,
    pub advertised_capabilities: Vec<Capability>,
}
impl Register {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        self.peer.validate()?;
        self.terminal.validate()?;
        if self.version != VERSION
            || self.operation_id.is_nil()
            || self.expected_authority_revision == 0
            || !self.peer.matches(&self.terminal)
        {
            return Err(ContractError::Stale);
        }
        match self.input_mode {
            InputMode::FiniteV1 => {
                if self.capability.as_str() != CAPABILITY {
                    return Err(ContractError::Unsupported);
                }
                negotiate(&self.advertised_capabilities)?
            }
            InputMode::EpochV1 => {
                if self.capability.as_str() != crate::epoch::CAPABILITY {
                    return Err(ContractError::Unsupported);
                }
                negotiate_epoch(&self.advertised_capabilities)?
            }
        }
        wire_size(self)
    }
}
