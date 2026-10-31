//! Node-only public recovery conformance against a finite loopback peer.
use aq_agent_api::{Binding, valid};
use aq_agent_client::node::NodeClient;
use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
fn fixtures() -> Value {
    serde_json::from_str(include_str!("fixtures/agent-base.json")).unwrap()
}
fn binding() -> Binding {
    serde_json::from_value(fixtures()["context"]["binding"].clone()).unwrap()
}
fn lookup() -> Value {
    fixtures()["node_recovery_request"].clone()
}
fn response(request: &Value, operations: Vec<Value>, cursor: Option<&str>) -> Value {
    json!({"api_version":1,"request_id":request["request_id"],"attempt":request["attempt"],"operations":operations,"next_cursor":cursor,"coverage":{"status":if cursor.is_some(){"partial"}else{"complete"},"complete_for":"authorized_projection","reasons":if cursor.is_some(){vec!["scan_budget"]}else{vec![]}}})
}
struct Peer {
    responses: Vec<(u16, Value)>,
    requests: Vec<(String, Value)>,
}
async fn endpoint(
    State(peer): State<Arc<Mutex<Peer>>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    assert_eq!(
        headers["authorization"],
        format!("Bearer {}", "n".repeat(40))
    );
    assert_eq!(headers["X-AQ-Agent-API-Version"], "1");
    let mut p = peer.lock().unwrap();
    p.requests.push((uri.path().into(), request));
    assert!(matches!(
        uri.path(),
        "/v2/worker/agent-operations/query" | "/v2/worker/agent-operations/retry"
    ));
    let (status, value) = p.responses.remove(0);
    (
        StatusCode::from_u16(status).unwrap(),
        [("X-AQ-Agent-API-Version", "1")],
        Json(value),
    )
        .into_response()
}
async fn peer(
    responses: Vec<(u16, Value)>,
) -> (NodeClient, Arc<Mutex<Peer>>, tokio::task::JoinHandle<()>) {
    let p = Arc::new(Mutex::new(Peer {
        responses,
        requests: vec![],
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node = NodeClient::new(
        &format!("http://{}", listener.local_addr().unwrap()),
        binding().node_id,
        "n".repeat(40),
        None,
    )
    .unwrap();
    let app = Router::new()
        .route("/v2/worker/agent-operations/query", post(endpoint))
        .route("/v2/worker/agent-operations/retry", post(endpoint))
        .with_state(p.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (node, p, task)
}
#[tokio::test]
async fn known_lookup_and_retry_use_only_retained_ids_and_node_credential() {
    let request = lookup();
    let operation = fixtures()["operation_statuses"]["confirmed"].clone();
    let reply = response(&request, vec![operation.clone()], None);
    assert!(valid("RecoveryResponse", &reply));
    let (node, p, server) = peer(vec![(200, reply.clone()), (200, reply)]).await;
    assert_eq!(
        node.recover(&binding(), request.clone(), false)
            .await
            .unwrap()["operations"][0],
        operation
    );
    assert!(valid("RecoveryRetry", &request));
    node.recover(&binding(), request.clone(), true)
        .await
        .unwrap();
    let p = p.lock().unwrap();
    assert_eq!(p.requests.len(), 2);
    assert_eq!(
        p.requests[0],
        ("/v2/worker/agent-operations/query".into(), request.clone())
    );
    assert_eq!(
        p.requests[1],
        ("/v2/worker/agent-operations/retry".into(), request)
    );
    assert!(
        !serde_json::to_string(&p.requests)
            .unwrap()
            .contains("expected_credential_version")
    );
    server.abort();
}
#[tokio::test]
async fn bounded_paginated_list_returns_server_cursor_without_new_authority() {
    let mut first = fixtures()["node_list_request"].clone();
    first["limit"] = 1.into();
    let mut second = first.clone();
    second["request_id"] = Uuid::new_v4().to_string().into();
    second["cursor"] = "opaque-centrally-owned-cursor".into();
    let operation = fixtures()["operation_statuses"]["unknown"].clone();
    let a = response(
        &first,
        vec![operation.clone()],
        Some("opaque-centrally-owned-cursor"),
    );
    let b = response(&second, vec![], None);
    let (node, p, server) = peer(vec![(200, a), (200, b)]).await;
    let page = node
        .recover(&binding(), first.clone(), false)
        .await
        .unwrap();
    assert_eq!(page["next_cursor"], second["cursor"]);
    assert_eq!(page["coverage"]["status"], "partial");
    let end = node
        .recover(&binding(), second.clone(), false)
        .await
        .unwrap();
    assert!(end["operations"].as_array().unwrap().is_empty());
    assert_eq!(p.lock().unwrap().requests[1].1, second);
    server.abort();
}
#[tokio::test]
async fn foreign_response_bindings_ids_status_and_malformed_body_refuse() {
    let request = lookup();
    let original = fixtures()["operation_statuses"]["confirmed"].clone();
    let mut variants = vec![];
    let mut foreign = original.clone();
    foreign["binding"]["attempt_id"] = Uuid::new_v4().to_string().into();
    variants.push((200, response(&request, vec![foreign], None)));
    let mut foreign = original.clone();
    foreign["operation_id"] = Uuid::new_v4().to_string().into();
    variants.push((200, response(&request, vec![foreign], None)));
    let mut wrong_request = response(&request, vec![original.clone()], None);
    wrong_request["request_id"] = Uuid::new_v4().to_string().into();
    variants.push((200, wrong_request));
    variants.push((202, response(&request, vec![original.clone()], None)));
    variants.push((200, json!({"api_version":1})));
    let mut bad = response(&request, vec![original], None);
    bad["attempt"]["generation"] = 2.into();
    variants.push((200, bad));
    let count = variants.len();
    let (node, p, server) = peer(variants).await;
    for _ in 0..count {
        assert!(
            node.recover(&binding(), request.clone(), false)
                .await
                .is_err()
        );
    }
    assert_eq!(p.lock().unwrap().requests.len(), count);
    server.abort();
}
#[tokio::test]
async fn client_refuses_new_payload_list_retry_and_foreign_request_before_http() {
    let request = lookup();
    let (node, p, server) = peer(vec![]).await;
    let mut payload = request.clone();
    payload["command"] = json!({"kind":"invented_mutation"});
    assert!(node.recover(&binding(), payload, true).await.is_err());
    assert!(
        node.recover(&binding(), fixtures()["node_list_request"].clone(), true)
            .await
            .is_err()
    );
    let mut foreign = request.clone();
    foreign["attempt"]["generation"] = 2.into();
    assert!(node.recover(&binding(), foreign, false).await.is_err());
    let mut foreign = request;
    foreign["node_id"] = Uuid::new_v4().to_string().into();
    assert!(node.recover(&binding(), foreign, false).await.is_err());
    assert!(p.lock().unwrap().requests.is_empty());
    assert!(
        NodeClient::new(
            "https://fixture.invalid",
            binding().node_id,
            format!("aqa_{}", "s".repeat(40)),
            None
        )
        .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn foreign_workspace_duplicate_ids_limit_overrun_and_lookup_cursor_refuse() {
    let request = lookup();
    let original = fixtures()["operation_statuses"]["confirmed"].clone();
    let mut foreign = original.clone();
    foreign["receipt"]["workspace_id"] = Uuid::new_v4().to_string().into();
    let mut list = fixtures()["node_list_request"].clone();
    list["limit"] = 1.into();
    let mut other = original.clone();
    other["operation_id"] = Uuid::new_v4().to_string().into();
    let variants = vec![
        (
            request.clone(),
            response(&request, vec![foreign], None),
            false,
        ),
        (
            request.clone(),
            response(&request, vec![original.clone(), original.clone()], None),
            false,
        ),
        (
            list.clone(),
            response(&list, vec![original.clone(), other], None),
            false,
        ),
        (
            request.clone(),
            response(
                &request,
                vec![original.clone()],
                Some("forbidden-lookup-cursor"),
            ),
            false,
        ),
        (
            request.clone(),
            response(
                &request,
                vec![{
                    let mut foreign = original;
                    foreign["operation_id"] = Uuid::new_v4().to_string().into();
                    foreign
                }],
                None,
            ),
            true,
        ),
    ];
    let replies = variants.iter().map(|(_, r, _)| (200, r.clone())).collect();
    let (node, p, server) = peer(replies).await;
    for (request, _, retry) in &variants {
        assert!(
            node.recover(&binding(), request.clone(), *retry)
                .await
                .is_err()
        );
    }
    assert_eq!(p.lock().unwrap().requests.len(), variants.len());
    server.abort();
}
