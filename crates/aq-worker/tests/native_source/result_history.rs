use aq_agent_result_local as wire;
use aq_session_protocol::Digest;
use aq_worker::native::{
    journal::RuntimeIdentity,
    result::{ResultInstallationPins, history},
};
use history::ClaimRecord;
use uuid::Uuid;
#[path = "../support/result_history.rs"]
mod fixture;
use fixture::history as recorded_history;

#[test]
fn result_history_exact_claim_identity_order_and_canonical_bytes() {
    let mut h = recorded_history();
    h.validate().unwrap();
    let value = wire::Submission::new(
        h.scope.binding.clone(),
        wire::SubmissionInput {
            schema_version: 1,
            conclusion: wire::Conclusion::new("Explicit claim").unwrap(),
        },
    )
    .unwrap();
    let bytes = wire::encode(&value).unwrap();
    h.claim = Some(ClaimRecord {
        bytes: bytes.clone(),
        bytes_digest: Digest::of(&bytes),
        submission_digest: value.digest().unwrap(),
        recorded_revision: 4,
    });
    h.intake_closed = true;
    h.validate().unwrap();
    assert!(matches!(
        h.acknowledgment().unwrap().unwrap().phase,
        wire::Phase::Accepted
    ));
    h.claim.as_mut().unwrap().recorded_revision = 3;
    assert!(h.validate().is_err());
    h.claim.as_mut().unwrap().recorded_revision = 4;
    h.claim.as_mut().unwrap().bytes.push(b' ');
    assert!(h.validate().is_err());
}
#[test]
fn result_history_protects_scope_budget_and_does_not_project_unclaimed_success() {
    let mut h = recorded_history();
    assert!(h.acknowledgment().unwrap().is_none());
    h.scope.operations.export = h.scope.operations.checkpoint;
    assert!(h.validate().is_err());
    h.scope.operations.export = Uuid::from_u128(41);
    h.scope.original_deadline_ns = 201;
    assert!(h.validate().is_err());
    h.scope.original_deadline_ns = 100;
    h.scope.reserved_disk_bytes = 29999;
    assert!(h.validate().is_err());
}
#[test]
fn result_format11_header_is_refused_before_pending_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("journal");
    let limits = aq_worker::native::journal::Limits {
        slots: 1,
        memory_bytes: 1000,
        cpu_millis: 1000,
        disk_bytes: 100000,
    };
    let node = Uuid::new_v4();
    let journal = aq_worker::native::journal::Journal::create(&root, node, limits.clone()).unwrap();
    drop(journal);
    let file = root.join("journal.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    value["state"]["format"] = 11.into();
    let raw = serde_json::to_vec(&value).unwrap();
    std::fs::write(&file, &raw).unwrap();
    let pending = root.join("journal.pending");
    std::fs::write(&pending, b"preserve ambiguous old-format evidence").unwrap();
    assert!(aq_worker::native::journal::Journal::open(&root, node, limits).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), raw);
    assert_eq!(
        std::fs::read(&pending).unwrap(),
        b"preserve ambiguous old-format evidence"
    );
}

/// Spec 0016 §5.2 (S6): a pushed result names its task ref and head, or the conflict; no receipt.
#[test]
fn a_pushed_result_names_its_ref_and_head_instead_of_a_retention_receipt() {
    let mut h = recorded_history();
    assert!(h.source.receipt_ids().is_err() && h.source.pushed_headers().is_empty());
    let (task_ref, commit) = ("refs/heads/aq/t".to_owned(), "b".repeat(40));
    h.source.pushed = Some(aq_node_git::Pushed::Pushed { task_ref, commit });
    assert!(h.source.receipt_ids().unwrap().is_empty());
    let named = [
        ("AQ-Pushed-Ref", "refs/heads/aq/t".into()),
        ("AQ-Pushed-Head", "b".repeat(40)),
    ];
    assert_eq!(h.source.pushed_headers(), named);
    // A moved task ref is reported as the conflict alone, never as a pushed head.
    h.source.pushed = Some(aq_node_git::Pushed::Conflict);
    assert!(h.source.receipt_ids().unwrap().is_empty());
    assert_eq!(
        h.source.pushed_headers(),
        [("AQ-Push-Conflict", "true".into())]
    );
}
