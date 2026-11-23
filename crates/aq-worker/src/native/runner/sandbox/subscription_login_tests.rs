//! The material refresh wire contract with a scoped runner HTTP client.
use super::subscription_login::WriteBack;
use aq_session_protocol::Digest;
use serde_json::{Value, json};
use std::collections::VecDeque;
use uuid::Uuid;
type Replies = std::sync::Arc<std::sync::Mutex<VecDeque<(u16, &'static str)>>>;
const LEASE: Uuid = Uuid::from_u128(0xC6B);
fn codex_login(refresh: &str, last_refresh: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"auth_mode":"chatgpt","OPENAI_API_KEY":null,
        "tokens":{"id_token":"offline-id","access_token":"offline-access",
            "refresh_token":refresh,"account_id":"offline-account"},
        "last_refresh":last_refresh}))
    .unwrap()
}

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    routing::any,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::{Arc, Mutex};
type Seen = std::sync::Arc<std::sync::Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;
async fn writeback_peer() -> (String, Seen, tokio::task::JoinHandle<()>) {
    let replies: Replies = Arc::new(Mutex::new(
        [
            (200, r#"{"accepted":true,"version":8,"replayed":false}"#),
            (409, r#"{"accepted":false,"version":9,"replayed":false}"#),
            (
                409,
                r#"{"error":"material refresh is not currently authorized"}"#,
            ),
            (503, "{}"),
        ]
        .into(),
    ));
    let seen = Seen::default();
    async fn record(
        State((seen, replies)): State<(Seen, Replies)>,
        uri: Uri,
        headers: HeaderMap,
        bytes: Bytes,
    ) -> (StatusCode, &'static str) {
        seen.lock()
            .unwrap()
            .push((uri.to_string(), headers, bytes.to_vec()));
        let (status, body) = replies.lock().unwrap().pop_front().unwrap();
        (StatusCode::from_u16(status).unwrap(), body)
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .fallback(any(record))
        .with_state((seen.clone(), replies));
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (origin, seen, task)
}
fn recorded_refresh(first: &(String, HeaderMap, Vec<u8>), body: &[u8]) -> Value {
    let (path, headers, sent) = first;
    assert_eq!(
        (path.as_str(), headers["content-type"].to_str().unwrap()),
        (
            "/v1/worker/profiled/material/refresh",
            "application/octet-stream"
        )
    );
    assert_eq!(
        headers["authorization"],
        "Bearer fixture_only_not_a_real_secret_00000000"
    );
    assert_eq!(sent, body);
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(headers["aq-material-refresh"].as_bytes())
            .unwrap(),
    )
    .unwrap()
}
#[tokio::test]
async fn runner_write_back_speaks_c5b_contract() {
    let (origin, seen, task) = writeback_peer().await;
    let (_dir, http, permit) = super::auth_tests::client(&origin, false);
    let body = codex_login("refresh-v8", "2026-11-04T12:00:00Z");
    for expected in [
        WriteBack::Accepted {
            version: 8,
            replayed: false,
        },
        WriteBack::Superseded { version: 9 },
        WriteBack::NotRenewable,
    ] {
        assert_eq!(http.refresh_login(LEASE, 7, &body).await.unwrap(), expected);
    }
    assert!(
        http.refresh_login(LEASE, 7, &body).await.is_err(),
        "5xx is transient"
    );
    assert!(!http.authority_lost(), "CAS answers are not authority loss");
    task.abort();
    let refresh = recorded_refresh(&seen.lock().unwrap()[0], &body);
    let expected = json!({
        "schema_version":1,
        "lease_id":LEASE,
        "base_version":7,
        "content_digest":Digest::of(&body),
        "binding":http.launch_binding(),
    });
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&refresh[key], value, "refresh field {key}");
    }
    assert_eq!(
        (
            &refresh["identity"]["node_id"],
            &refresh["identity"]["boot_id"]
        ),
        (
            &json!(permit.binding.node.node_id),
            &json!(permit.binding.node.boot_id)
        )
    );
    let keys: Vec<_> = refresh.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys.len(), 7, "exactly MaterialRefresh's fields: {keys:?}");
}
