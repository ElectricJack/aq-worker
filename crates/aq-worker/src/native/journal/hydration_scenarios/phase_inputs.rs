async fn phase_physical(
    h: &Inputs,
    git: GitClient,
) -> (
    Arc<crate::native::http::preparation_v2::AcceptedPreparation>,
    Vec<crate::native::VerifiedArtifact>,
    crate::native::preparation::HydratedWorkspace,
) {
    let accepted = h.phase.as_ref().unwrap().clone();
    let before = h.pipeline().snapshot().await.unwrap();
    let a = &before.attempts[&h.id];
    let cap = a.reservation.deadline_ns;
    crate::native::http::test_clock::expire(cap).await;
    let spec = h
        .pipeline()
        .artifact_phase(h.id, accepted.clone(), h.reference.clone())
        .await
        .unwrap();
    let workspace = h
        .pipeline()
        .hydrate_source_phase(h.id, accepted.clone(), spec, git)
        .await
        .unwrap();
    assert!(workspace.observed_ns() > cap);
    let all = h
        .pipeline()
        .artifacts_phase(h.id, accepted.clone())
        .await
        .unwrap();
    for lease in &accepted.delivery().receipt.lease.materials {
        h.pipeline()
            .material_phase(h.id, accepted.clone(), lease.lease_id)
            .await
            .unwrap();
    }
    (accepted, all, workspace)
}
async fn phase_join(h: &Inputs, git: GitClient) -> crate::native::preparation::PreparedPhaseInputs {
    let (accepted, all, workspace) = phase_physical(h, git).await;
    let joined = h
        .pipeline()
        .prepare_inputs_phase(h.id, accepted.clone(), all, workspace)
        .await
        .unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    joined
        .request()
        .validate_for(
            &accepted.delivery().receipt,
            &a.phase_acceptance.as_ref().unwrap().request,
            &a.phase_acceptance.as_ref().unwrap().proposal,
        )
        .unwrap();
    assert!(a.preparation.is_none() && a.prepared.is_none() && a.initial_grant.is_none());
    assert!(a.phase_prepared.is_some());
    joined
}
#[tokio::test]
async fn phase_inputs_collect_real_bytes_and_git_after_original_proposal_expiry() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let joined = phase_join(&h, git.client.clone()).await;
    assert_eq!(
        joined.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_phase_inputs_and_prepare_after_original_proposal_expiry() {
    actual_git_hydration(false, true).await;
}

async fn shorter_phase(
    h: &Inputs,
    ms: u64,
) -> Arc<crate::native::http::preparation_v2::AcceptedPreparation> {
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    let intent = a.phase_acceptance.as_ref().unwrap();
    let p = h
        .client
        .propose_preparation(
            &identity(),
            &a.reservation.offer,
            &intent.proposal.negotiation,
            a.reservation.deadline_ns,
        )
        .await
        .unwrap();
    h.phase_window.store(ms, Ordering::SeqCst);
    Arc::new(
        h.client
            .accept_preparation(&p, &intent.request, h.phase.as_deref())
            .await
            .unwrap(),
    )
}
#[tokio::test]
async fn phase_input_expiry_and_foreign_observation_refuse_without_intents() {
    let clock = crate::native::http::test_clock::TestClock::new();
    let h = Inputs::new_phase(synthetic()).await;
    let short = shorter_phase(&h, 50).await;
    // Acceptance succeeds before the cap even if HTTP is delayed by shared load.
    // Refuse at the exact original deadline, without granting a fresh interval.
    clock.advance_to(short.deadline_ns());
    assert!(
        h.pipeline()
            .artifacts_phase(h.id, short.clone())
            .await
            .is_err()
    );
    let lease = short.delivery().receipt.lease.materials[0].lease_id;
    assert!(
        h.pipeline()
            .material_phase(h.id, short, lease)
            .await
            .is_err()
    );
    let other = Inputs::new_phase(synthetic()).await;
    assert!(
        h.pipeline()
            .artifacts_phase(h.id, other.phase.as_ref().unwrap().clone())
            .await
            .is_err()
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    assert!(a.downloads.is_empty() && a.material_downloads.is_empty() && a.source.is_none());
    assert_eq!(state.attempts.len(), 1);
}
#[tokio::test]
async fn phase_cancelled_artifact_batch_keeps_exact_ids_and_no_partial_completion() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let h = Inputs::new_phase(synthetic()).await;
    h.mode.store(4, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let a = h.phase.as_ref().unwrap().clone();
    let id = h.id;
    let task = tokio::spawn(async move { pipeline.artifacts_phase(id, a).await });
    h.inline_entered.acquire_many(2).await.unwrap().forget();
    task.abort();
    assert!(matches!(task.await,Err(error) if error.is_cancelled()));
    let before = h.pipeline().snapshot().await.unwrap();
    let original = before.attempts[&id].downloads.clone();
    assert!(original.values().all(|d| d.receipt.is_none()));
    h.mode.store(0, Ordering::SeqCst);
    h.inline_release.add_permits(2);
    h.pipeline()
        .artifacts_phase(id, h.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    let after = h.pipeline().snapshot().await.unwrap();
    for (key, old) in original {
        let new = &after.attempts[&id].downloads[&key];
        assert_eq!(
            canonical_bytes(&old.requests).unwrap(),
            canonical_bytes(&new.requests).unwrap()
        );
        assert!(new.receipt.is_some());
    }
}
#[tokio::test]
async fn phase_input_write_cuts_hold_old_or_complete_plan_and_history_only_reopen() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let mut h = Inputs::new_phase(synthetic()).await;
        let a = h.phase.as_ref().unwrap().clone();
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(h.pipeline().artifacts_phase(h.id, a.clone()).await.is_err());
        drop(h.pipeline.take());
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
        assert!(reopened.available().is_err());
        assert!(reopened.check_phase_preparation(h.id, &a).is_err());
        let attempt = &reopened.state().attempts[&h.id];
        assert!(attempt.preparation.is_none() && attempt.initial_grant.is_none());
        assert!(
            attempt.downloads.is_empty()
                || attempt.downloads.len() == attempt.reservation.offer.offered.artifacts.len()
        );
        assert!(
            attempt
                .downloads
                .values()
                .all(|d| matches!(d.preparation, InputOrigin::Phase(_)) && d.receipt.is_none())
        );
    }
}
#[tokio::test]
async fn phase_origin_tampering_cannot_be_hidden_by_valid_journal_checksum() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let mut h = Inputs::new_phase(synthetic()).await;
    h.pipeline()
        .artifacts_phase(h.id, h.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    drop(h.pipeline.take());
    rewrite(&h.root, |s| {
        let op = s
            .attempts
            .get_mut(&h.id)
            .unwrap()
            .downloads
            .values_mut()
            .next()
            .unwrap();
        let InputOrigin::Phase(p) = &mut op.preparation else {
            panic!()
        };
        p.receipt_digest = Digest::of(b"changed accepted receipt");
    });
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    assert!(Journal::open(&h.root, identity().node_id, capacity).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_prepare_cancelled_fsync_retains_exact_request_and_replay_does_not_retime() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let (a, all, workspace) = phase_physical(&h, git.client.clone()).await;
    let pause = WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let id = h.id;
    let task =
        tokio::spawn(async move { pipeline.prepare_inputs_phase(id, a, all, workspace).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await
        }
    })
    .await
    .unwrap();
    task.abort();
    pause.release();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    // Snapshot must wait for the admitted fsync owner, not an arbitrary sleep.
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match h.pipeline().snapshot().await {
                Ok(s) => break s,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap();
    let original = first.attempts[&id].phase_prepared.clone().unwrap();
    let joined = phase_join(&h, git.client.clone()).await;
    assert_eq!(
        canonical_bytes(joined.request()).unwrap(),
        canonical_bytes(&original.request).unwrap()
    );
    let last = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        canonical_bytes(last.attempts[&id].phase_prepared.as_ref().unwrap()).unwrap(),
        canonical_bytes(&original).unwrap()
    );
    assert!(last.attempts[&id].initial_grant.is_none());
}
#[tokio::test]
async fn phase_shared_malformed_owner_is_rejected_even_when_every_input_copy_matches() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let mut h = Inputs::new_phase(synthetic()).await;
    h.pipeline()
        .artifacts_phase(h.id, h.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    drop(h.pipeline.take());
    rewrite(&h.root, |state| {
        let a = state.attempts.get_mut(&h.id).unwrap();
        let r = a.phase_preparation.as_mut().unwrap();
        r.delivery.receipt.lease.choice_digest = Digest::of(b"forged shared owner");
        r.receipt_digest = r.delivery.receipt.digest().unwrap();
        for op in a.downloads.values_mut() {
            op.preparation = InputOrigin::Phase(Box::new(r.clone()));
        }
    });
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    assert!(Journal::open(&h.root, identity().node_id, capacity).is_err());
}

#[tokio::test]
async fn phase_and_legacy_input_provenance_cannot_cross_on_reopen() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let mut phase = Inputs::new_phase(synthetic()).await;
    phase
        .pipeline()
        .artifacts_phase(phase.id, phase.phase.as_ref().unwrap().clone())
        .await
        .unwrap();
    let mut legacy = Inputs::new(synthetic()).await;
    let _verified = legacy.spec().await;
    let phase_state = phase.pipeline().snapshot().await.unwrap();
    let phase_record = phase_state.attempts[&phase.id]
        .phase_preparation
        .as_ref()
        .unwrap()
        .clone();
    let legacy_state = legacy.pipeline().snapshot().await.unwrap();
    let legacy_record = legacy_state.attempts[&legacy.id]
        .preparation
        .as_ref()
        .unwrap()
        .clone();
    drop(phase.pipeline.take());
    drop(legacy.pipeline.take());
    rewrite(&phase.root, |s| {
        s.attempts
            .get_mut(&phase.id)
            .unwrap()
            .downloads
            .values_mut()
            .next()
            .unwrap()
            .preparation = InputOrigin::Legacy(Box::new(legacy_record));
    });
    rewrite(&legacy.root, |s| {
        s.attempts
            .get_mut(&legacy.id)
            .unwrap()
            .downloads
            .values_mut()
            .next()
            .unwrap()
            .preparation = InputOrigin::Phase(Box::new(phase_record));
    });
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    assert!(Journal::open(&phase.root, identity().node_id, capacity.clone()).is_err());
    assert!(Journal::open(&legacy.root, identity().node_id, capacity).is_err());
}
#[tokio::test]
async fn phase_prepare_rename_cut_retains_whole_intent_without_live_owner_on_reopen() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let export = synthetic();
    let manifest = export.manifest.clone();
    let mut h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let (a, all, workspace) = phase_physical(&h, git.client.clone()).await;
    h.pipeline()
        .test_journal_fault(WriteFault::AfterRename)
        .await
        .unwrap();
    assert!(
        h.pipeline()
            .prepare_inputs_phase(h.id, a.clone(), all, workspace)
            .await
            .is_err()
    );
    drop(h.pipeline.take());
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
    assert!(reopened.available().is_err());
    assert!(reopened.check_phase_preparation(h.id, &a).is_err());
    let attempt = &reopened.state().attempts[&h.id];
    assert!(
        attempt.phase_prepared.is_some()
            && attempt.prepared.is_none()
            && attempt.initial_grant.is_none()
    );
    assert!(attempt.source.as_ref().unwrap().complete());
}

#[tokio::test]
async fn production_collect_dispatch_retains_actual_frozen_inputs_without_launch() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_collect(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let state = snapshot_retrying_overload(h.pipeline()).await;
    let a = &state.attempts[&h.id];
    assert!(a.phase_acceptance.is_none() && a.refresh_acceptance.is_none());
    let (_config_root, mut config) = crate::native::runner::tests::config();
    let published = published_phase_fixture();
    config.negotiation =
        serde_json::from_value(published["proposal"]["negotiation"].clone()).unwrap();
    config.capabilities = capabilities();
    config.source_profile =
        serde_json::from_value(published["accept"]["hold"]["source_profile"].clone()).unwrap();
    config.maximum_preparation_ms = 30000;
    config.renderer = a.reservation.offer.startup.renderer_qualification.clone();
    let grant = crate::native::runner::TestPreparation {
        pipeline: h.pipeline(),
        inventory: &h.inventory().await,
        config: &config,
    }
    .collect(
        &a.reservation.offer,
        a.reservation.deadline_ns,
        git.client.clone(),
    )
    .await
    .unwrap();
    h.pipeline().check_phase_execution(&grant).await.unwrap();
    let state = snapshot_retrying_overload(h.pipeline()).await;
    assert!(state.attempts[&h.id].phase_execution.is_some());
    assert!(state.attempts[&h.id].native_host.is_none());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
