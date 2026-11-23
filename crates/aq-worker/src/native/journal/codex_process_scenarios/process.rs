#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires exact pinned Codex0.166, helper0.1.5 and actual tmux; loopback only"]
async fn actual_codex_finite_response_has_exact_auth_startup_readiness_output_and_stopped_acks() {
    let peer = provider().await;
    let (h, _git, tx, task, output) = begin_execution(&peer, false, false).await;
    if tokio::time::timeout(std::time::Duration::from_secs(12), peer.entered.notified())
        .await
        .is_err()
    {
        let bytes: Vec<u8> = output
            .uploads
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| {
                serde_json::from_slice::<PeerUpload>(b)
                    .unwrap()
                    .frames
                    .into_iter()
                    .flat_map(|f| f.bytes)
                    .collect::<Vec<_>>()
            })
            .collect();
        let _ = tx.send(true);
        let driver_result = tokio::time::timeout(std::time::Duration::from_secs(3), task).await;
        eprintln!("actual CLI driver cleanup: {driver_result:?}");
        panic!(
            "actual CLI made no loopback request; observed physical output: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    assert_private_auth(&h).await;
    check_provider_input(&peer.requests.lock().unwrap()[0]);
    tokio::time::timeout(
        std::time::Duration::from_secs(12),
        output.entered.notified(),
    )
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "unwatched output409 must not stop actual Codex"
    );
    check_ready_output(&output);
    peer.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
    assert_eq!(peer.requests.lock().unwrap().len(), 1);
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(history.baseline.is_some() && history.running.is_some());
    assert_eq!(
        history.stopped.as_ref().unwrap().observation.root_status,
        Some(0),
        "actual Codex completed the finite successful response"
    );
    let directory = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string());
    assert!(
        !directory.join("codex-home/auth.json").exists(),
        "auth removed only after physical stopped proof"
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires exact pinned Codex0.166, helper0.1.5 and actual tmux; loopback only"]
async fn actual_codex_shutdown_during_blocked_output_stops_without_provider_response() {
    let peer = provider().await;
    let (h, _git, tx, task, output) = begin_execution(&peer, true, false).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(12),
        output.entered.notified(),
    )
    .await
    .unwrap();
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
    assert_eq!(output.uploads.lock().unwrap().len(), 1);
    assert!(peer.requests.lock().unwrap().len() <= 1);
    assert_eq!(
        peer.release.available_permits(),
        0,
        "provider response was never released"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires exact pinned Codex0.166, helper0.1.5 and actual tmux; loopback only"]
async fn actual_codex_shutdown_during_delayed_started_keeps_unknown_receipt_after_physical_stop() {
    let peer = provider().await;
    let (h, _git, tx, task, output) = begin_execution(&peer, false, true).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while h.phase_terminal_calls.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tx.send(true).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    stopped(&h, false).await;
    assert!(output.uploads.lock().unwrap().is_empty());
    assert!(peer.requests.lock().unwrap().len() <= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "qualification probe: unified exec currently ENOENT; requires independently confined runtime and admitted helper route"]
async fn actual_codex_exec_command_round_trip_uses_confined_workspace_and_actual_tool_output() {
    let peer = provider_mode(true).await;
    let (h, _git, _tx, task, _output) = begin_execution(&peer, false, false).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), peer.entered.notified())
        .await
        .unwrap();
    assert_private_auth(&h).await;
    {
        let requests = peer.requests.lock().unwrap();
        assert!(
            requests[0]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["type"] == "function" && v["name"] == "exec_command")
        );
    }
    peer.release.add_permits(1);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        wait_for_requests(&peer, 2),
    )
    .await
    .unwrap();
    tool_result(&peer.requests.lock().unwrap()[1]).unwrap();
    let workspace = fixture_workspace(&h).await;
    assert_eq!(
        fs::read(workspace.path.join("aq-local-tool-proof.txt")).unwrap(),
        b"aq-local-tool-proof"
    );
    peer.release.add_permits(1);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    expect_clean_stop(&h).await;
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit named-readonly qualification; requires private fixture, exact pins and root-approved paired source"]
async fn actual_named_readonly_codex_exec_command_round_trip_has_owned_cap_and_physical_cleanup() {
    let peer = provider_mode(true).await;
    let (h, _git, tx, mut task, output) =
        begin_fixture_execution(&peer, false, false, FixtureImage::NamedReadonly).await;
    let deadline = output.execution_deadline_ns.unwrap();
    // Every qualification stage shares the original Grant deadline. No new
    // wall-clock budget starts when an earlier stage happens to finish.
    let mut joined = false;
    let progress = async {
        let capability = observe_named_run(&h, &peer, &output, (deadline, &task)).await?;
        let completion =
            original_deadline_stage(deadline, "natural physical completion", &mut task).await?;
        joined = true;
        completion??;
        Ok::<String, anyhow::Error>(capability)
    }
    .await;
    if let Err(error) = &progress {
        // Always ask the real owner to stop and persist proof on a stage timeout.
        // Cleanup is allowed to finish after the Grant has stopped admission.
        let _ = tx.send(true);
        let cleanup_joined = joined
            || tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
                .await
                .is_ok();
        describe_failure(&h, &peer, &output, (error, cleanup_joined)).await;
    }
    let capability = progress.unwrap();
    let state = expect_clean_stop(&h).await;
    assert_eq!(peer.requests.lock().unwrap().len(), 2);
    let history = state.attempts[&h.id].native_host.as_ref().unwrap();
    let auth = history
        .intent
        .host_root
        .join(history.intent.launch_id.to_string())
        .join("codex-home/auth.json");
    assert!(
        !auth.exists(),
        "matching private capability cleanup follows physical stop"
    );
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    for request in peer.requests.lock().unwrap().iter() {
        assert!(
            !serde_json::to_string(request)
                .unwrap()
                .contains(&capability)
        );
    }
}
async fn describe_failure(
    h: &Inputs,
    peer: &Provider,
    output: &OutputPeer,
    (error, cleanup_joined): (&anyhow::Error, bool),
) {
    let state = h.pipeline().snapshot().await.unwrap();
    let history = state.attempts[&h.id].native_host.as_ref();
    eprintln!(
        "fixture failure: {error}; cleanup_joined={} provider_requests={} registrations={} uploads={} running_observed_boot_ms={:?} echild={} started_ack={} stopped_ack={}",
        cleanup_joined,
        peer.requests.lock().unwrap().len(),
        output.registrations.lock().unwrap().len(),
        output.uploads.lock().unwrap().len(),
        history
            .and_then(|v| v.running.as_ref())
            .map(|v| v.observation.observed_boot_ms),
        history
            .and_then(|v| v.stopped.as_ref())
            .is_some_and(|v| v.observation.kernel_wait_echild),
        history
            .and_then(|v| v.terminal.as_ref())
            .is_some_and(|v| v.started.receipt.is_some()),
        history
            .and_then(|v| v.terminal.as_ref())
            .and_then(|v| v.stopped.as_ref())
            .is_some_and(|v| v.receipt.is_some())
    );
}
async fn observe_named_run(
    h: &Inputs,
    peer: &Provider,
    output: &OutputPeer,
    (deadline, task): (u64, &tokio::task::JoinHandle<anyhow::Result<()>>),
) -> anyhow::Result<String> {
    original_deadline_stage(
        deadline,
        "first provider request",
        wait_for_requests(peer, 1),
    )
    .await?;
    let capability = assert_private_auth(h).await;
    {
        let requests = peer.requests.lock().unwrap();
        anyhow::ensure!(
            !serde_json::to_string(&requests[0])
                .unwrap()
                .contains(&capability),
            "child capability entered model input"
        );
    }
    {
        let requests = peer.requests.lock().unwrap();
        anyhow::ensure!(
            requests[0]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["type"] == "function" && v["name"] == "exec_command")
        );
    }
    peer.release.add_permits(1);
    original_deadline_stage(
        deadline,
        "real tool result request",
        wait_for_requests(peer, 2),
    )
    .await?;
    tool_result(&peer.requests.lock().unwrap()[1])?;
    let workspace = fixture_workspace(h).await;
    anyhow::ensure!(
        fs::read(workspace.path.join("aq-local-tool-proof.txt"))? == b"aq-local-tool-proof",
        "real workspace tool file differs"
    );
    // Output delivery is independent of inference. Hold the second response
    // while checking it; do not demand an upload before releasing the tool call.
    original_deadline_stage(deadline, "terminal readiness upload", async {
        while output.uploads.lock().unwrap().is_empty() {
            output.entered.notified().await;
        }
    })
    .await?;
    anyhow::ensure!(
        !task.is_finished(),
        "no current Watch must not stop a named agent"
    );
    check_named_output(output, &capability)?;
    peer.release.add_permits(1);
    Ok(capability)
}
