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
        "ended_report_result",
        "ended_report_result_at_expiry",
        "ended_report_result_after_expiry",
        "ended_report_result_at_grant_and_hard_deadline",
        "ended_report_result_after_hard_deadline",
        "ended_preparation_cannot_report_result",
        "ended_stopped_exact_replay",
        "ended_stopped_changed_replay",
        "ended_stopped_changed_operation",
        "ended_stopped_expired_replay",
        "ended_stopped_foreign_replay",
        "ended_stopped_preparation_replay",
    ] {
        assert!(names.contains(required), "missing {required}");
    }
    assert_eq!(names.len(), fixtures.len(), "duplicate fixture names");
    for operation in [
        "fetch_preparation_material",
        "observe",
        "upload_artifact",
        "started",
        "renew",
        "terminal",
        "fetch_execution_material",
        "accept_offer",
        "register_incarnation",
        "prepared",
        "mailbox_poll",
        "mailbox_ack",
    ] {
        assert!(names.contains(format!("ended_refuses_{operation}").as_str()));
    }
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
