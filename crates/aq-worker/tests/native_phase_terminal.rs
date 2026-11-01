#![cfg(feature = "test-support")]
//! Real loopback transport; synthetic public vectors, not daemon authority.
use aq_phase_terminal_protocol as wire;
use aq_worker::{config::WorkerConfig, native::http::NativeClient};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
fn fixture() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json"
    ))
    .unwrap()
}
struct Peer {
    mode: AtomicUsize,
    calls: Mutex<Vec<(String, Value)>>,
    entered: Semaphore,
    release: Semaphore,
}
async fn handler(
    State(p): State<Arc<Peer>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    Json(q): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer synthetic-terminal");
    assert_eq!(headers["aq-phase-terminal-api-version"], "1");
    p.calls.lock().unwrap().push((path.clone(), q.clone()));
    let mode = p.mode.load(Ordering::SeqCst);
    if mode == 1 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if mode == 5 && path == "started" {
        p.entered.add_permits(1);
        let _permit = p.release.acquire().await.unwrap();
    }
    let f = fixture();
    let mut r = f[format!("{path}_receipt")].clone();
    if mode == 2 {
        r["request_digest"] = json!(aq_admission::Digest::of(b"changed wrapper"));
    }
    if mode == 3 {
        return (
            [
                ("aq-phase-terminal-api-version", "1"),
                ("content-type", "application/json"),
            ],
            "x".repeat(wire::MAX_BYTES + 1),
        )
            .into_response();
    }
    let version = if mode == 4 { "2" } else { "1" };
    ([("aq-phase-terminal-api-version", version)], Json(r)).into_response()
}
struct Server {
    peer: Arc<Peer>,
    client: NativeClient,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new() -> Self {
        let peer = Arc::new(Peer {
            mode: AtomicUsize::new(0),
            calls: Mutex::new(vec![]),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let q: wire::StartedRequest =
            serde_json::from_value(fixture()["started_request"].clone()).unwrap();
        let aq_admission::SessionIdentity::Remote { node_id, .. } = q.original.binding.session
        else {
            panic!()
        };
        let client = NativeClient::from_config(&WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id,
            credential: "synthetic-terminal".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let app = Router::new()
            .route("/v1/worker/profiled-v2/{path}", post(handler))
            .with_state(peer.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { peer, client, task }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort()
    }
}
#[tokio::test]
async fn phase_terminal_exact_started_stopped_without_delivery_clock() {
    let s = Server::new().await;
    let f = fixture();
    let started = serde_json::from_value(f["started_request"].clone()).unwrap();
    let stopped = serde_json::from_value(f["stopped_request"].clone()).unwrap();
    s.client.phase_terminal_started(&started).await.unwrap();
    s.client.phase_terminal_stopped(&stopped).await.unwrap();
    assert_eq!(s.peer.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn phase_terminal_failure_never_automatically_retries_or_falls_back() {
    let s = Server::new().await;
    let q = serde_json::from_value(fixture()["started_request"].clone()).unwrap();
    s.peer.mode.store(1, Ordering::SeqCst);
    assert!(s.client.phase_terminal_started(&q).await.is_err());
    assert_eq!(s.peer.calls.lock().unwrap().len(), 1);
    s.peer.mode.store(0, Ordering::SeqCst);
    s.client.phase_terminal_started(&q).await.unwrap();
    let calls = s.peer.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
}
#[tokio::test]
async fn phase_terminal_wrong_ack_oversize_and_version_refuse() {
    let s = Server::new().await;
    let q = serde_json::from_value(fixture()["started_request"].clone()).unwrap();
    for mode in [2, 3, 4] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(s.client.phase_terminal_started(&q).await.is_err());
    }
    assert_eq!(s.peer.calls.lock().unwrap().len(), 3);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase_terminal_stopped_lane_progresses_while_started_slots_blocked() {
    let s = Server::new().await;
    s.peer.mode.store(5, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let c = s.client.clone();
        let q = serde_json::from_value(fixture()["started_request"].clone()).unwrap();
        tasks.push(tokio::spawn(
            async move { c.phase_terminal_started(&q).await },
        ));
    }
    let _permit = s.peer.entered.acquire_many(2).await.unwrap();
    let stop = serde_json::from_value(fixture()["stopped_request"].clone()).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        s.client.phase_terminal_stopped(&stop),
    )
    .await
    .unwrap()
    .unwrap();
    s.peer.release.add_permits(2);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}
