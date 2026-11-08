//! Synthetic HTTP peers exercise exact published request/receipt correlation.
//! Actual owner authorization remains qualified in agent-q's PostgreSQL tests.
use aq_admission::{
    Digest, MaterialLease, RetainedArtifact, result_record, started, stopped,
    transport::{ArtifactFetch, DeliveryClock, MaterialDelivery, MaterialFetch, WorkerIdentity},
};
use aq_worker::{
    config::WorkerConfig,
    native::{
        ArtifactAssembly,
        http::{HttpStatus, NativeClient},
    },
};
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

fn started_request() -> started::Request {
    started::decode_request(include_bytes!(
        "../../../vendor/aq-admission-0.1.6/artifacts/started.fixture.json"
    ))
    .unwrap()
}
fn started_receipt(r: &started::Request) -> started::Receipt {
    started::Receipt {
        schema_version: 1,
        operation_id: r.operation_id,
        binding: r.binding.clone(),
        grant_id: r.grant_id,
        request_digest: r.digest().unwrap(),
        recorded_at_ms: 1500,
        authority_revision_at_record: 3,
    }
}
fn identity() -> WorkerIdentity {
    WorkerIdentity {
        node_id: Uuid::from_u128(11),
        boot_id: Uuid::from_u128(12),
    }
}
fn auth(headers: &HeaderMap) {
    assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
    assert_eq!(headers["cf-access-client-id"], "synthetic-access-id");
    assert_eq!(
        headers["cf-access-client-secret"],
        "synthetic-access-secret"
    );
}
struct Server {
    config: WorkerConfig,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            config: WorkerConfig {
                server_url: format!("http://{addr}"),
                node_id: identity().node_id,
                credential: "synthetic-node-secret".into(),
                cf_client_id: Some("synthetic-access-id".into()),
                cf_client_secret: Some("synthetic-access-secret".into()),
            },
            task,
        }
    }
    fn client(&self) -> NativeClient {
        NativeClient::from_config(&self.config).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn artifact_fixture() -> (ArtifactFetch, RetainedArtifact, Vec<u8>) {
    let value: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-admission-0.1.6/artifacts/startup.fixture.json"
    ))
    .unwrap();
    let offer = aq_admission::startup::decode_native_offer(
        &serde_json::to_vec(&value["native_offer"]).unwrap(),
    )
    .unwrap();
    let descriptor = offer
        .offered
        .artifacts
        .iter()
        .find(|a| a.artifact == offer.startup.payload)
        .unwrap()
        .clone();
    let request = ArtifactFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity(),
        binding: started_request().binding,
        artifact: descriptor.artifact.clone(),
        offset: 0,
        length: 262_144,
    };
    (
        request,
        descriptor,
        value["payload_utf8"].as_str().unwrap().as_bytes().to_vec(),
    )
}
fn octets(name: &str, metadata: &impl serde::Serialize, bytes: Vec<u8>) -> Response {
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert("content-type", "application/octet-stream".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response.headers_mut().insert(
        axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(metadata).unwrap())
            .parse()
            .unwrap(),
    );
    response
}
#[tokio::test]
async fn exact_native_started_stopped_result_receipts_and_stable_replay() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started_calls = calls.clone();
    let server=Server::new(Router::new()
        .route("/v1/worker/profiled/register",post(|headers:HeaderMap,Json(r):Json<aq_admission::transport::TransportDocument>|async move {auth(&headers);r.validate().unwrap();Json(r)}))
        .route("/v1/worker/profiled/started",post(move|headers:HeaderMap,Json(r):Json<started::Request>| {let calls=started_calls.clone();async move {
            auth(&headers);assert_eq!(r.digest().unwrap(),started_request().digest().unwrap());calls.fetch_add(1,Ordering::SeqCst);Json(started_receipt(&r))}}))
        .route("/v1/worker/profiled/stopped",post(|headers:HeaderMap,Json(r):Json<stopped::Request>|async move {
            auth(&headers);r.validate().unwrap();Json(stopped::Receipt {schema_version:1,operation_id:r.evidence.stop_operation_id,
                binding:r.evidence.binding.clone(),grant_id:r.grant_id().unwrap(),request_digest:r.digest().unwrap(),recorded_at_ms:1500,authority_revision_at_record:4})}))
        .route("/v1/worker/profiled/results",post(|headers:HeaderMap,Json(r):Json<aq_admission::transport::ResultReport>|async move {
            auth(&headers);let fixture:serde_json::Value=serde_json::from_slice(include_bytes!("../../../vendor/aq-admission-0.1.6/artifacts/result-record.fixture.json")).unwrap();
            let receipt:result_record::Receipt=serde_json::from_value(fixture["receipt"].clone()).unwrap();receipt.validate_for(&r).unwrap();Json(receipt)}))).await;
    let client = server.client();
    let request = started_request();
    let registered = aq_admission::transport::Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity(),
        runtime: request.runtime.clone(),
        capabilities: vec![],
        features: vec![
            aq_admission::transport::Feature::ArtifactTransferV1,
            aq_admission::transport::Feature::ServerMaterialV1,
            aq_admission::transport::Feature::DurableHostJournalV1,
            aq_admission::transport::Feature::HostStopEvidenceV1,
        ],
    };
    client.register(&registered).await.unwrap();
    let a = client.started(&request).await.unwrap();
    let b = client.started(&request).await.unwrap();
    assert_eq!(a.request_digest, b.request_digest);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let stop = stopped::decode_request(include_bytes!(
        "../../../vendor/aq-admission-0.1.6/artifacts/stopped.fixture.json"
    ))
    .unwrap();
    client.stopped(&stop).await.unwrap();
    let fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-admission-0.1.6/artifacts/result-record.fixture.json"
    ))
    .unwrap();
    let report = serde_json::from_value(fixture["report"].clone()).unwrap();
    let expected: result_record::Receipt =
        serde_json::from_value(fixture["receipt"].clone()).unwrap();
    assert_eq!(
        client
            .report(
                &report,
                &expected.expected_outcome,
                &expected.stopped_receipt
            )
            .await
            .unwrap()
            .disposition,
        result_record::Disposition::Recorded
    );
    let mut changed = expected.expected_outcome;
    changed.digest = Digest::of(b"different outcome");
    assert!(
        client
            .report(&report, &changed, &expected.stopped_receipt)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn wrong_node_changed_receipt_unknown_fields_and_redirects_refuse() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = calls.clone();
    let server = Server::new(Router::new().route(
        "/v1/worker/profiled/started",
        post(move |Json(r): Json<started::Request>| {
            let calls = handler.clone();
            async move {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        let mut receipt = started_receipt(&r);
                        receipt.request_digest = Digest::of(b"wrong");
                        Json(receipt).into_response()
                    }
                    1 => {
                        let mut value = serde_json::to_value(started_receipt(&r)).unwrap();
                        value["grant_allowed"] = true.into();
                        Json(value).into_response()
                    }
                    2 => vec![b'x'; started::MAX_BYTES + 1].into_response(),
                    _ => (
                        StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/v1/worker/profiled/started")],
                        "synthetic secret body",
                    )
                        .into_response(),
                }
            }
        }),
    ))
    .await;
    let mut wrong = server.config.clone();
    wrong.node_id = Uuid::new_v4();
    assert!(
        NativeClient::from_config(&wrong)
            .unwrap()
            .started(&started_request())
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let client = server.client();
    for _ in 0..4 {
        let error = client.started(&started_request()).await.unwrap_err();
        assert!(!format!("{error:#}").contains("synthetic secret"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4); // no automatic retries/redirect follow
}
#[tokio::test]
async fn artifact_headers_ranges_and_exact_assembly_are_checked() {
    let (request, descriptor, bytes) = artifact_fixture();
    let expected = bytes.clone();
    let expected_descriptor = descriptor.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let server = Server::new(Router::new().route(
        "/v1/worker/profiled/artifact",
        post(move |headers: HeaderMap, Json(r): Json<ArtifactFetch>| {
            let mut d = descriptor.clone();
            let bytes = bytes.clone();
            let calls = calls.clone();
            async move {
                auth(&headers);
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 3 {
                    d.retention_receipt.digest = Digest::of(b"different retention owner");
                }
                let mut response = octets("aq-artifact-retention", &d, bytes);
                response.headers_mut().insert(
                    "aq-artifact-offset",
                    (r.offset + u64::from(n == 1)).to_string().parse().unwrap(),
                );
                if n == 2 {
                    response.headers_mut().remove("cache-control");
                }
                response
            }
        }),
    ))
    .await;
    let client = server.client();
    let chunk = client
        .artifact(&request, &expected_descriptor)
        .await
        .unwrap();
    let mut assembly = ArtifactAssembly::new(chunk.descriptor).unwrap();
    assembly.append(chunk.offset, &chunk.bytes).unwrap();
    assert_eq!(assembly.finish().unwrap().bytes(), expected);
    // Wrong range, missing no-store, and a substituted retention owner all refuse.
    for _ in 0..3 {
        assert!(
            client
                .artifact(&request, &expected_descriptor)
                .await
                .is_err()
        );
    }
}
/// A synthetic artifact route that answers the scripted refusals (status, `Retry-After`) in
/// order, then the artifact; returns the server and its request count.
async fn scripted_artifact(script: Vec<(u16, Option<&'static str>)>) -> (Server, Arc<AtomicUsize>) {
    let (_, descriptor, bytes) = artifact_fixture();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let server = Server::new(Router::new().route(
        "/v1/worker/profiled/artifact",
        post(move |headers: HeaderMap, Json(r): Json<ArtifactFetch>| {
            let (d, bytes, calls) = (descriptor.clone(), bytes.clone(), calls.clone());
            let script = script.clone();
            async move {
                auth(&headers);
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if let Some(&(status, after)) = script.get(n) {
                    let mut refused =
                        (StatusCode::from_u16(status).unwrap(), "overloaded").into_response();
                    if let Some(after) = after {
                        refused
                            .headers_mut()
                            .insert("retry-after", after.parse().unwrap());
                    }
                    return refused;
                }
                let mut response = octets("aq-artifact-retention", &d, bytes);
                let offset = r.offset.to_string().parse().unwrap();
                response.headers_mut().insert("aq-artifact-offset", offset);
                response
            }
        }),
    ))
    .await;
    (server, seen)
}
/// R1: a transient refusal of an artifact download (the live `503 overloaded`) is retried
/// with the same request until it succeeds; a persistent one fails after `FETCH_TRIES`.
#[tokio::test]
async fn transient_artifact_refusals_are_retried_then_succeed_or_fail_bounded() {
    let (request, descriptor, bytes) = artifact_fixture();
    let (server, calls) = scripted_artifact(vec![(503, None), (503, Some("0"))]).await;
    let chunk = server
        .client()
        .artifact(&request, &descriptor)
        .await
        .unwrap();
    assert_eq!(chunk.bytes, bytes);
    assert_eq!(calls.load(Ordering::SeqCst), 3, "503, 503, then 200");

    let (server, calls) = scripted_artifact(vec![(503, Some("0")); 9]).await;
    let e = server
        .client()
        .artifact(&request, &descriptor)
        .await
        .err()
        .unwrap();
    assert_eq!(
        e.downcast_ref::<HttpStatus>().unwrap().0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let tries = aq_worker::native::http::FETCH_TRIES as usize;
    assert_eq!(calls.load(Ordering::SeqCst), tries, "bounded");
}
/// Only 502/503/504, 408 and 429 are retried, a `Retry-After` is waited exactly, and one past
/// the backoff cap ends the retries; any other 4xx (or 500) is final at once.
#[tokio::test]
async fn artifact_retry_honors_retry_after_and_never_retries_other_refusals() {
    let (request, descriptor, _) = artifact_fixture();
    for status in [429, 408, 502, 504] {
        let (server, calls) = scripted_artifact(vec![(status, Some("0"))]).await;
        server
            .client()
            .artifact(&request, &descriptor)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{status} retried");
    }
    let (server, calls) = scripted_artifact(vec![(429, Some("1"))]).await;
    let started = std::time::Instant::now();
    server
        .client()
        .artifact(&request, &descriptor)
        .await
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "Retry-After waited"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    for (status, after) in [
        (400, None),
        (404, None),
        (409, None),
        (500, None),
        (429, Some("60")),
    ] {
        let (server, calls) = scripted_artifact(vec![(status, after); 2]).await;
        let e = server
            .client()
            .artifact(&request, &descriptor)
            .await
            .err()
            .unwrap();
        assert_eq!(e.downcast_ref::<HttpStatus>().unwrap().0.as_u16(), status);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{status} {after:?} is final"
        );
    }
}
/// The retry classification: transport failures are transient and back off within
/// [0, 500 ms * 2^n] capped at 8 s; a refusal's `Retry-After` overrides the backoff.
#[test]
fn fetch_retry_wait_classifies_and_bounds() {
    use aq_worker::native::http::{TransportFailed, fetch_retry_wait};
    let transport: anyhow::Error = TransportFailed("native request transport failed").into();
    for attempt in 0..8 {
        let cap = Duration::from_millis(500 << attempt).min(Duration::from_secs(8));
        let wait = fetch_retry_wait(&transport, attempt).expect("transient");
        assert!(wait <= cap, "{attempt}: {wait:?} > {cap:?}");
    }
    let wait = |status: StatusCode, after: Option<u64>| {
        let e: anyhow::Error = HttpStatus(status, after.map(Duration::from_secs)).into();
        fetch_retry_wait(&e, 0)
    };
    assert_eq!(
        wait(StatusCode::TOO_MANY_REQUESTS, Some(3)),
        Some(Duration::from_secs(3))
    );
    assert_eq!(
        wait(StatusCode::SERVICE_UNAVAILABLE, Some(9)),
        None,
        "past the cap"
    );
    assert!(wait(StatusCode::BAD_GATEWAY, None).is_some());
    assert_eq!(wait(StatusCode::FORBIDDEN, None), None);
    assert_eq!(wait(StatusCode::INTERNAL_SERVER_ERROR, None), None);
    assert_eq!(
        fetch_retry_wait(&anyhow::anyhow!("artifact response range"), 0),
        None
    );
}
#[tokio::test]
async fn two_stalled_downloads_do_not_consume_started_reporting_lane() {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let e = entered.clone();
    let r = release.clone();
    let server = Server::new(
        Router::new()
            .route(
                "/v1/worker/profiled/artifact",
                post(move || {
                    let e = e.clone();
                    let r = r.clone();
                    async move {
                        e.add_permits(1);
                        r.acquire().await.unwrap().forget();
                        StatusCode::CONFLICT
                    }
                }),
            )
            .route(
                "/v1/worker/profiled/started",
                post(|Json(r): Json<started::Request>| async move { Json(started_receipt(&r)) }),
            ),
    )
    .await;
    let client = server.client();
    let (request, expected_descriptor, _) = artifact_fixture();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let c = client.clone();
        let q = request.clone();
        let descriptor = expected_descriptor.clone();
        tasks.push(tokio::spawn(async move {
            c.artifact(&q, &descriptor).await.is_err()
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), entered.acquire_many(2))
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        client
            .artifact(&request, &expected_descriptor)
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(2), client.started(&started_request()))
        .await
        .unwrap()
        .unwrap();
    release.add_permits(2);
    for task in tasks {
        assert!(task.await.unwrap());
    }
}
#[tokio::test]
async fn material_digest_scope_and_full_network_interval_are_checked() {
    let fixtures: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-admission-0.1.6/artifacts/fixtures.json"
    ))
    .unwrap();
    let mut lease: MaterialLease = serde_json::from_value(
        fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == "material_lease")
            .unwrap()["body"]
            .clone(),
    )
    .unwrap();
    let bytes = b"synthetic provider material".to_vec();
    lease.material_bytes = bytes.len() as u64;
    lease.material_digest = Digest::of(&bytes);
    lease.issued_at_ms = 1000;
    lease.expires_at_ms = 2000;
    let request = MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity(),
        binding: lease.binding.clone(),
        lease_id: lease.lease_id,
    };
    let server_lease = lease.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let server = Server::new(Router::new().route(
        "/v1/worker/profiled/material",
        post(move |headers: HeaderMap| {
            let lease = server_lease.clone();
            let bytes = bytes.clone();
            let calls = calls.clone();
            async move {
                auth(&headers);
                let n = calls.fetch_add(1, Ordering::SeqCst);
                // R1: 3 and 5 are overloaded (5 asks for a wait past the retrieval deadline).
                if let Some(after) = [(3, "0"), (5, "1")].iter().find(|(i, _)| *i == n) {
                    let mut busy = (StatusCode::SERVICE_UNAVAILABLE, "overloaded").into_response();
                    busy.headers_mut()
                        .insert("retry-after", after.1.parse().unwrap());
                    return busy;
                }
                let server_time = if n == 2 { 1999 } else { 1000 };
                if n == 2 {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                let body = if n == 1 {
                    vec![b'x'; bytes.len()]
                } else {
                    bytes
                };
                octets(
                    "aq-material-delivery",
                    &MaterialDelivery {
                        clock: DeliveryClock::new(server_time, lease.expires_at_ms).unwrap(),
                        lease,
                    },
                    body,
                )
            }
        }),
    ))
    .await;
    let client = server.client();
    let received = client.material(&request, &lease).await.unwrap();
    assert_eq!(received.bytes(), b"synthetic provider material");
    assert!(client.material(&request, &lease).await.is_err());
    assert!(client.material(&request, &lease).await.is_err()); // expired in transit
    // The material route only reads the lease: an overloaded read is retried, and each try
    // is timed from its own send (the 503 took no part of the network interval).
    let retried = client.material(&request, &lease).await.unwrap();
    assert_eq!(retried.bytes(), b"synthetic provider material");
    assert_eq!(seen.load(Ordering::SeqCst), 5, "503 then 200");
    // A retry whose wait would end past the retrieval deadline is not attempted.
    let deadline = client.fetch_until(boot_ns() + 200_000_000);
    let e = deadline.material(&request, &lease).await.err().unwrap();
    assert_eq!(
        e.downcast_ref::<HttpStatus>().unwrap().0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(seen.load(Ordering::SeqCst), 6, "no retry past the deadline");
}
fn boot_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut t) },
        0
    );
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
#[test]
fn incomplete_machine_auth_and_credential_bearing_urls_refuse_without_echo() {
    let base = WorkerConfig {
        server_url: "https://example.invalid".into(),
        node_id: identity().node_id,
        credential: "private".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    for url in [
        "http://example.invalid",
        "http://localhost",
        "http://192.0.2.1",
        "http://[2001:db8::1]",
        "https://private:secret@example.invalid",
        "https://example.invalid?secret=x",
        "https://example.invalid#secret",
    ] {
        let mut config = base.clone();
        config.server_url = url.into();
        let error = NativeClient::from_config(&config).err().unwrap();
        assert!(!error.to_string().contains("secret"));
    }
    for url in [
        "https://example.invalid",
        "http://127.0.0.1:4321",
        "http://[::1]:4321",
    ] {
        let mut config = base.clone();
        config.server_url = url.into();
        assert!(NativeClient::from_config(&config).is_ok());
    }
    let mut config = base;
    config.cf_client_id = Some("id".into());
    assert!(NativeClient::from_config(&config).is_err());
}

#[test]
fn native_transport_ignores_ambient_proxy_configuration() {
    // Child process isolates environment changes from parallel HTTP tests.
    // Reuse the actual wire roundtrip instead of merely inspecting a builder.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "exact_native_started_stopped_result_receipts_and_stable_replay",
            "--nocapture",
        ])
        .envs([
            ("HTTP_PROXY", "http://127.0.0.1:9"),
            ("http_proxy", "http://127.0.0.1:9"),
            ("HTTPS_PROXY", "http://127.0.0.1:9"),
            ("https_proxy", "http://127.0.0.1:9"),
            ("ALL_PROXY", "http://127.0.0.1:9"),
            ("all_proxy", "http://127.0.0.1:9"),
            ("NO_PROXY", ""),
            ("no_proxy", ""),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
