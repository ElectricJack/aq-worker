use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[path = "../../tests/support/controller_config.rs"]
mod fixture;
pub(super) use fixture::{config, now_ms, request};
use fixture::{reason, unavailable};

#[tokio::test]
async fn unavailable_provider_refuses_before_backend_effects() {
    let dir = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let mut c = Controller::open(
        &dir.path().join("node"),
        Uuid::from_u128(1),
        config(),
        backend,
        super::UnavailableProvider,
    )
    .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    assert!(c.run(rx).await.is_err());
    assert!(c.state().attempts.is_empty());
    assert_eq!(c.inventory().refusal, Some(RefusalReason::GrantDenied));
    assert!(
        c.backend
            .list_owned(c.identity().node_id)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn heartbeat_peer(
    stop: tokio::sync::watch::Sender<bool>,
) -> (
    crate::client::Client,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{Json, Router, routing::post};
    let heartbeats = Arc::new(AtomicUsize::new(0));
    let counter = heartbeats.clone();
    let app = Router::new()
        .route(
            "/v1/worker/probe",
            post(|Json(req): Json<aq_protocol::ProbeRequest>| async move {
                Json(serde_json::json!({"nonce":req.nonce,"server_time":"2026-11-02T00:00:00Z"}))
            }),
        )
        .route(
            "/v1/worker/heartbeat",
            post(move || {
                let counter = counter.clone();
                let stop = stop.clone();
                async move {
                    // Require two actual HTTP heartbeats, exercising the interval wait.
                    if counter.fetch_add(1, Ordering::SeqCst) == 1 {
                        stop.send(true).unwrap();
                    }
                    Json(aq_protocol::HeartbeatResponse {
                        server_version: "test".into(),
                        heartbeat_interval_secs: 1,
                    })
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = crate::client::Client::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        Some("test-credential".into()),
        None,
        None,
    )
    .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (client, heartbeats, server)
}
#[tokio::test]
async fn unavailable_provider_keeps_real_foundation_heartbeat_and_root_until_shutdown() {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let (client, heartbeats, server) = heartbeat_peer(tx).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("node");
    let mut c = unavailable(&root).unwrap();
    let mut shutdown = rx.clone();
    let heartbeat = crate::client::run_loop(&client, async move {
        while !*shutdown.borrow() {
            if shutdown.changed().await.is_err() {
                break;
            }
        }
    });
    {
        let service = c.run_with_heartbeat(rx, heartbeat);
        tokio::pin!(service);
        tokio::select! {
            result = &mut service => panic!("run exited before heartbeat interval: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
        // The registration refusal does not release the lock while heartbeat runs.
        assert!(unavailable(&root).is_err());
        tokio::time::timeout(std::time::Duration::from_secs(5), service)
            .await
            .unwrap()
            .unwrap();
    }
    server.abort();
    assert_eq!(heartbeats.load(Ordering::SeqCst), 2);
    assert!(!c.registered);
    assert_eq!(c.inventory().refusal, Some(RefusalReason::Draining));
    assert!(c.state().attempts.is_empty());
    assert!(
        c.backend
            .list_owned(c.identity().node_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reason(c.reserve(request(c.identity(), 20)).unwrap_err()),
        RefusalReason::Draining
    );
}
