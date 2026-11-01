//! Actual HTTP/fsync/private byte/current Git owners, synthetic issuer facts.
use super::*;
use crate::native::{
    http::preparation_v2::ExecutionObservation,
    preparation::{PhaseExecutionCommit, PreparedPhaseInputs},
};
async fn observed(
    h: &Inputs,
    inputs: &PreparedPhaseInputs,
    previous: Option<&ExecutionObservation>,
) -> ExecutionObservation {
    observed_request(h, inputs.request(), previous).await
}
async fn observed_request(
    h: &Inputs,
    request: &aq_admission::preparation_v2::PrepareRequest,
    previous: Option<&ExecutionObservation>,
) -> ExecutionObservation {
    let s = h.pipeline().snapshot().await.unwrap();
    let a = &s.attempts[&h.id];
    let intent = a.phase_acceptance.as_ref().unwrap();
    h.client
        .prepare_execution(
            h.proposed.as_ref().unwrap(),
            &intent.request,
            h.phase.as_ref().unwrap(),
            request,
            previous,
        )
        .await
        .unwrap()
}
async fn pause_entered(pause: &WritePause) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await
        }
    })
    .await
    .unwrap();
}
async fn snapshot(h: &Inputs) -> State {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match h.pipeline().snapshot().await {
                Ok(s) => break s,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_is_durable_before_usable_and_preserves_original_bundle_and_caps() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let o = observed(&h, &inputs, None).await;
    let deadline = o.deadline_ns();
    let first = o.first_send_deadline_ns();
    let PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .record_phase_execution(inputs, o)
        .await
        .unwrap()
    else {
        panic!("expected current execution owner")
    };
    h.pipeline().check_phase_execution(&owner).await.unwrap();
    assert!(owner.deadline_ns() <= deadline);
    let state = h.pipeline().snapshot().await.unwrap();
    let record = state.attempts[&h.id].phase_execution.as_ref().unwrap();
    assert_eq!(record.first_send_deadline_ns, first);
    assert!(state.attempts[&h.id].initial_grant.is_none());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_replay_never_replaces_first_history_or_extends_caps() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let request = inputs.request().clone();
    let o = observed(&h, &inputs, None).await;
    let PhaseExecutionCommit::Usable(first) = h
        .pipeline()
        .record_phase_execution(inputs, o)
        .await
        .unwrap()
    else {
        panic!()
    };
    let before = h.pipeline().snapshot().await.unwrap();
    let record = before.attempts[&h.id].phase_execution.as_ref().unwrap();
    let o = observed_request(&h, &request, None).await;
    let PhaseExecutionCommit::Usable(replay) =
        h.pipeline().retry_phase_execution(h.id, o).await.unwrap()
    else {
        panic!()
    };
    assert!(replay.deadline_ns() <= first.deadline_ns());
    assert!(replay.hard_deadline_ns() <= first.hard_deadline_ns());
    let after = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        canonical_bytes(record).unwrap(),
        canonical_bytes(after.attempts[&h.id].phase_execution.as_ref().unwrap()).unwrap()
    );
    assert_eq!(before.revision, after.revision);
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    h.grant_mode.store(1, Ordering::SeqCst);
    let changed = observed_request(&h, &request, None).await;
    assert!(
        h.pipeline()
            .retry_phase_execution(h.id, changed)
            .await
            .is_err()
    );
    h.pipeline().check_phase_execution(&first).await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_outlives_preparation_without_retiming_historical_source_proof() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let mut h = Inputs::new_phase(export).await;
    let short = shorter_phase(&h, 4000).await;
    h.phase = Some(short);
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let old_source_cap = inputs.workspace().deadline_ns();
    let o = Arc::new(observed(&h, &inputs, None).await);
    let request = inputs.request().clone();
    while boot_ns().unwrap() <= old_source_cap {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .record_phase_execution(inputs, o.clone())
        .await
        .unwrap()
    else {
        panic!("new execution owns a separate live interval")
    };
    h.pipeline().check_phase_execution(&owner).await.unwrap();
    let s = h.pipeline().snapshot().await.unwrap();
    assert!(
        s.attempts[&h.id]
            .phase_preparation
            .as_ref()
            .unwrap()
            .deadline_ns
            > old_source_cap
    );
    assert!(s.attempts[&h.id].phase_execution.as_ref().unwrap().sent_ns < old_source_cap);
    let first_record = s.attempts[&h.id].phase_execution.as_ref().unwrap();
    let replay = observed_request(&h, &request, Some(o.as_ref())).await;
    assert_eq!(replay.first_send_deadline_ns(), o.first_send_deadline_ns());
    assert_eq!(
        canonical_bytes(replay.first_transmission().0).unwrap(),
        canonical_bytes(o.delivery()).unwrap()
    );
    assert!(
        replay.deadline_ns() <= o.deadline_ns()
            && replay.hard_deadline_ns() <= o.hard_deadline_ns()
    );
    let PhaseExecutionCommit::Usable(retried) = h
        .pipeline()
        .retry_phase_execution(h.id, Arc::new(replay))
        .await
        .unwrap()
    else {
        panic!("exact late wire replay")
    };
    assert!(
        retried.deadline_ns() <= owner.deadline_ns()
            && retried.hard_deadline_ns() <= owner.hard_deadline_ns()
    );
    let final_state = h.pipeline().snapshot().await.unwrap();
    assert_eq!(
        canonical_bytes(first_record).unwrap(),
        canonical_bytes(
            final_state.attempts[&h.id]
                .phase_execution
                .as_ref()
                .unwrap()
        )
        .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_expiry_during_fsync_is_history_only_and_keeps_resources() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(120, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let o = observed(&h, &inputs, None).await;
    let end = o.deadline_ns();
    let pause = WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.record_phase_execution(inputs, o).await });
    pause_entered(&pause).await;
    while boot_ns().unwrap() <= end {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    pause.release();
    assert!(matches!(
        task.await.unwrap().unwrap(),
        PhaseExecutionCommit::HistoryOnly(_)
    ));
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .phase_execution
            .is_some()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_unpolled_and_cancelled_futures_preserve_exclusive_bundle_and_commit() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let q = inputs.request().clone();
    let o = observed(&h, &inputs, None).await;
    let future = h.pipeline().record_phase_execution(inputs, o);
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    drop(future);
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .phase_execution
            .is_none()
    );
    let o = observed_request(&h, &q, None).await;
    let pause = WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let id = h.id;
    let task = tokio::spawn(async move { pipeline.retry_phase_execution(id, o).await });
    pause_entered(&pause).await;
    task.abort();
    pause.release();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    let before = snapshot(&h).await;
    assert!(before.attempts[&id].phase_execution.is_some());
    let o = observed_request(&h, &q, None).await;
    assert!(matches!(
        h.pipeline().retry_phase_execution(id, o).await.unwrap(),
        PhaseExecutionCommit::Usable(_)
    ));
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_write_cuts_reopen_history_only_and_preserve_all_holds() {
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
        h.grant_window_ms.store(5000, Ordering::SeqCst);
        let inputs = phase_join(&h, git.client.clone()).await;
        let o = observed(&h, &inputs, None).await;
        h.pipeline().test_journal_fault(fault).await.unwrap();
        assert!(
            h.pipeline()
                .record_phase_execution(inputs, o)
                .await
                .is_err()
        );
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
        drop(h.pipeline.take());
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        let reopened = Journal::open(&h.root, identity().node_id, cap).unwrap();
        assert!(reopened.available().is_err());
        let a = &reopened.state().attempts[&h.id];
        assert!(
            a.phase_prepared.is_some()
                && a.source.as_ref().unwrap().complete()
                && a.initial_grant.is_none()
        );
        if let Some(r) = &a.phase_execution {
            assert!(
                reopened
                    .check_phase_execution(
                        h.id,
                        &r.digest().unwrap(),
                        r.deadline_ns,
                        r.hard_deadline_ns
                    )
                    .is_err()
            );
        }
        assert_eq!(
            a.phase_execution.is_some(),
            matches!(
                fault,
                WriteFault::AfterRename | WriteFault::AfterDirectorySync
            )
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_current_private_record_and_git_changes_refuse_without_erasing_history() {
    for kind in 0..3 {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let h = Inputs::new_phase(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        h.grant_window_ms.store(5000, Ordering::SeqCst);
        let inputs = phase_join(&h, git.client.clone()).await;
        let lease = inputs.request().original.materials[0].lease_id;
        let o = observed(&h, &inputs, None).await;
        if kind == 0 {
            fs::write(
                h._dir
                    .path()
                    .join("materials")
                    .join(lease.to_string())
                    .join("material.bin"),
                b"changed",
            )
            .unwrap();
        } else if kind == 1 {
            fs::write(
                h._dir
                    .path()
                    .join("materials")
                    .join(lease.to_string())
                    .join("record.json"),
                b"{}",
            )
            .unwrap();
        } else {
            git.mode.store(2, Ordering::SeqCst);
        }
        assert!(matches!(
            h.pipeline()
                .record_phase_execution(inputs, o)
                .await
                .unwrap(),
            PhaseExecutionCommit::HistoryOnly(_)
        ));
        assert!(
            h.pipeline().snapshot().await.unwrap().attempts[&h.id]
                .phase_execution
                .is_some()
        );
        assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_and_legacy_resource_maps_share_admission_and_keep_first_exclusive_owner() {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let legacy = Inputs::new(export).await;
    let old_git = GitPeer::new(legacy.root.clone(), manifest);
    legacy.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = granted_scenarios::joined(&legacy, &old_git).await;
    let o = granted_scenarios::observed(&legacy, inputs.request()).await;
    let crate::native::preparation::GrantCommit::Usable(old) = legacy
        .pipeline()
        .record_initial_grant(inputs, o)
        .await
        .unwrap()
    else {
        panic!()
    };
    let export = synthetic();
    let manifest = export.manifest.clone();
    let phase = Inputs::new_phase(export).await;
    let git = GitPeer::new(phase.root.clone(), manifest);
    phase.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = phase_join(&phase, git.client.clone()).await;
    let o = observed(&phase, &inputs, None).await;
    // Both fixtures deliberately use the same attempt identity. A second protocol
    // cannot obtain an independent map slot or replace the first physical bundle.
    assert_eq!(legacy.id, phase.id);
    let before = legacy.pipeline().retained_input_stats().unwrap();
    assert!(
        legacy
            .pipeline()
            .record_phase_execution(inputs, o)
            .await
            .is_err()
    );
    assert_eq!(before, legacy.pipeline().retained_input_stats().unwrap());
    legacy.pipeline().check_initial_grant(&old).await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_history_reopen_refuses_changed_runtime_request_and_sender_cap() {
    for kind in 0..3 {
        let export = synthetic();
        let manifest = export.manifest.clone();
        let mut h = Inputs::new_phase(export).await;
        let git = GitPeer::new(h.root.clone(), manifest);
        h.grant_window_ms.store(5000, Ordering::SeqCst);
        let inputs = phase_join(&h, git.client.clone()).await;
        let o = observed(&h, &inputs, None).await;
        let PhaseExecutionCommit::Usable(owner) = h
            .pipeline()
            .record_phase_execution(inputs, o)
            .await
            .unwrap()
        else {
            panic!()
        };
        let other = Inputs::new_phase(synthetic()).await;
        assert!(
            other
                .pipeline()
                .check_phase_execution(&owner)
                .await
                .is_err()
        );
        drop(owner);
        drop(h.pipeline.take());
        rewrite(&h.root, |s| {
            let a = s.attempts.get_mut(&h.id).unwrap();
            let cap = a.phase_preparation.as_ref().unwrap().deadline_ns;
            let r = a.phase_execution.as_mut().unwrap();
            match kind {
                0 => r.runtime.qualification.digest = Digest::of(b"changed runtime"),
                1 => r.request_digest = Digest::of(b"other Prepare"),
                _ => r.first_send_deadline_ns = cap + 1,
            }
        });
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        assert!(Journal::open(&h.root, identity().node_id, cap).is_err());
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_execution_foreign_actual_observation_cannot_rebind_prepared_owner_and_retry_keeps_first_bundle()
 {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(5000, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let export = synthetic();
    let manifest = export.manifest.clone();
    let other = Inputs::new_phase(export).await;
    let peer = GitPeer::new(other.root.clone(), manifest);
    other.grant_window_ms.store(5000, Ordering::SeqCst);
    let other_inputs = phase_join(&other, peer.client.clone()).await;
    let wrong = observed(&other, &other_inputs, None).await;
    let correct = observed(&h, &inputs, None).await;
    assert!(
        h.pipeline()
            .record_phase_execution(inputs, wrong)
            .await
            .is_err()
    );
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .phase_execution
            .is_none()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    let PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .retry_phase_execution(h.id, correct)
        .await
        .unwrap()
    else {
        panic!()
    };
    h.pipeline().check_phase_execution(&owner).await.unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
