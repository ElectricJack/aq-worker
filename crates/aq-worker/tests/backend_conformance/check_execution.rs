use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::watch;
use uuid::Uuid;

use aq_worker::backend::{
    FakeBackend, PrivateMaterial, ResourceLimits, RunnerExit, SandboxState, TemplateDigest,
};
use aq_worker::backend::{PlacementBackend as _, SandboxIdentity};
use aq_worker::checks::client::{CheckCallError, CheckTransport};
use aq_worker::checks::journal::{Delivery, Journal, Record};
use aq_worker::checks::runner::{CheckRunner, CheckpointSource, RunnerConfig, ValidatorTemplate};
use aq_worker::checks::*;

#[path = "../support/check_claim.rs"]
mod fixture;
use fixture::{NODE, claim_json, validator};
const TEMPLATE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
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

fn runner(
    dir: &std::path::Path,
    backend: FakeBackend,
    transport: &FakeTransport,
    source: Option<&[u8]>,
) -> CheckRunner<FakeBackend, FakeTransport, FixedSource> {
    CheckRunner::new(
        backend,
        transport.clone(),
        FixedSource(source.map(<[u8]>::to_vec)),
        dir,
        config(60_000),
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
    let mut runner = CheckRunner::new(
        FakeBackend::default(),
        transport.clone(),
        FixedSource(Some(b"b".to_vec())),
        dir.path(),
        config(0),
        NODE,
    )
    .unwrap();
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
