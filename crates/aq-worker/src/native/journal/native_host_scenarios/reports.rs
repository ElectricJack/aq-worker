#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned test-support helper and actual tmux3.4; isolated missed-exec fault"]
async fn native_phase_host_short_exec_without_positive_guardian_proof_has_no_started() {
    if std::env::var_os("AQ_NATIVE_SHORT_HOST_CHILD").is_none() {
        let test = concat!(
            "native::journal::tests::hydration_scenarios::native_host_scenarios::",
            "native_phase_host_short_exec_without_positive_guardian_proof_has_no_started"
        );
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env("AQ_NATIVE_SHORT_HOST_CHILD", "1")
            .env("AQ_SESSION_HOST_FAULT_STAGE", "observed-exec-permit")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let (h, _git, execution) = ready(5000).await;
    let p = profile(&h, SyntheticMode::ExitImmediately);
    let pipeline = h.pipeline().clone();
    let task = tokio::spawn(async move { pipeline.launch_phase_synthetic(&execution, p).await });
    let history = wait_for_host(&h, |h| h.baseline.is_some()).await;
    let directory = host_directory(&history);
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
    assert!(current_host(&h).running.is_none());
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
    let host = running_host(&h, &execution).await;
    let first = host.started_request().unwrap();
    assert!(h.pipeline().report_phase_started(&host).await.is_err());
    assert!(current_host(&h).started_receipt.is_none());
    h.host_report_mode.store(2, Ordering::SeqCst);
    assert!(h.pipeline().report_phase_started(&host).await.is_err());
    h.host_report_mode.store(1, Ordering::SeqCst);
    let receipt = h.pipeline().report_phase_started(&host).await.unwrap();
    receipt.validate_for(&first).unwrap();
    let calls = h.host_report_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(same(&calls[0], &calls[1]).unwrap());
    assert!(same(&calls[0], &first).unwrap());
    let before = h.pipeline().snapshot().await.unwrap();
    let again = h.pipeline().report_phase_started(&host).await.unwrap();
    assert!(same(&again, &receipt).unwrap());
    assert_eq!(h.host_report_calls.lock().unwrap().len(), 2);
    assert_eq!(
        h.pipeline().snapshot().await.unwrap().revision,
        before.revision
    );
    stop_and_persist(&h).await;
    let historical = h.pipeline().report_phase_started(&host).await.unwrap();
    assert!(same(&historical, &receipt).unwrap());
    assert_eq!(h.host_report_calls.lock().unwrap().len(), 2);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_unadmitted_claim_is_released_but_stop_fence_cannot_be_reset() {
    let (h, _git, execution) = ready(5000).await;
    let future = launch_wait(&h, &execution);
    drop(future);
    assert!(h.pipeline().phase_host_history(h.id).is_err());
    let slot = h.pipeline().test_hold_bootstrap().unwrap();
    assert!(launch_wait(&h, &execution).await.is_err());
    drop(slot);
    assert!(h.pipeline().phase_host_history(h.id).is_err());
    let slot = h.pipeline().test_hold_writer().unwrap();
    assert!(launch_wait(&h, &execution).await.is_err());
    drop(slot);
    assert!(h.pipeline().phase_host_history(h.id).is_err());
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .native_host
            .is_none()
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    let future = launch_wait(&h, &execution);
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::Unknown
    ));
    drop(future);
    assert!(launch_wait(&h, &execution).await.is_err());
    assert!(
        h.pipeline().snapshot().await.unwrap().attempts[&h.id]
            .native_host
            .is_none()
    );
}
