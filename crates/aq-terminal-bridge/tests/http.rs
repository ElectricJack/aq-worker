#![cfg(feature = "test-support")]
use aq_session_protocol::*;
use aq_terminal_bridge::http::*;
use aq_terminal_relay_protocol::*;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;
use uuid::Uuid;

fn peer() -> Peer {
    Peer::Remote {
        node_id: Uuid::from_u128(1),
        boot_id: Uuid::from_u128(2),
    }
}
fn poll() -> RelayPoll {
    RelayPoll {
        version: VERSION,
        capability: Text::new(CAPABILITY).unwrap(),
        peer: peer(),
        cursor: 0,
        hold_ms: 20000,
        active_subscriptions: 0,
        active_controllers: 1,
        advertised_capabilities: vec![Capability {
            id: Text::new(CAPABILITY).unwrap(),
            version: VERSION,
        }],
    }
}
fn upload() -> PeerUpload {
    PeerUpload {
        version: VERSION,
        capability: Text::new(CAPABILITY).unwrap(),
        peer: peer(),
        exchange_id: Uuid::from_u128(3),
        cursor: 0,
        receipts: vec![],
        epoch_receipts: vec![],
        resize_receipts: vec![],
        fence_acks: vec![],
        frames: vec![],
        snapshots: vec![],
        gaps: vec![],
    }
}
fn ack(r: &PeerUpload) -> UploadAck {
    UploadAck {
        version: VERSION,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: r.exchange_id,
        cursor: r.cursor,
        retirements: vec![],
        dropped_output: vec![],
    }
}
fn authorize(h: &HeaderMap) {
    assert_eq!(h["authorization"], "Bearer synthetic-node-credential");
}
async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (origin, task)
}
fn client(origin: &str) -> RelayHttp {
    RelayHttp::development_loopback(
        origin,
        &NodeCredential::new("synthetic-node-credential".into()).unwrap(),
    )
    .unwrap()
}

#[test]
fn production_requires_https_and_credentials_remain_redacted() {
    let secret = NodeCredential::new("synthetic-node-credential".into()).unwrap();
    assert!(!format!("{secret:?}").contains("synthetic-node"));
    assert!(RelayHttp::new("http://127.0.0.1:1234", &secret).is_err());
    assert!(RelayHttp::new("https://user:password@example.invalid", &secret).is_err());
    assert!(RelayHttp::new("https://example.invalid/path", &secret).is_err());
    assert!(RelayHttp::development_loopback("http://example.invalid", &secret).is_err());
    assert!(NodeCredential::new("credential\nprivate".into()).is_err());
}

#[derive(Default)]
struct Lanes {
    poll_started: Notify,
    release_poll: Notify,
    polls: AtomicUsize,
    uploads: AtomicUsize,
}
async fn held_poll(
    State(s): State<Arc<Lanes>>,
    h: HeaderMap,
    Json(r): Json<RelayPoll>,
) -> Json<Envelope> {
    authorize(&h);
    s.polls.fetch_add(1, Ordering::SeqCst);
    s.poll_started.notify_one();
    s.release_poll.notified().await;
    Json(Envelope {
        version: VERSION,
        capability: r.capability,
        peer: r.peer,
        exchange_id: Uuid::new_v4(),
        cursor: r.cursor,
        effects: vec![],
    })
}
async fn control(
    State(s): State<Arc<Lanes>>,
    h: HeaderMap,
    Json(r): Json<PeerUpload>,
) -> Json<UploadAck> {
    authorize(&h);
    assert!(r.frames.is_empty() && r.snapshots.is_empty() && r.gaps.is_empty());
    s.uploads.fetch_add(1, Ordering::SeqCst);
    Json(ack(&r))
}
#[tokio::test]
async fn held_poll_and_saturated_poll_lane_preserve_reserved_control_progress() {
    let state = Arc::new(Lanes::default());
    let (origin, server) = serve(
        Router::new()
            .route("/v1/terminal-relay/poll", post(held_poll))
            .route("/v1/terminal-relay/control-upload", post(control))
            .with_state(state.clone()),
    )
    .await;
    let http = client(&origin);
    let request = poll();
    let task_http = http.clone();
    let task_request = request.clone();
    let held = tokio::spawn(async move { task_http.poll(&task_request).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.poll_started.notified(),
    )
    .await
    .unwrap();
    assert!(http.poll(&request).await.is_err());
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        http.control_upload(&upload()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(state.polls.load(Ordering::SeqCst), 1);
    assert_eq!(state.uploads.load(Ordering::SeqCst), 1);
    state.release_poll.notify_one();
    held.await.unwrap().unwrap();
    server.abort();
}

#[tokio::test]
async fn lost_control_reply_is_not_automatically_retried_and_content_never_uses_control_lane() {
    let count = Arc::new(AtomicUsize::new(0));
    let handler_count = count.clone();
    let (origin, server) = serve(Router::new().route(
        "/v1/terminal-relay/control-upload",
        post(move |h: HeaderMap, Json(r): Json<PeerUpload>| {
            let count = handler_count.clone();
            async move {
                authorize(&h);
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"private": "must-not-be-echoed"})),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        Json(serde_json::to_value(ack(&r)).unwrap()),
                    )
                }
            }
        }),
    ))
    .await;
    let http = client(&origin);
    let request = upload();
    let error = http.control_upload(&request).await.unwrap_err();
    assert!(!error.to_string().contains("must-not-be-echoed"));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    http.control_upload(&request).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let mut content = request;
    let terminal = TerminalIdentity {
        session: SessionIdentity::Remote {
            node_id: Uuid::from_u128(1),
            boot_id: Uuid::from_u128(2),
            attempt_id: Uuid::from_u128(4),
            assignment_generation: 1,
            session_id: Uuid::from_u128(5),
        },
        terminal_id: Uuid::from_u128(6),
        epoch: 1,
    };
    content.frames.push(OutputFrame {
        terminal,
        sequence: 1,
        bytes: b"private-output".to_vec(),
        byte_length: 14,
        digest: Digest::of(b"private-output"),
    });
    assert!(http.control_upload(&content).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn redirects_and_wrong_scope_acknowledgements_are_refused() {
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let (destination, destination_server) = serve(Router::new().route(
        "/stolen",
        post(move || {
            c.fetch_add(1, Ordering::SeqCst);
            async { "unexpected" }
        }),
    ))
    .await;
    let location = format!("{destination}/stolen");
    let (origin, server) = serve(Router::new().route(
        "/v1/terminal-relay/control-upload",
        post(move || {
            let location = location.clone();
            async move { axum::response::Redirect::temporary(&location) }
        }),
    ))
    .await;
    assert!(client(&origin).control_upload(&upload()).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    server.abort();
    destination_server.abort();
    let (origin, server) = serve(Router::new().route(
        "/v1/terminal-relay/control-upload",
        post(|Json(r): Json<PeerUpload>| async move {
            let mut reply = ack(&r);
            reply.exchange_id = Uuid::new_v4();
            Json(reply)
        }),
    ))
    .await;
    assert!(client(&origin).control_upload(&upload()).await.is_err());
    server.abort();
}
