//! Guardian stop-proof artifact (design review rev-agile-falcon, Gap 4b).
//!
//! `StopObservation::ReportedStopped.proof` names a centrally retained artifact
//! with role `result` whose bytes are the canonical JSON of [`StopProof`] and
//! whose `ArtifactRef.digest` is the SHA-256 of exactly those bytes. The
//! successor SessionHost fetches the bytes and verifies guardian instance and
//! writer set with [`verify_stop_proof`]; the server verifies only lineage
//! (the retention receipt's attempt binding). Neither check proves the writers
//! are still stopped now: it binds a guardian claim to retained evidence.
use crate::*;

/// Upper bound on retained stop-proof bytes. The canonical body is ~170 bytes.
pub const STOP_PROOF_MAX_BYTES: usize = 1024;

record!(StopProof {
    guardian_instance_id: Uuid,
    writer_set_digest: Digest,
    stopped_at_boot_ms: u64
});
impl Validate for StopProof {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.guardian_instance_id)
    }
}
impl StopProof {
    /// The exact retained bytes: canonical JSON (sorted keys, compact UTF-8).
    pub fn to_bytes(&self) -> Result<Vec<u8>, ContractError> {
        canonical_bytes(self)
    }
    /// SHA-256 of [`Self::to_bytes`]; the `ArtifactRef.digest` of the proof.
    pub fn digest(&self) -> Result<Digest, ContractError> {
        canonical_digest(self)
    }
    /// The artifact reference a `ReportedStopped` observation carries.
    pub fn artifact(&self, id: Text) -> Result<ArtifactRef, ContractError> {
        Ok(ArtifactRef {
            id,
            version: Text::new("1")?,
            digest: self.digest()?,
        })
    }
    /// Strict decode of retained bytes: bounded, no unknown fields, canonical.
    /// Non-canonical bytes are refused so that one proof has one digest.
    pub fn decode(bytes: &[u8]) -> Result<Self, ContractError> {
        ensure(bytes.len() <= STOP_PROOF_MAX_BYTES, "stop proof byte bound")?;
        let proof: Self =
            serde_json::from_slice(bytes).map_err(|e| ContractError::Json(e.to_string()))?;
        ensure(
            proof.to_bytes()? == bytes,
            "stop proof is not canonical JSON",
        )?;
        Ok(proof)
    }
    /// The `ReportedStopped` observation whose proof is this artifact.
    pub fn observation(
        &self,
        observation_id: Uuid,
        writer_count: u32,
        observed_at_ms: u64,
        artifact_id: Text,
    ) -> Result<StopObservation, ContractError> {
        let stop = StopObservation::ReportedStopped {
            observation_id,
            guardian_instance_id: self.guardian_instance_id,
            writer_set_digest: self.writer_set_digest.clone(),
            writer_count,
            observed_at_ms,
            proof: self.artifact(artifact_id)?,
        };
        stop.validate()?;
        Ok(stop)
    }
}

/// Successor-side verification: `bytes` are the retained proof named by `stop`,
/// and both the observation and the proof name the expected guardian instance
/// and writer set. Returns the decoded proof.
pub fn verify_stop_proof(
    stop: &StopObservation,
    bytes: &[u8],
    guardian_instance_id: Uuid,
    writer_set_digest: &Digest,
) -> Result<StopProof, ContractError> {
    stop.validate()?;
    let StopObservation::ReportedStopped {
        guardian_instance_id: observed_guardian,
        writer_set_digest: observed_writers,
        proof: reference,
        ..
    } = stop
    else {
        return Err(ContractError::Invalid("stop was not reported"));
    };
    ensure(Digest::of(bytes) == reference.digest, "stop proof digest")?;
    let proof = StopProof::decode(bytes)?;
    ensure(
        proof.guardian_instance_id == *observed_guardian
            && proof.guardian_instance_id == guardian_instance_id,
        "stop proof guardian instance",
    )?;
    ensure(
        proof.writer_set_digest == *observed_writers
            && proof.writer_set_digest == *writer_set_digest,
        "stop proof writer set",
    )?;
    Ok(proof)
}

/// The `result`-role retention receipt for `stop`'s proof, if retained.
pub fn stop_proof_receipt<'a>(
    stop: &StopObservation,
    receipts: &'a [RetentionReceipt],
) -> Option<&'a RetentionReceipt> {
    let StopObservation::ReportedStopped { proof, .. } = stop else {
        return None;
    };
    receipts
        .iter()
        .find(|r| r.role == ArtifactRole::Result && r.artifact == *proof)
}

pub fn stop_proof_schema() -> serde_json::Value {
    let mut schema = schema_for::<StopProof>();
    schema["x-max-wire-bytes"] = serde_json::json!(STOP_PROOF_MAX_BYTES);
    schema["x-canonical"] = serde_json::json!(
        "retained bytes are canonical JSON; ArtifactRef.digest is their lowercase SHA-256"
    );
    schema
}
