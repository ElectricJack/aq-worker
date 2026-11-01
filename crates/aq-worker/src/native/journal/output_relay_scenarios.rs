//! Actual pinned host ownership, synthetic coordinator/relay. No Snapshot or
//! claim of initial output completeness; real-byte Watch composition is separate.
use super::*;
use aq_terminal_bridge::http::{NodeCredential, RelayHttp};
use aq_terminal_relay::{
    api::{MutationReply, TerminalInfo},
    draft::ControlStatus,
};
use aq_terminal_relay_protocol::{InputMode, Register};
async fn relay(
    policy: aq_session_protocol::TerminalPolicy,
) -> (
    RelayHttp,
    Arc<Mutex<Vec<Register>>>,
    tokio::task::JoinHandle<()>,
) {
    let saved = Arc::new(Mutex::new(vec![]));
    let calls = saved.clone();
    let app = Router::new().route(
        "/v1/terminal-relay/register",
        post(move |Json(q): Json<Register>| {
            let policy = policy.clone();
            let calls = calls.clone();
            async move {
                q.validate().unwrap();
                calls.lock().unwrap().push(q.clone());
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
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = RelayHttp::development_loopback(
        &origin,
        &NodeCredential::new("synthetic-node-credential".into()).unwrap(),
    )
    .unwrap();
    (client, saved, server)
}
async fn ready_output() -> (Inputs, GitPeer, DurablePhaseExecution) {
    let export = synthetic();
    let manifest = export.manifest.clone();
    let h = Inputs::new_phase_output(export).await;
    let git = GitPeer::new(h.root.clone(), manifest);
    h.grant_window_ms.store(5000, Ordering::SeqCst);
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
    let PhaseExecutionCommit::Usable(execution) = h
        .pipeline()
        .record_phase_execution(inputs, observed)
        .await
        .unwrap()
    else {
        panic!("output host permit expired")
    };
    (h, git, execution)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn actual_output_drain_precedes_ack_but_registration_uses_actual_retained_revision() {
    let (h, _git, execution) = ready_output().await;
    let p = profile(&h, SyntheticMode::Wait);
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, p)
        .await
        .unwrap()
    else {
        panic!("actual Running absent")
    };
    let snapshot = h.pipeline().snapshot().await.unwrap();
    let a = &snapshot.attempts[&h.id];
    let expected = a.native_host.as_ref().unwrap().intent.terminal.clone();
    let (client, calls, server) =
        relay(a.reservation.offer.offered.offer.manifest.terminal.clone()).await;
    let output = h
        .pipeline()
        .start_phase_output(&host, client.clone())
        .unwrap();
    assert_eq!(output.terminal(), &expected);
    assert!(output.status().unwrap().draining);
    assert!(output.register().await.is_err());
    assert!(calls.lock().unwrap().is_empty());
    assert!(h.pipeline().poll_phase_output(&host).is_err());
    assert!(h.pipeline().start_phase_output(&host, client).is_err());
    h.host_report_mode.store(1, Ordering::SeqCst);
    let ack = h
        .pipeline()
        .report_phase_terminal_started(h.id)
        .await
        .unwrap();
    let info = output.register().await.unwrap();
    assert_eq!(
        info.session_revision,
        ack.original.authority_revision_at_record
    );
    assert_eq!(info.terminal, expected);
    let registration = calls.lock().unwrap()[0].clone();
    assert_eq!(registration.operation_id, ack.original.operation_id);
    assert_eq!(
        registration.expected_authority_revision,
        ack.original.authority_revision_at_record
    );
    assert!(!info.terminal_policy.input_enabled);
    output.register().await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while output.status().unwrap().draining {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    assert_eq!(h.pipeline().retained_input_stats().unwrap().attempts, 1);
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicitly pinned native host/helper/synthetic ELF and actual tmux3.4"]
async fn actual_output_attachment_refuses_input_enabled_sealed_policy() {
    let (h, _git, execution) = ready(5000).await;
    let NativeHostLaunchResult::Running(host) = h
        .pipeline()
        .launch_phase_synthetic(&execution, profile(&h, SyntheticMode::Wait))
        .await
        .unwrap()
    else {
        panic!("actual Running absent")
    };
    let policy = h.pipeline().snapshot().await.unwrap().attempts[&h.id]
        .reservation
        .offer
        .offered
        .offer
        .manifest
        .terminal
        .clone();
    assert!(policy.input_enabled);
    let (client, _calls, server) = relay(policy).await;
    assert!(h.pipeline().start_phase_output(&host, client).is_err());
    assert!(matches!(
        h.pipeline().stop_phase_host(h.id).await.unwrap(),
        NativeStopOutcome::StoppedPendingJournal(_)
    ));
    h.pipeline().persist_phase_host_stop(h.id).await.unwrap();
    server.abort();
}
