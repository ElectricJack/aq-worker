use super::{ArtifactPin, EXECUTION_PROFILE};
use serde_json::{Value, json};
use uuid::Uuid;

const VALIDATOR_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub(super) fn validator() -> ArtifactPin {
    ArtifactPin {
        id: "cargo-test".into(),
        version: "1".into(),
        digest: VALIDATOR_DIGEST.into(),
    }
}

pub(super) fn claim_json(operation: Uuid) -> Value {
    json!({
        "operation_id": operation,
        "attempt_id": Uuid::new_v4(),
        "source_operation_id": Uuid::from_u128(7),
        "subject": {"id": "task-1", "revision": 3},
        "check": {"id": "unit", "validator": validator()},
        "source": {"format": "sha1", "hex": "0123456789abcdef0123456789abcdef01234567"},
        "execution_profile": EXECUTION_PROFILE,
    })
}

pub(super) const NODE: Uuid = Uuid::from_u128(0x11);
