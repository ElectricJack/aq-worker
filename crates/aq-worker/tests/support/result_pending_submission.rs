//! Pending-submission recovery unit tests of `native::result`, kept outside `src/` (build budget).
use super::*;
use crate::native::result::stopped::stopped_submission;

#[test]
fn pane_exit_keeps_the_exact_unpolled_claim_after_intake_closes() {
    let mut history = history();
    let (_dir, original) = pending_submission(&mut history);
    assert!(stopped_submission(&history).is_err());
    history.intake_closed = true;

    let spool = history.scope.root.join(wire::SUBMISSIONS_DIRECTORY);
    let writer = std::fs::File::open(spool.join(wire::WRITER_LOCK_FILE)).unwrap();
    fs2::FileExt::try_lock_exclusive(&writer).unwrap();
    assert!(stopped_submission(&history).is_err());
    drop(writer);

    let claim = stopped_submission(&history).unwrap().unwrap();
    assert_eq!(claim, original);
    assert_eq!(
        wire::encode(&claim).unwrap(),
        std::fs::read(spool.join(wire::SUBMISSION_FILE)).unwrap()
    );
    assert!(history.claim.is_none());
}

#[test]
fn retained_submission_requires_the_original_directories_and_binding() {
    let mut h = history();
    let (temp, expected) = pending_submission(&mut h);
    assert_eq!(retained_submission(&h.scope).unwrap(), Some(expected));
    let original = h.scope.binding.clone();
    h.scope.binding.nonce = Uuid::new_v4();
    assert!(retained_submission(&h.scope).is_err());
    h.scope.binding = original;
    let moved = temp.path().join("moved-result");
    std::fs::rename(&h.scope.root, &moved).unwrap();
    h.scope.root = temp.path().join("fresh-result");
    let fresh = ChannelStorage::create(&h.scope.root, &temp.path().join("workspace")).unwrap();
    assert!(retained_submission(&h.scope).is_err());
    drop(fresh);
}
