//! Candidate portable AQ-owned relay protocol; no transport or terminal hosting.
pub mod epoch;
mod wire;
use aq_session_protocol::ContractError;
pub use wire::*;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid epoch contract")]
    Invalid,
    #[error("stale epoch identity")]
    Stale,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "contract",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WireDocument {
    Downstream(Envelope),
    Upstream(PeerUpload),
    Poll(RelayPoll),
    UploadAck(UploadAck),
    Register(Register),
}
impl WireDocument {
    pub fn validate(&self) -> std::result::Result<(), ContractError> {
        match self {
            Self::Downstream(v) => v.validate(),
            Self::Upstream(v) => v.validate(),
            Self::Poll(v) => v.validate(),
            Self::UploadAck(v) => v.validate(),
            Self::Register(v) => v.validate(),
        }
    }
}

/// Bounded, redacted decoder for portable peer publication envelopes.
pub fn decode_document(bytes: &[u8]) -> std::result::Result<WireDocument, ContractError> {
    if bytes.len() > aq_session_protocol::MAX_BATCH_BYTES {
        return Err(ContractError::Invalid("relay document bytes"));
    }
    let document: WireDocument =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("relay document JSON"))?;
    document.validate()?;
    Ok(document)
}
