//! Explicit, known-fact-only parser reconciliation. Byte verification is not
//! retention-owner authentication: a host-supplied authority must admit the
//! exact plan before this module exposes a verified replay handle.
use crate::{lifecycle::*, source::Cursor};
use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
pub const MAX_REPLAY_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayFact {
    pub prior: EventRef,
    pub selected_record_digest: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReparsePlan {
    pub operation_id: Uuid,
    pub source_id: Uuid,
    pub source_generation: u64,
    pub native_session_key: Text,
    pub binding: Binding,
    pub old_parser: ArtifactRef,
    pub new_parser: ArtifactRef,
    pub retained_cursor: Cursor,
    pub transcript: RetentionReceipt,
    pub known: BTreeMap<String, ReplayFact>,
}
impl ReparsePlan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.operation_id.is_nil()
                && !self.source_id.is_nil()
                && self.source_generation > 0
                && !self.known.is_empty()
                && self.known.len() <= 128,
            "reparse identity/bounds"
        );
        self.binding.validate()?;
        self.native_session_key.validate()?;
        artifact(&self.old_parser)?;
        artifact(&self.new_parser)?;
        ensure!(
            self.old_parser != self.new_parser,
            "reparse requires a distinct parser artifact"
        );
        self.retained_cursor.validate()?;
        self.transcript.validate()?;
        ensure!(
            self.transcript.role == ArtifactRole::Transcript
                && self.transcript.binding_digest == canonical_digest(&self.binding)?
                && self.transcript.bytes > 0
                && self.transcript.bytes <= MAX_REPLAY_BYTES as u64,
            "retained transcript binding/role/bound"
        );
        for (key, fact) in &self.known {
            ensure!(
                !key.is_empty() && key.len() <= 1024,
                "reparse native key bound"
            );
            fact.prior.validate()?;
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        crate::normalize::value_digest(&serde_json::to_value(self)?)
    }
}
/// Must be implemented by the authenticated retention/admission owner adapter.
/// No default permissive verifier or worker-authored receipt shortcut is given.
pub trait ReplayAuthority {
    fn admit(&self, plan: &ReparsePlan) -> Result<()>;
}
pub struct VerifiedReplay {
    plan: ReparsePlan,
    bytes: Vec<u8>,
}
impl VerifiedReplay {
    pub fn verify(
        plan: ReparsePlan,
        bytes: Vec<u8>,
        now_ms: u64,
        authority: &impl ReplayAuthority,
    ) -> Result<Self> {
        plan.validate()?;
        ensure!(
            bytes.len() as u64 == plan.transcript.bytes
                && Digest::of(&bytes) == plan.transcript.artifact.digest
                && now_ms >= plan.transcript.retained_at_ms
                && now_ms < plan.transcript.retained_until_ms,
            "retained transcript bytes/interval refused"
        );
        authority.admit(&plan)?;
        ensure!(
            bytes.ends_with(b"\n"),
            "retained prefix must end at a complete record"
        );
        let mut records = BTreeSet::new();
        let mut count = 0;
        for record in bytes.split_inclusive(|b| *b == b'\n') {
            count += 1;
            ensure!(
                count <= 4096 && record.len() <= crate::framing::MAX_RECORD_BYTES,
                "retained replay record/scan bound"
            );
            records.insert(Digest::of(record));
        }
        ensure!(
            plan.known
                .values()
                .all(|f| records.contains(&f.selected_record_digest)),
            "selected correction record is absent from retained bytes"
        );
        Ok(Self { plan, bytes })
    }
    pub fn plan(&self) -> &ReparsePlan {
        &self.plan
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReparseMetadata {
    pub operation_id: Uuid,
    pub known: BTreeMap<String, ReplayFact>,
    pub handled: BTreeSet<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayState {
    pub plan: ReparsePlan,
    pub plan_digest: Digest,
    pub offset: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayCompletion {
    pub operation_id: Uuid,
    pub plan_digest: Digest,
}
