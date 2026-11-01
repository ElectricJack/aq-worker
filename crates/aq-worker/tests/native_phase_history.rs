//! Loopback history transport only: no SQL/worker proof or live authority claim.
use aq_phase_terminal_protocol as wire;
use aq_worker::{
    config::WorkerConfig,
    native::http::{HttpStatus, NativeClient},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
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
    calls: Mutex<Vec<Value>>,
    entered: Semaphore,
    release: Semaphore,
}
async fn handler(State(p): State<Arc<Peer>>, headers: HeaderMap, Json(q): Json<Value>) -> Response {
    assert_eq!(headers["authorization"], "Bearer synthetic-history");
    assert_eq!(
        headers
            .get_all("aq-phase-terminal-api-version")
            .iter()
            .count(),
        1
    );
    assert_eq!(headers["aq-phase-terminal-api-version"], "1");
    assert_eq!(headers["content-type"], "application/json");
    p.calls.lock().unwrap().push(q);
    let mode = p.mode.load(Ordering::SeqCst);
    if mode == 10 {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if mode == 11 {
        p.entered.add_permits(1);
        let _permit = p.release.acquire().await.unwrap();
    }
    let f = fixture();
    let mut value = f[if mode == 1 {
        "history_absent"
    } else {
        "history_known"
    }]
    .clone();
    match mode {
        5 => value["request_digest"] = json!(aq_admission::Digest::of(b"wrong lookup")),
        6 => {
            value["identity"]["origin"]["accepted_receipt_digest"] =
                json!(aq_admission::Digest::of(b"other accepted"))
        }
        7 => value["identity"]["binding"]["attempt_id"] = json!(uuid::Uuid::from_u128(999)),
        8 => value["live_permission"] = json!(true),
        12 => {
            value.as_object_mut().unwrap().remove("identity");
        }
        _ => {}
    }
    let mut response = if mode == 9 {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.resize(wire::MAX_BYTES + 1, b' ');
        (
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response()
    } else {
        Json(value).into_response()
    };
    if mode != 2 {
        response.headers_mut().insert(
            "aq-phase-terminal-api-version",
            HeaderValue::from_static(if mode == 4 { "2" } else { "1" }),
        );
    }
    if mode == 3 {
        response.headers_mut().append(
            "aq-phase-terminal-api-version",
            HeaderValue::from_static("1"),
        );
    }
    response
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
        let q: wire::HistoryRequest =
            serde_json::from_value(fixture()["history_request"].clone()).unwrap();
        let aq_admission::SessionIdentity::Remote { node_id, .. } = q.binding.session else {
            panic!()
        };
        let client = NativeClient::from_config(&WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id,
            credential: "synthetic-history".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let app = Router::new()
            .route("/v1/worker/profiled-v2/history", post(handler))
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
fn request() -> wire::HistoryRequest {
    serde_json::from_value(fixture()["history_request"].clone()).unwrap()
}
#[tokio::test]
async fn known_keys_return_history_without_delivery_clock_or_local_permission() {
    let s = Server::new().await;
    let q = request();
    let r = s.client.phase_terminal_history(&q).await.unwrap();
    r.validate_for(&q).unwrap();
    let id = r.identity.as_ref().unwrap();
    assert_eq!(id.execution_operation_id, q.prepare_operation_id);
    assert_eq!(id.origin.accepted_receipt_digest, q.accepted_receipt_digest);
    assert_eq!(id.binding, q.binding);
    let v = serde_json::to_value(&r).unwrap();
    for field in [
        "delivery",
        "execution_delivery",
        "remaining_ms",
        "sent_ns",
        "received_ns",
        "nonce",
    ] {
        assert!(v.get(field).is_none());
        assert!(v["identity"].get(field).is_none());
    }
}
#[tokio::test]
async fn absence_can_advance_to_known_for_same_lookup_and_never_means_nevergranted() {
    let s = Server::new().await;
    let q = request();
    s.peer.mode.store(1, Ordering::SeqCst);
    let missing = s.client.phase_terminal_history(&q).await.unwrap();
    assert!(missing.identity.is_none());
    s.peer.mode.store(0, Ordering::SeqCst);
    let known = s.client.phase_terminal_history(&q).await.unwrap();
    assert!(known.identity.is_some());
    let calls = s.peer.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
}
#[tokio::test]
async fn strict_history_version_scope_required_nullable_extra_and_byte_bounds() {
    let s = Server::new().await;
    let q = request();
    for mode in [2, 3, 4, 5, 6, 7, 8, 9, 12] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client.phase_terminal_history(&q).await.is_err(),
            "mode{mode}"
        );
    }
    assert_eq!(s.peer.calls.lock().unwrap().len(), 9);
}
#[tokio::test]
async fn failed_history_read_is_not_automatically_retried_or_adopted() {
    let s = Server::new().await;
    let q = request();
    s.peer.mode.store(10, Ordering::SeqCst);
    let e = s.client.phase_terminal_history(&q).await.unwrap_err();
    assert_eq!(
        e.downcast_ref::<HttpStatus>().unwrap().0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(s.peer.calls.lock().unwrap().len(), 1);
    s.peer.mode.store(0, Ordering::SeqCst);
    s.client.phase_terminal_history(&q).await.unwrap();
    assert_eq!(s.peer.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn invalid_or_foreign_lookup_refuses_before_network() {
    let s = Server::new().await;
    let mut q = request();
    if let aq_admission::SessionIdentity::Remote { node_id, .. } = &mut q.binding.session {
        *node_id = uuid::Uuid::from_u128(999);
    }
    assert!(s.client.phase_terminal_history(&q).await.is_err());
    let mut q = request();
    q.negotiation.features.clear();
    assert!(s.client.phase_terminal_history(&q).await.is_err());
    assert!(s.peer.calls.lock().unwrap().is_empty());
}
#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_uses_ordinary_slots_not_reserved_stopped_capacity() {
    let s = Server::new().await;
    s.peer.mode.store(11, Ordering::SeqCst);
    let mut tasks = vec![];
    for _ in 0..2 {
        let c = s.client.clone();
        tasks.push(tokio::spawn(async move {
            c.phase_terminal_history(&request()).await
        }));
    }
    let _permits = s.peer.entered.acquire_many(2).await.unwrap();
    assert!(s.client.phase_terminal_history(&request()).await.is_err());
    // Add a separate versioned Stopped route on another listener is unnecessary:
    // the independently qualified client exposes its reserved lane by successfully
    // issuing a Stopped request while both ordinary read permits remain held.
    let stop: wire::StoppedRequest =
        serde_json::from_value(fixture()["stopped_request"].clone()).unwrap();
    let e = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        s.client.phase_terminal_stopped(&stop),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(
        e.downcast_ref::<HttpStatus>().unwrap().0,
        StatusCode::NOT_FOUND
    );
    s.peer.release.add_permits(2);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}
