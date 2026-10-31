use aq_worker::{
    config::WorkerConfig,
    execution_client::ExecutionClient,
    transfer_client::{TransferClient, TransferHttpError},
};
use aq_worker_protocol::{testing::*, *};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post, put},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::Notify;
use uuid::Uuid;

async fn serve(app: Router) -> (WorkerConfig, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = WorkerConfig {
        server_url: format!("http://{}", socket.local_addr().unwrap()),
        node_id: sample_offer().identity.node_id,
        credential: "synthetic-source-token".into(),
        cf_client_id: Some("synthetic-access-id".into()),
        cf_client_secret: Some("synthetic-access-token".into()),
    };
    let handle = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (config, handle)
}
fn manifest() -> TransferManifest {
    let oid = ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: "a".repeat(40),
    };
    TransferManifest {
        transfer_id: Uuid::from_u128(101),
        role: SourceRole::Base,
        checkpoint: oid.clone(),
        attempt: sample_offer().attempt,
        resolved_spec_digest: sample_offer().spec.resolved_spec_digest,
        source: SourceBinding {
            repository_id: Uuid::from_u128(102),
            binding_generation: 1,
            logical_identity: "fixture-repository".into(),
            base: oid,
        },
        artifact_id: Uuid::from_u128(103),
        manifest_digest: "b".repeat(64),
        bundle_digest: None,
        manifest_bytes: 3,
        bundle_bytes: 0,
    }
}
fn status(offset: u64) -> TransferStatus {
    TransferStatus {
        transfer_id: manifest().transfer_id,
        manifest_offset: offset,
        bundle_offset: 0,
        receipt: None,
    }
}
fn status_request() -> TransferStatusRequest {
    TransferStatusRequest {
        identity: sample_offer().identity,
        operation_id: Uuid::from_u128(105),
        transfer_id: manifest().transfer_id,
    }
}
fn chunk() -> TransferChunkRequest {
    TransferChunkRequest {
        identity: sample_offer().identity,
        operation_id: Uuid::from_u128(104),
        transfer_id: manifest().transfer_id,
        file: TransferFile::Manifest,
        offset: 0,
        length: 3,
        digest: format!("{:x}", Sha256::digest(b"abc")),
    }
}
fn assert_headers(h: &HeaderMap) {
    assert_eq!(h[header::AUTHORIZATION], "Bearer synthetic-source-token");
    assert_eq!(h["cf-access-client-id"], "synthetic-access-id");
    assert_eq!(h["cf-access-client-secret"], "synthetic-access-token");
}

#[tokio::test]
async fn finite_authenticated_routes_preserve_exact_ids_bytes_and_ranges() {
    let app = Router::new()
        .route(
            "/v1/execution/sources/begin",
            post(|h: HeaderMap, Json(v): Json<Value>| async move {
                assert_headers(&h);
                assert_eq!(v["identity"], json!(sample_offer().identity));
                assert_eq!(v["operation_id"], Uuid::from_u128(106).to_string());
                assert_eq!(v["manifest"], json!(manifest()));
                Json(status(0))
            }),
        )
        .route(
            "/v1/execution/sources/{id}/{file}",
            put(
                |h: HeaderMap,
                 Path((id, file)): Path<(Uuid, String)>,
                 Query(q): Query<BTreeMap<String, String>>,
                 bytes: Bytes| async move {
                    assert_headers(&h);
                    assert_eq!(id, manifest().transfer_id);
                    assert_eq!(file, "manifest");
                    assert_eq!(q["operation_id"], chunk().operation_id.to_string());
                    assert_eq!(q["offset"], "0");
                    assert_eq!(q["digest"], chunk().digest);
                    assert_eq!(bytes.as_ref(), b"abc");
                    Json(status(3))
                },
            ),
        )
        .route(
            "/v1/execution/sources/base/{id}",
            get(|h: HeaderMap, Path(id): Path<Uuid>| async move {
                assert_headers(&h);
                assert_eq!(id, manifest().attempt.attempt_id);
                Json(manifest())
            }),
        )
        .route(
            "/v1/execution/sources/base/{id}/{file}",
            get(
                |h: HeaderMap,
                 Path((id, file)): Path<(Uuid, String)>,
                 Query(q): Query<BTreeMap<String, String>>| async move {
                    assert_headers(&h);
                    assert_eq!(id, manifest().attempt.attempt_id);
                    assert_eq!(file, "manifest");
                    assert_eq!(q["offset"], "1");
                    assert_eq!(q["limit"], "2");
                    b"bc".to_vec()
                },
            ),
        );
    let (config, server) = serve(app).await;
    let client = TransferClient::from_config(&config, 1).unwrap();
    assert_eq!(
        client
            .begin(&sample_offer().identity, Uuid::from_u128(106), &manifest())
            .await
            .unwrap(),
        status(0)
    );
    assert_eq!(
        client.upload(&chunk(), b"abc".to_vec()).await.unwrap(),
        status(3)
    );
    assert_eq!(
        client.base(manifest().attempt.attempt_id).await.unwrap(),
        manifest()
    );
    assert_eq!(
        client
            .download(manifest().attempt.attempt_id, TransferFile::Manifest, 1, 2)
            .await
            .unwrap(),
        b"bc"
    );
    server.abort();
}

#[tokio::test]
async fn invalid_local_input_cannot_reach_remote_transport() {
    let (config, server) = serve(Router::new().fallback(|| async {
        panic!("invalid input reached transport");
        #[allow(unreachable_code)]
        StatusCode::OK
    }))
    .await;
    let client = TransferClient::from_config(&config, 1).unwrap();
    assert!(
        client
            .upload(&chunk(), b"abd".to_vec())
            .await
            .unwrap_err()
            .to_string()
            .contains("digest")
    );
    let mut r = chunk();
    r.identity.node_id = Uuid::new_v4();
    assert!(
        client
            .upload(&r, b"abc".to_vec())
            .await
            .unwrap_err()
            .to_string()
            .contains("identity")
    );
    assert!(
        client
            .download(
                manifest().attempt.attempt_id,
                TransferFile::Bundle,
                0,
                MAX_CHUNK_BYTES + 1
            )
            .await
            .is_err()
    );
    assert!(
        client
            .download(
                manifest().attempt.attempt_id,
                TransferFile::Bundle,
                u64::MAX,
                1
            )
            .await
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn mismatched_peer_identity_offsets_or_unconfirmed_retention_refuse() {
    let app = Router::new()
        .route(
            "/v1/execution/sources/status",
            post(|| async {
                let mut s = status(0);
                s.transfer_id = Uuid::new_v4();
                Json(s)
            }),
        )
        .route(
            "/v1/execution/sources/{id}/{file}",
            put(|| async { Json(status(2)) }),
        )
        .route(
            "/v1/execution/sources/finalize",
            post(|| async { Json(status(3)) }),
        )
        .route(
            "/v1/execution/sources/base/{id}",
            get(|| async {
                let mut m = manifest();
                m.role = SourceRole::Checkpoint;
                Json(m)
            }),
        );
    let (config, server) = serve(app).await;
    let client = TransferClient::from_config(&config, 1).unwrap();
    assert!(
        client
            .status(&status_request())
            .await
            .unwrap_err()
            .to_string()
            .contains("association")
    );
    assert!(
        client
            .upload(&chunk(), b"abc".to_vec())
            .await
            .unwrap_err()
            .to_string()
            .contains("acknowledgement")
    );
    assert!(
        client
            .finalize(&status_request())
            .await
            .unwrap_err()
            .to_string()
            .contains("unconfirmed")
    );
    assert!(
        client
            .base(manifest().attempt.attempt_id)
            .await
            .unwrap_err()
            .to_string()
            .contains("association")
    );
    server.abort();
}

#[tokio::test]
async fn bounded_responses_and_ranges_reject_large_short_or_untrusted_bodies() {
    let app = Router::new()
        .route(
            "/v1/execution/sources/status",
            post(|| async { vec![b'x'; 65537] }),
        )
        .route(
            "/v1/execution/sources/base/{id}/manifest",
            get(|| async { b"x".to_vec() }),
        )
        .route(
            "/v1/execution/sources/base/{id}/bundle",
            get(|| async { b"abc".to_vec() }),
        );
    let (config, server) = serve(app).await;
    let client = TransferClient::from_config(&config, 1).unwrap();
    assert!(
        client
            .status(&status_request())
            .await
            .unwrap_err()
            .to_string()
            .contains("bound")
    );
    assert!(
        client
            .download(manifest().attempt.attempt_id, TransferFile::Manifest, 0, 2)
            .await
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
    assert!(
        client
            .download(manifest().attempt.attempt_id, TransferFile::Bundle, 0, 2)
            .await
            .unwrap_err()
            .to_string()
            .contains("bound")
    );
    server.abort();
}

#[tokio::test]
async fn blocked_bulk_does_not_queue_or_block_status_and_execution_control() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let e = entered.clone();
    let r = release.clone();
    let app = Router::new()
        .route(
            "/v1/execution/sources/base/{id}",
            get(move || {
                let e = e.clone();
                let r = r.clone();
                async move {
                    e.notify_one();
                    r.notified().await;
                    Json(manifest())
                }
            }),
        )
        .route(
            "/v1/execution/sources/status",
            post(|| async { Json(status(0)) }),
        )
        .route(
            "/v2/worker/reconcile",
            post(|| async {
                Json(ReconcileResponse {
                    attempts: vec![],
                    next_cursor: None,
                })
            }),
        );
    let (config, server) = serve(app).await;
    let client = TransferClient::from_config(&config, 1).unwrap();
    let bulk = client.clone();
    let pending = tokio::spawn(async move { bulk.base(manifest().attempt.attempt_id).await });
    entered.notified().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            client.base(manifest().attempt.attempt_id)
        )
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("capacity")
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), client.status(&status_request()))
            .await
            .unwrap()
            .unwrap(),
        status(0)
    );
    let execution = ExecutionClient::from_config(&config).unwrap();
    let request = ReconcileRequest {
        identity: sample_offer().identity,
        operation_id: Uuid::new_v4(),
        attempts: vec![],
        cursor: None,
        max_items: 16,
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(1), execution.reconcile(&request))
            .await
            .unwrap()
            .unwrap()
            .attempts
            .is_empty()
    );
    release.notify_one();
    pending.await.unwrap().unwrap();
    server.abort();
}

#[tokio::test]
async fn redirects_never_forward_credentials_and_remote_secrets_are_not_errors() {
    let (target, target_server) = serve(Router::new().fallback(|| async {
        panic!("redirect followed");
        #[allow(unreachable_code)]
        StatusCode::OK
    }))
    .await;
    let app = Router::new()
        .route(
            "/v1/execution/sources/status",
            post(move || {
                let url = target.server_url.clone();
                async move {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, url)],
                        "private response secret",
                    )
                }
            }),
        )
        .route(
            "/v1/execution/sources/finalize",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "30")],
                    "private response secret",
                )
            }),
        );
    let (config, server) = serve(app).await;
    let client = TransferClient::from_config(&config, 1).unwrap();
    let err = client.status(&status_request()).await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<TransferHttpError>().unwrap().status,
        StatusCode::FOUND
    );
    assert!(!format!("{err:?}").contains("private response secret"));
    let err = client.finalize(&status_request()).await.unwrap_err();
    assert_eq!(
        err.downcast_ref::<TransferHttpError>()
            .unwrap()
            .retry_after_secs,
        Some(30)
    );
    assert!(!format!("{err:?}").contains("private response secret"));
    server.abort();
    target_server.abort();
}

#[test]
fn credentials_in_urls_partial_access_credentials_and_unbounded_lanes_refuse() {
    let mut config = WorkerConfig {
        server_url: "http://user:secret@localhost/".into(),
        node_id: Uuid::new_v4(),
        credential: "synthetic".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    assert!(TransferClient::from_config(&config, 1).is_err());
    config.server_url = "http://localhost/?token=secret".into();
    assert!(TransferClient::from_config(&config, 1).is_err());
    config.server_url = "http://localhost/".into();
    assert!(TransferClient::from_config(&config, 0).is_err());
    assert!(TransferClient::from_config(&config, 9).is_err());
    config.cf_client_id = Some("id".into());
    assert!(TransferClient::from_config(&config, 1).is_err());
}
