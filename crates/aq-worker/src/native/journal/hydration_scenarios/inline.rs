#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_overlaps_only_two_reads_and_preserves_control_capacity() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    h.mode.store(4, Ordering::SeqCst);
    let pipeline = h.pipeline().clone();
    let preparation = h.preparation().clone();
    let id = h.id;
    let job = tokio::spawn(async move { pipeline.artifacts(id, preparation).await });
    // Both requests must reach the independently blocked HTTP peer. Sequential
    // fetching cannot satisfy this barrier; no timing benchmark is involved.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.inline_entered.acquire_many(2),
    )
    .await
    .unwrap()
    .unwrap()
    .forget();
    assert_eq!(h.inline_max_active.load(Ordering::SeqCst), 2);
    let pending = h.pipeline().snapshot().await.unwrap();
    assert_eq!(pending.revision, before.revision + 1);
    let attempt = &pending.attempts[&h.id];
    assert!(attempt.downloads.values().all(|d| d.receipt.is_none()));
    // Both existing bulk slots are occupied, but authenticated control still
    // completes on its independent lane. Replaying does not extend the cap.
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.client.accept(
            attempt.acceptance.as_ref().unwrap(),
            attempt.reservation.deadline_ns,
            Some(h.preparation()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(replay.deadline_ns() <= h.preparation().deadline_ns());
    let requests: usize = attempt.downloads.values().map(|d| d.requests.len()).sum();
    h.inline_release.add_permits(requests);
    let inputs = job.await.unwrap().unwrap();
    assert_eq!(inputs.len(), attempt.downloads.len());
    assert_eq!(h.inline_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.pipeline().snapshot().await.unwrap().revision,
        before.revision + 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_uses_two_durable_commits_and_replay_reuses_every_original_request() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    let first = h
        .pipeline()
        .artifacts(h.id, h.preparation().clone())
        .await
        .unwrap();
    let saved = h.pipeline().snapshot().await.unwrap();
    assert_eq!(saved.revision, before.revision + 2);
    assert_eq!(
        first.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
    );
    for op in saved.attempts[&h.id].downloads.values() {
        assert_eq!(op.created_revision, before.revision);
        assert!(op.receipt.is_some() && op.completed_ns.is_some());
    }
    let second = h
        .pipeline()
        .artifacts(h.id, h.preparation().clone())
        .await
        .unwrap();
    assert_eq!(second.len(), first.len());
    for (a, b) in first.iter().zip(&second) {
        assert_eq!(a.bytes(), b.bytes());
    }
    assert!(same(&saved, &h.pipeline().snapshot().await.unwrap()).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_inline_batch_keeps_exact_plan_without_partial_receipt_completion() {
    let h = Inputs::new(synthetic()).await;
    let before = h.pipeline().snapshot().await.unwrap();
    h.mode.store(3, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .artifacts(h.id, h.preparation().clone())
            .await
            .is_err()
    );
    let partial = h.pipeline().snapshot().await.unwrap();
    assert_eq!(partial.revision, before.revision + 1);
    let plans = &partial.attempts[&h.id].downloads;
    assert_eq!(
        plans.len(),
        before.attempts[&h.id]
            .reservation
            .offer
            .offered
            .artifacts
            .len()
    );
    assert!(
        plans
            .values()
            .all(|p| p.receipt.is_none() && p.completed_ns.is_none())
    );
    h.mode.store(0, Ordering::SeqCst);
    h.pipeline()
        .artifacts(h.id, h.preparation().clone())
        .await
        .unwrap();
    let complete = h.pipeline().snapshot().await.unwrap();
    assert_eq!(complete.revision, before.revision + 2);
    for (key, p) in plans {
        let actual = &complete.attempts[&h.id].downloads[key];
        assert!(same(&p.requests, &actual.requests).unwrap());
        assert_eq!(p.request_digests, actual.request_digests);
        assert_eq!(p.created_revision, actual.created_revision);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_batch_fsync_cuts_never_reopen_partial_plan_or_completion() {
    for completion in [false, true] {
        for fault in [
            WriteFault::BeforeWrite,
            WriteFault::AfterFileSync,
            WriteFault::AfterRename,
            WriteFault::AfterDirectorySync,
        ] {
            check_inline_reopen(fault, completion).await;
        }
    }
}

async fn check_inline_reopen(fault: WriteFault, completion: bool) {
    let mut h = Inputs::new(synthetic()).await;
    if completion {
        h.mode.store(3, Ordering::SeqCst);
        assert!(
            h.pipeline()
                .artifacts(h.id, h.preparation().clone())
                .await
                .is_err()
        );
        h.mode.store(0, Ordering::SeqCst);
    }
    let before = h.pipeline().snapshot().await.unwrap();
    h.pipeline().test_journal_fault(fault).await.unwrap();
    assert!(
        h.pipeline()
            .artifacts(h.id, h.preparation().clone())
            .await
            .is_err()
    );
    drop(h.pipeline.take());
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    let old = Journal::open(&h.root, identity().node_id, capacity).unwrap();
    assert!(old.available().is_err());
    let a = &old.state().attempts[&h.id];
    let count = a.reservation.offer.offered.artifacts.len();
    assert!(a.downloads.is_empty() || a.downloads.len() == count);
    let complete = a.downloads.values().filter(|d| d.receipt.is_some()).count();
    assert!(complete == 0 || complete == count);
    assert!(old.state().revision == before.revision || old.state().revision == before.revision + 1);
    if completion {
        for (key, op) in &before.attempts[&h.id].downloads {
            assert!(same(&op.requests, &a.downloads[key].requests).unwrap());
        }
    }
}
