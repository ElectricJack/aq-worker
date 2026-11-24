// Two Started submissions, the stopped 503 attempts, a malformed ACK and the final ACK.
const EXPECTED_REPORTS: usize = 4 + aq_node_service::native_transport::SAME_TRIES as usize;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native synthetic host and actual tmux3.4; phase terminal peer is synthetic"]
async fn native_phase_terminal_actual_pair_lost_ack_immutable_retry_and_restart_readback() {
    let (mut h, _git, execution) = ready(HOST_OBSERVATION_WINDOW_MS).await;
    let host = running_host(&h, &execution).await;
    let first = terminal_history(&h).await;
    assert!(first.started.receipt.is_none() && first.stopped.is_none());
    assert_eq!(
        first.started.request.original.operation_id,
        host.started_request().unwrap().operation_id
    );
    assert_started_origin(&h, &first).await;
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
    let stop_ack = report_stopped_retry(&h).await;
    drop(host);
    drop(execution);
    drop(h.pipeline.take());
    let recovered = reopen_pipeline(&h);
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
    assert_eq!(
        h.phase_terminal_calls.lock().unwrap().len(),
        EXPECTED_REPORTS
    );
    assert_eq!(recovered.snapshot().await.unwrap().attempts.len(), 1);
    assert_eq!(recovered.retained_input_stats().unwrap().attempts, 0);
}
async fn assert_started_origin(
    h: &Inputs,
    first: &crate::native::journal::phase_terminal::PhaseTerminalHistory,
) {
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
}
async fn report_stopped_retry(h: &Inputs) -> aq_phase_terminal_protocol::StoppedReceipt {
    stop_and_persist(h).await;
    let pair = terminal_history(h).await;
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
    assert_eq!(calls.len(), EXPECTED_REPORTS);
    assert_eq!(calls[2].0, "stopped");
    for replay in &calls[3..] {
        assert_eq!(replay, &calls[2]);
    }
    stop_ack
}
fn reopen_pipeline(h: &Inputs) -> InputPipeline {
    let cap = host_limits();
    let journal = Journal::open(&h.root, identity().node_id, cap).unwrap();
    assert!(journal.recovery_required());
    assert!(journal.available().is_err());
    let store = crate::native::materials::MaterialStore::open(
        &h._dir.path().join("materials"),
        identity(),
        32,
    )
    .unwrap();
    InputPipeline::new(journal, store, h.client.clone())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned native synthetic host and actual tmux3.4; historical report after restart only"]
async fn native_phase_terminal_unacked_restart_retries_without_live_authority_or_discard() {
    let (mut h, _git, execution) = ready(HOST_OBSERVATION_WINDOW_MS).await;
    let host = running_host(&h, &execution).await;
    let original = terminal_history(&h).await;
    h.host_report_mode.store(2, Ordering::SeqCst);
    assert!(
        h.pipeline()
            .report_phase_terminal_started(h.id)
            .await
            .is_err()
    );
    stop_and_persist(&h).await;
    drop(host);
    drop(execution);
    drop(h.pipeline.take());
    let recovered = reopen_pipeline(&h);
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
