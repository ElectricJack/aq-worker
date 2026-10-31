//! Explicit human epoch-input wrapper; worker wire types live independently.
use crate::{Error, Result};
use aq_session_api::InputRequest;
use aq_session_protocol::*;
pub use aq_terminal_relay_protocol::epoch::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub capability: Text,
    pub epoch_id: Uuid,
    pub sequence: u64,
    pub input: InputRequest,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        self.input.terminal.validate().map_err(|_| Error::Invalid)?;
        if self.version != VERSION
            || self.capability.as_str() != CAPABILITY
            || self.epoch_id.is_nil()
            || self.sequence == 0
            || self.input.operation_id.is_nil()
            || self.input.lease_id.is_nil()
            || self.input.expected_session_revision == 0
            || self.input.expected_controller_generation == 0
            || self.input.expires_at_ms == 0
            || self.input.bytes.is_empty()
            || self.input.bytes.len() > MAX_INPUT_BYTES
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochRequest")
            .field("epoch_id", &self.epoch_id)
            .field("sequence", &self.sequence)
            .field("operation_id", &self.input.operation_id)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Open {
    pub terminal: TerminalIdentity,
    pub operation_id: Uuid,
    pub expected_session_revision: u64,
    pub expected_controller_generation: u64,
    pub lease_id: Uuid,
    /// Short intent delivery deadline, not an extension of epoch authority.
    /// The resulting epoch is bounded by the already-current controller lease.
    pub expires_at_ms: u64,
}
impl Open {
    pub fn validate(&self) -> Result<()> {
        self.terminal.validate().map_err(|_| Error::Invalid)?;
        if self.operation_id.is_nil()
            || self.lease_id.is_nil()
            || self.expected_session_revision == 0
            || self.expected_controller_generation == 0
            || self.expires_at_ms == 0
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
