use aq_worker::config::WorkerConfig;
use aq_worker_protocol::{testing::*, *};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
struct Peer {
    coordinator: Option<FakeCoordinator>,
    accepts: usize,
    reports: usize,
    observations: usize,
    polls: usize,
    cancellation: bool,
    expiry: bool,
    expiry_offset: Option<std::path::PathBuf>,
    renewals: usize,
    last_grant: Option<Grant>,
    last_report: Option<ResultReport>,
    directive_acks: usize,
    register_calls: usize,
    post_stop_observation_failures: usize,
    post_stop_observation_requests: Vec<Observation>,
}
type Shared = Arc<Mutex<Peer>>;
async fn register(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Json(r): Json<RegisterRequest>,
) -> Json<RegisterResponse> {
    assert_eq!(headers["authorization"], "Bearer fixture-node-secret");
    r.validate().unwrap();
    let mut offer = sample_offer();
    offer.identity = r.identity.clone();
    let mut s = shared.lock().unwrap();
    s.register_calls += 1;
    if s.expiry {
        offer.reservation_ttl_ms = 3_000;
    }
    s.coordinator = Some(FakeCoordinator::new(offer, 100));
    Json(RegisterResponse {
        identity: r.identity,
        protocol_version: 2,
        capabilities: r.capabilities,
    })
}
async fn poll(State(shared): State<Shared>, Json(r): Json<PollRequest>) -> Json<PollResponse> {
    r.validate().unwrap();
    let mut peer = shared.lock().unwrap();
    peer.polls += 1;
    let directives = if peer.cancellation && peer.polls > 1 {
        let c = peer.coordinator.as_mut().unwrap();
        c.cancelled = true;
        vec![Directive {
            directive_id: Uuid::from_u128(55),
            attempt: c.offer.attempt.clone(),
            kind: DirectiveKind::Cancel,
        }]
    } else {
        vec![]
    };
    Json(PollResponse {
        server_time_ms: 100,
        offers: vec![peer.coordinator.as_ref().unwrap().offer.clone()],
        directives,
    })
}
async fn accept(
    State(shared): State<Shared>,
    Json(r): Json<AcceptRequest>,
) -> Result<Json<AcceptResponse>, (StatusCode, Json<ProtocolError>)> {
    let mut s = shared.lock().unwrap();
    s.accepts += 1;
    let result = s.coordinator.as_mut().unwrap().accept(&r);
    if let Ok(response) = &result {
        s.last_grant = Some(response.grant.clone());
    }
    result
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, Json(e)))
}
async fn observe(
    State(shared): State<Shared>,
    Json(r): Json<Observation>,
) -> Result<Json<Ack>, (StatusCode, Json<ProtocolError>)> {
    let mut s = shared.lock().unwrap();
    s.observations += 1;
    if r.phase == Phase::Reporting {
        s.post_stop_observation_requests.push(r.clone());
        if s.post_stop_observation_failures > 0 {
            s.post_stop_observation_failures -= 1;
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ProtocolError {
                    code: ErrorCode::Overloaded,
                    message: "injected post-stop observation outage".into(),
                    retryable: true,
                    operation_id: Some(r.operation_id),
                }),
            ));
        }
    }
    s.coordinator
        .as_mut()
        .unwrap()
        .observe(&r)
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, Json(e)))
}
async fn report(
    State(shared): State<Shared>,
    Json(r): Json<ResultReport>,
) -> Result<Json<ResultResponse>, (StatusCode, Json<ProtocolError>)> {
    let mut s = shared.lock().unwrap();
    s.reports += 1;
    s.last_report = Some(r.clone());
    s.coordinator
        .as_mut()
        .unwrap()
        .report(&r)
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, Json(e)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_executes_real_child_against_fake_http_peer_with_lost_responses() {
    let shared = Arc::new(Mutex::new(Peer::default()));
    let app = Router::new()
        .route("/v2/worker/register", post(register))
        .route("/v2/worker/poll", post(poll))
        .route("/v2/worker/accept", post(accept))
        .route("/v2/worker/observe", post(observe))
        .route("/v2/worker/result", post(report))
        .route("/v2/worker/renew", post(renew))
        .route("/v2/worker/directive-ack", post(directive_ack))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::TempDir::new().unwrap();
    let cfg = WorkerConfig {
        server_url: base,
        node_id: Uuid::from_u128(1),
        credential: "fixture-node-secret".into(),
        cf_client_id: None,
        cf_client_secret: None,
    };
    let path = temp.path().join("worker.toml");
    cfg.save(&path).unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(&path)
        .arg("run-execution")
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .arg("--fixture-agent")
        .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"))
        .args([
            "--exit-after-result",
            "--drop-acceptance-response-once",
            "--drop-report-response-once",
            "--idle-timeout-secs",
            "5",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let s = shared.lock().unwrap();
    assert_eq!(s.accepts, 2);
    assert_eq!(s.reports, 2);
    assert_eq!(s.observations, 2);
    let journal: aq_worker::journal::State =
        serde_json::from_slice(&std::fs::read(temp.path().join("state/journal.json")).unwrap())
            .unwrap();
    assert_eq!(journal.attempts.len(), 1);
    let attempt = journal.attempts.values().next().unwrap();
    assert_eq!(attempt.phase, Phase::Retired);
    assert_eq!(attempt.stop_certainty, StopCertainty::ConfirmedStopped);
    assert!(attempt.pending_observations.is_empty());
    assert_eq!(
        std::fs::read_dir(temp.path().join("state/launches"))
            .unwrap()
            .count(),
        0
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(output.contains("registered"));
    assert!(output.contains("result_acknowledged"));
    assert!(!output.contains("fixture-node-secret"));
    server.abort();
}

async fn renew(
    State(shared): State<Shared>,
    Json(_r): Json<RenewRequest>,
) -> Result<Json<RenewResponse>, (StatusCode, Json<ProtocolError>)> {
    let mut s = shared.lock().unwrap();
    s.renewals += 1;
    if s.expiry {
        // Semantic barrier: only the observed failed renewal advances both
        // fixture clocks through expiry; startup timing cannot skip coverage.
        if let Some(path) = &s.expiry_offset {
            aq_worker::guardian::durable_replace(path, b"100000").unwrap();
        }
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ProtocolError {
                code: ErrorCode::Overloaded,
                message: "injected renewal outage".into(),
                retryable: true,
                operation_id: None,
            }),
        ));
    }
    let grant = s.last_grant.as_mut().unwrap();
    grant.grant_sequence += 1;
    Ok(Json(RenewResponse {
        grant: grant.clone(),
    }))
}
async fn directive_ack(State(shared): State<Shared>, Json(r): Json<DirectiveAck>) -> Json<Ack> {
    shared.lock().unwrap().directive_acks += 1;
    Json(Ack {
        operation_id: r.operation_id,
    })
}

async fn blocked_fixture(cancellation: bool) {
    let shared = Arc::new(Mutex::new(Peer {
        cancellation,
        expiry: !cancellation,
        ..Peer::default()
    }));
    let app = Router::new()
        .route("/v2/worker/register", post(register))
        .route("/v2/worker/poll", post(poll))
        .route("/v2/worker/accept", post(accept))
        .route("/v2/worker/observe", post(observe))
        .route("/v2/worker/result", post(report))
        .route("/v2/worker/renew", post(renew))
        .route("/v2/worker/directive-ack", post(directive_ack))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("worker.toml");
    WorkerConfig {
        server_url: base,
        node_id: Uuid::from_u128(1),
        credential: "fixture-node-secret".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&path)
    .unwrap();
    let offset = temp.path().join("clock-offset");
    std::fs::write(&offset, "0").unwrap();
    if !cancellation {
        shared.lock().unwrap().expiry_offset = Some(offset.clone());
    }
    let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(&path)
        .arg("run-execution")
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .arg("--fixture-agent")
        .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"))
        .args(["--exit-after-result", "--idle-timeout-secs", "5"])
        .env("AQ_FIXTURE_HOLD", "1")
        .env("AQ_WORKER_CLOCK_OFFSET_FILE", &offset)
        .env("AQ_GUARDIAN_CLOCK_OFFSET_FILE", &offset)
        .kill_on_drop(true)
        .output();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), command)
        .await
        .expect("control failed to stop blocked fixture")
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let s = shared.lock().unwrap();
    let report = s.last_report.as_ref().unwrap();
    assert_eq!(
        report.outcome,
        if cancellation {
            ProcessOutcome::Cancelled
        } else {
            ProcessOutcome::Expired
        }
    );
    assert_eq!(report.stop_certainty, StopCertainty::ConfirmedStopped);
    assert!(!report.claimed_success);
    if cancellation {
        assert_eq!(s.directive_acks, 1);
    } else {
        assert!(s.renewals > 0);
    }
    let journal: aq_worker::journal::State =
        serde_json::from_slice(&std::fs::read(temp.path().join("state/journal.json")).unwrap())
            .unwrap();
    let attempt = journal.attempts.values().next().unwrap();
    assert_eq!(attempt.phase, Phase::Retired);
    assert!(aq_worker::fixture::process_evidence(attempt.process.as_ref().unwrap().pid).is_err());
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_fixture_is_killed_and_reaped_on_cancellation() {
    blocked_fixture(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_fixture_is_killed_and_reaped_on_grant_expiry() {
    blocked_fixture(false).await;
}

async fn reconcile(
    State(shared): State<Shared>,
    Json(r): Json<ReconcileRequest>,
) -> Json<ReconcileResponse> {
    let peer = shared.lock().unwrap();
    assert_eq!(
        peer.register_calls, 0,
        "old reports must recover before a new boot registers"
    );
    Json(ReconcileResponse {
        attempts: r
            .attempts
            .into_iter()
            .map(|a| ReconcileDisposition {
                attempt: a.attempt,
                action: ReconcileAction::AwaitReport,
                grant: None,
                result: None,
            })
            .collect(),
        next_cursor: None,
    })
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_report_recovers_before_new_boot_registration_without_relaunch() {
    use aq_worker::{
        clock::FakeClock,
        journal::{Journal, Limits},
        runtime::Runtime,
    };
    let temp = tempfile::TempDir::new().unwrap();
    let root = temp.path().join("state");
    let offer = sample_offer();
    let limits = Limits {
        capacity: Resources {
            slots: 1,
            cpu_millis: 10000,
            memory_bytes: 100000,
            disk_bytes: 100000,
        },
        max_pending_results: 16,
        max_pending_bytes: 4096,
        max_preparing: 16,
        minimum_free_disk_bytes: 0,
    };
    let journal = Journal::open(&root, offer.identity.clone(), limits).unwrap();
    let mut runtime = Runtime::new(journal, FakeClock::new(100));
    let mut peer = FakeCoordinator::new(offer.clone(), 100);
    let accept = runtime.reserve(offer).unwrap();
    let grant = peer.accept(&accept).unwrap().grant;
    runtime.authorize(grant, 100).unwrap();
    let handle = runtime
        .launch_fixture(
            accept.attempt.attempt_id,
            std::path::Path::new(env!("CARGO_BIN_EXE_aq-fixture-agent")),
        )
        .unwrap();
    let report_payload = runtime
        .finish_fixture(accept.attempt.attempt_id, handle)
        .unwrap();
    drop(runtime);
    let shared = Arc::new(Mutex::new(Peer {
        coordinator: Some(peer),
        ..Peer::default()
    }));
    let app = Router::new()
        .route("/v2/worker/register", post(register))
        .route("/v2/worker/reconcile", post(reconcile))
        .route("/v2/worker/result", post(report))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let path = temp.path().join("worker.toml");
    WorkerConfig {
        server_url: base,
        node_id: Uuid::from_u128(1),
        credential: "fixture-node-secret".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&path)
    .unwrap();
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(&path)
        .arg("run-execution")
        .arg("--state-dir")
        .arg(&root)
        .arg("--fixture-agent")
        .arg("/must-not-launch")
        .arg("--exit-after-result")
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let peer = shared.lock().unwrap();
    assert_eq!(peer.register_calls, 0);
    assert_eq!(peer.reports, 1);
    assert_eq!(peer.last_report.as_ref(), Some(&report_payload));
    let state: aq_worker::journal::State =
        serde_json::from_slice(&std::fs::read(root.join("journal.json")).unwrap()).unwrap();
    assert_eq!(state.attempts.len(), 1);
    assert_eq!(
        state.attempts.values().next().unwrap().phase,
        Phase::Retired
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_stop_observation_outage_preserves_and_replays_before_report() {
    let shared = Arc::new(Mutex::new(Peer {
        post_stop_observation_failures: 1,
        ..Peer::default()
    }));
    let app = Router::new()
        .route("/v2/worker/register", post(register))
        .route("/v2/worker/poll", post(poll))
        .route("/v2/worker/accept", post(accept))
        .route("/v2/worker/observe", post(observe))
        .route("/v2/worker/result", post(report))
        .route("/v2/worker/renew", post(renew))
        .route("/v2/worker/directive-ack", post(directive_ack))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("worker.toml");
    WorkerConfig {
        server_url: base,
        node_id: Uuid::from_u128(1),
        credential: "fixture-node-secret".into(),
        cf_client_id: None,
        cf_client_secret: None,
    }
    .save(&path)
    .unwrap();
    let state = temp.path().join("state");
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-worker"))
        .arg("--config")
        .arg(&path)
        .arg("run-execution")
        .arg("--state-dir")
        .arg(&state)
        .arg("--fixture-agent")
        .arg(env!("CARGO_BIN_EXE_aq-fixture-agent"))
        .args(["--exit-after-result", "--idle-timeout-secs", "5"])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let peer = shared.lock().unwrap();
    assert_eq!(peer.post_stop_observation_requests.len(), 2);
    assert_eq!(
        peer.post_stop_observation_requests[0],
        peer.post_stop_observation_requests[1]
    );
    assert_eq!(peer.reports, 1);
    assert_eq!(peer.accepts, 1);
    assert_eq!(peer.renewals, 0);
    let journal: aq_worker::journal::State =
        serde_json::from_slice(&std::fs::read(state.join("journal.json")).unwrap()).unwrap();
    let r = journal.attempts.values().next().unwrap();
    assert_eq!(r.phase, Phase::Retired);
    assert_eq!(r.stop_certainty, StopCertainty::ConfirmedStopped);
    assert!(r.pending_observations.is_empty());
    assert!(r.stopped_proof.as_ref().unwrap().kernel_wait_echild);
    server.abort();
}
