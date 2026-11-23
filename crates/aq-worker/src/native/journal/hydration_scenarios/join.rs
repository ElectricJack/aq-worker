#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_retains_exact_request_and_requires_actual_inputs_again_for_retry() {
    let (h, git) = checkout_fixture().await;
    let all = h.all_inputs().await;
    let source = h
        .pipeline()
        .hydrate_source(
            h.id,
            h.preparation().clone(),
            h.spec().await,
            git.client.clone(),
        )
        .await
        .unwrap();
    let before = h.pipeline().snapshot().await.unwrap();
    let prepared = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation().clone(), all, source)
        .await
        .unwrap();
    assert_eq!(prepared.request().journal_revision, before.revision);
    assert_eq!(
        prepared.request().journal_digest,
        canonical_digest(&before).unwrap()
    );
    assert_eq!(
        prepared.request().artifacts.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
            + 2
    );
    assert_eq!(
        prepared.materials()[0].read_private().unwrap().as_slice(),
        b"synthetic-key"
    );
    let saved = h.pipeline().snapshot().await.unwrap();
    assert!(
        same(
            &saved.attempts[&h.id].prepared.as_ref().unwrap().request,
            prepared.request()
        )
        .unwrap()
    );
    let first_request = prepared.request().clone();
    drop(prepared);
    let all = h.all_inputs().await;
    let source = h
        .pipeline()
        .hydrate_source(
            h.id,
            h.preparation().clone(),
            h.spec().await,
            git.client.clone(),
        )
        .await
        .unwrap();
    let retry = h
        .pipeline()
        .prepare_inputs(h.id, h.preparation().clone(), all, source)
        .await
        .unwrap();
    assert!(same(&first_request, retry.request()).unwrap());
    assert_eq!(retry.request().journal_revision, before.revision);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_refuses_missing_inputs_and_retention_changed_after_hydration() {
    for mode in [0, 2, 3, 4] {
        let (h, git) = checkout_fixture().await;
        let mut all = h.all_inputs().await;
        let source = h
            .pipeline()
            .hydrate_source(
                h.id,
                h.preparation().clone(),
                h.spec().await,
                git.client.clone(),
            )
            .await
            .unwrap();
        let before = h.pipeline().snapshot().await.unwrap();
        match mode {
            0 => {
                all.pop();
            }
            2 => git.mode.store(2, Ordering::SeqCst),
            3 => git.mode.store(3, Ordering::SeqCst),
            _ => {
                let lease = h.preparation().plan().materials[0].lease_id;
                fs::write(
                    h._dir
                        .path()
                        .join("materials")
                        .join(lease.to_string())
                        .join("material.bin"),
                    b"corrupt installed key",
                )
                .unwrap();
            }
        }
        assert!(
            h.pipeline()
                .prepare_inputs(h.id, h.preparation().clone(), all, source)
                .await
                .is_err(),
            "mode {mode}"
        );
        let after = h.pipeline().snapshot().await.unwrap();
        assert!(same(&before, &after).unwrap());
        assert!(after.attempts[&h.id].prepared.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_join_fsync_cuts_retain_original_or_complete_intent_and_reopen_without_authority() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let mut h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        let all = h.all_inputs().await;
        let source = h
            .pipeline()
            .hydrate_source(
                h.id,
                h.preparation().clone(),
                h.spec().await,
                git.client.clone(),
            )
            .await
            .unwrap();
        let before = h.pipeline().snapshot().await.unwrap();
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(
            h.pipeline()
                .prepare_inputs(h.id, h.preparation().clone(), all, source)
                .await
                .is_err()
        );
        assert!(
            h.pipeline().snapshot().await.unwrap().attempts[&h.id]
                .reservation
                .disk_bytes
                > 0
        );
        drop(h.pipeline.take());
        let mut capacity = limits();
        capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, capacity).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        if let Some(p) = &reopened.state().attempts[&h.id].prepared {
            assert_eq!(p.request.journal_revision, before.revision);
            assert_eq!(p.request.journal_digest, canonical_digest(&before).unwrap());
        } else {
            assert!(same(reopened.state(), &before).unwrap());
        }
    }
}

#[test]
fn format_three_refusal_preserves_old_journal_and_pending_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("old-three");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |s| s.format = 3);
    let original = fs::read(root.join("journal.json")).unwrap();
    fs::write(root.join("pending"), b"unknown old write").unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(fs::read(root.join("journal.json")).unwrap(), original);
    assert_eq!(
        fs::read(root.join("pending")).unwrap(),
        b"unknown old write"
    );
}
