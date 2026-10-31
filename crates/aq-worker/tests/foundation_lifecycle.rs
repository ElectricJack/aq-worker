//! Standalone connector qualification against a local v1 contract fake. Product
//! tests against the real foundation server live in agent-q.
use aq_protocol::*;
use aq_worker::{
    client::{CallError, Client},
    config::WorkerConfig,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;
type Revoked = Arc<AtomicBool>;
async fn enroll(Json(r): Json<EnrollRequest>) -> Json<EnrollResponse> {
    assert_eq!(r.protocol_version, 1);
    assert_eq!(r.code, "fixture-enrollment");
    Json(EnrollResponse {
        node_id: Uuid::from_u128(1),
        credential: "fixture-credential".into(),
        heartbeat_interval_secs: 15,
    })
}
async fn heartbeat(
    State(revoked): State<Revoked>,
    h: HeaderMap,
    Json(r): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, (StatusCode, Json<ApiError>)> {
    assert_eq!(h["authorization"], "Bearer fixture-credential");
    assert_eq!(r.protocol_version, 1);
    if revoked.load(Ordering::SeqCst) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: CREDENTIAL_REVOKED.into(),
                message: "revoked".into(),
            }),
        ));
    }
    Ok(Json(HeartbeatResponse {
        server_version: "fixture-server".into(),
        heartbeat_interval_secs: 15,
    }))
}
async fn serve(revoked: Revoked) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/v1/worker/enroll", post(enroll))
        .route("/v1/worker/heartbeat", post(heartbeat))
        .with_state(revoked);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base, task)
}
#[tokio::test]
async fn enroll_saved_identity_reconnect_and_revocation_need_no_sibling() {
    let revoked = Arc::new(AtomicBool::new(false));
    let (base, server) = serve(revoked.clone()).await;
    let enrolled = Client::new(&base, None, None, None)
        .unwrap()
        .enroll(&EnrollRequest {
            code: "fixture-enrollment".into(),
            name: "local-test".into(),
            worker_version: "0.1.0".into(),
            protocol_version: PROTOCOL_VERSION,
        })
        .await
        .unwrap();
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("worker.toml");
    WorkerConfig {
        server_url: base,
        node_id: enrolled.node_id,
        credential: enrolled.credential,
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&path)
    .unwrap();
    let mut cfg = WorkerConfig::load(&path).unwrap();
    let hb = HeartbeatRequest {
        worker_version: "0.1.0".into(),
        protocol_version: 1,
    };
    Client::from_config(&cfg)
        .unwrap()
        .heartbeat(&hb)
        .await
        .unwrap();
    server.abort();
    let _ = server.await;
    let (new_base, server) = serve(revoked.clone()).await;
    cfg.server_url = new_base;
    cfg.save(&path).unwrap();
    let saved = WorkerConfig::load(&path).unwrap();
    assert_eq!(saved.node_id, enrolled.node_id);
    let client = Client::from_config(&saved).unwrap();
    client.heartbeat(&hb).await.unwrap();
    revoked.store(true, Ordering::SeqCst);
    assert!(matches!(
        client.heartbeat(&hb).await,
        Err(CallError::Revoked)
    ));
    server.abort();
}
