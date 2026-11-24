#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_reopen_only_stops_exact_existing_host_and_never_restores_execution() {
    let (mut h, _git, execution) = ready(5000).await;
    let host = running_host(&h, &execution).await;
    let first = h.pipeline().snapshot().await.unwrap();
    let history = first.attempts[&h.id].native_host.as_ref().unwrap();
    let configuration = history.host_intent.clone();
    drop(host);
    drop(execution);
    drop(h.pipeline.take());
    let recovered = Arc::new(reopen_pipeline(&h));
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
            let cap = host_limits();
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
                    assert!(!host_directory(history).join("fixture-marker").exists());
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
    let _host = running_host(&h, &execution).await;
    let history = current_host(&h);
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
