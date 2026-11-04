#[path = "support/sandbox_runner.rs"]
#[allow(dead_code)]
mod support;
use aq_worker::native::runner::sandbox::{
    auth::RunnerHttp,
    mail::{AttemptInput, Delivery, Injection, MailLoop, SafeBoundary},
    private,
};
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use uuid::Uuid;

#[derive(Default)]
struct Mail {
    batch: Mutex<Vec<Value>>,
    acks: Mutex<Vec<Value>>,
    refuse_ack: AtomicBool,
}
fn fixture(name: &str) -> Value {
    let all: Value = serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-agent-api-0.1.0/artifacts/fixtures/messaging.json"
    ))
    .unwrap();
    all.as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == name)
        .unwrap()["value"]
        .clone()
}
fn delivery(n: u128, digest: char) -> Value {
    let mut d = fixture("node-batch")["deliveries"][0].clone();
    d["delivery_id"] = json!(Uuid::from_u128(0xd00 + n));
    d["message_id"] = json!(Uuid::from_u128(0x500 + n));
    d["cursor"] = json!(n.to_string());
    d["payload_digest"] = json!(digest.to_string().repeat(64));
    d
}
fn id(n: u128) -> Uuid {
    Uuid::from_u128(0xd00 + n)
}
async fn poll(State(m): State<Arc<Mail>>, Json(request): Json<Value>) -> Json<Value> {
    assert!(aq_agent_client::mail::valid("MessageNodePoll", &request));
    Json(json!({"api_version":1,"attempt":request["attempt"],
        "deliveries":*m.batch.lock().unwrap()}))
}
async fn ack(State(m): State<Arc<Mail>>, Json(request): Json<Value>) -> axum::response::Response {
    assert!(aq_agent_client::mail::valid("MessageNodeAck", &request));
    m.acks.lock().unwrap().push(request.clone());
    if m.refuse_ack.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(
        json!({"api_version":1,"operation_id":request["operation_id"],
        "delivery_id":request["delivery_id"],"attempt":request["attempt"],
        "disposition":request["disposition"]}),
    )
    .into_response()
}
struct Rig {
    f: support::Fixture,
    http: RunnerHttp,
    mail: Arc<Mail>,
    server: tokio::task::JoinHandle<()>,
}
impl Rig {
    async fn new() -> Self {
        let f = support::Fixture::new(false, 5000);
        let files = private::RunnerFiles::create(&f.dir.path().join("private")).unwrap();
        files
            .write_secret("permit.json", &serde_json::to_vec(&f.permit).unwrap())
            .unwrap();
        let mail = Arc::new(Mail::default());
        let router = Router::new()
            .route("/v2/worker/messages/poll", post(poll))
            .route("/v2/worker/messages/ack", post(ack))
            .with_state(mail.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let http = RunnerHttp::for_execution(&origin, &files.root().join("permit.json"), &f.frozen)
            .unwrap();
        Self {
            f,
            http,
            mail,
            server,
        }
    }
    fn open(&self) -> MailLoop {
        MailLoop::open(self.http.clone(), &self.f.dir.path().join("mail")).unwrap()
    }
    fn serve(&self, batch: Vec<Value>) {
        *self.mail.batch.lock().unwrap() = batch;
    }
    fn acks(&self) -> Vec<(Value, Value)> {
        let acks = self.mail.acks.lock().unwrap();
        acks.iter()
            .map(|a| (a["delivery_id"].clone(), a["disposition"].clone()))
            .collect()
    }
}
/// A qualified boundary stand-in recording every injection it is handed.
struct Sink {
    ready: bool,
    outcome: Injection,
    seen: Vec<Value>,
}
impl Sink {
    fn new() -> Self {
        Self {
            ready: true,
            outcome: Injection::Handed,
            seen: vec![],
        }
    }
    fn ids(&self) -> Vec<Value> {
        self.seen.iter().map(|d| d["delivery_id"].clone()).collect()
    }
}
impl SafeBoundary for Sink {
    fn ready(&mut self) -> bool {
        self.ready
    }
    fn inject(&mut self, delivery: &Value) -> Injection {
        self.seen.push(delivery.clone());
        self.outcome
    }
}

#[tokio::test]
async fn message_delivered_once_across_runner_restart() {
    let rig = Rig::new().await;
    let mut sink = Sink::new();
    let native = AttemptInput::Accepting;
    // Loop A hands D1 to the qualified boundary exactly once.
    let mut a = rig.open();
    rig.serve(vec![delivery(1, 'a')]);
    let acked = a
        .poll_once(native, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(1), "handed_to_harness")]);
    assert_eq!(sink.ids(), vec![json!(id(1))]);
    assert_eq!(sink.seen[0]["body"], "Review é界🙂 safely.");
    drop(a);

    // Runner restart: the server replays D1 (received_by_node) alongside D2.
    let b = rig.open();
    let mut replayed = delivery(1, 'a');
    replayed["disposition"] = json!("received_by_node");
    rig.serve(vec![replayed.clone(), delivery(2, 'a')]);
    let mut b = b;
    let acked = b
        .poll_once(native, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(2), "handed_to_harness")]);
    assert_eq!(sink.ids(), vec![json!(id(1)), json!(id(2))]);

    // A crash after the injection intent and before its outcome is never
    // retried: it is acked delivery_unknown without touching the boundary.
    b.record_intent(id(3), Uuid::from_u128(0x503), &"a".repeat(64))
        .unwrap();
    rig.serve(vec![replayed.clone(), delivery(2, 'a'), delivery(3, 'a')]);
    let acked = b
        .poll_once(native, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(3), "delivery_unknown")]);
    assert_eq!(sink.seen.len(), 2);

    // A full replay of journaled, acked deliveries delivers and acks nothing.
    let acked = b
        .poll_once(native, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert!(acked.is_empty());
    assert_eq!(sink.seen.len(), 2);
    assert_eq!(rig.acks().len(), 3);

    // A lost ack response: the outcome is journaled, so the restarted loop
    // re-acks the same operation and disposition instead of re-delivering.
    rig.mail.refuse_ack.store(true, Ordering::SeqCst);
    rig.serve(vec![delivery(4, 'a')]);
    assert!(
        b.poll_once(native, Delivery::Native(&mut sink))
            .await
            .is_err()
    );
    assert_eq!(sink.seen.len(), 3);
    rig.mail.refuse_ack.store(false, Ordering::SeqCst);
    drop(b);
    let mut c = rig.open();
    let acked = c
        .poll_once(native, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(4), "handed_to_harness")]);
    assert_eq!(sink.seen.len(), 3);
    let acks = rig.mail.acks.lock().unwrap().clone();
    assert_eq!(acks[3]["operation_id"], acks[4]["operation_id"]);
    assert_eq!(acks[4]["disposition"], "handed_to_harness");

    // The same delivery_id replayed with a different digest is refused and
    // nothing in that batch is delivered or acked.
    rig.serve(vec![delivery(5, 'a'), delivery(1, 'b')]);
    assert!(
        c.poll_once(native, Delivery::Native(&mut sink))
            .await
            .is_err()
    );
    assert_eq!(sink.seen.len(), 3);
    assert_eq!(rig.acks().len(), 5);
    rig.server.abort();
}

#[tokio::test]
async fn mail_inbox_mode_and_rejecting_attempt() {
    let rig = Rig::new().await;
    let mut mail = rig.open();
    // Inbox mode: the agent reads it with `aq-agent message inbox`.
    rig.serve(vec![delivery(1, 'a')]);
    let acked = mail
        .poll_once(AttemptInput::Accepting, Delivery::Inbox)
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(1), "received_by_node")]);
    // A stopped or suspending attempt never injects new mail.
    let mut sink = Sink::new();
    rig.serve(vec![delivery(2, 'a')]);
    let acked = mail
        .poll_once(AttemptInput::Rejecting, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(2), "stale")]);
    // A boundary that is not ready defers: receipt only, no intent, and the
    // next replay at a ready boundary hands it over once.
    sink.ready = false;
    rig.serve(vec![delivery(3, 'a')]);
    let acked = mail
        .poll_once(AttemptInput::Accepting, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(3), "received_by_node")]);
    let acked = mail
        .poll_once(AttemptInput::Accepting, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert!(acked.is_empty(), "receipt is acked once while deferred");
    sink.ready = true;
    let acked = mail
        .poll_once(AttemptInput::Accepting, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(3), "handed_to_harness")]);
    // An uncertain injection is reported once and never retried.
    sink.outcome = Injection::Unknown;
    rig.serve(vec![delivery(3, 'a'), delivery(4, 'a')]);
    let acked = mail
        .poll_once(AttemptInput::Accepting, Delivery::Native(&mut sink))
        .await
        .unwrap();
    assert_eq!(acked, vec![(id(4), "delivery_unknown")]);
    assert_eq!(sink.ids(), vec![json!(id(3)), json!(id(4))]);
    assert!(sink.seen.iter().all(|d| d["delivery_id"] != json!(id(1))));
    rig.server.abort();
}
