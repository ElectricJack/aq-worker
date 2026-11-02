fn published_phase_fixture() -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(include_bytes!(
        "../../../../../vendor/aq-admission-0.1.10/artifacts/preparation-refresh-v1.json"
    ))
    .unwrap()["fixture"]
        .clone()
}
fn refresh_registry_fact(
    f: &mut aq_admission::preparation_refresh_v1::OwnerFact,
    c: &aq_admission::preparation_refresh_v1::ChoicePins,
    a: &aq_admission::preparation_refresh_v1::AcceptedReceipt,
) {
    if let aq_admission::preparation_refresh_v1::FactValue::Registry {
        selected_registry,
        accepted_registry,
        preflight_registry,
        current_registry,
        credential_profile,
        credential_revision,
    } = &mut f.body.value
    {
        *selected_registry = c.registry.clone();
        *accepted_registry = a.lease.registry_at_accept.clone();
        preflight_registry.digest = c.registry.digest.clone();
        current_registry.digest = c.registry.digest.clone();
        *credential_profile = c.credential_profile.clone();
        *credential_revision = c.credential_revision;
        f.body.revision =
            aq_admission::preparation_refresh_v1::registry_revision(current_registry).unwrap();
    }
}
super::physical_fixture::physical_fixture! {aq_admission::preparation_refresh_v1,aq_phase_terminal_protocol::refresh_v1,preparation_refresh_v1,refresh_acceptance,refresh_preparation,propose_refresh_preparation,begin_refresh_accept,accept_refresh_preparation,record_refresh_preparation}
async fn phase_physical(
    h: &Inputs,
    git: GitClient,
) -> (
    Arc<crate::native::http::preparation_refresh_v1::AcceptedPreparation>,
    Vec<crate::native::VerifiedArtifact>,
    crate::native::preparation::HydratedWorkspace,
    ArtifactRef,
) {
    let accepted = h.phase.as_ref().unwrap().clone();
    let before = h.pipeline().snapshot().await.unwrap();
    let a = &before.attempts[&h.id];
    let cap = a.reservation.deadline_ns;
    while boot_ns().unwrap() <= cap {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let spec = h
        .pipeline()
        .artifact_refresh(h.id, accepted.clone(), h.reference.clone())
        .await
        .unwrap();
    let workspace = h
        .pipeline()
        .hydrate_source_refresh(h.id, accepted.clone(), spec, git)
        .await
        .unwrap();
    assert!(workspace.observed_ns() > cap);
    let all = h
        .pipeline()
        .artifacts_refresh(h.id, accepted.clone())
        .await
        .unwrap();
    for lease in &accepted.delivery().receipt.lease.materials {
        h.pipeline()
            .material_refresh(h.id, accepted.clone(), lease.lease_id)
            .await
            .unwrap();
    }
    let renderer = a.reservation.offer.startup.renderer_qualification.clone();
    (accepted, all, workspace, renderer)
}
async fn phase_join(
    h: &Inputs,
    git: GitClient,
) -> crate::native::preparation::PreparedRefreshInputs {
    let (accepted, all, workspace, renderer) = phase_physical(h, git).await;
    let joined = h
        .pipeline()
        .prepare_inputs_refresh(h.id, accepted.clone(), all, workspace, renderer)
        .await
        .unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    joined
        .request()
        .validate_for(
            &accepted.delivery().receipt,
            &a.refresh_acceptance.as_ref().unwrap().request,
            &a.refresh_acceptance.as_ref().unwrap().proposal,
        )
        .unwrap();
    assert!(a.preparation.is_none() && a.prepared.is_none() && a.initial_grant.is_none());
    assert!(a.refresh_prepared.is_some());
    joined
}

#[tokio::test]
async fn actual_refresh_inputs_join_execution_and_reopen_stay_typed_history_only() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let mut h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = phase_join(&h, git.client.clone()).await;
    assert_eq!(
        inputs.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    let intent = a.refresh_acceptance.as_ref().unwrap();
    let observed = std::sync::Arc::new(
        h.client
            .prepare_refresh_execution(
                h.proposed.as_ref().unwrap(),
                &intent.request,
                h.phase.as_ref().unwrap(),
                inputs.request(),
                None,
            )
            .await
            .unwrap(),
    );
    let crate::native::preparation::PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .record_refresh_execution(inputs, observed.clone())
        .await
        .unwrap()
    else {
        panic!("expected actual usable owner")
    };
    h.pipeline().check_phase_execution(&owner).await.unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    assert!(
        a.phase_execution.is_none()
            && a.phase_prepared.is_none()
            && a.phase_preparation.is_none()
            && a.phase_acceptance.is_none()
    );
    assert!(a.refresh_execution.is_some());
    assert!(
        a.downloads
            .values()
            .all(|x| matches!(x.preparation, InputOrigin::Refresh(_)))
    );
    let first = canonical_bytes(a.refresh_execution.as_ref().unwrap()).unwrap();
    let stats = h.pipeline().retained_input_stats().unwrap();
    assert_eq!(stats.attempts, 1);
    assert!(stats.startup_bytes > 0 && stats.inline_bytes >= stats.startup_bytes);
    assert!(matches!(
        h.pipeline()
            .retry_refresh_execution(h.id, observed)
            .await
            .unwrap(),
        crate::native::preparation::PhaseExecutionCommit::Usable(_)
    ));
    assert_eq!(
        first,
        canonical_bytes(
            h.pipeline().snapshot().await.unwrap().attempts[&h.id]
                .refresh_execution
                .as_ref()
                .unwrap()
        )
        .unwrap()
    );
    drop(owner);
    drop(h.pipeline.take());
    let mut cap = limits();
    cap.disk_bytes = 64 * 1024 * 1024 * 1024;
    let reopened = Journal::open(&h.root, identity().node_id, cap).unwrap();
    assert!(reopened.recovery_required());
    assert!(reopened.available().is_err());
    assert_eq!(
        first,
        canonical_bytes(
            reopened.state().attempts[&h.id]
                .refresh_execution
                .as_ref()
                .unwrap()
        )
        .unwrap()
    );
}

#[tokio::test]
async fn refresh_unpolled_request_retains_actual_bundle_before_network_and_retry_keeps_first_owner()
{
    let export = synthetic();
    let manifest = export.manifest.clone();
    let mut h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = phase_join(&h, git.client.clone()).await;
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    let intent = a.refresh_acceptance.as_ref().unwrap();
    let observed = Arc::new(
        h.client
            .prepare_refresh_execution(
                h.proposed.as_ref().unwrap(),
                &intent.request,
                h.phase.as_ref().unwrap(),
                inputs.request(),
                None,
            )
            .await
            .unwrap(),
    );
    let accepted = crate::native::preparation::RefreshPreparation {
        proposed: Arc::new(h.proposed.take().unwrap()),
        request: intent.request.clone(),
        observed: h.phase.as_ref().unwrap().clone(),
    };
    let pending = h.pipeline().request_refresh_execution(inputs, &accepted);
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    drop(pending);
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .refresh_execution
            .is_none()
    );
    let crate::native::preparation::PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .retry_refresh_execution(h.id, observed)
        .await
        .unwrap()
    else {
        panic!("original actual inputs must remain available")
    };
    h.pipeline().check_phase_execution(&owner).await.unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}

#[tokio::test]
async fn refresh_fsync_loss_reopens_history_without_live_authority_and_keeps_every_capacity_hold() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let mut h = Inputs::new_phase(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        let inputs = phase_join(&h, git.client.clone()).await;
        let state = h.pipeline().snapshot().await.unwrap();
        let a = &state.attempts[&h.id];
        let intent = a.refresh_acceptance.as_ref().unwrap();
        let observed = Arc::new(
            h.client
                .prepare_refresh_execution(
                    h.proposed.as_ref().unwrap(),
                    &intent.request,
                    h.phase.as_ref().unwrap(),
                    inputs.request(),
                    None,
                )
                .await
                .unwrap(),
        );
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(
            h.pipeline()
                .record_refresh_execution(inputs, observed)
                .await
                .is_err()
        );
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
        drop(h.pipeline.take());
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, cap).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        let a = &reopened.state().attempts[&h.id];
        assert!(a.refresh_prepared.is_some());
        assert!(a.phase_execution.is_none());
        assert_eq!(reopened.state().attempts.len(), 1);
        assert_eq!(
            a.refresh_execution.is_some(),
            matches!(
                fault,
                WriteFault::AfterRename | WriteFault::AfterDirectorySync
            )
        );
    }
}

#[tokio::test]
async fn refresh_shared_saved_owner_cannot_be_rehashed_around_capture_or_mixed_profile_history() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let _inputs = phase_join(&h, git.client.clone()).await;
    let saved = h.pipeline().snapshot().await.unwrap();
    let mut cap = limits();
    cap.disk_bytes = 64 * 1024 * 1024 * 1024;
    for mode in 0..3 {
        let mut state = saved.clone();
        let a = state.attempts.get_mut(&h.id).unwrap();
        match mode {
            0 => {
                let record = a.refresh_preparation.as_mut().unwrap();
                record.receipt_digest = Digest::of(b"substituted saved owner");
                for input in a.downloads.values_mut() {
                    input.preparation = InputOrigin::Refresh(Box::new(record.clone()));
                }
            }
            1 => {
                let source = a.source.as_mut().unwrap();
                let InputOrigin::Refresh(p) = &mut source.preparation else {
                    panic!("expected typed source")
                };
                p.deadline_ns = u64::MAX;
            }
            _ => {
                let fixture: Value = serde_json::from_slice(include_bytes!(
                    "../../../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
                ))
                .unwrap();
                let proposal: aq_admission::preparation_v2::Proposal =
                    serde_json::from_value(fixture["proposal"].clone()).unwrap();
                let request: aq_admission::preparation_v2::AcceptRequest =
                    serde_json::from_value(fixture["accept"].clone()).unwrap();
                a.phase_acceptance = Some(super::super::preparation_v2::PhaseAcceptance {
                    proposal_digest: proposal.digest().unwrap(),
                    request_digest: request.digest().unwrap(),
                    proposal,
                    request,
                    created_ns: 1,
                    original_deadline_ns: 2,
                });
            }
        }
        assert!(state.validate(&cap).is_err(), "tamper mode {mode}");
    }
}

#[tokio::test]
async fn production_collect_dispatch_retains_actual_refresh_inputs_without_launch() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_collect(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let state = h.pipeline().snapshot().await.unwrap();
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
    let grant = crate::native::runner::test_collect(
        h.pipeline(),
        &h.client,
        &config,
        h.id,
        &a.reservation.offer,
        a.reservation.deadline_ns,
        git.client.clone(),
    )
    .await
    .unwrap();
    h.pipeline().check_phase_execution(&grant).await.unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(state.attempts[&h.id].refresh_execution.is_some());
    assert!(state.attempts[&h.id].native_host.is_none());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
