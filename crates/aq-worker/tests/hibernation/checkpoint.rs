#[test]
fn stop_proof_artifact_is_retained_with_sha256_of_canonical_json() {
    let a = node();
    dirty_workspace(&a.layout);
    let (parked, retention, _aq, _ports) = hibernate(&a);
    let stop = &parked.report.stop;
    let StopObservation::ReportedStopped {
        proof: reference,
        guardian_instance_id,
        writer_set_digest,
        ..
    } = stop
    else {
        panic!("stop was reported");
    };
    assert_eq!(*guardian_instance_id, GUARDIAN);
    assert_eq!(*writer_set_digest, writer_set());

    // The parking report carries the result-role receipt for exactly that proof.
    let receipt = stop_proof_receipt(stop, &parked.report.central_receipts)
        .expect("stop proof retained with role result");
    assert_eq!(receipt.role, ArtifactRole::Result);
    assert_eq!(
        receipt.binding_digest,
        canonical_digest(&predecessor()).unwrap()
    );

    // ArtifactRef.digest is the SHA-256 of the canonical JSON body.
    let bytes = retention.fetch(receipt).unwrap();
    let expected = StopProof {
        guardian_instance_id: GUARDIAN,
        writer_set_digest: writer_set(),
        stopped_at_boot_ms: 1200,
    };
    assert_eq!(bytes, expected.to_bytes().unwrap());
    assert_eq!(reference.digest, Digest::of(&bytes));
    assert_eq!(reference.digest, expected.digest().unwrap());
    assert_eq!(receipt.bytes, bytes.len() as u64);
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        format!(
            "{{\"guardian_instance_id\":\"{GUARDIAN}\",\"stopped_at_boot_ms\":1200,\"writer_set_digest\":\"{}\"}}",
            writer_set().as_str()
        )
    );

    // The successor verifies guardian instance and writer set from the bytes.
    assert_eq!(
        verify_stop_proof(stop, &bytes, GUARDIAN, &writer_set()).unwrap(),
        expected
    );
    assert!(verify_stop_proof(stop, &bytes, Uuid::from_u128(0x99), &writer_set()).is_err());
    assert!(verify_stop_proof(stop, &bytes, GUARDIAN, &Digest::of(b"other")).is_err());
    assert_proof_upload_recovery(&expected);
}
fn assert_proof_upload_recovery(expected: &StopProof) {
    // Recovery after a crash mid-proof-upload reuses the journaled artifact.
    for boundary in ["stop_recorded", "upload_chunk:result"] {
        let b = node();
        dirty_workspace(&b.layout);
        let mut retention = FakeRetention::default();
        let mut ports = FakeNode::new(&mut retention);
        let mut aq = FakeAq::new(predecessor());
        assert!(
            Suspender::new(&b.journal, predecessor(), b.layout.clone())
                .with_boundaries(Boundaries::crash_after(boundary))
                .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
                .is_err()
        );
        let parked = Suspender::new(&b.journal, predecessor(), b.layout.clone())
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .unwrap();
        let receipt = stop_proof_receipt(&parked.report.stop, &parked.report.central_receipts)
            .unwrap_or_else(|| panic!("{boundary}: proof receipt"));
        assert_eq!(
            retention.fetch(receipt).unwrap(),
            expected.to_bytes().unwrap()
        );
        assert_eq!(ports.stops, 1, "{boundary}");
    }
}

#[test]
fn checkpoint_mediation_trusts_only_the_verified_retained_summary() {
    let request = CheckpointRequest::for_intent(&intent()).unwrap();
    assert_eq!(request.role, ArtifactRole::Checkpoint);
    assert_eq!(
        request.artifact_id,
        CheckpointRequest::for_intent(&intent())
            .unwrap()
            .artifact_id,
        "deterministic artifact id per continuation generation"
    );
    let body = serde_json::to_vec(&summary(SUMMARY)).unwrap();
    let mut retention = FakeRetention::default();
    let good = retention.agent_retain(&request, ArtifactRole::Checkpoint, &body);
    assert_eq!(
        agent_checkpoint_claim(&request, Some(&good), &retention).unwrap(),
        summary_claim(SUMMARY)
    );

    let missing = |claim: CheckpointClaim| matches!(claim, CheckpointClaim::Missing { .. });
    // No answer by the deadline is an explicit missing claim.
    assert!(missing(
        agent_checkpoint_claim(&request, None, &retention).unwrap()
    ));
    // Wrong role, artifact id or attempt binding is never trusted.
    let wrong_role = retention.agent_retain(&request, ArtifactRole::Result, &body);
    let mut other_id = request.clone();
    other_id.artifact_id = Text::new("someone-else").unwrap();
    let wrong_id = retention.agent_retain(&other_id, ArtifactRole::Checkpoint, &body);
    let mut other_binding = request.clone();
    other_binding.binding = successor();
    let wrong_binding = retention.agent_retain(&other_binding, ArtifactRole::Checkpoint, &body);
    let not_summary = retention.agent_retain(&request, ArtifactRole::Checkpoint, b"{\"text\":1}");
    for bad in [&wrong_role, &wrong_id, &wrong_binding, &not_summary] {
        assert!(missing(
            agent_checkpoint_claim(&request, Some(bad), &retention).unwrap()
        ));
    }
    // A receipt that lies about the retained bytes is refused.
    let mut lying = good.clone();
    lying.artifact.digest = Digest::of(b"forged");
    assert!(missing(
        agent_checkpoint_claim(&request, Some(&lying), &retention).unwrap()
    ));
    retention.corrupt = Some(ArtifactRole::Checkpoint);
    assert!(missing(
        agent_checkpoint_claim(&request, Some(&good), &retention).unwrap()
    ));
    // Unavailable retention is an infrastructure error, so the step retries.
    retention.corrupt = None;
    retention.lose = Some(ArtifactRole::Checkpoint);
    assert!(agent_checkpoint_claim(&request, Some(&good), &retention).is_err());

    assert_missing_agent_summary();
}
fn assert_missing_agent_summary() {
    // End to end: an agent that never answers parks with a missing claim.
    let a = node();
    dirty_workspace(&a.layout);
    let mut retention = FakeRetention::default();
    let mut ports = FakeNode::new(&mut retention);
    ports.agent_checkpoint = None;
    let mut aq = FakeAq::new(predecessor());
    let parked = Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    parked.report.validate().unwrap();
    assert_eq!(
        ports.pending,
        [Vec::<Uuid>::new()],
        "no invented pending ids"
    );
    let checkpoint = retention
        .all_bytes()
        .into_iter()
        .find(|(r, _)| *r == ArtifactRole::Checkpoint)
        .unwrap()
        .1;
    let checkpoint: Checkpoint = serde_json::from_slice(&checkpoint).unwrap();
    assert!(matches!(checkpoint.claim, CheckpointClaim::Missing { .. }));
}

#[test]
fn close_input_fence_records_last_accepted_operation_and_reaches_effects() {
    let dir = tempfile::tempdir().unwrap();
    let gate = InputGate::new(dir.path()).unwrap();
    assert_eq!(gate.fence().unwrap(), None);
    let (first, last) = (Uuid::from_u128(0x51), Uuid::from_u128(0x52));
    gate.admit(first).unwrap();
    gate.admit(last).unwrap();
    let fence = gate.close().unwrap();
    assert_eq!(fence.last_accepted_operation_id, Some(last));
    // Closing is idempotent and durable; nothing is admitted afterwards.
    assert_eq!(gate.close().unwrap(), fence);
    assert!(gate.admit(Uuid::from_u128(0x53)).is_err());
    let reopened = InputGate::new(dir.path()).unwrap();
    assert_eq!(reopened.fence().unwrap(), Some(fence.clone()));
    assert!(reopened.admit(Uuid::from_u128(0x54)).is_err());

    // The suspension journals the fence and hands it to effects reconciliation.
    let a = node();
    dirty_workspace(&a.layout);
    let mut retention = FakeRetention::default();
    let mut ports = FakeNode::new(&mut retention);
    ports.gate.admit(first).unwrap();
    ports.gate.admit(last).unwrap();
    let mut aq = FakeAq::new(predecessor());
    assert!(
        Suspender::new(&a.journal, predecessor(), a.layout.clone())
            .with_boundaries(Boundaries::crash_after("input_closed"))
            .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
            .is_err()
    );
    assert!(
        ports.gate.admit(Uuid::from_u128(0x55)).is_err(),
        "closed before stop"
    );
    Suspender::new(&a.journal, predecessor(), a.layout.clone())
        .suspend(&suspend_directive(), &mut ports, &mut aq, &mut retention)
        .unwrap();
    assert_eq!(ports.closes, 1);
    assert_eq!(ports.fences, [fence]);
}

// ---------- continuation ----------
