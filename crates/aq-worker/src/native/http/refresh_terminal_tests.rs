//! Actual local HTTP transport with independently published typed synthetic facts.
use super::*;
use aq_phase_terminal_protocol::refresh_v1 as wire;
#[tokio::test]
async fn refresh_terminal_and_history_use_exact_new_profile_on_shared_routes() {
    let fixtures: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-phase-terminal-protocol-0.1.1/artifacts/refresh-v1/fixtures.json"
    ))
    .unwrap();
    let started: wire::StartedRequest =
        serde_json::from_value(fixtures["started_request"].clone()).unwrap();
    let stopped: wire::StoppedRequest =
        serde_json::from_value(fixtures["stopped_request"].clone()).unwrap();
    let history: wire::HistoryRequest =
        serde_json::from_value(fixtures["history_request"].clone()).unwrap();
    let node_id = match &started.original.binding.session {
        SessionIdentity::Remote { node_id, .. } => *node_id,
        _ => panic!("worker fixture"),
    };
    let routes = axum::Router::new().route(
        "/v1/worker/profiled-v2/{kind}",
        axum::routing::post(
            move |axum::extract::Path(kind): axum::extract::Path<String>,
                  axum::Json(_): axum::Json<serde_json::Value>| {
                let fixture = fixtures.clone();
                async move {
                    let value = match kind.as_str() {
                        "started" => fixture["started_receipt"].clone(),
                        "stopped" => fixture["stopped_receipt"].clone(),
                        "history" => fixture["history_known"].clone(),
                        _ => panic!("unexpected route"),
                    };
                    ([("aq-phase-terminal-api-version", "1")], axum::Json(value))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    let client = NativeClient::from_config(&crate::config::WorkerConfig {
        server_url: format!("http://{address}"),
        node_id,
        credential: "synthetic-refresh-terminal".into(),
        cf_client_id: None,
        cf_client_secret: None,
    })
    .unwrap();
    client
        .refresh_terminal_started(&started)
        .await
        .unwrap()
        .validate_for(&started)
        .unwrap();
    client
        .refresh_terminal_stopped(&stopped)
        .await
        .unwrap()
        .validate_for(&stopped)
        .unwrap();
    client
        .refresh_terminal_history(&history)
        .await
        .unwrap()
        .validate_for(&history)
        .unwrap();
    let old: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../../../vendor/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json"
    ))
    .unwrap();
    let old_started: aq_phase_terminal_protocol::StartedRequest =
        serde_json::from_value(old["started_request"].clone()).unwrap();
    assert!(client.phase_terminal_started(&old_started).await.is_err());
    server.abort();
}
