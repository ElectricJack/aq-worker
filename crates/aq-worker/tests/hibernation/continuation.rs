#[test]
fn continuation_hydrates_checkpoint_in_new_sandbox() {
    // Node A hibernates, then disappears entirely (no local cache survives).
    let a = node();
    dirty_workspace(&a.layout);
    let old_secrets = plant_credentials(&a.layout);
    let expected_status = git(&a.layout.workspace(), &["status", "--porcelain"]);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    drop(a);

    // AQ grants a fresh attempt on node B with the continuation attachment.
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    let mut aq_b = FakeAq::new(successor());
    aq_b.peer.receive(&wake).unwrap();
    let b = node();
    let mut launcher = FakeLauncher::default();
    let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
    let woken = boot.wake(&wake, &retention, &mut launcher).unwrap();

    assert_restored_worktree(&b, &expected_status);
    assert_bootstrap_instructions(&b, &woken);
    // A fresh session started for the successor with fresh credentials only.
    assert_eq!(launcher.launches.len(), 1);
    assert_eq!(launcher.launches[0].0, successor());
    for path in b.layout.paths() {
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert!(fresh.starts_with(&format!("fresh-{}", successor().session_id())));
        assert!(!old_secrets.contains(&fresh));
    }

    // Operations stay closed until AQ records the exact BootstrapAck.
    assert_eq!(woken.gate, OperationsGate::AwaitingBootstrapAck);
    assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    assert!(!aq_b.peer.bootstrap_acknowledged());
    let gate = boot
        .acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
        .unwrap();
    assert_eq!(gate, OperationsGate::Open);
    assert!(aq_b.peer.bootstrap_acknowledged());
    assert_eq!(aq_b.sent.last(), Some(&woken.ack_message_id));

    // A replayed attachment recovers the same wake without relaunching.
    let again = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
        .wake(&wake, &retention, &mut launcher)
        .unwrap();
    assert_eq!(again.gate, OperationsGate::Open);
    assert_eq!(launcher.launches.len(), 1);
}

#[test]
fn lost_bootstrap_ack_retries_with_same_receipt_identity() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    let mut aq_b = FakeAq::new(successor());
    aq_b.peer.receive(&wake).unwrap();
    let b = node();
    let mut launcher = FakeLauncher::default();
    let woken = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
        .wake(&wake, &retention, &mut launcher)
        .unwrap();
    let ack = ack_for(&woken, &attachment);

    aq_b.drop_receipts = 1;
    let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
    assert!(boot.acknowledge(&ack, &mut aq_b).is_err());
    assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    // Crash/restart: the retry reuses the journaled message id; AQ dedups it.
    let gate = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
        .acknowledge(&ack, &mut aq_b)
        .unwrap();
    assert_eq!(gate, OperationsGate::Open);
    assert_eq!(aq_b.sent, vec![woken.ack_message_id, woken.ack_message_id]);
}

#[test]
fn stale_bootstrap_ack_cannot_open_operations() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    let mut aq_b = FakeAq::new(successor());
    aq_b.peer.receive(&wake).unwrap();
    let b = node();
    let mut launcher = FakeLauncher::default();
    let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
    let woken = boot.wake(&wake, &retention, &mut launcher).unwrap();

    let mut from_predecessor = ack_for(&woken, &attachment);
    from_predecessor.successor = predecessor();
    let mut old_generation = ack_for(&woken, &attachment);
    old_generation.continuation.generation = 2;
    let mut other_attachment = ack_for(&woken, &attachment);
    other_attachment.attachment_digest = Digest::of(b"other attachment");
    for stale in [from_predecessor, old_generation, other_attachment] {
        assert!(boot.acknowledge(&stale, &mut aq_b).is_err());
        assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
    }
    assert!(
        aq_b.sent.is_empty(),
        "stale acks never reach AQ as this attempt"
    );
    assert!(!aq_b.peer.bootstrap_acknowledged());
    // The exact ack still opens operations afterwards.
    assert_eq!(
        boot.acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
            .unwrap(),
        OperationsGate::Open
    );
}

#[test]
fn missing_or_corrupt_retained_artifact_refuses_wake() {
    for (lose, corrupt) in [
        (Some(ArtifactRole::Checkpoint), None),
        (Some(ArtifactRole::Source), None),
        (None, Some(ArtifactRole::Source)),
        (None, Some(ArtifactRole::Checkpoint)),
    ] {
        let a = node();
        dirty_workspace(&a.layout);
        let (parked, mut retention, _aq, _ports) = hibernate(&a);
        retention.lose = lose.clone();
        retention.corrupt = corrupt.clone();
        let wake = continuation_packet(&parked);
        let b = node();
        let mut launcher = FakeLauncher::default();
        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
        assert!(
            boot.wake(&wake, &retention, &mut launcher).is_err(),
            "{lose:?} {corrupt:?}"
        );
        assert_unavailable_inputs(&b, &launcher, &boot);
    }
    // A wake attachment for another grant is refused outright.
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let b = node();
    let mut launcher = FakeLauncher::default();
    let other_grant = predecessor();
    assert!(
        ContinuationBootstrap::new(&b.journal, other_grant, b.layout.clone())
            .wake(&continuation_packet(&parked), &retention, &mut launcher)
            .is_err()
    );
}

#[test]
fn continuation_recovers_at_every_boundary() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let wake = continuation_packet(&parked);
    let attachment = attachment_of(&wake);
    for boundary in CONTINUATION_BOUNDARIES {
        let mut aq_b = FakeAq::new(successor());
        aq_b.peer.receive(&wake).unwrap();
        let b = node();
        let mut launcher = FakeLauncher::default();
        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone())
            .with_boundaries(Boundaries::crash_after(boundary));
        let crashed = boot.wake(&wake, &retention, &mut launcher).and_then(|w| {
            boot.acknowledge(&ack_for(&w, &attachment), &mut aq_b)
                .map(|_| w)
        });
        let err = crashed.unwrap_err();
        assert!(
            err.downcast_ref::<Crashed>().is_some(),
            "{boundary}: {err:#}"
        );
        // Only a journaled BootstrapAcknowledgmentRecorded receipt opens the gate.
        let expected = if *boundary == "ack_recorded" {
            OperationsGate::Open
        } else {
            OperationsGate::AwaitingBootstrapAck
        };
        assert_eq!(boot.gate().unwrap(), expected, "{boundary}");

        let mut boot = ContinuationBootstrap::new(&b.journal, successor(), b.layout.clone());
        let woken = boot.wake(&wake, &retention, &mut launcher).unwrap();
        let gate = boot
            .acknowledge(&ack_for(&woken, &attachment), &mut aq_b)
            .unwrap();
        assert_eq!(gate, OperationsGate::Open, "{boundary}");
        assert_continuation_recovered(&woken, &aq_b, &launcher, boundary);
        assert_eq!(
            std::fs::read_to_string(b.layout.workspace().join("untracked.txt")).unwrap(),
            "new work\n",
            "{boundary}"
        );
    }
}
fn assert_restored_worktree(b: &Node, expected_status: &str) {
    // Source, including dirty and untracked work, is restored from retention.
    let repo = b.layout.workspace();
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
        "committed\ndirty edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("untracked.txt")).unwrap(),
        "new work\n"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), expected_status);
    assert!(
        expected_status.contains(" M tracked.txt") && expected_status.contains("?? untracked.txt")
    );
}

fn assert_bootstrap_instructions(b: &Node, woken: &Woken) {
    // The summary and exact generation are in the bootstrap instructions.
    assert_eq!(woken.summary, summary_claim(SUMMARY));
    let bootstrap: Value =
        serde_json::from_slice(&std::fs::read(b.layout.work_root.join(BOOTSTRAP_FILE)).unwrap())
            .unwrap();
    assert_eq!(bootstrap["continuation"]["generation"], 1);
    assert_eq!(bootstrap["summary"]["summary"]["text"], SUMMARY);
    assert_eq!(
        bootstrap["attachment_digest"],
        woken.attachment_digest.as_str()
    );
}

fn assert_unavailable_inputs(b: &Node, launcher: &FakeLauncher, boot: &ContinuationBootstrap) {
    assert!(
        launcher.launches.is_empty(),
        "no session without verified inputs"
    );
    assert!(!b.layout.workspace().exists(), "nothing hydrated");
    assert_eq!(boot.gate().unwrap(), OperationsGate::AwaitingBootstrapAck);
}

fn assert_continuation_recovered(
    woken: &Woken,
    aq_b: &FakeAq,
    launcher: &FakeLauncher,
    boundary: &str,
) {
    assert_eq!(
        launcher.launches.len(),
        1,
        "{boundary}: exactly one fresh session"
    );
    assert!(aq_b.peer.bootstrap_acknowledged(), "{boundary}");
    assert!(
        aq_b.sent.iter().all(|id| *id == woken.ack_message_id),
        "{boundary}"
    );
}
