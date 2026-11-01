//! Actual HTTP sampling, private input owners and fsync; synthetic Git peer here.
//! The separately ignored real-Git test retains its explicit qualification.
use super::*;
use crate::native::{
    http::control::Granted,
    preparation::{GrantCommit, PreparedInputs},
};
async fn joined(h: &Inputs, git: &GitPeer) -> PreparedInputs {
    let spec = h.spec().await;
    let workspace = h
        .pipeline()
        .hydrate_source(h.id, h.preparation.clone(), spec, git.client.clone())
        .await
        .unwrap();
    let all = h.all_inputs().await;
    let renderer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
        .reservation
        .offer
        .startup
        .renderer_qualification
        .clone();
    h.pipeline()
        .prepare_inputs(h.id, h.preparation.clone(), all, workspace, renderer)
        .await
        .unwrap()
}
async fn observed(h: &Inputs, request: &aq_admission::transport::Prepare) -> Granted {
    let offer = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
        .reservation
        .offer
        .clone();
    h.client
        .prepare(request, &h.preparation, &offer, None)
        .await
        .unwrap()
}
async fn barrier(pause: &super::super::WritePause) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("fsync barrier reached");
}
async fn wait_expired(deadline: u64) {
    let remaining = deadline.saturating_sub(boot_ns().unwrap());
    tokio::time::sleep(std::time::Duration::from_nanos(
        remaining.saturating_add(1_000_000),
    ))
    .await;
    assert!(boot_ns().unwrap() >= deadline);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn received_grant_is_durable_before_usable_and_replay_preserves_first_clocks_and_bundle() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let request = inputs.request().clone();
    let grant = observed(&h, &request).await;
    let sent = grant.initial_sent_ns();
    let received = grant.initial_received_ns();
    let cap = grant.deadline_ns();
    let hard = grant.hard_deadline_ns();
    let result = h
        .pipeline()
        .record_initial_grant(inputs, grant)
        .await
        .unwrap();
    let GrantCommit::Usable(live) = result else {
        panic!("initial one-second synthetic receipt expired before commit")
    };
    assert!(live.history().observed_after_sync_ns() >= received);
    h.pipeline().check_initial_grant(&live).await.unwrap();
    let before = h.pipeline().snapshot().await.unwrap();
    let record = before.attempts[&h.id].initial_grant.as_ref().unwrap();
    assert_eq!(
        (
            record.sent_ns,
            record.received_ns,
            record.deadline_ns,
            record.hard_deadline_ns
        ),
        (sent, received, cap, hard)
    );
    assert!(record.observed_before_write_ns <= live.history().observed_after_sync_ns());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    let fresh = observed(&h, &request).await;
    let replay = h.pipeline().retry_initial_grant(h.id, fresh).await.unwrap();
    if let GrantCommit::Usable(replayed) = replay {
        assert!(replayed.deadline_ns() <= cap);
        assert!(replayed.hard_deadline_ns() <= hard);
    }
    let after = h.pipeline().snapshot().await.unwrap();
    assert_eq!(before.revision, after.revision);
    assert_eq!(
        canonical_bytes(&before.attempts[&h.id].initial_grant).unwrap(),
        canonical_bytes(&after.attempts[&h.id].initial_grant).unwrap()
    );
    let stats = h.pipeline().retained_input_stats().unwrap();
    assert_eq!(stats.attempts, 1);
    assert!(
        stats.inline_bytes <= crate::native::MAX_INLINE_SET_BYTES
            && stats.startup_bytes <= stats.inline_bytes
    );
    assert!(
        !String::from_utf8(fs::read(h.root.join("journal.json")).unwrap())
            .unwrap()
            .contains("synthetic-key")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_prepare_network_association_refuses_but_retains_inputs_for_narrow_retry() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let request = inputs.request().clone();
    let mut changed = request.clone();
    changed.journal_digest = Digest::of(b"not the actual local Prepared checkpoint");
    let grant = observed(&h, &changed).await;
    assert!(
        h.pipeline()
            .record_initial_grant(inputs, grant)
            .await
            .is_err()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .initial_grant
            .is_none()
    );
    let correct = observed(&h, &request).await;
    assert!(matches!(
        h.pipeline()
            .retry_initial_grant(h.id, correct)
            .await
            .unwrap(),
        GrantCommit::Usable(_)
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_same_operation_grant_cannot_replace_first_receipt_or_owner() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let request = inputs.request().clone();
    let grant = observed(&h, &request).await;
    let _first = h
        .pipeline()
        .record_initial_grant(inputs, grant)
        .await
        .unwrap();
    let before = h.pipeline().snapshot().await.unwrap();
    h.grant_mode.store(1, Ordering::SeqCst);
    let changed = observed(&h, &request).await;
    assert!(
        h.pipeline()
            .retry_initial_grant(h.id, changed)
            .await
            .is_err()
    );
    let after = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        canonical_bytes(&before).unwrap(),
        canonical_bytes(&after).unwrap()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_unpolled_commit_future_already_retains_inputs_without_grant_history() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let request = inputs.request().clone();
    let grant = observed(&h, &request).await;
    let future = h.pipeline().record_initial_grant(inputs, grant);
    drop(future);
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .initial_grant
            .is_none()
    );
    let fresh = observed(&h, &request).await;
    assert!(matches!(
        h.pipeline().retry_initial_grant(h.id, fresh).await.unwrap(),
        GrantCommit::Usable(_)
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_expiring_before_commit_is_history_only_without_releasing_inputs() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    h.grant_window_ms.store(100, Ordering::SeqCst);
    let grant = observed(&h, inputs.request()).await;
    let deadline = grant.deadline_ns();
    wait_expired(deadline).await;
    let result = h
        .pipeline()
        .record_initial_grant(inputs, grant)
        .await
        .unwrap();
    let GrantCommit::HistoryOnly(history) = result else {
        panic!("expired receipt produced usable wrapper")
    };
    assert!(history.observed_after_sync_ns() >= deadline);
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .initial_grant
            .is_some()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_during_fsync_retains_history_and_never_yields_usable_wrapper() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    h.grant_window_ms.store(100, Ordering::SeqCst);
    let grant = observed(&h, inputs.request()).await;
    let deadline = grant.deadline_ns();
    let pause = super::super::WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let job = tokio::spawn(async move { pipeline.record_initial_grant(inputs, grant).await });
    barrier(&pause).await;
    wait_expired(deadline).await;
    pause.release();
    let result = job.await.unwrap().unwrap();
    let GrantCommit::HistoryOnly(history) = result else {
        panic!("post-fsync expiry produced usable wrapper")
    };
    assert!(history.observed_after_sync_ns() >= deadline);
    let state = h.pipeline().snapshot().await.unwrap();
    let saved = state.attempts[&h.id].initial_grant.as_ref().unwrap();
    assert!(saved.observed_before_write_ns < deadline);
    assert_eq!(saved.deadline_ns, deadline);
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_fsync_waiter_retains_bundle_and_completion_for_explicit_retry() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let request = inputs.request().clone();
    let grant = observed(&h, &request).await;
    let pause = super::super::WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let job = tokio::spawn(async move { pipeline.record_initial_grant(inputs, grant).await });
    barrier(&pause).await;
    job.abort();
    assert!(matches!(job.await, Err(error) if error.is_cancelled()));
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    assert!(
        matches!(h.pipeline().snapshot().await, Err(error) if error.to_string().contains("storage lane overloaded"))
    );
    pause.release();
    let state = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match h.pipeline().snapshot().await {
                Ok(state) => break state,
                Err(e) if e.to_string().contains("storage lane overloaded") => {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await
                }
                Err(_) => panic!("unexpected owner failure"),
            }
        }
    })
    .await
    .unwrap();
    assert!(state.attempts[&h.id].initial_grant.is_some());
    let fresh = observed(&h, &request).await;
    let _explicit = h.pipeline().retry_initial_grant(h.id, fresh).await.unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}

fn capacity() -> Limits {
    let mut capacity = limits();
    capacity.disk_bytes = 64 * 1024 * 1024 * 1024;
    capacity
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_grant_write_fault_preserves_holds_and_reopen_never_restores_authority() {
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
        let inputs = joined(&h, &git).await;
        let request = inputs.request().clone();
        let grant = observed(&h, &request).await;
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(
            h.pipeline()
                .record_initial_grant(inputs, grant)
                .await
                .is_err()
        );
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
        let fresh = observed(&h, &request).await;
        assert!(h.pipeline().retry_initial_grant(h.id, fresh).await.is_err());
        let memory = h.pipeline().snapshot().await.unwrap();
        assert_eq!(memory.attempts.len(), 1);
        assert!(memory.attempts[&h.id].prepared.is_some());
        assert!(memory.attempts[&h.id].reservation.disk_bytes > 0);
        drop(h.pipeline.take());
        let reopened = Journal::open(&h.root, identity().node_id, capacity()).unwrap();
        assert!(reopened.recovery_required());
        assert!(reopened.available().is_err());
        let a = &reopened.state().attempts[&h.id];
        assert!(a.prepared.is_some() && a.source.as_ref().unwrap().complete());
        if let Some(record) = &a.initial_grant {
            assert!(
                reopened
                    .check_initial_grant(
                        h.id,
                        &record.digest().unwrap(),
                        record.deadline_ns,
                        record.hard_deadline_ns
                    )
                    .is_err()
            );
        }
        let saved = fs::read(h.root.join("journal.json")).unwrap();
        drop(reopened);
        assert!(Journal::create_identity(&h.root, identity(), capacity()).is_err());
        assert_eq!(saved, fs::read(h.root.join("journal.json")).unwrap());
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_grant_history_survives_restart_but_changed_associations_are_refused() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let mut h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let grant = observed(&h, inputs.request()).await;
    let result = h
        .pipeline()
        .record_initial_grant(inputs, grant)
        .await
        .unwrap();
    let before = h.pipeline().snapshot().await.unwrap();
    drop(result);
    drop(h.pipeline.take());
    let original = fs::read(h.root.join("journal.json")).unwrap();
    let reopened = Journal::open(&h.root, identity().node_id, capacity()).unwrap();
    assert!(reopened.recovery_required());
    assert!(reopened.available().is_err());
    assert!(same(reopened.state(), &before).unwrap());
    drop(reopened);
    for case in 0..9 {
        fs::write(h.root.join("journal.json"), &original).unwrap();
        rewrite(&h.root, |state| {
            let a = state.attempts.get_mut(&h.id).unwrap();
            let g = a.initial_grant.as_mut().unwrap();
            match case {
                0 => g.prepare_digest = Digest::of(b"wrong Prepare association"),
                1 => g.runtime.qualification.digest = Digest::of(b"another qualification"),
                2 => g.kernel_boot_id = Uuid::new_v4(),
                3 => g.clock_source = "reanchored_wall_clock".into(),
                4 => g.received_ns = g.sent_ns.saturating_sub(1),
                5 => g.deadline_ns = g.hard_deadline_ns.saturating_add(1),
                6 => g.observed_before_write_ns = g.received_ns.saturating_sub(1),
                7 => g.record_revision = a.prepared.as_ref().unwrap().request.journal_revision + 1,
                8 => {
                    g.delivery.original.materials[0].material_digest =
                        Digest::of(b"another material");
                    g.delivery_digest = canonical_digest(&g.delivery).unwrap();
                }
                _ => unreachable!(),
            }
        });
        let tampered = fs::read(h.root.join("journal.json")).unwrap();
        assert!(
            Journal::open(&h.root, identity().node_id, capacity()).is_err(),
            "tamper case{case}"
        );
        assert_eq!(tampered, fs::read(h.root.join("journal.json")).unwrap());
    }
}
#[test]
fn every_old_format_is_refused_before_rewriting_journal_or_pending_evidence() {
    use std::os::unix::fs::OpenOptionsExt;
    for format in 1..=4 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("history");
        drop(create(&root));
        // Deliberately opaque old contents: refusal must happen at the version
        // header before interpreting any old schema or pending file bytes.
        let raw = serde_json::to_vec(
            &json!({"state":{"format":format},"opaque_original":"preserve unchanged"}),
        )
        .unwrap();
        fs::write(root.join("journal.json"), &raw).unwrap();
        let pending = b"opaque original pending evidence";
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(root.join("journal.pending"))
            .unwrap();
        file.write_all(pending).unwrap();
        drop(file);
        assert!(Journal::open(&root, identity().node_id, limits()).is_err());
        assert_eq!(fs::read(root.join("journal.json")).unwrap(), raw);
        assert_eq!(fs::read(root.join("journal.pending")).unwrap(), pending);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_current_source_keeps_received_history_but_refuses_usable_wrapper() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let grant = observed(&h, inputs.request()).await;
    git.mode.store(3, Ordering::SeqCst);
    assert!(matches!(
        h.pipeline()
            .record_initial_grant(inputs, grant)
            .await
            .unwrap(),
        GrantCommit::HistoryOnly(_)
    ));
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .initial_grant
            .is_some()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_private_material_keeps_received_history_without_exposing_or_using_bytes() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    let inputs = joined(&h, &git).await;
    let lease = inputs.request().materials[0].lease_id;
    let grant = observed(&h, inputs.request()).await;
    fs::write(
        h._dir
            .path()
            .join("materials")
            .join(lease.to_string())
            .join("material.bin"),
        b"changed test bytes",
    )
    .unwrap();
    assert!(matches!(
        h.pipeline()
            .record_initial_grant(inputs, grant)
            .await
            .unwrap(),
        GrantCommit::HistoryOnly(_)
    ));
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .initial_grant
            .is_some()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
