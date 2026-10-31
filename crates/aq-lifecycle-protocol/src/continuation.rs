use crate::*;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct SummaryText(#[schemars(length(min = 1, max = 16384))] String);
impl SummaryText {
    pub fn new(text: impl Into<String>) -> Result<Self, ContractError> {
        let value = Self(text.into());
        value.validate()?;
        Ok(value)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Validate for SummaryText {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            !self.0.is_empty() && self.0.len() <= 16384 && !self.0.contains('\0'),
            "summary UTF-8 bytes",
        )
    }
}
impl<'de> Deserialize<'de> for SummaryText {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
record!(ContinuationRef {
    continuation_id: Uuid,
    generation: u64,
    wait: WaitRef
});
impl Validate for ContinuationRef {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.continuation_id)?;
        ensure(self.generation > 0, "continuation generation")?;
        self.wait.validate()
    }
}
// Agent-authored summary, not authoritative progress or stopped-writer proof.
record!(SummaryClaim {text:SummaryText,context_references:Vec<ArtifactRef>,pending_operation_ids:Vec<Uuid>});
impl Validate for SummaryClaim {
    fn validate(&self) -> Result<(), ContractError> {
        self.text.validate()?;
        ensure(
            self.context_references.len() <= MAX_REFS,
            "summary references",
        )?;
        for r in &self.context_references {
            artifact(r)?;
        }
        unique_ids(&self.pending_operation_ids, MAX_REFS)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointClaim {
    Supplied {
        summary: SummaryClaim,
        summary_digest: Digest,
    },
    Missing {
        reason: Text,
    },
}
impl Validate for CheckpointClaim {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Supplied {
                summary,
                summary_digest,
            } => {
                summary.validate()?;
                ensure(
                    canonical_digest(summary)? == *summary_digest,
                    "checkpoint summary digest",
                )
            }
            Self::Missing { reason } => reason.validate(),
        }
    }
}
record!(Checkpoint {
    binding: Binding,
    continuation: ContinuationRef,
    claim: CheckpointClaim
});
impl Validate for Checkpoint {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.continuation.validate()?;
        self.claim.validate()
    }
}
record!(SuspensionIntent {
    binding: Binding,
    continuation: ContinuationRef,
    issued_at_ms: u64,
    deadline_ms: u64,
    max_summary_bytes: u32,
    retention_policy: ArtifactRef
});
impl Validate for SuspensionIntent {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.continuation.validate()?;
        artifact(&self.retention_policy)?;
        ensure(
            self.deadline_ms > self.issued_at_ms && self.deadline_ms - self.issued_at_ms <= 300000,
            "suspension deadline bound",
        )?;
        ensure(
            self.max_summary_bytes > 0 && self.max_summary_bytes <= 16384,
            "checkpoint budget",
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopObservation {
    Unknown {
        reason: Text,
    },
    ReportedStopped {
        observation_id: Uuid,
        guardian_instance_id: Uuid,
        writer_set_digest: Digest,
        writer_count: u32,
        observed_at_ms: u64,
        proof: ArtifactRef,
    },
}
impl Validate for StopObservation {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Unknown { reason } => reason.validate(),
            Self::ReportedStopped {
                observation_id,
                guardian_instance_id,
                writer_count,
                proof,
                ..
            } => {
                uuid(*observation_id)?;
                uuid(*guardian_instance_id)?;
                ensure(*writer_count <= 65536, "writer count bound")?;
                artifact(proof)
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    Source,
    Checkpoint,
    Transcript,
    Result,
}
// An exact central retention receipt reference, still requiring runtime owner
// verification. Local bytes and worker-authored fields do not prove retention.
record!(RetentionReceipt {
    receipt_id: Uuid,
    owner: Text,
    binding_digest: Digest,
    artifact: ArtifactRef,
    role: ArtifactRole,
    bytes: u64,
    retained_at_ms: u64,
    retained_until_ms: u64
});
impl Validate for RetentionReceipt {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.receipt_id)?;
        self.owner.validate()?;
        ensure(
            self.retained_until_ms > self.retained_at_ms,
            "retention interval",
        )?;
        artifact(&self.artifact)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EffectsObservation {
    Pending { operation_ids: Vec<Uuid> },
    Reconciled { receipt: ArtifactRef },
}
impl Validate for EffectsObservation {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Pending { operation_ids } => unique_ids(operation_ids, MAX_REFS),
            Self::Reconciled { receipt } => artifact(receipt),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SuspensionDisposition {
    Preparing,
    Held { reason: Text },
    StoppedRetaining,
    RetainedSuspended,
}
record!(ParkingReport {binding:Binding,continuation:ContinuationRef,disposition:SuspensionDisposition,stop:StopObservation,local_retained_bytes:u64,central_receipts:Vec<RetentionReceipt>,effects:EffectsObservation,compute_released:bool,protected_disk_released:bool,disk_release_directive:Option<ArtifactRef>});
pub fn validate_retention(
    receipts: &[RetentionReceipt],
    binding: &Binding,
) -> Result<(), ContractError> {
    ensure(receipts.len() <= MAX_REFS, "retention receipt bound")?;
    let mut ids = std::collections::BTreeSet::new();
    let digest = canonical_digest(binding)?;
    for r in receipts {
        r.validate()?;
        ensure(r.binding_digest == digest, "retention attempt binding")?;
        ensure(ids.insert(r.receipt_id), "duplicate retention receipt")?;
    }
    Ok(())
}
pub fn has_migratable_refs(receipts: &[RetentionReceipt]) -> bool {
    receipts.iter().any(|r| r.role == ArtifactRole::Source)
        && receipts.iter().any(|r| r.role == ArtifactRole::Checkpoint)
}
impl Validate for ParkingReport {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.continuation.validate()?;
        self.stop.validate()?;
        self.effects.validate()?;
        validate_retention(&self.central_receipts, &self.binding)?;
        if let SuspensionDisposition::Held { reason } = &self.disposition {
            reason.validate()?;
        }
        let stopped = matches!(self.stop, StopObservation::ReportedStopped { .. });
        if self.compute_released
            || matches!(
                self.disposition,
                SuspensionDisposition::StoppedRetaining | SuspensionDisposition::RetainedSuspended
            )
        {
            ensure(stopped, "compute release needs stopped observation")?;
        }
        if self.disposition == SuspensionDisposition::RetainedSuspended {
            ensure(
                has_migratable_refs(&self.central_receipts)
                    && matches!(self.effects, EffectsObservation::Reconciled { .. }),
                "retained suspension prerequisites",
            )?;
        }
        if self.protected_disk_released {
            ensure(
                self.disposition == SuspensionDisposition::RetainedSuspended
                    && self.disk_release_directive.is_some(),
                "protected disk release is independent",
            )?;
        }
        if let Some(r) = &self.disk_release_directive {
            artifact(r)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CostBudget {
    NotConfigured,
    Unknown {
        reason: UnknownReason,
    },
    SoftEstimateRemaining {
        remaining_microunits: u64,
        currency: Text,
        rate_card: ArtifactRef,
        basis: EstimateBasis,
    },
}
impl Validate for CostBudget {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::NotConfigured | Self::Unknown { .. } => Ok(()),
            Self::SoftEstimateRemaining {
                currency,
                rate_card,
                ..
            } => {
                ensure(
                    currency.as_str().len() == 3
                        && currency.as_str().bytes().all(|b| b.is_ascii_uppercase()),
                    "budget currency",
                )?;
                artifact(rate_card)
            }
        }
    }
}
record!(RemainingBudgets {
    lineage_id: Uuid,
    policy: ArtifactRef,
    ledger_checkpoint: ArtifactRef,
    remaining_failure_retries: u32,
    remaining_elapsed_ms: u64,
    cost: CostBudget
});
impl Validate for RemainingBudgets {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.lineage_id)?;
        artifact(&self.policy)?;
        artifact(&self.ledger_checkpoint)?;
        self.cost.validate()
    }
}
// An attachment to a normal fresh grant. This is not the grant, a launch command
// or evidence that the wait producer succeeded. Native provider resume optional.
record!(ContinuationAttachment {admission_id:Uuid,continuation:ContinuationRef,predecessor:Binding,successor:Binding,fresh_offer_digest:Digest,outcome:OutcomeRef,retained:Vec<RetentionReceipt>,pending_operation_ids:Vec<Uuid>,held_task_message_ids:Vec<Uuid>,remaining:RemainingBudgets,native_resume:Option<ArtifactRef>});
impl Validate for ContinuationAttachment {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.admission_id)?;
        self.continuation.validate()?;
        self.predecessor.validate()?;
        self.successor.validate()?;
        self.outcome.validate()?;
        ensure(
            self.outcome.wait == self.continuation.wait,
            "continuation wait outcome",
        )?;
        ensure(
            self.predecessor.attempt_id != self.successor.attempt_id
                && self.predecessor.session_id() != self.successor.session_id(),
            "wake uses fresh attempt and session",
        )?;
        ensure(
            self.predecessor.task_id == self.successor.task_id
                && self.predecessor.project_id == self.successor.project_id
                && self.predecessor.lineage_id == self.successor.lineage_id,
            "continuation lineage",
        )?;
        validate_retention(&self.retained, &self.predecessor)?;
        ensure(
            has_migratable_refs(&self.retained),
            "wake retained source and checkpoint",
        )?;
        unique_ids(&self.pending_operation_ids, MAX_REFS)?;
        unique_ids(&self.held_task_message_ids, MAX_REFS)?;
        self.remaining.validate()?;
        ensure(
            self.remaining.lineage_id == self.predecessor.lineage_id,
            "budget lineage",
        )?;
        if let Some(r) = &self.native_resume {
            artifact(r)?;
        }
        Ok(())
    }
}
// Report of an authenticated AQ acknowledgment receipt. Merely placing bytes in
// bootstrap instructions cannot construct a legitimate runtime acknowledgment.
record!(BootstrapAck {
    admission_id: Uuid,
    continuation: ContinuationRef,
    successor: Binding,
    attachment_digest: Digest,
    outcome: OutcomeRef,
    acknowledgment_receipt: ArtifactRef
});
impl Validate for BootstrapAck {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.admission_id)?;
        self.continuation.validate()?;
        self.successor.validate()?;
        self.outcome.validate()?;
        ensure(
            self.outcome.wait == self.continuation.wait,
            "acknowledgment wait",
        )?;
        artifact(&self.acknowledgment_receipt)
    }
}
