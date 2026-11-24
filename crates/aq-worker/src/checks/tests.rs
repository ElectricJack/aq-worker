use super::client::{CheckCallError, CheckClient, CheckTransport};
use super::journal::{Journal, Record};
use super::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
#[path = "../../tests/support/check_claim.rs"]
mod fixture;
use fixture::{NODE, claim_json, validator};
#[test]
fn stop_serializes_in_the_server_shape() {
    let op = Uuid::from_u128(5);
    let digest = inputs_digest(&claim_json(op));
    let exited = StopReport::new(op, StopKind::Exited { code: 0 }, digest.clone(), true);
    assert_eq!(
        serde_json::from_slice::<Value>(&exited.body().unwrap()).unwrap(),
        json!({"operation_id": op, "stop": {"kind": "exited", "code": 0},
            "executor_version": EXECUTOR_VERSION, "inputs_digest": digest,
            "inputs_complete": true, "result_artifacts": []})
    );
    for (kind, name) in [
        (StopKind::Signalled, "signalled"),
        (StopKind::TimedOut, "timed_out"),
        (StopKind::Cancelled, "cancelled"),
        (StopKind::Lost, "lost"),
    ] {
        let stop = StopReport::new(op, kind, digest.clone(), true);
        let body: Value = serde_json::from_slice(&stop.body().unwrap()).unwrap();
        assert_eq!(body["stop"], json!({"kind": name}));
        assert_eq!(
            body["inputs_complete"],
            json!(false),
            "only exited is complete"
        );
    }
}

#[test]
fn stop_body_enforces_the_server_limits() {
    let op = Uuid::from_u128(5);
    let digest = inputs_digest(&claim_json(op));
    let mut stop = StopReport::new(op, StopKind::Exited { code: 1 }, digest.clone(), true);
    stop.inputs_digest = "sha256:ABC".into();
    assert!(stop.body().is_err());
    stop.inputs_digest = digest;
    stop.result_artifacts = vec![validator(); MAX_RESULT_ARTIFACTS + 1];
    assert!(stop.body().is_err());
    stop.result_artifacts = vec![validator(); MAX_RESULT_ARTIFACTS];
    assert!(stop.body().is_ok());
    stop.executor_version = "x".repeat(129);
    assert!(stop.body().is_err());
    assert!(
        StopReport::new(Uuid::nil(), StopKind::Lost, stop.inputs_digest, false)
            .body()
            .is_err()
    );
}

#[test]
fn inputs_digest_is_canonical_and_excludes_attempt_identities() {
    let a = claim_json(Uuid::from_u128(1));
    let mut b = claim_json(Uuid::from_u128(2));
    let digest = inputs_digest(&a);
    assert!(sha256_text(&digest));
    assert_eq!(
        digest,
        inputs_digest(&b),
        "operation and attempt ids excluded"
    );
    // Same content, different key order.
    let reordered: Value = serde_json::from_str(
        r#"{"validator":{"digest":"x","version":"1","id":"cargo-test"},"id":"unit"}"#,
    )
    .unwrap();
    b["check"] = reordered.clone();
    let mut c = a.clone();
    c["check"] = serde_json::from_str(
        r#"{"id":"unit","validator":{"id":"cargo-test","version":"1","digest":"x"}}"#,
    )
    .unwrap();
    assert_eq!(inputs_digest(&b), inputs_digest(&c));
    b["source"]["hex"] = json!("1123456789abcdef0123456789abcdef01234567");
    assert_ne!(inputs_digest(&b), inputs_digest(&c));
}

#[test]
fn claim_validation_refuses_unrunnable_claims() {
    let ok: CheckClaim = serde_json::from_value(claim_json(Uuid::from_u128(1))).unwrap();
    ok.validate().unwrap();
    let mut bad = ok.clone();
    bad.execution_profile = "aq.check.host/v1".into();
    assert!(bad.validate().is_err());
    let mut bad = ok.clone();
    bad.source.hex = "../../etc/passwd".into();
    assert!(bad.validate().is_err());
    let mut bad = ok.clone();
    bad.source.format = "md5".into();
    assert!(bad.validate().is_err());
    let mut bad = ok;
    bad.check.validator.digest = "sha256:short".into();
    assert!(bad.validate().is_err());
}

#[test]
fn journal_is_exclusive_and_keeps_the_first_stop() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    assert!(Journal::open(dir.path()).is_err(), "one runner per journal");
    let op = Uuid::from_u128(9);
    let digest = inputs_digest(&claim_json(op));
    let mut record = Record::new(op, digest.clone());
    record.decide(StopReport::new(
        op,
        StopKind::Exited { code: 0 },
        digest.clone(),
        true,
    ));
    record.decide(StopReport::new(op, StopKind::Lost, digest, false));
    journal.save(&record).unwrap();
    let loaded = journal.load(op).unwrap().unwrap();
    assert_eq!(loaded.stop.unwrap().stop, StopKind::Exited { code: 0 });
}

async fn serve(
    status: axum::http::StatusCode,
    reply: Value,
) -> (String, Arc<Mutex<Vec<(String, Option<String>, Value)>>>) {
    use axum::{Json, Router, extract::Request, routing::post};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().route(
        "/v1/worker/checks/{op}",
        post(move |request: Request| {
            let log = log.clone();
            let reply = reply.clone();
            async move {
                let path = request.uri().path().to_string();
                let auth = request
                    .headers()
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string());
                let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                    .await
                    .unwrap();
                log.lock()
                    .unwrap()
                    .push((path, auth, serde_json::from_slice(&body).unwrap()));
                (status, Json(reply))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, seen)
}

fn client(base: &str) -> CheckClient {
    CheckClient::from_config(&crate::config::WorkerConfig {
        server_url: base.into(),
        node_id: NODE,
        credential: "node-secret".into(),
        cf_client_id: None,
        cf_client_secret: None,
    })
    .unwrap()
}

#[tokio::test]
async fn client_claims_and_stops_with_the_node_credential() {
    use axum::http::StatusCode;
    let op = Uuid::from_u128(0x200);
    let (base, seen) = serve(StatusCode::OK, json!({"checks": [claim_json(op)]})).await;
    let checks = client(&base).claim(99).await.unwrap();
    assert_eq!(checks[0]["operation_id"], json!(op));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "/v1/worker/checks/claim");
        assert_eq!(seen[0].1.as_deref(), Some("Bearer node-secret"));
        assert_eq!(
            seen[0].2,
            json!({"limit": MAX_CLAIM_LIMIT}),
            "limit clamped"
        );
    }

    let (base, seen) = serve(
        StatusCode::OK,
        json!({"operation_id": op, "recorded": true}),
    )
    .await;
    let stop = StopReport::new(op, StopKind::Lost, inputs_digest(&claim_json(op)), false);
    client(&base).stop(&stop.body().unwrap()).await.unwrap();
    assert_eq!(seen.lock().unwrap()[0].0, "/v1/worker/checks/stop");

    for (status, expected) in [
        (StatusCode::CONFLICT, CheckCallError::Conflict),
        (StatusCode::NOT_FOUND, CheckCallError::NotFound),
    ] {
        let (base, _) = serve(status, json!({"error": "x"})).await;
        assert_eq!(
            client(&base).stop(&stop.body().unwrap()).await,
            Err(expected)
        );
    }
    let (base, _) = serve(StatusCode::SERVICE_UNAVAILABLE, json!({})).await;
    assert!(matches!(
        client(&base).stop(&stop.body().unwrap()).await,
        Err(CheckCallError::Transient(_))
    ));
    let (base, _) = serve(StatusCode::BAD_REQUEST, json!({})).await;
    assert!(matches!(
        client(&base).stop(&stop.body().unwrap()).await,
        Err(CheckCallError::Invalid(_))
    ));
}
