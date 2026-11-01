//! Actual physical host, synthetic typed control and output peers. No provider.
use super::*;
use aq_terminal_relay_protocol::PeerUpload;
use axum::response::IntoResponse;
use tokio::sync::{Notify, Semaphore, watch};
struct OutputPeer {
    uploads: Arc<Mutex<Vec<Vec<u8>>>>,
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for OutputPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn output_peer(
    policy: aq_session_protocol::TerminalPolicy,
    slow: bool,
) -> (RelayHttp, OutputPeer) {
    let uploads = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let saved = uploads.clone();
    let signal = entered.clone();
    let gate = release.clone();
    let app = Router::new()
        .route(
            "/v1/terminal-relay/register",
            post(move |Json(q): Json<Register>| {
                let policy = policy.clone();
                async move {
                    q.validate().unwrap();
                    Json(MutationReply::Registered {
                        operation_id: q.operation_id,
                        info: TerminalInfo {
                            terminal: q.terminal.clone(),
                            session_revision: q.expected_authority_revision,
                            input_mode: InputMode::FiniteV1,
                            terminal_policy: policy,
                            control: ControlStatus {
                                terminal: q.terminal,
                                session_revision: q.expected_authority_revision,
                                generation: 0,
                                lease: None,
                                fence_acknowledged: false,
                                input_paused: false,
                            },
                            last_peer_observed_at_ms: 1,
                            last_output_observed_at_ms: 0,
                            disconnected: false,
                        },
                    })
                }
            }),
        )
        .route(
            "/v1/terminal-relay/upload",
            post(move |body: axum::body::Bytes| {
                let saved = saved.clone();
                let signal = signal.clone();
                let gate = gate.clone();
                async move {
                    let q: PeerUpload = serde_json::from_slice(&body).unwrap();
                    q.validate().unwrap();
                    saved.lock().unwrap().push(body.to_vec());
                    signal.notify_one();
                    if slow {
                        let _ = gate.acquire().await.unwrap();
                    }
                    (axum::http::StatusCode::CONFLICT, "no current Watch").into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = RelayHttp::development_loopback(
        &origin,
        &NodeCredential::new("synthetic-node-credential".into()).unwrap(),
    )
    .unwrap();
    (
        http,
        OutputPeer {
            uploads,
            entered,
            release,
            task,
        },
    )
}
async fn launched() -> (
    Inputs,
    GitPeer,
    DurablePhaseExecution,
    crate::native::launch::LocalPhaseHost,
    aq_session_protocol::TerminalPolicy,
) {
    let (h, git, grant) = ready_output().await;
    h.host_report_mode.store(1, Ordering::SeqCst);
    let profile = profile(&h, SyntheticMode::Wait);
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&grant, profile)
        .await
        .unwrap()
    else {
        panic!("actual Running required")
    };
    let state = h.pipeline().snapshot().await.unwrap();
    let policy = state.attempts[&h.id]
        .reservation
        .offer
        .offered
        .offer
        .manifest
        .terminal
        .clone();
    (h, git, grant, host, policy)
}
async fn stopped(h: &Inputs, paired: bool) {
    let state = h.pipeline().snapshot().await.unwrap();
    let host = state.attempts[&h.id].native_host.as_ref().unwrap();
    assert!(
        host.stopped
            .as_ref()
            .unwrap()
            .observation
            .kernel_wait_echild
    );
    let terminal = host.terminal.as_ref().unwrap();
    assert_eq!(terminal.started.receipt.is_some(), paired);
    assert!(terminal.stopped.as_ref().unwrap().receipt.is_some());
    let calls = h.phase_terminal_calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|(p, _)| p == "started").count(), 1);
    assert_eq!(calls.iter().filter(|(p, _)| p == "stopped").count(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned physical helper/periodic synthetic ELF and actual tmux"]
async fn unwatched_output_409_keeps_actual_host_running_and_preserves_pending_bytes() {
    let (h, _git, grant, host, policy) = launched().await;
    let (http, peer) = output_peer(policy, false).await;
    let (tx, rx) = watch::channel(false);
    let pipeline = h.pipeline().clone();
    let deadline = grant.deadline_ns();
    let task = tokio::spawn(crate::native::runner::drive_running(
        pipeline, host, deadline, http, rx,
    ));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while peer.uploads.lock().unwrap().len() < 2 {
            peer.entered.notified().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "missing human Watch must not stop a running agent"
    );
    let state = h.pipeline().snapshot().await.unwrap();
    assert!(
        state.attempts[&h.id]
            .native_host
            .as_ref()
            .unwrap()
            .stopped
            .is_none()
    );
    {
        let uploads = peer.uploads.lock().unwrap();
        assert_eq!(
            uploads[0], uploads[1],
            "uncertain output exchange must retain exact bytes"
        );
    }
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned physical helper/periodic synthetic ELF and actual tmux"]
async fn shutdown_during_slow_output_http_stops_and_acknowledges_without_waiting_for_upload() {
    let (h, _git, grant, host, policy) = launched().await;
    let (http, peer) = output_peer(policy, true).await;
    let (tx, rx) = watch::channel(false);
    let pipeline = h.pipeline().clone();
    let deadline = grant.deadline_ns();
    let task = tokio::spawn(crate::native::runner::drive_running(
        pipeline, host, deadline, http, rx,
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), peer.entered.notified())
        .await
        .unwrap();
    tx.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stopped(&h, true).await;
    assert_eq!(peer.uploads.lock().unwrap().len(), 1);
    peer.release.add_permits(1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pinned physical helper/periodic synthetic ELF and actual tmux"]
async fn shutdown_during_delayed_started_ack_stops_physically_and_preserves_uncertainty() {
    let (h, _git, grant, host, policy) = launched().await;
    h.host_report_mode.store(4, Ordering::SeqCst);
    let (http, _peer) = output_peer(policy, false).await;
    let (tx, rx) = watch::channel(false);
    let pipeline = h.pipeline().clone();
    let deadline = grant.deadline_ns();
    let task = tokio::spawn(crate::native::runner::drive_running(
        pipeline, host, deadline, http, rx,
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while h.phase_terminal_calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tx.send(true).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    stopped(&h, false).await;
}
