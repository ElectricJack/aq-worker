//! Explicit local tmux3.4/pinned synthetic ELF. No real issuer/provider claim.
use super::*;
use crate::native::{
    launch::{
        Checkpoint, InstalledFilePin, NativeHostLaunchResult, NativeStopOutcome,
        SyntheticHostConfig, SyntheticHostProfile, SyntheticMode,
    },
    preparation::{DurablePhaseExecution, PhaseExecutionCommit},
};
fn pin(binary: &str, hash: &str, id: &str) -> InstalledFilePin {
    let path =
        PathBuf::from(std::env::var_os(binary).expect("explicit native host fixture binary"));
    let digest: Digest = serde_json::from_value(json!(
        std::env::var(hash).expect("explicit native host fixture SHA256")
    ))
    .unwrap();
    InstalledFilePin {
        path: fs::canonicalize(path).unwrap(),
        artifact: ArtifactRef {
            id: aq_session_protocol::Text::new(id).unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest,
        },
    }
}
pub(super) fn profile(h: &Inputs, mode: SyntheticMode) -> SyntheticHostProfile {
    let root = h._dir.path().join("host");
    if !root.exists() {
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let snapshot: Snapshot =
        serde_json::from_slice(&fs::read(h.root.join("journal.json")).unwrap()).unwrap();
    let a = &snapshot.state.attempts[&h.id];
    SyntheticHostProfile::new(SyntheticHostConfig {
        root,
        helper: pin(
            "AQ_NATIVE_HOST_BINARY",
            "AQ_NATIVE_HOST_SHA256",
            "aq.synthetic-host-helper",
        ),
        tmux: pin(
            "AQ_NATIVE_TMUX_BINARY",
            "AQ_NATIVE_TMUX_SHA256",
            "aq.synthetic-system-tmux34",
        ),
        program: pin(
            "AQ_NATIVE_SYNTHETIC_ELF",
            "AQ_NATIVE_SYNTHETIC_SHA256",
            "aq.synthetic-native-harness",
        ),
        harness: a.reservation.offer.offered.offer.manifest.harness.clone(),
        capabilities: capabilities(),
        renderer: ArtifactRef {
            id: aq_session_protocol::Text::new("aq.synthetic-baseline-ascii").unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic baseline only; no Snapshot qualification"),
        },
        columns: 80,
        rows: 24,
        mode,
    })
    .unwrap()
}
async fn ready(window: u64) -> (Inputs, GitPeer, DurablePhaseExecution) {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(window, Ordering::SeqCst);
    let inputs = phase_join(&h, git.client.clone()).await;
    let snapshot = h.pipeline().snapshot().await.unwrap();
    let a = &snapshot.attempts[&h.id];
    let intent = a.phase_acceptance.as_ref().unwrap();
    let observed = h
        .client
        .prepare_execution(
            h.proposed.as_ref().unwrap(),
            &intent.request,
            h.phase.as_ref().unwrap(),
            inputs.request(),
            None,
        )
        .await
        .unwrap();
    let PhaseExecutionCommit::Usable(owner) = h
        .pipeline()
        .record_phase_execution(inputs, observed)
        .await
        .unwrap()
    else {
        panic!("synthetic execution owner expired before native host admission")
    };
    (h, git, owner)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_actual_running_then_stop_retains_typed_history() {
    let (h, _git, execution) = ready(5000).await;
    let p = profile(&h, SyntheticMode::Wait);
    let result = h
        .pipeline()
        .launch_phase_synthetic(&execution, p)
        .await
        .unwrap();
    let NativeHostLaunchResult::Running(host) = result else {
        let snapshot = h.pipeline().snapshot().await.unwrap();
        let bytes = canonical_bytes(&snapshot.attempts[&h.id].native_host).unwrap();
        panic!(
            "native host did not reach actual Running: {}",
            String::from_utf8_lossy(&bytes)
        );
    };
    let request = host.started_request().unwrap();
    request.validate().unwrap();
    assert_ne!(request.guardian_id, request.observation.guardian.nonce);
    let snapshot = h.pipeline().snapshot().await.unwrap();
    let history = snapshot.attempts[&h.id].native_host.as_ref().unwrap();
    let directory = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string());
    assert_eq!(
        fs::read(directory.join("fixture-marker")).unwrap(),
        b"instructions\n\ntask-context"
    );
    let created = &snapshot.attempts[&h.id].source.as_ref().unwrap().operations["create"]
        .result
        .as_ref()
        .unwrap();
    let workspace: crate::source::Workspace = serde_json::from_value((*created).clone()).unwrap();
    assert_eq!(
        fs::read_to_string(directory.join("fixture-marker.cwd")).unwrap(),
        workspace.path.to_str().unwrap()
    );
    assert!(history.baseline.is_some() && history.running.is_some());
    assert!(history.terminal.is_some());
    assert!(history.started_receipt.is_none());
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    let final_state = h.pipeline().snapshot().await.unwrap();
    let stopped = final_state.attempts[&h.id]
        .native_host
        .as_ref()
        .unwrap()
        .stopped
        .as_ref()
        .unwrap();
    assert!(stopped.observation.kernel_wait_echild);
    assert!(stopped.observation.root_status.is_some());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    assert!(
        h.pipeline()
            .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
            .await
            .is_err()
    );
}
async fn barrier(pause: &WritePause) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_stop_progresses_while_baseline_writer_is_paused() {
    let (h, _git, execution) = ready(5000).await;
    let pause = WritePause::new();
    h.pipeline()
        .test_pause_native(Checkpoint::Baseline, &pause)
        .unwrap();
    let p = profile(&h, SyntheticMode::Wait);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.launch_phase_synthetic(&execution, p).await });
    barrier(&pause).await;
    assert!(h.pipeline().snapshot().await.is_err());
    let started = std::time::Instant::now();
    let outcome = h.pipeline().stop_phase_host(h.id).await.unwrap();
    assert!(matches!(
        outcome,
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let history = h.pipeline().phase_host_history(h.id).unwrap().unwrap();
    assert!(history.running.is_none());
    assert!(
        !history
            .intent
            .host_root
            .join(history.intent.launch_id.to_string())
            .join("fixture-marker")
            .exists()
    );
    pause.release();
    assert!(matches!(
        task.await.unwrap().unwrap(),
        NativeHostLaunchResult::StoppedBeforeStarted(_)
    ));
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.stopped.is_some() && history.running.is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_original_expiry_stops_guardian_while_baseline_writer_is_paused() {
    let (h, _git, execution) = ready(1800).await;
    let end = execution.deadline_ns();
    let pause = WritePause::new();
    h.pipeline()
        .test_pause_native(Checkpoint::Baseline, &pause)
        .unwrap();
    let p = profile(&h, SyntheticMode::Wait);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.launch_phase_synthetic(&execution, p).await });
    barrier(&pause).await;
    let history = h.pipeline().phase_host_history(h.id).unwrap().unwrap();
    let directory = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string());
    let observation = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            if let Ok(raw) = fs::read(directory.join("state.json"))
                && let Ok(o) = serde_json::from_slice::<aq_session_host::HostObservation>(&raw)
                && o.phase == aq_session_host::HostPhase::Stopped
                && o.kernel_wait_echild
            {
                break o;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(boot_ns().unwrap() >= end);
    assert!(
        observation
            .stop_reason
            .as_ref()
            .is_some_and(|r| r.contains("expired"))
    );
    assert!(!directory.join("fixture-marker").exists());
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    pause.release();
    assert!(matches!(
        task.await.unwrap().unwrap(),
        NativeHostLaunchResult::StoppedBeforeStarted(_)
    ));
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.running.is_none() && history.stopped.is_some());
    assert_eq!(history.intent.deadline_ns, end);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_cancelled_waiter_keeps_one_exact_admitted_job() {
    let (h, _git, execution) = ready(5000).await;
    let pause = WritePause::new();
    h.pipeline()
        .test_pause_native(Checkpoint::Baseline, &pause)
        .unwrap();
    let p = profile(&h, SyntheticMode::Wait);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.launch_phase_synthetic(&execution, p).await });
    barrier(&pause).await;
    task.abort();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    pause.release();
    let history = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(Some(h)) = h.pipeline().phase_host_history(h.id)
                && h.running.is_some()
            {
                break h;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await
        }
    })
    .await
    .unwrap();
    assert!(history.baseline.is_some());
    assert_eq!(fs::read_dir(&history.intent.host_root).unwrap().count(), 1);
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if h.pipeline().persist_phase_host_stop(h.id).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let final_state = h.pipeline().snapshot().await.unwrap();
    assert!(
        final_state.attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .stopped
            .is_some()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_reopen_only_stops_exact_existing_host_and_never_restores_execution() {
    let (mut h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!()
    };
    let first = h.pipeline().snapshot().await.unwrap();
    let history = first.attempts[&h.id].native_host.as_ref().unwrap();
    let configuration = history.host_intent.clone();
    drop(host);
    drop(execution);
    drop(h.pipeline.take());
    let mut cap = limits();
    cap.disk_bytes = 64 * 1024 * 1024 * 1024;
    let journal = Journal::open(&h.root, identity().node_id, cap).unwrap();
    assert!(journal.available().is_err());
    let store = crate::native::materials::MaterialStore::open(
        &h._dir.path().join("materials"),
        identity(),
        32,
    )
    .unwrap();
    let recovered = Arc::new(InputPipeline::new(journal, store, h.client.clone()));
    assert!(matches!(
        recovered.stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    recovered.persist_phase_host_stop(h.id).await.unwrap();
    let last = recovered.snapshot().await.unwrap();
    assert_eq!(last.identity, first.identity);
    let history = last.attempts[&h.id].native_host.as_ref().unwrap();
    assert_eq!(history.host_intent, configuration);
    assert!(history.stopped.is_some());
    assert_eq!(fs::read_dir(&history.intent.host_root).unwrap().count(), 1);
    assert_eq!(recovered.retained_input_stats().unwrap().attempts, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_every_permission_write_cut_is_unknown_and_never_relaunched() {
    for stage in [
        Checkpoint::Outer,
        Checkpoint::Configured,
        Checkpoint::Guardian,
        Checkpoint::StartIntent,
        Checkpoint::Baseline,
        Checkpoint::Started,
    ] {
        for fault in [WriteFault::BeforeWrite, WriteFault::AfterRename] {
            let (mut h, _git, execution) = ready(5000).await;
            h.pipeline().test_fault_native(stage, fault).unwrap();
            let p = profile(&h, SyntheticMode::Wait);
            let result = h
                .pipeline()
                .launch_phase_synthetic(&execution, p)
                .await
                .unwrap();
            assert!(matches!(result, NativeHostLaunchResult::Unknown(_)));
            assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
            drop(result);
            drop(execution);
            drop(h.pipeline.take());
            let mut cap = limits();
            cap.disk_bytes = 64 * 1024 * 1024 * 1024;
            let journal = Journal::open(&h.root, identity().node_id, cap).unwrap();
            assert!(journal.available().is_err());
            let a = &journal.state().attempts[&h.id];
            if stage == Checkpoint::Outer && matches!(fault, WriteFault::BeforeWrite) {
                assert!(a.native_host.is_none());
            } else {
                let history = a.native_host.as_ref().unwrap();
                assert!(history.started_receipt.is_none());
                assert_eq!(
                    history.running.is_some(),
                    stage == Checkpoint::Started && matches!(fault, WriteFault::AfterRename)
                );
                if stage != Checkpoint::Started {
                    assert!(
                        !history
                            .intent
                            .host_root
                            .join(history.intent.launch_id.to_string())
                            .join("fixture-marker")
                            .exists()
                    );
                }
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_root_replacement_after_intent_never_switches_namespace() {
    let (h, _git, execution) = ready(5000).await;
    let pause = WritePause::new();
    h.pipeline()
        .test_pause_native(Checkpoint::Outer, &pause)
        .unwrap();
    let p = profile(&h, SyntheticMode::Wait);
    let root = h._dir.path().join("host");
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.launch_phase_synthetic(&execution, p).await });
    barrier(&pause).await;
    fs::rename(&root, h._dir.path().join("old-host")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    pause.release();
    assert!(matches!(
        task.await.unwrap().unwrap(),
        NativeHostLaunchResult::Unknown(_)
    ));
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    assert_eq!(
        fs::read_dir(h._dir.path().join("old-host"))
            .unwrap()
            .count(),
        0
    );
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.host_intent.is_none() && history.running.is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_guardian_loss_is_unknown_never_a_stopped_proof() {
    let (h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(_host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!()
    };
    let history = h.pipeline().phase_host_history(h.id).unwrap().unwrap();
    let running = &history.running.as_ref().unwrap().observation;
    let guardian = aq_session_host::ownership::verified_pidfd(&running.guardian.process).unwrap();
    aq_session_host::ownership::signal(&guardian, libc::SIGKILL).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while aq_session_host::ownership::pidfd_is_live(&guardian).unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::Unknown
    ));
    h.pipeline()
        .persist_phase_host_uncertain(h.id)
        .await
        .unwrap();
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.uncertain.is_some() && history.stopped.is_none());
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    // Explicit fault-fixture cleanup targets only the exact verified known children;
    // this is not a runtime stopped proof or an ECHILD/release assertion.
    let pane = running.pane.as_ref().unwrap();
    for p in [&pane.pane, &pane.server] {
        if let Ok(fd) = aq_session_host::ownership::verified_pidfd(p) {
            let _ = aq_session_host::ownership::signal(&fd, libc::SIGKILL);
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_invalid_sealed_program_never_falls_back_to_shell() {
    let (h, _git, execution) = ready(5000).await;
    let p = profile(&h, SyntheticMode::Wait);
    let mut config = p.config_for_test();
    let bad = h._dir.path().join("invalid-elf");
    fs::write(&bad, b"not an ELF and never a shell command").unwrap();
    config.program.path = bad;
    config.program.artifact.digest = Digest::of(b"not an ELF and never a shell command");
    let p = SyntheticHostProfile::new(config).unwrap();
    assert!(matches!(
        h.pipeline()
            .launch_phase_synthetic(&execution, p)
            .await
            .unwrap(),
        NativeHostLaunchResult::Unknown(_)
    ));
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(
        history.host_intent.is_none() && history.guardian.is_none() && history.running.is_none()
    );
    assert_eq!(fs::read_dir(&history.intent.host_root).unwrap().count(), 0);
}
#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned test-support helper and actual tmux3.4; isolated missed-exec fault"]
async fn native_phase_host_short_exec_without_positive_guardian_proof_has_no_started() {
    if std::env::var_os("AQ_NATIVE_SHORT_HOST_CHILD").is_none() {
        let status=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","native::journal::tests::hydration_scenarios::native_host_scenarios::native_phase_host_short_exec_without_positive_guardian_proof_has_no_started","--ignored","--nocapture"]).env("AQ_NATIVE_SHORT_HOST_CHILD","1").env("AQ_SESSION_HOST_FAULT_STAGE","observed-exec-permit").status().unwrap();
        assert!(status.success());
        return;
    }
    let (h, _git, execution) = ready(5000).await;
    let p = profile(&h, SyntheticMode::ExitImmediately);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.launch_phase_synthetic(&execution, p).await });
    let history = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(Some(h)) = h.pipeline().phase_host_history(h.id)
                && h.baseline.is_some()
            {
                break h;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await
        }
    })
    .await
    .unwrap();
    let directory = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !directory.join("guardian-fault.json").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let state: aq_session_host::HostObservation =
        serde_json::from_slice(&fs::read(directory.join("state.json")).unwrap()).unwrap();
    let fd = aq_session_host::ownership::verified_pidfd(&state.guardian.process).unwrap();
    struct Resume(std::os::fd::OwnedFd);
    impl Drop for Resume {
        fn drop(&mut self) {
            let _ = aq_session_host::ownership::signal(&self.0, libc::SIGCONT);
        }
    }
    let resume = Resume(fd);
    let pane = state.pane.as_ref().unwrap().pane.clone();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while aq_session_host::ownership::process_evidence(pane.pid).is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        h.pipeline()
            .phase_host_history(h.id)
            .unwrap()
            .unwrap()
            .running
            .is_none()
    );
    drop(resume);
    let outcome = task.await.unwrap().unwrap();
    assert!(!matches!(outcome, NativeHostLaunchResult::Running(_)));
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(
        state.attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .running
            .is_none()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_started_outbox_never_fakes_ack_and_replays_exact_fact() {
    let (h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!()
    };
    let first = host.started_request().unwrap();
    assert!(h.pipeline().report_phase_started(&host).await.is_err());
    assert!(
        h.pipeline()
            .phase_host_history(h.id)
            .unwrap()
            .unwrap()
            .started_receipt
            .is_none()
    );
    h.host_report_mode.store(2, Ordering::SeqCst);
    assert!(h.pipeline().report_phase_started(&host).await.is_err());
    h.host_report_mode.store(1, Ordering::SeqCst);
    let receipt = h.pipeline().report_phase_started(&host).await.unwrap();
    receipt.validate_for(&first).unwrap();
    let calls = h.host_report_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        canonical_bytes(&calls[0]).unwrap(),
        canonical_bytes(&calls[1]).unwrap()
    );
    assert_eq!(
        canonical_bytes(&calls[0]).unwrap(),
        canonical_bytes(&first).unwrap()
    );
    let before = h.pipeline().snapshot().await.unwrap();
    let again = h.pipeline().report_phase_started(&host).await.unwrap();
    assert_eq!(
        canonical_bytes(&again).unwrap(),
        canonical_bytes(&receipt).unwrap()
    );
    assert_eq!(h.host_report_calls.lock().unwrap().len(), 2);
    assert_eq!(
        h.pipeline().snapshot().await.unwrap().revision,
        before.revision
    );
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    let historical = h.pipeline().report_phase_started(&host).await.unwrap();
    assert_eq!(
        canonical_bytes(&historical).unwrap(),
        canonical_bytes(&receipt).unwrap()
    );
    assert_eq!(h.host_report_calls.lock().unwrap().len(), 2);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_unadmitted_claim_is_released_but_stop_fence_cannot_be_reset() {
    let (h, _git, execution) = ready(5000).await;
    let future = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait));
    drop(future);
    assert!(h.pipeline().phase_host_history(h.id).is_err());
    let slot = h.pipeline().test_hold_bootstrap().unwrap();
    assert!(
        h.pipeline()
            .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
            .await
            .is_err()
    );
    drop(slot);
    assert!(h.pipeline().phase_host_history(h.id).is_err());
    let slot = h.pipeline().test_hold_writer().unwrap();
    assert!(
        h.pipeline()
            .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
            .await
            .is_err()
    );
    drop(slot);
    assert!(h.pipeline().phase_host_history(h.id).is_err());
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .native_host
            .is_none()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    let future = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait));
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::Unknown
    ));
    drop(future);
    assert!(
        h.pipeline()
            .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
            .await
            .is_err()
    );
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .native_host
            .is_none()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native synthetic host and actual tmux3.4; phase terminal peer is synthetic"]
async fn native_phase_terminal_actual_pair_lost_ack_immutable_retry_and_restart_readback() {
    let (mut h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!("actual Running absent")
    };
    let first = h
        .pipeline()
        .phase_terminal_history(h.id)
        .await
        .unwrap()
        .unwrap();
    assert!(first.started.receipt.is_none() && first.stopped.is_none());
    assert_eq!(
        first.started.request.original.operation_id,
        host.started_request().unwrap().operation_id
    );
    let before = h.pipeline().snapshot().await.unwrap();
    let a = &before.attempts[&h.id];
    assert_eq!(
        first.started.request.origin.execution_receipt_digest,
        a.phase_execution
            .as_ref()
            .unwrap()
            .delivery
            .receipt
            .digest()
            .unwrap()
    );
    assert_eq!(
        first.started.request.origin.accepted_receipt_digest,
        a.phase_preparation.as_ref().unwrap().receipt_digest
    );
    h.host_report_mode.store(2, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .report_phase_terminal_started(h.id)
            .await
            .is_err()
    );
    h.host_report_mode.store(1, Ordering::SeqCst);
    let started = h
        .pipeline()
        .report_phase_terminal_started(h.id)
        .await
        .unwrap();
    started.validate_for(&first.started.request).unwrap();
    let calls = h.phase_terminal_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    let pair = h
        .pipeline()
        .phase_terminal_history(h.id)
        .await
        .unwrap()
        .unwrap();
    let stopped = pair.stopped.as_ref().unwrap();
    stopped
        .request
        .original
        .validate_after(&pair.started.request.original)
        .unwrap();
    assert_eq!(stopped.request.origin, pair.started.request.origin);
    h.host_report_mode.store(2, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .report_phase_terminal_stopped(h.id)
            .await
            .is_err()
    );
    h.host_report_mode.store(3, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .report_phase_terminal_stopped(h.id)
            .await
            .is_err()
    );
    h.host_report_mode.store(1, Ordering::SeqCst);
    let stop_ack = h
        .pipeline()
        .report_phase_terminal_stopped(h.id)
        .await
        .unwrap();
    stop_ack.validate_for(&stopped.request).unwrap();
    let calls = h.phase_terminal_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[2], calls[3]);
    assert_eq!(calls[3], calls[4]);
    drop(host);
    drop(execution);
    drop(h.pipeline.take());
    let mut cap = limits();
    cap.disk_bytes = 64 * 1024 * 1024 * 1024;
    let journal = Journal::open(&h.root, identity().node_id, cap).unwrap();
    assert!(journal.available().is_err());
    let store = crate::native::materials::MaterialStore::open(
        &h._dir.path().join("materials"),
        identity(),
        32,
    )
    .unwrap();
    let recovered = Arc::new(InputPipeline::new(journal, store, h.client.clone()));
    let revision = recovered.snapshot().await.unwrap().revision;
    assert!(
        same(
            &recovered.report_phase_terminal_started(h.id).await.unwrap(),
            &started
        )
        .unwrap()
    );
    assert!(
        same(
            &recovered.report_phase_terminal_stopped(h.id).await.unwrap(),
            &stop_ack
        )
        .unwrap()
    );
    assert_eq!(recovered.snapshot().await.unwrap().revision, revision);
    assert_eq!(h.phase_terminal_calls.lock().unwrap().len(), 5);
    assert_eq!(recovered.snapshot().await.unwrap().attempts.len(), 1);
    assert_eq!(recovered.retained_input_stats().unwrap().attempts, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native synthetic host and actual tmux3.4; historical report after restart only"]
async fn native_phase_terminal_unacked_restart_retries_without_live_authority_or_discard() {
    let (mut h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!()
    };
    let original = h
        .pipeline()
        .phase_terminal_history(h.id)
        .await
        .unwrap()
        .unwrap();
    h.host_report_mode.store(2, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .report_phase_terminal_started(h.id)
            .await
            .is_err()
    );
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    drop(host);
    drop(execution);
    drop(h.pipeline.take());
    let mut cap = limits();
    cap.disk_bytes = 64 * 1024 * 1024 * 1024;
    let journal = Journal::open(&h.root, identity().node_id, cap).unwrap();
    assert!(journal.recovery_required());
    assert!(journal.available().is_err());
    let store = crate::native::materials::MaterialStore::open(
        &h._dir.path().join("materials"),
        identity(),
        32,
    )
    .unwrap();
    let recovered = InputPipeline::new(journal, store, h.client.clone());
    // Unsupported/no matching centrally admitted Started: both queues remain held.
    h.host_report_mode.store(0, Ordering::SeqCst);
    assert!(recovered.report_phase_terminal_started(h.id).await.is_err());
    assert!(recovered.report_phase_terminal_stopped(h.id).await.is_err());
    let held = recovered
        .phase_terminal_history(h.id)
        .await
        .unwrap()
        .unwrap();
    assert!(held.started.receipt.is_none() && held.stopped.as_ref().unwrap().receipt.is_none());
    assert_eq!(
        held.started.request.digest().unwrap(),
        original.started.request.digest().unwrap()
    );
    h.host_report_mode.store(1, Ordering::SeqCst);
    recovered.report_phase_terminal_started(h.id).await.unwrap();
    recovered.report_phase_terminal_stopped(h.id).await.unwrap();
    let calls = h.phase_terminal_calls.lock().unwrap();
    assert_eq!(calls[0], calls[1]);
    assert_eq!(recovered.retained_input_stats().unwrap().attempts, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native synthetic host and actual tmux3.4; fsync cut copies are history-only"]
async fn native_phase_terminal_ack_write_cuts_reopen_old_or_whole_ack_without_freeing_holds() {
    let (h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(_host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    let initial = h.pipeline().snapshot().await.unwrap();
    let t = initial.attempts[&h.id]
        .native_host
        .as_ref()
        .unwrap()
        .terminal
        .as_ref()
        .unwrap();
    h.host_report_mode.store(1, Ordering::SeqCst);
    let started = h
        .client
        .phase_terminal_started(&t.started.request)
        .await
        .unwrap();
    let stopped = h
        .client
        .phase_terminal_stopped(&t.stopped.as_ref().unwrap().request)
        .await
        .unwrap();
    for (is_stop, fault) in [
        (false, WriteFault::BeforeWrite),
        (false, WriteFault::AfterRename),
        (true, WriteFault::BeforeWrite),
        (true, WriteFault::AfterRename),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::copy(h.root.join("journal.json"), root.join("journal.json")).unwrap();
        fs::set_permissions(root.join("journal.json"), fs::Permissions::from_mode(0o600)).unwrap();
        let mut cap = limits();
        cap.disk_bytes = 64 * 1024 * 1024 * 1024;
        let mut journal = Journal::open(&root, identity().node_id, cap.clone()).unwrap();
        journal.inject_fault(fault);
        let result = if is_stop {
            journal.phase_stopped_receipt(h.id, stopped.clone())
        } else {
            journal.phase_started_receipt(h.id, started.clone())
        };
        assert!(result.is_err());
        assert!(journal.recovery_required());
        drop(journal);
        let mut reopened = Journal::open(&root, identity().node_id, cap).unwrap();
        assert!(reopened.available().is_err());
        assert_eq!(reopened.state().attempts.len(), 1);
        let saved = reopened.state().attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .terminal
            .as_ref()
            .unwrap();
        assert_eq!(
            saved.started.request.digest().unwrap(),
            t.started.request.digest().unwrap()
        );
        assert_eq!(
            saved.stopped.as_ref().unwrap().request.digest().unwrap(),
            t.stopped.as_ref().unwrap().request.digest().unwrap()
        );
        let present = if is_stop {
            saved.stopped.as_ref().unwrap().receipt.is_some()
        } else {
            saved.started.receipt.is_some()
        };
        assert_eq!(present, matches!(fault, WriteFault::AfterRename));
        if present {
            let revision = reopened.state().revision;
            let changed = if is_stop {
                let mut changed = stopped.clone();
                changed.original.recorded_at_ms += 1;
                reopened.phase_stopped_receipt(h.id, changed)
            } else {
                let mut changed = started.clone();
                changed.original.recorded_at_ms += 1;
                reopened.phase_started_receipt(h.id, changed)
            };
            assert!(changed.is_err());
            assert_eq!(reopened.state().revision, revision);
        }
    }
    // Cancel only the waiter after the ACK writer owns the fsync job. The
    // pipeline retains that job, immutable outbox and resources through commit.
    let pause = WritePause::new();
    h.pipeline().test_pause_grant_commit(&pause).await.unwrap();
    let pipeline = h.pipeline().clone();
    let id = h.id;
    let pending = tokio::spawn(async move { pipeline.report_phase_terminal_stopped(id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !pause.entered() {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    pause.release();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(Some(history)) = h.pipeline().phase_terminal_history(h.id).await
                && history
                    .stopped
                    .as_ref()
                    .is_some_and(|s| s.receipt.is_some())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    assert_eq!(h.pipeline().snapshot().await.unwrap().attempts.len(), 1);
}

#[cfg(feature = "test-support")]
#[path = "output_relay_scenarios.rs"]
mod output_relay_scenarios;
