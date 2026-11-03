use aq_admission_sandbox::sandbox::{self, Fixture, FixtureCase};
use std::collections::BTreeSet;

#[test]
fn permit_fixture_conformance() {
    let fixtures: Vec<Fixture> = serde_json::from_str(include_str!(
        "../../../protocol/aq-admission/artifacts/sandbox-v1.fixtures.json"
    ))
    .unwrap();
    let names: BTreeSet<_> = fixtures.iter().map(|f| f.name.as_str()).collect();
    for required in [
        "valid_preparation",
        "valid_execution",
        "other_attempt",
        "expired",
        "wrong_boot",
        "execution_before_grant",
        "readoption_replay",
        "unknown_required_capability",
        "phase_v2_renewal",
        "phase_refresh_renewal",
        "phase_renewal_after_readoption",
        "renewal_exceeds_original_hard_deadline",
        "renewal_wrong_nonce",
        "renewal_delivery_expiry_mismatch",
    ] {
        assert!(names.contains(required), "missing {required}");
    }
    assert_eq!(names.len(), fixtures.len(), "duplicate fixture names");
    let other = fixtures.iter().find(|f| f.name == "other_attempt").unwrap();
    let FixtureCase::Permit {
        permit, context, ..
    } = &other.case
    else {
        panic!("permit fixture required")
    };
    assert_ne!(permit.binding.attempt_id, context.binding.attempt_id);
    for fixture in fixtures {
        assert_eq!(
            sandbox::validate_fixture(&fixture),
            fixture.expected,
            "{}",
            fixture.name
        );
    }
}
