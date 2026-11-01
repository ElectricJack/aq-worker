use super::*;
use axum::{Router, body::Bytes, http::HeaderMap, routing::post};
use std::sync::{Arc, Mutex};
#[tokio::test]
async fn exact_cap_reaches_allowlisted_peer_with_distinct_upstream_key_and_preserved_sse() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap, body: Bytes| {
            let capture = capture.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer upstream-private-key");
                capture
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"type\":\"response.completed\"}\n\n",
                )
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let scope = policy::tests::scope();
    let expected_binding = scope.binding().clone();
    let owner = ScopedResponsesTransport::start(
        scope,
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        Zeroizing::new(b"local-attempt-capability".to_vec()),
        Zeroizing::new(b"upstream-private-key".to_vec()),
    )
    .await
    .unwrap();
    assert_eq!(owner.binding(), &expected_binding);
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for bad in [
        br#"{"model":"offline-model","model":"foreign"}"#.as_slice(),
        br#"{"model":"foreign"}"#.as_slice(),
    ] {
        assert!(
            client
                .post(format!("{}/v1/responses", owner.origin()))
                .bearer_auth("local-attempt-capability")
                .header("content-type", "application/json")
                .body(bad.to_vec())
                .send()
                .await
                .is_err()
        );
    }
    let mut foreign: serde_json::Value = serde_json::from_slice(&policy::tests::body()).unwrap();
    foreign["input"] = serde_json::json!([{"type":"item_reference","id":"item_foreign"}]);
    assert!(
        client
            .post(format!("{}/v1/responses", owner.origin()))
            .bearer_auth("local-attempt-capability")
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&foreign).unwrap())
            .send()
            .await
            .is_err()
    );
    assert!(seen.lock().unwrap().is_empty());
    let reply = client
        .post(format!("{}/v1/responses", owner.origin()))
        .bearer_auth("local-attempt-capability")
        .header("content-type", "application/json")
        .body(policy::tests::body())
        .send()
        .await
        .unwrap();
    assert_eq!(
        reply.text().await.unwrap(),
        "data: {\"type\":\"response.completed\"}\n\n"
    );
    let mut request = seen.lock().unwrap()[0].clone();
    assert_eq!(
        request.as_object_mut().unwrap().remove("max_output_tokens"),
        Some(100.into())
    );
    assert_eq!(
        request,
        serde_json::from_slice::<serde_json::Value>(&policy::tests::body()).unwrap()
    );
    owner.stop().await.unwrap();
    server.abort();
}
#[tokio::test]
async fn stop_cancels_pending_upstream_and_ambiguous_http_never_forwards() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let notify = entered.clone();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let notify = notify.clone();
            async move {
                notify.notify_one();
                std::future::pending::<&'static str>().await
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let owner = ScopedResponsesTransport::start(
        policy::tests::scope(),
        UpstreamEndpoint::development_loopback(&origin).unwrap(),
        Zeroizing::new(b"local-capability".to_vec()),
        Zeroizing::new(b"upstream-private-key".to_vec()),
    )
    .await
    .unwrap();
    let authority = owner.origin().strip_prefix("http://").unwrap();
    for extra in [
        "Content-Length: 2\r\nContent-Length: 2\r\n",
        "Content-Length: 2\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: 2\r\nContent-Encoding: gzip\r\n",
    ] {
        let mut connection = TcpStream::connect(authority).await.unwrap();
        connection.write_all(format!("POST /v1/responses HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer local-capability\r\nContent-Type: application/json\r\n{extra}\r\n{{}}").as_bytes()).await.unwrap();
        let mut bytes = Vec::new();
        let closed =
            tokio::time::timeout(Duration::from_secs(1), connection.read_to_end(&mut bytes))
                .await
                .unwrap();
        assert!(
            closed.is_ok()
                || closed.is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset)
        );
        assert!(bytes.is_empty());
    }
    let endpoint = format!("{}/v1/responses", owner.origin());
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(endpoint)
            .bearer_auth("local-capability")
            .header("content-type", "application/json")
            .body(policy::tests::body())
            .send()
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), owner.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server.abort();
}
#[tokio::test]
async fn endpoint_and_key_scope_refuse_before_serving() {
    for url in [
        "https://127.0.0.1:444",
        "http://localhost:444",
        "http://127.0.0.1:444/path",
        "http://127.0.0.1:444/?q=1",
        "http://user@127.0.0.1:444",
        "http://127.0.0.1",
        "http://127.0.0.2:444",
    ] {
        assert!(UpstreamEndpoint::development_loopback(url).is_err());
    }
    assert!(
        ScopedResponsesTransport::start(
            policy::tests::scope(),
            UpstreamEndpoint::openai(),
            Zeroizing::new(b"same-secret".to_vec()),
            Zeroizing::new(b"same-secret".to_vec())
        )
        .await
        .is_err()
    );
}
