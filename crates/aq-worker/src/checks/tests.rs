use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use super::client::{CheckCallError, CheckClient, CheckTransport};
use super::journal::{Delivery, Journal, Record};
use super::runner::{CheckRunner, CheckpointSource, RunnerConfig, ValidatorTemplate};
use super::*;
use crate::backend::{
    FakeBackend, PrivateMaterial, ResourceLimits, RunnerExit, SandboxState, TemplateDigest,
};
use crate::backend::{PlacementBackend as _, SandboxIdentity};

const VALIDATOR_DIGEST: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TEMPLATE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn validator() -> ArtifactPin {
    ArtifactPin {
        id: "cargo-test".into(),
        version: "1".into(),
        digest: VALIDATOR_DIGEST.into(),
    }
}

fn claim_json(operation: Uuid) -> Value {
    json!({
        "operation_id": operation,
        "attempt_id": Uuid::new_v4(),
        "source_operation_id": Uuid::from_u128(7),
        "subject": {"id": "task-1", "revision": 3},
        "check": {"id": "unit", "validator": validator()},
        "source": {"format": "sha1", "hex": "0123456789abcdef0123456789abcdef01234567"},
        "execution_profile": EXECUTION_PROFILE,
    })
}

#[derive(Clone, Default)]
struct FakeTransport(Arc<Inner>);
#[derive(Default)]
struct Inner {
    claims: Mutex<VecDeque<Vec<Value>>>,
    replies: Mutex<VecDeque<Result<(), CheckCallError>>>,
    sent: Mutex<Vec<Vec<u8>>>,
}
impl FakeTransport {
    fn queue_claim(&self, entries: Vec<Value>) {
        self.0.claims.lock().unwrap().push_back(entries);
    }
    fn queue_reply(&self, reply: Result<(), CheckCallError>) {
        self.0.replies.lock().unwrap().push_back(reply);
    }
    fn sent(&self) -> Vec<Value> {
        let sent = self.0.sent.lock().unwrap();
        sent.iter()
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect()
    }
    fn sent_raw(&self) -> Vec<Vec<u8>> {
        self.0.sent.lock().unwrap().clone()
    }
}
impl CheckTransport for FakeTransport {
    async fn claim(&self, _limit: u32) -> Result<Vec<Value>, CheckCallError> {
        Ok(self
            .0
            .claims
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default())
    }
    async fn stop(&self, body: &[u8]) -> Result<(), CheckCallError> {
        self.0.sent.lock().unwrap().push(body.to_vec());
        self.0.replies.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }
}

struct FixedSource(Option<Vec<u8>>);
impl CheckpointSource for FixedSource {
    fn materialize(&self, _claim: &CheckClaim) -> anyhow::Result<Option<PrivateMaterial>> {
        Ok(self.0.clone().map(PrivateMaterial::new))
    }
}

fn config(timeout_ms: u64) -> RunnerConfig {
    RunnerConfig {
        validators: vec![ValidatorTemplate {
            validator: validator(),
            template: TemplateDigest::parse(TEMPLATE).unwrap(),
        }],
        timeout_ms,
        poll_interval_ms: 0,
        claim_interval_ms: 0,
        claim_limit: 1,
        resources: ResourceLimits {
            cpu_millis: 1000,
            memory_bytes: 64 * 1024 * 1024,
            pids: 64,
            disk_bytes: None,
        },
    }
}

const NODE: Uuid = Uuid::from_u128(0x11);

fn runner(
    dir: &std::path::Path,
    backend: FakeBackend,
    transport: &FakeTransport,
    source: Option<&[u8]>,
    timeout_ms: u64,
) -> CheckRunner<FakeBackend, FakeTransport, FixedSource> {
    CheckRunner::new(
        backend,
        transport.clone(),
        FixedSource(source.map(<[u8]>::to_vec)),
        dir,
        config(timeout_ms),
        NODE,
    )
    .unwrap()
}

fn exiting(exit: RunnerExit) -> FakeBackend {
    let mut backend = FakeBackend::default();
    backend.exit_runners_on_start(exit);
    backend
}

fn idle() -> watch::Receiver<bool> {
    watch::channel(false).1
}

async fn sandbox_state(
    runner: &mut CheckRunner<FakeBackend, FakeTransport, FixedSource>,
    sandbox: &SandboxIdentity,
) -> SandboxState {
    runner.backend_mut().inspect(sandbox).await.unwrap().state
}

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

#[tokio::test]
async fn exited_zero_reports_complete_inputs_then_releases_the_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    let op = Uuid::from_u128(0x100);
    transport.queue_claim(vec![claim_json(op)]);
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Exited(0)),
        &transport,
        Some(b"bundle"),
        60_000,
    );
    assert_eq!(runner.tick(&idle()).await.unwrap(), 1);
    let sent = transport.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["stop"], json!({"kind": "exited", "code": 0}));
    assert_eq!(sent[0]["inputs_complete"], json!(true));
    assert_eq!(
        sent[0]["inputs_digest"],
        json!(inputs_digest(&claim_json(op)))
    );
    let record = runner.journal().load(op).unwrap().unwrap();
    assert_eq!(record.delivery, Delivery::Recorded);
    assert!(record.released);
    let sandbox = record.sandbox.unwrap();
    assert_eq!(sandbox.owner.attempt_id, op);
    assert_eq!(sandbox.owner.node_id, NODE);
    assert_eq!(
        sandbox_state(&mut runner, &sandbox).await,
        SandboxState::Destroyed
    );
}

#[tokio::test]
async fn nonzero_exit_is_reported_as_its_code() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    transport.queue_claim(vec![claim_json(Uuid::from_u128(0x101))]);
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Exited(3)),
        &transport,
        Some(b"b"),
        60_000,
    );
    runner.tick(&idle()).await.unwrap();
    let sent = transport.sent();
    assert_eq!(sent[0]["stop"], json!({"kind": "exited", "code": 3}));
    assert_eq!(sent[0]["inputs_complete"], json!(true));
}

#[tokio::test]
async fn signalled_runner_is_reported_signalled_and_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    transport.queue_claim(vec![claim_json(Uuid::from_u128(0x102))]);
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Signalled),
        &transport,
        Some(b"b"),
        60_000,
    );
    runner.tick(&idle()).await.unwrap();
    assert_eq!(transport.sent()[0]["stop"], json!({"kind": "signalled"}));
    assert_eq!(transport.sent()[0]["inputs_complete"], json!(false));
}

#[tokio::test]
async fn unknown_validator_or_missing_source_is_lost_without_a_sandbox() {
    for (mutate, source) in [(true, Some(&b"b"[..])), (false, None)] {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::default();
        let op = Uuid::new_v4();
        let mut claim = claim_json(op);
        if mutate {
            claim["check"]["validator"]["version"] = json!("2");
        }
        transport.queue_claim(vec![claim]);
        let mut runner = runner(
            dir.path(),
            exiting(RunnerExit::Exited(0)),
            &transport,
            source,
            60_000,
        );
        runner.tick(&idle()).await.unwrap();
        let sent = transport.sent();
        assert_eq!(sent[0]["stop"], json!({"kind": "lost"}));
        assert_eq!(sent[0]["inputs_complete"], json!(false));
        let record = runner.journal().load(op).unwrap().unwrap();
        assert!(record.sandbox.is_none(), "no sandbox was created");
        assert!(
            runner
                .backend_mut()
                .list_owned(NODE)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn malformed_claim_with_an_identity_is_stopped_lost() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    let op = Uuid::from_u128(0x103);
    let mut claim = claim_json(op);
    claim["subject"] = json!("not-an-object");
    transport.queue_claim(vec![claim, json!({"no": "identity"})]);
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Exited(0)),
        &transport,
        Some(b"b"),
        60_000,
    );
    assert_eq!(runner.tick(&idle()).await.unwrap(), 2);
    let sent = transport.sent();
    assert_eq!(
        sent.len(),
        1,
        "an entry without an operation cannot be stopped"
    );
    assert_eq!(sent[0]["operation_id"], json!(op));
    assert_eq!(sent[0]["stop"], json!({"kind": "lost"}));
}

#[tokio::test]
async fn validator_past_its_deadline_is_timed_out_and_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    let op = Uuid::from_u128(0x104);
    transport.queue_claim(vec![claim_json(op)]);
    // The fake runner keeps running; a zero timeout expires at the first poll.
    let mut runner = runner(
        dir.path(),
        FakeBackend::default(),
        &transport,
        Some(b"b"),
        0,
    );
    runner.tick(&idle()).await.unwrap();
    assert_eq!(transport.sent()[0]["stop"], json!({"kind": "timed_out"}));
    let record = runner.journal().load(op).unwrap().unwrap();
    assert!(record.released);
}

#[tokio::test]
async fn transient_stop_failure_resends_the_identical_body() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    let op = Uuid::from_u128(0x105);
    transport.queue_claim(vec![claim_json(op)]);
    transport.queue_reply(Err(CheckCallError::Transient("reset".into())));
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Exited(0)),
        &transport,
        Some(b"b"),
        60_000,
    );
    runner.tick(&idle()).await.unwrap();
    let record = runner.journal().load(op).unwrap().unwrap();
    assert_eq!(record.delivery, Delivery::Pending);
    assert!(
        !record.released,
        "sandbox retained until the stop is recorded"
    );
    // The server re-offers the still-running attempt; it is resent, not rerun.
    transport.queue_claim(vec![claim_json(op)]);
    runner.tick(&idle()).await.unwrap();
    let raw = transport.sent_raw();
    assert_eq!(raw.len(), 2);
    assert_eq!(raw[0], raw[1], "identical repeat");
    let record = runner.journal().load(op).unwrap().unwrap();
    assert_eq!(record.delivery, Delivery::Recorded);
    assert!(record.released);
}

#[tokio::test]
async fn conflicting_stop_is_never_retried_and_keeps_the_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let transport = FakeTransport::default();
    let op = Uuid::from_u128(0x106);
    transport.queue_claim(vec![claim_json(op)]);
    transport.queue_reply(Err(CheckCallError::Conflict));
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Exited(1)),
        &transport,
        Some(b"retained-source"),
        60_000,
    );
    runner.tick(&idle()).await.unwrap();
    transport.queue_claim(vec![claim_json(op)]);
    runner.tick(&idle()).await.unwrap();
    assert_eq!(transport.sent().len(), 1);
    let record = runner.journal().load(op).unwrap().unwrap();
    assert!(matches!(record.delivery, Delivery::Refused { .. }));
    let sandbox = record.sandbox.unwrap();
    assert_eq!(
        runner.backend().delivered_material(&sandbox),
        Some(&b"retained-source"[..]),
        "the retained checkpoint was delivered and the sandbox is kept"
    );
}

#[tokio::test]
async fn restart_mid_run_reports_lost_and_never_reruns() {
    let dir = tempfile::tempdir().unwrap();
    let op = Uuid::from_u128(0x107);
    {
        // A previous process recorded the claim but crashed before its stop.
        let journal = Journal::open(dir.path()).unwrap();
        journal
            .save(&Record::new(op, inputs_digest(&claim_json(op))))
            .unwrap();
    }
    let transport = FakeTransport::default();
    transport.queue_claim(vec![claim_json(op)]);
    let mut runner = runner(
        dir.path(),
        exiting(RunnerExit::Exited(0)),
        &transport,
        Some(b"b"),
        60_000,
    );
    runner.tick(&idle()).await.unwrap();
    let sent = transport.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["stop"], json!({"kind": "lost"}));
    assert!(
        runner
            .backend_mut()
            .list_owned(NODE)
            .await
            .unwrap()
            .is_empty(),
        "not rerun"
    );
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
