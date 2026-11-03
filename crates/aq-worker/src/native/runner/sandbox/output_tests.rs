//! Held output HTTP cannot own renewal/cancellation or unbounded producer space.
use super::{
    auth::RunnerRoute,
    auth_tests::client,
    channels::{SandboxChannels, TerminalDemand},
};
use crate::native::terminal_output::TerminalOutput;
use aq_session_host::terminal_output::ControlEvent;
use aq_session_protocol::{
    ArtifactRef, Digest, TerminalHost, TerminalIdentity, TerminalPolicy, Text,
};
use aq_terminal_relay_protocol::{PeerUpload, UploadAck};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore, watch};
use uuid::Uuid;

fn renewal_fixture() -> serde_json::Value {
    let fixtures: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../../protocol/aq-admission/artifacts/sandbox-v1.fixtures.json"
    ))
    .unwrap();
    fixtures
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "phase_v2_renewal")
        .unwrap()["case"]
        .clone()
}

#[derive(Clone)]
struct Peer {
    output_arrived: Arc<Notify>,
    output_release: Arc<Semaphore>,
    uploads: Arc<Mutex<Vec<Vec<u8>>>>,
    renewals: Arc<Mutex<usize>>,
}
async fn exchange(State(p): State<Peer>, uri: Uri, h: HeaderMap, body: Bytes) -> Response {
    assert_eq!(
        h["authorization"],
        "Bearer fixture_only_not_a_real_secret_00000000"
    );
    if uri.path() == "/v1/worker/profiled/renew" {
        *p.renewals.lock().unwrap() += 1;
        let fixture = renewal_fixture();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            fixture["request"]
        );
        return axum::Json(fixture["response"].clone()).into_response();
    }
    assert_eq!(uri.path(), "/v1/terminal-relay/upload");
    let q: PeerUpload = serde_json::from_slice(&body).unwrap();
    q.validate().unwrap();
    p.uploads.lock().unwrap().push(body.to_vec());
    p.output_arrived.notify_one();
    p.output_release.acquire().await.unwrap().forget();
    if p.uploads.lock().unwrap().len() == 1 {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    axum::Json(UploadAck {
        version: q.version,
        capability: q.capability,
        peer: q.peer,
        exchange_id: q.exchange_id,
        cursor: q.cursor,
        retirements: vec![],
        dropped_output: vec![],
    })
    .into_response()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_flood_does_not_delay_renewal() {
    let _clock = crate::native::http::test_clock::TestClock::new();
    let state = Peer {
        output_arrived: Default::default(),
        output_release: Arc::new(Semaphore::new(0)),
        uploads: Default::default(),
        renewals: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(any(exchange))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (_dir, http, permit) = client(&origin, false);
    let channels = SandboxChannels::new(&http).unwrap();
    channels
        .set_demand(TerminalDemand {
            subscriptions: 1,
            controllers: 0,
        })
        .unwrap();
    let terminal = TerminalIdentity {
        session: http.launch_binding().session,
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let policy = TerminalPolicy {
        host: TerminalHost::Tmux,
        input_enabled: false,
        max_input_bytes: 1,
        max_pending_input_bytes: 1,
        max_frame_bytes: 4,
        replay_bytes: 8,
    };
    let artifact = ArtifactRef {
        id: Text::new("aq-terminal-renderer").unwrap(),
        version: Text::new("0.1.0").unwrap(),
        digest: Digest::of(b"fixture"),
    };
    let output = TerminalOutput::from_blank(terminal, &policy, artifact, 40, 4).unwrap();
    let lane = channels.output_from(output).unwrap();
    lane.accept(&ControlEvent::Bytes(b"before".to_vec()))
        .unwrap();
    let held = {
        let lane = lane.clone();
        tokio::spawn(async move { lane.upload_once().await })
    };
    state.output_arrived.notified().await;
    // Upload is held; the producer must still drain without waiting for it.
    for _ in 0..256 {
        lane.accept(&ControlEvent::Bytes(b"flood".to_vec()))
            .unwrap();
    }
    assert!(lane.retained_bytes().unwrap() <= 8);
    assert!(lane.has_gap().unwrap());
    assert!(!held.is_finished());
    // Same delegated authority, independent pool and no output lock.
    let fixture = renewal_fixture();
    let request: aq_admission_sandbox::sandbox::RunnerRenewalRequest =
        serde_json::from_value(fixture["request"].clone()).unwrap();
    let sent = super::boot_ns().unwrap();
    let response = http
        .post(RunnerRoute::Renew, &permit.binding, &request)
        .await
        .unwrap();
    let received = super::boot_ns().unwrap();
    let delivery = super::RenewalDelivery {
        response: serde_json::from_slice(&response).unwrap(),
        context: serde_json::from_value(fixture["context"].clone()).unwrap(),
        server_now_ms: fixture["now_ms"].as_u64().unwrap(),
    };
    http.accept_renewal(
        &request,
        &delivery,
        serde_json::from_value(fixture["nonce"].clone()).unwrap(),
        sent,
        received,
    )
    .unwrap();
    assert_eq!(*state.renewals.lock().unwrap(), 1);
    assert!(!held.is_finished(), "renewal waited for terminal upload");
    state.output_release.add_permits(1);
    assert!(held.await.unwrap().is_err());
    // Exact retry of the cut, including operation/snapshot IDs, after flood.
    let retry = {
        let lane = lane.clone();
        tokio::spawn(async move { lane.upload_once().await })
    };
    state.output_arrived.notified().await;
    {
        let uploads = state.uploads.lock().unwrap();
        assert_eq!(uploads[0], uploads[1]);
    }
    // Cancellation wins over a held network future and releases upload ownership.
    retry.abort();
    assert!(retry.await.unwrap_err().is_cancelled());
    let (cancel, shutdown) = watch::channel(false);
    let running = {
        let lane = lane.clone();
        tokio::spawn(async move { lane.run(shutdown).await })
    };
    state.output_arrived.notified().await;
    cancel.send(true).unwrap();
    running.await.unwrap().unwrap();
    assert!(!http.authority_lost());
    server.abort();
}
