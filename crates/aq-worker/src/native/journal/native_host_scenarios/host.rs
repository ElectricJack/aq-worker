#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn native_phase_host_actual_running_then_stop_retains_typed_history() {
    let (h, _git, execution) = ready(HOST_OBSERVATION_WINDOW_MS).await;
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
    let directory = host_directory(history);
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
    stop_and_persist(&h).await;
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
    assert!(launch_wait(&h, &execution).await.is_err());
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
    let history = current_host(&h);
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
    let history = current_host(&h);
    let directory = host_directory(&history);
    let observation = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let observed = fs::read(directory.join("state.json")).ok().and_then(|raw| {
                serde_json::from_slice::<aq_session_host::HostObservation>(&raw).ok()
            });
            if let Some(o) = observed
                .filter(|o| o.phase == aq_session_host::HostPhase::Stopped && o.kernel_wait_echild)
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
    let history = wait_for_host(&h, |h| h.running.is_some()).await;
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
