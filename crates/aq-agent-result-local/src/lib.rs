//! Pure local claims/projections only. Matching JSON, file presence, a nonce,
//! kernel lock or lease projection cannot construct execution/stop authority.
use aq_session_protocol::{
    ArtifactRef, ContractError, Digest, LaunchBinding, SessionIdentity, Text, Validate,
    canonical_bytes, canonical_digest,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CONTRACT: &str = "aq-agent-result-local-1";
pub const MAX_BYTES: usize = 8192;
pub const MAX_INPUT_BYTES: usize = 4096;
pub const MAX_CONCLUSION_BYTES: usize = 512;
pub const DEFINITION_BYTES: &[u8] = include_bytes!("../artifacts/contract.json");
/// Static instruction bytes for the centrally counted startup module. Runtime
/// mappers must not append this text after the admission/counting seal.
pub const STARTUP_TEXT: &str = include_str!("../artifacts/startup.md");
pub fn startup_artifact() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("aq.agent-result-startup").expect("constant startup id"),
        version: Text::new("1").expect("constant startup version"),
        digest: Digest::of(STARTUP_TEXT.as_bytes()),
    }
}
/// Actual immutable public definition bytes, not qualification/live authority.
pub fn artifact() -> ArtifactRef {
    ArtifactRef {
        id: Text::new("aq.agent-result-local").expect("constant definition id"),
        version: Text::new("1").expect("constant definition version"),
        digest: Digest::of(DEFINITION_BYTES),
    }
}
pub const RESULT_DIR_ENV: &str = "AQ_AGENT_RESULT_DIR";
pub const COMMAND_ENV: &str = "AQ_AGENT_RESULT_COMMAND";
pub const SUBMISSIONS_DIRECTORY: &str = "submissions";
pub const WRITER_LOCK_FILE: &str = "writer.lock";
pub const BINDING_FILE: &str = "binding.json";
pub const PRESENCE_FILE: &str = "presence.json";
pub const OWNER_LOCK_FILE: &str = "owner.lock";
pub const SUBMISSION_FILE: &str = "submission.json";
pub const ACK_FILE: &str = "ack.json";
fn require(ok: bool, why: &'static str) -> Result<(), ContractError> {
    if ok {
        Ok(())
    } else {
        Err(ContractError::Invalid(why))
    }
}
fn validate_artifact(value: &ArtifactRef) -> Result<(), ContractError> {
    value.id.validate()?;
    value.version.validate()
}
/// Existing ResultReport Label semantics: one nonblank UTF8 line, <=512 bytes.
/// This is an agent's narrative claim, never a trusted check/review receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Conclusion(Text);
impl Conclusion {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        require(
            !value.trim().is_empty() && value.len() <= MAX_CONCLUSION_BYTES,
            "result conclusion bound",
        )?;
        Ok(Self(Text::new(value)?))
    }
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}
impl<'de> Deserialize<'de> for Conclusion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub schema_version: u32,
    pub launch: LaunchBinding,
    pub execution_record_digest: Digest,
    pub expected_outcome: ArtifactRef,
    pub operation_id: Uuid,
    pub report_id: Uuid,
    pub nonce: Uuid,
}
impl Validate for Binding {
    fn validate(&self) -> Result<(), ContractError> {
        self.launch.validate()?;
        validate_artifact(&self.expected_outcome)?;
        require(
            self.schema_version == 1
                && matches!(self.launch.session, SessionIdentity::Remote { .. })
                && self.expected_outcome.id.as_str() == "aq.expected-outcome"
                && self.expected_outcome.version.as_str() == "1"
                && !self.operation_id.is_nil()
                && !self.report_id.is_nil()
                && !self.nonce.is_nil()
                && self.operation_id != self.report_id
                && self.nonce != self.operation_id
                && self.nonce != self.report_id,
            "local result binding",
        )
    }
}
impl Binding {
    pub fn digest(&self) -> Result<Digest, ContractError> {
        canonical_digest(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmissionInput {
    pub schema_version: u32,
    pub conclusion: Conclusion,
}
impl Validate for SubmissionInput {
    fn validate(&self) -> Result<(), ContractError> {
        require(self.schema_version == 1, "result input version")
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub schema_version: u32,
    pub binding: Binding,
    pub conclusion: Conclusion,
}
impl Validate for Submission {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        require(self.schema_version == 1, "result submission version")
    }
}
impl Submission {
    pub fn new(binding: Binding, input: SubmissionInput) -> Result<Self, ContractError> {
        input.validate()?;
        let value = Self {
            schema_version: 1,
            binding,
            conclusion: input.conclusion,
        };
        encode(&value)?;
        Ok(value)
    }
    pub fn digest(&self) -> Result<Digest, ContractError> {
        canonical_digest(self)
    }
}
/// Refusal/presence projection only. The worker owns the actual current grant.
/// This immutable original hard ceiling is not renewed by this local channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Presence {
    pub schema_version: u32,
    pub binding_digest: Digest,
    pub kernel_boot_id: Uuid,
    pub original_execution_deadline_boot_ns: u64,
    pub original_hard_deadline_boot_ns: u64,
}
impl Validate for Presence {
    fn validate(&self) -> Result<(), ContractError> {
        require(
            self.schema_version == 1
                && !self.kernel_boot_id.is_nil()
                && self.original_execution_deadline_boot_ns > 0
                && self.original_execution_deadline_boot_ns <= self.original_hard_deadline_boot_ns,
            "local result presence projection",
        )
    }
}
impl Presence {
    pub fn matches_at(
        &self,
        binding: &Binding,
        kernel_boot_id: Uuid,
        now_boot_ns: u64,
    ) -> Result<bool, ContractError> {
        self.validate()?;
        Ok(self.binding_digest == binding.digest()?
            && self.kernel_boot_id == kernel_boot_id
            && now_boot_ns < self.original_execution_deadline_boot_ns
            && now_boot_ns < self.original_hard_deadline_boot_ns)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCode {
    OwnerUnavailable,
    ExecutionUnavailable,
    StorageUncertain,
    StopUnknown,
    SourceRetentionUnknown,
    ReportUnknown,
    ScopeChanged,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Phase {
    Accepted,
    Finalizing,
    Recorded { receipt: ArtifactRef },
    NeedsReconciliation { code: RecoveryCode },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgment {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub report_id: Uuid,
    pub binding_digest: Digest,
    pub submission_digest: Digest,
    pub phase: Phase,
}
impl Validate for Acknowledgment {
    fn validate(&self) -> Result<(), ContractError> {
        require(
            self.schema_version == 1
                && !self.operation_id.is_nil()
                && !self.report_id.is_nil()
                && self.operation_id != self.report_id,
            "local result acknowledgment",
        )?;
        if let Phase::Recorded { receipt } = &self.phase {
            validate_artifact(receipt)?;
            require(
                receipt.id.as_str() == format!("aq.result-record/{}", self.report_id)
                    && receipt.version.as_str() == "1",
                "result recorded reference",
            )?;
        }
        Ok(())
    }
}
impl Acknowledgment {
    /// Association only. The worker must verify its real B1 response before
    /// publishing Recorded; this projection never replaces that owner check.
    pub fn validate_for(&self, submission: &Submission) -> Result<(), ContractError> {
        self.validate()?;
        submission.validate()?;
        require(
            self.operation_id == submission.binding.operation_id
                && self.report_id == submission.binding.report_id
                && self.binding_digest == submission.binding.digest()?
                && self.submission_digest == submission.digest()?,
            "result acknowledgment scope/digest",
        )
    }
}
pub fn encode<T: Serialize + Validate>(value: &T) -> Result<Vec<u8>, ContractError> {
    let bytes = canonical_bytes(value)?;
    require(bytes.len() <= MAX_BYTES, "local result envelope byte bound")?;
    Ok(bytes)
}
pub fn decode<T: serde::de::DeserializeOwned + Serialize + Validate>(
    bytes: &[u8],
) -> Result<T, ContractError> {
    require(bytes.len() <= MAX_BYTES, "local result input byte bound")?;
    let value: T =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("local result JSON"))?;
    encode(&value)?;
    Ok(value)
}
pub fn decode_input(bytes: &[u8]) -> Result<SubmissionInput, ContractError> {
    require(
        bytes.len() <= MAX_INPUT_BYTES,
        "result request input byte bound",
    )?;
    decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }
    fn binding() -> Binding {
        let session = SessionIdentity::Remote {
            node_id: id(1),
            boot_id: id(2),
            attempt_id: id(3),
            assignment_generation: 4,
            session_id: id(5),
        };
        Binding {
            schema_version: 1,
            launch: LaunchBinding {
                schema_version: 1,
                session,
                offer_id: id(6),
                attempt_id: id(3),
                assignment_generation: 4,
                execution_spec_digest: Digest::of(b"spec"),
                launch_manifest_digest: Digest::of(b"manifest"),
                graph_access_profile_digest: Digest::of(b"profile"),
            },
            execution_record_digest: Digest::of(
                b"actual worker must verify this, fixture is only data",
            ),
            expected_outcome: ArtifactRef {
                id: Text::new("aq.expected-outcome").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"outcome"),
            },
            operation_id: id(7),
            report_id: id(8),
            nonce: id(9),
        }
    }
    #[test]
    fn conclusion_enforces_full_utf8_byte_bound_and_existing_label_semantics() {
        assert!(Conclusion::new("a".repeat(512)).is_ok());
        assert!(Conclusion::new("😀".repeat(128)).is_ok());
        for bad in [
            "a".repeat(513),
            "😀".repeat(129),
            " \t".into(),
            "line\nline".into(),
            "NUL\0".into(),
        ] {
            assert!(Conclusion::new(bad).is_err());
        }
    }
    #[test]
    fn strict_input_never_accepts_duplicate_or_privileged_fields() {
        assert!(
            decode_input(
                br#"{"schema_version":1,"conclusion":"Changes; check command exit0; limits none"}"#
            )
            .is_ok()
        );
        for bad in [
            br#"{"schema_version":1,"conclusion":"a","conclusion":"b"}"#.as_slice(),
            br#"{"schema_version":1,"conclusion":"a","stop":true}"#.as_slice(),
            br#"{"schema_version":1,"conclusion":"a","source_receipt_ids":[]}"#.as_slice(),
        ] {
            assert!(decode_input(bad).is_err());
        }
        assert!(decode_input(&vec![b' '; MAX_INPUT_BYTES + 1]).is_err());
    }
    #[test]
    fn canonical_frozen_submission_and_ack_pins_reject_other_incarnation_or_bytes() {
        let first = Submission::new(
            binding(),
            SubmissionInput {
                schema_version: 1,
                conclusion: Conclusion::new("Changes; cargo test -p example exit0; limits none")
                    .unwrap(),
            },
        )
        .unwrap();
        let bytes = encode(&first).unwrap();
        let reopened: Submission = decode(&bytes).unwrap();
        assert_eq!(first.digest().unwrap(), reopened.digest().unwrap());
        assert_eq!(bytes, encode(&reopened).unwrap());
        let ack = Acknowledgment {
            schema_version: 1,
            operation_id: first.binding.operation_id,
            report_id: first.binding.report_id,
            binding_digest: first.binding.digest().unwrap(),
            submission_digest: first.digest().unwrap(),
            phase: Phase::Accepted,
        };
        ack.validate_for(&first).unwrap();
        let mut other = first.clone();
        other.binding.nonce = id(10);
        assert!(ack.validate_for(&other).is_err());
        let mut other = first.clone();
        other.conclusion = Conclusion::new("different claim").unwrap();
        assert!(ack.validate_for(&other).is_err());
        let mut other = first;
        other.binding.expected_outcome.digest = Digest::of(b"different expectation");
        assert!(ack.validate_for(&other).is_err());
        assert!(decode::<Submission>(&vec![b' '; MAX_BYTES + 1]).is_err());
    }
    #[test]
    fn recorded_is_an_exact_record_reference_not_a_task_completion_claim() {
        let submission = Submission::new(
            binding(),
            SubmissionInput {
                schema_version: 1,
                conclusion: Conclusion::new("ready for review").unwrap(),
            },
        )
        .unwrap();
        let mut ack = Acknowledgment {
            schema_version: 1,
            operation_id: submission.binding.operation_id,
            report_id: submission.binding.report_id,
            binding_digest: submission.binding.digest().unwrap(),
            submission_digest: submission.digest().unwrap(),
            phase: Phase::Recorded {
                receipt: ArtifactRef {
                    id: Text::new(format!("aq.result-record/{}", submission.binding.report_id))
                        .unwrap(),
                    version: Text::new("1").unwrap(),
                    digest: Digest::of(
                        b"actual worker must verify remote receipt, this fixture is data",
                    ),
                },
            },
        };
        ack.validate_for(&submission).unwrap();
        ack.report_id = id(99);
        assert!(ack.validate_for(&submission).is_err());
    }
    #[test]
    fn presence_only_refuses_original_boot_ceiling_or_scope_changes() {
        let scope = binding();
        let p = Presence {
            schema_version: 1,
            binding_digest: scope.digest().unwrap(),
            kernel_boot_id: id(20),
            original_execution_deadline_boot_ns: 150,
            original_hard_deadline_boot_ns: 200,
        };
        assert!(p.matches_at(&scope, id(20), 149).unwrap());
        assert!(!p.matches_at(&scope, id(20), 150).unwrap());
        assert!(
            !p.matches_at(&scope, id(20), 199).unwrap(),
            "hard ceiling does not extend the original lease"
        );
        let mut invalid = p.clone();
        invalid.original_execution_deadline_boot_ns = 201;
        assert!(invalid.validate().is_err());
        assert!(!p.matches_at(&scope, id(21), 100).unwrap());
        let mut other = scope;
        other.execution_record_digest = Digest::of(b"new execution");
        assert!(!p.matches_at(&other, id(20), 100).unwrap());
    }
}
