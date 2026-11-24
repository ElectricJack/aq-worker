use super::publication::{AUTH, private_directory, read_member};
use super::*;
fn info(directory: &Path) -> CandidatePreparedInfo {
    let meta = std::fs::metadata(directory).unwrap();
    let reference = ArtifactRef {
        id: aq_session_protocol::Text::new("synthetic-owner").unwrap(),
        version: aq_session_protocol::Text::new("1").unwrap(),
        digest: Digest::of(b"no authority"),
    };
    CandidatePreparedInfo {
        owner_intent: reference,
        preparation_digest: Digest::of(b"fixture-plan"),
        capsule_id: Uuid::new_v4(),
        original_deadline_boot_ms: ownership::raw_boot_ms() + 5000,
        guardian: ProcessEvidence {
            pid: 10,
            start_ticks: 1,
        },
        namespace_init: ProcessEvidence {
            pid: 11,
            start_ticks: 2,
        },
        exec_stub: ProcessEvidence {
            pid: 12,
            start_ticks: 3,
        },
        origin: "http://127.0.0.1:32123".into(),
        argument_prefix: vec!["fixture-only".into()],
        auth_directory: (meta.dev(), meta.ino()),
    }
}
#[test]
fn synthetic_publication_is_private_and_commit_metadata_does_not_contain_capability_bytes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let info = info(root.path());
    let cap = format!(
        "aq-responses-{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let published =
        CandidatePublishedInputs::publish_synthetic(&info, root.path(), cap.as_bytes()).unwrap();
    let commit = CandidateCommit::new(
        &info,
        published,
        CandidateStaticRecipe::new(&info),
        Digest::of(b"synthetic transport"),
    )
    .unwrap();
    assert!(!serde_json::to_string(&commit).unwrap().contains(&cap));
    assert_eq!(
        std::fs::metadata(root.path().join(AUTH)).unwrap().mode() & 0o7777,
        0o600
    );
    let directory = private_directory(root.path(), info.auth_directory).unwrap();
    let (auth, _) = read_member(&directory, AUTH).unwrap();
    crate::private_inputs::verify_local_capability_auth(&auth, &Digest::of(cap.as_bytes()))
        .unwrap();
    assert!(
        CandidatePublishedInputs::publish_synthetic(&info, root.path(), cap.as_bytes()).is_err()
    );
}
#[test]
fn correctly_rehashed_conflicting_scope_and_recipe_still_refuse() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let info = info(root.path());
    let cap = format!(
        "aq-responses-{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let published =
        CandidatePublishedInputs::publish_synthetic(&info, root.path(), cap.as_bytes()).unwrap();
    let commit = CandidateCommit::new(
        &info,
        published,
        CandidateStaticRecipe::new(&info),
        Digest::of(b"synthetic"),
    )
    .unwrap();
    let mut changed = commit.0.clone();
    changed.scope.origin = "http://127.0.0.1:1".into();
    changed.digest = commit_digest(&changed).unwrap();
    assert!(validate_record(&changed, &info).is_err());
    let mut changed = commit.0;
    changed
        .recipe
        .environment
        .insert("HOME".into(), "/unqualified".into());
    changed.digest = commit_digest(&changed).unwrap();
    assert!(validate_record(&changed, &info).is_err());
}
