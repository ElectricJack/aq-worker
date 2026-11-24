#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_native_pipeline_journals_every_external_intent_and_keeps_lost_import_id() {
    let (h, git) = checkout_fixture().await;
    git.mode.store(1, Ordering::SeqCst);
    assert!(hydrate(&h, &git.client).await.is_err());
    let first = h.pipeline().snapshot().await.unwrap();
    let imported = first.attempts[&h.id].source.as_ref().unwrap().operations["import"].clone();
    assert!(imported.result.is_none());
    assert!(
        !first.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .operations
            .contains_key("create")
    );
    let proof = hydrate(&h, &git.client).await.unwrap();
    assert_eq!(proof.attempt_id(), h.id);
    assert_eq!(proof.manifest().source.base.hex, "a".repeat(40));
    assert!(proof.workspace().path.exists());
    let calls = git.calls.lock().unwrap();
    let imports: Vec<_> = calls
        .iter()
        .filter(|r| r["operation"] == "artifact.import")
        .collect();
    assert_eq!(imports.len(), 2);
    assert_eq!(imports[0], imports[1]);
    assert_eq!(
        calls
            .iter()
            .filter(|r| r["operation"] == "workspace.create")
            .count(),
        1
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_receipts_cannot_bypass_fresh_retention_or_head_checks() {
    let (h, git) = checkout_fixture().await;
    hydrate(&h, &git.client).await.unwrap();
    git.mode.store(2, Ordering::SeqCst);
    assert!(hydrate(&h, &git.client).await.is_err());
    git.mode.store(3, Ordering::SeqCst);
    assert!(hydrate(&h, &git.client).await.is_err());
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(
        state.attempts[&h.id].source.as_ref().unwrap().operations["import"]
            .result
            .is_some()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_range_is_exact_pending_and_format_two_is_never_upgraded() {
    let (h, git) = checkout_fixture().await;
    h.mode.store(1, Ordering::SeqCst);
    assert!(hydrate(&h, &git.client).await.is_err());
    let first = h.pipeline().snapshot().await.unwrap();
    let range = first.attempts[&h.id]
        .source
        .as_ref()
        .unwrap()
        .pending
        .clone()
        .unwrap();
    assert_eq!(range.offset, 0);
    assert_eq!(
        first.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .manifest_offset,
        0
    );
    h.mode.store(0, Ordering::SeqCst);
    hydrate(&h, &git.client).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("old");
    let journal = create(&root);
    drop(journal);
    rewrite(&root, |s| s.format = 2);
    let bytes = fs::read(root.join("journal.json")).unwrap();
    assert!(Journal::open(&root, identity().node_id, limits()).is_err());
    assert_eq!(fs::read(root.join("journal.json")).unwrap(), bytes);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_file_sync_and_cursor_commit_faults_reopen_old_or_matching_new_history() {
    for fault in [
        WriteFault::BeforeWrite,
        WriteFault::AfterFileSync,
        WriteFault::AfterRename,
        WriteFault::AfterDirectorySync,
    ] {
        let export = synthetic();
        let m = export.manifest.clone();
        let mut h = Inputs::new(export).await;
        let git = GitPeer::new(h.root.clone(), m.clone());
        h.mode.store(1, Ordering::SeqCst);
        assert!(hydrate(&h, &git.client).await.is_err());
        let before = h.pipeline().snapshot().await.unwrap();
        let pending = before.attempts[&h.id]
            .source
            .as_ref()
            .unwrap()
            .pending
            .clone()
            .unwrap();
        let spec = h.spec().await;
        h.pipeline().test_journal_fault(fault).await.unwrap();
        h.mode.store(0, Ordering::SeqCst);
        assert!(
            h.pipeline()
                .hydrate_source(h.id, h.preparation().clone(), spec, git.client.clone())
                .await
                .is_err()
        );
        // File sync preceded the attempted metadata commit. Its exact tail is
        // retained even when the selected journal still has the old cursor.
        let file = git
            .client
            .root()
            .join(".native-inputs")
            .join(m.artifact_id.to_string())
            .join("download/manifest.json");
        assert_eq!(
            fs::metadata(&file).unwrap().len(),
            u64::from(pending.length)
        );
        drop(h.pipeline.take());
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        let mut reopened = Journal::open(&h.root, identity().node_id, cap).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        assert!(reopened.source_range(h.id).is_err());
        let s = reopened.state().attempts[&h.id].source.as_ref().unwrap();
        assert!(s.operations.is_empty());
        match fault {
            WriteFault::BeforeWrite | WriteFault::AfterFileSync => {
                assert_eq!(s.manifest_offset, 0);
                assert_eq!(
                    s.pending.as_ref().unwrap().operation_id,
                    pending.operation_id
                )
            }
            WriteFault::AfterRename | WriteFault::AfterDirectorySync => {
                assert_eq!(s.manifest_offset, u64::from(pending.length));
                assert!(s.pending.is_none())
            }
        }
    }
}
async fn snapshot_retrying_overload(pipeline: &InputPipeline) -> State {
    let retry = async {
        loop {
            match pipeline.snapshot().await {
                Err(e) if e.to_string().contains("storage lane overloaded") => {}
                result => break result.unwrap(),
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), retry)
        .await
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_source_read_allows_control_and_async_cancellation_retains_exact_pending_range() {
    let (h, git) = checkout_fixture().await;
    h.mode.store(2, Ordering::SeqCst);
    let spec = h.spec().await;
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation().clone();
    let local = git.client.clone();
    let id = h.id;
    let task =
        tokio::spawn(async move { pipeline.hydrate_source(id, preparation, spec, local).await });
    let before = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if h.range_entered.load(Ordering::SeqCst) {
                let bytes = fs::read(h.root.join("journal.json")).unwrap();
                break serde_json::from_slice::<Snapshot>(&bytes).unwrap().state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await
        }
    })
    .await
    .unwrap();
    let record = &before.attempts[&h.id];
    let pending = record
        .source
        .as_ref()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .operation_id;
    let control = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        h.client.accept(
            record.acceptance.as_ref().unwrap(),
            record.reservation.deadline_ns,
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    drop(control);
    task.abort();
    assert!(matches!(task.await, Err(e) if e.is_cancelled()));
    let after = snapshot_retrying_overload(h.pipeline()).await;
    assert_eq!(after.attempts.len(), 1);
    let s = after.attempts[&h.id].source.as_ref().unwrap();
    assert_eq!(s.pending.as_ref().unwrap().operation_id, pending);
    assert_eq!(s.manifest_offset, 0);
    assert!(s.operations.is_empty());
    h.mode.store(0, Ordering::SeqCst);
    hydrate(&h, &git.client).await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_source_geometry_refuses_before_any_range_or_git_mutation() {
    let mut export = synthetic();
    export.manifest.bundle_bytes = (1u64 << 30) + (1 << 20);
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    assert!(hydrate(&h, &git.client).await.is_err());
    let state = h.pipeline().snapshot().await.unwrap();
    let s = state.attempts[&h.id].source.as_ref().unwrap();
    assert!(s.manifest.is_none() && s.pending.is_none() && s.operations.is_empty());
    assert_eq!(git.calls.lock().unwrap().len(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_native_inspection_capabilities_refuse_without_adopting_source_plan() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::profile(h.root.clone(), manifest, false);
    assert!(git.client.native_reservation_bytes().is_err());
    assert!(hydrate(&h, &git.client).await.is_err());
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(state.attempts[&h.id].source.is_none());
    assert_eq!(git.calls.lock().unwrap().len(), 1);
}
