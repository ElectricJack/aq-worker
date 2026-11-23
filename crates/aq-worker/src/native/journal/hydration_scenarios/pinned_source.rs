#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_native_source_http_import_retention_and_clean_workspace() {
    actual_git_hydration(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_GIT_TEST_BINARY and AQ_GIT_TEST_SHA256; no sibling checkout"]
async fn pinned_git_received_initial_grant_retains_actual_workspace_and_material_owners() {
    actual_git_hydration(true, false).await;
}
async fn actual_git_hydration(retain_grant: bool, phase_mode: bool) {
    let (exe, pin) = pinned_hydration_git();
    let dir = tempfile::tempdir().unwrap();
    let repository = dir.path().join("repository");
    let oid = committed_fixture(&repository);
    let rid = Uuid::new_v4();
    let central = dir.path().join("central");
    let worker = dir.path().join("worker");
    let repository = CentralRepository {
        exe: &exe,
        root: &central,
        id: rid,
    }
    .export(&repository, oid);
    let service = crate::source::GitService::start(&exe, &pin, &worker).unwrap();
    let h = Inputs::new_count(repository, phase_mode, if phase_mode { 126 } else { 0 }).await;
    if phase_mode {
        assert_phase_hydration(&h, &service.client).await;
        return;
    }
    let joined = join_checkout(&h, &service.client, &worker, rid).await;
    if retain_grant {
        assert_checkout_grant(&h, joined).await;
    } else {
        drop(joined);
        assert_released_checkout(&h, &service).await;
    }
}
async fn assert_phase_hydration(h: &Inputs, git: &GitClient) {
    let joined = phase_join(h, git.clone()).await;
    assert_eq!(
        fs::read(joined.workspace().workspace().path.join("native-input.txt")).unwrap(),
        b"synthetic native source bytes\n"
    );
    assert_eq!(
        joined.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .phase_prepared
            .is_some()
    );
    h.grant_window_ms.store(5000, Ordering::SeqCst);
    let state = h.pipeline().snapshot().await.unwrap();
    let a = &state.attempts[&h.id];
    let intent = a.phase_acceptance.as_ref().unwrap();
    let observed = h
        .client
        .prepare_execution(
            h.proposed.as_ref().unwrap(),
            &intent.request,
            h.phase.as_ref().unwrap(),
            joined.request(),
            None,
        )
        .await
        .unwrap();
    let crate::native::preparation::PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .record_phase_execution(joined, observed)
        .await
        .unwrap()
    else {
        panic!("actual Git phase execution observation not usable")
    };
    h.pipeline().check_phase_execution(&owner).await.unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
async fn join_checkout(
    h: &Inputs,
    git: &GitClient,
    worker: &Path,
    rid: Uuid,
) -> crate::native::preparation::PreparedInputs {
    let spec = h.spec().await;
    let proof = h
        .pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, git.clone())
        .await
        .unwrap();
    assert_eq!(proof.workspace().generation, 1);
    assert_eq!(proof.manifest().source.repository_id, rid);
    assert!(
        proof
            .workspace()
            .path
            .starts_with(worker.join("workspaces"))
    );
    assert_eq!(
        fs::read(proof.workspace().path.join("native-input.txt")).unwrap(),
        b"synthetic native source bytes\n"
    );
    let all = h.all_inputs().await;
    let joined = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation().clone(), all, proof)
        .await
        .unwrap();
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .prepared
            .is_some()
    );
    assert_eq!(
        joined.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    joined
}
async fn assert_checkout_grant(h: &Inputs, joined: crate::native::preparation::PreparedInputs) {
    let offer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
        .reservation
        .offer
        .clone();
    let observed = h
        .client
        .prepare(joined.request(), h.preparation(), &offer, None)
        .await
        .unwrap();
    let committed = h
        .pipeline()
        .record_initial_grant(joined, observed)
        .await
        .unwrap();
    let crate::native::preparation::GrantCommit::Usable(owner) = committed else {
        panic!("actual Git initial grant expired or changed before commit")
    };
    h.pipeline().check_initial_grant(&owner).await.unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(state.attempts[&h.id].initial_grant.is_some());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
async fn assert_released_checkout(h: &Inputs, service: &crate::source::GitService) {
    let state = h.pipeline().snapshot().await.unwrap();
    let source = state.attempts[&h.id].source.as_ref().unwrap();
    assert!(source.complete() && source.verified.is_some());
    assert_eq!(source.operations.len(), 5);
    // A fresh second call must inspect the real owner again; a prior receipt
    // cannot silently replace either current retention or the actual clean HEAD.
    let spec = h.spec().await;
    h.pipeline()
        .hydrate_source(h.id, h.preparation().clone(), spec, service.client.clone())
        .await
        .unwrap();
    let handle = Uuid::parse_str(
        source.operations["import"].result.as_ref().unwrap()["retention_handle"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let current = service
        .client
        .query(
            source.binding().unwrap(),
            "retention.inspect",
            json!({"handle_id":handle,"limit":1}),
        )
        .unwrap();
    let release = crate::source::GitAction::Release {
        handle_id: handle,
        generation: current["generation"].as_u64().unwrap(),
        refs: serde_json::from_value(current["refs"].clone()).unwrap(),
    }
    .request(source.binding().unwrap(), Uuid::new_v4());
    service.client.exchange(&release).unwrap();
    let spec = h.spec().await;
    assert!(
        h.pipeline()
            .hydrate_source(h.id, h.preparation().clone(), spec, service.client.clone())
            .await
            .is_err()
    );
}
