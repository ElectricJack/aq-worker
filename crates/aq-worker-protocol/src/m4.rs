//! Capability-negotiated immutable source and transfer contracts.
use super::*;
pub const SOURCE_CAPABILITY: &str = "source-handoff-v1";
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObjectAlgorithm {
    Sha1,
}
wire! {
    ObjectId { algorithm: ObjectAlgorithm, hex: String }
    SourceBinding { repository_id: Uuid, binding_generation: u64, logical_identity: String, base: ObjectId }
    GraphBinding { workspace_id: Uuid, profile_id: String, profile_revision: String, profile_digest: String, context_digest: String }
    ExecutionBindings { source: Option<SourceBinding>, graph: Option<GraphBinding> }
    TransferManifest { transfer_id: Uuid, role: SourceRole, checkpoint: ObjectId, attempt: AttemptRef, resolved_spec_digest: String, source: SourceBinding, artifact_id: Uuid, manifest_digest: String, bundle_digest: Option<String>, manifest_bytes: u64, bundle_bytes: u64 }
    TransferChunkRequest { identity: NodeIdentity, operation_id: Uuid, transfer_id: Uuid, file: TransferFile, offset: u64, length: u32, digest: String }
    TransferStatusRequest { identity: NodeIdentity, operation_id: Uuid, transfer_id: Uuid }
    TransferStatus { transfer_id: Uuid, manifest_offset: u64, bundle_offset: u64, receipt: Option<CentralRetentionReceipt> }
    CentralRetentionReceipt { receipt_id: Uuid, transfer_id: Uuid, repository_id: Uuid, binding_generation: u64, source: ObjectId, retention_handle: Uuid, manifest_digest: String }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferFile {
    Manifest,
    Bundle,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    Base,
    Checkpoint,
}
impl Validate for ObjectId {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            self.hex.len() == 40
                && self
                    .hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid typed oid",
        )
    }
}
impl Validate for SourceBinding {
    fn validate(&self) -> Result<(), ValidationError> {
        self.base.validate()?;
        check(
            !self.repository_id.is_nil()
                && self.binding_generation > 0
                && text(&self.logical_identity, 256)
                && !self.logical_identity.chars().any(char::is_control),
            "invalid source binding",
        )
    }
}
impl Validate for GraphBinding {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.workspace_id.is_nil()
                && text(&self.profile_id, 128)
                && self
                    .profile_revision
                    .parse::<i64>()
                    .is_ok_and(|n| n > 0 && n.to_string() == self.profile_revision)
                && digest(&self.profile_digest)
                && digest(&self.context_digest),
            "invalid graph binding",
        )
    }
}
impl Validate for ExecutionBindings {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(s) = &self.source {
            s.validate()?;
        }
        if let Some(g) = &self.graph {
            g.validate()?;
        }
        Ok(())
    }
}
impl Validate for TransferManifest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.attempt.validate()?;
        self.source.validate()?;
        self.checkpoint.validate()?;
        check(
            !self.transfer_id.is_nil()
                && !self.artifact_id.is_nil()
                && (self.role != SourceRole::Base || self.checkpoint == self.source.base)
                && digest(&self.resolved_spec_digest)
                && digest(&self.manifest_digest)
                && self.manifest_bytes > 0
                && self.manifest_bytes <= 1024 * 1024
                && self.bundle_bytes <= 16 * 1024 * 1024 * 1024
                && self.bundle_digest.as_ref().is_none_or(|s| digest(s))
                && (self.bundle_bytes == 0) == self.bundle_digest.is_none(),
            "invalid transfer manifest",
        )
    }
}
/// Chunk bytes travel as raw HTTP body; descriptor length/digest cover those exact bytes.
impl Validate for TransferChunkRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            !self.operation_id.is_nil()
                && !self.transfer_id.is_nil()
                && self.length > 0
                && self.length as usize <= MAX_CHUNK_BYTES
                && self.offset.checked_add(self.length as u64).is_some()
                && digest(&self.digest),
            "invalid transfer chunk",
        )
    }
}
impl Validate for TransferStatusRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            !self.operation_id.is_nil() && !self.transfer_id.is_nil(),
            "invalid transfer status request",
        )
    }
}
impl Validate for CentralRetentionReceipt {
    fn validate(&self) -> Result<(), ValidationError> {
        self.source.validate()?;
        check(
            !self.receipt_id.is_nil()
                && !self.transfer_id.is_nil()
                && !self.repository_id.is_nil()
                && !self.retention_handle.is_nil()
                && self.binding_generation > 0
                && digest(&self.manifest_digest),
            "invalid retention receipt",
        )
    }
}
impl Validate for TransferStatus {
    fn validate(&self) -> Result<(), ValidationError> {
        check(!self.transfer_id.is_nil(), "invalid transfer status")?;
        if let Some(r) = &self.receipt {
            r.validate()?;
            check(
                r.transfer_id == self.transfer_id,
                "receipt transfer mismatch",
            )?;
        }
        Ok(())
    }
}
