//! Real loopback HTTP, frozen 0.1.9 fixtures; not daemon or durable driver qualification.
use aq_admission::{
    preparation_v2::{AcceptRequest, AcceptedDelivery, AcceptedReceipt, Document, Proposal},
    transport::DeliveryClock,
};
use aq_worker::{
    config::WorkerConfig,
    native::http::{NativeClient, preparation_v2::ProposedPreparation},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Semaphore;
use uuid::Uuid;

fn fixture() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-admission-0.1.9/artifacts/preparation-v2.fixture.json"
    ))
    .unwrap()
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
struct Peer {
    mode: AtomicUsize,
    sends: AtomicUsize,
    nonces: Mutex<Vec<Uuid>>,
    entered: Semaphore,
    release: Semaphore,
}
async fn handler(
    State(peer): State<Arc<Peer>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer synthetic-phase-machine");
    peer.sends.fetch_add(1, Ordering::SeqCst);
    let nonce = Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
    {
        let mut nonces = peer.nonces.lock().unwrap();
        assert!(!nonces.contains(&nonce));
        nonces.push(nonce);
    }
    let data = fixture();
    let mode = peer.mode.load(Ordering::SeqCst);
    if mode == 8 {
        peer.entered.add_permits(1);
        peer.release.acquire().await.unwrap().forget();
    }
    if mode == 6 {
        return (
            StatusCode::OK,
            "x".repeat(aq_admission::preparation_v2::MAX_BYTES + 1),
        )
            .into_response();
    }
    if mode == 7 {
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/must-not-follow")],
        )
            .into_response();
    }
    let mut response = if path == "proposal" {
        assert_eq!(
            body,
            json!({"identity":data["accept"]["original"]["identity"],"binding":data["proposal"]["original"]["offered"]["offer"]["binding"],"negotiation":data["proposal"]["negotiation"]})
        );
        json!({"kind":"proposal","body":data["proposal"]})
    } else {
        assert_eq!(path, "accept");
        assert_eq!(body, json!({"kind":"accept","body":data["accept"]}));
        let receipt: AcceptedReceipt = serde_json::from_value(data["accepted"].clone()).unwrap();
        let clock =
            DeliveryClock::new(receipt.lease.issued_at_ms, receipt.lease.expires_at_ms).unwrap();
        serde_json::to_value(Document::AcceptedDelivery(Box::new(AcceptedDelivery {
            schema_version: 2,
            receipt,
            delivery: aq_admission::control::DeliveryStamp { nonce, clock },
        })))
        .unwrap()
    };
    match mode {
        1 if path == "accept" => response["body"]["delivery"]["nonce"] = json!(Uuid::new_v4()),
        2 => response = json!({"kind":"execution_delivery","body":data["delivery"]}),
        3 => response["unexpected"] = json!(true),
        4 if path == "accept" => {
            response["body"]["receipt"]["lease"]["lease_id"] = json!(Uuid::new_v4())
        }
        5 if path == "accept" => {
            response["body"]["delivery"]["clock"] =
                serde_json::to_value(DeliveryClock::new(31499, 31500).unwrap()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        9 if path == "proposal" => {
            response["body"]["original"]["offered"]["clock"]["server_time_ms"] = json!(1499)
        }
        _ => {}
    }
    Json(response).into_response()
}
struct Server {
    peer: Arc<Peer>,
    client: NativeClient,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new() -> Self {
        let peer = Arc::new(Peer {
            mode: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            nonces: Mutex::new(vec![]),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let q: AcceptRequest = serde_json::from_value(fixture()["accept"].clone()).unwrap();
        let client = NativeClient::from_config(&WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id: q.original.identity.node_id,
            credential: "synthetic-phase-machine".into(),
            cf_client_id: None,
            cf_client_secret: None,
        })
        .unwrap();
        let app = Router::new()
            .route("/v1/worker/profiled-v2/{path}", post(handler))
            .with_state(peer.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { peer, client, task }
    }
    async fn propose(&self) -> ProposedPreparation {
        let p: Proposal = serde_json::from_value(fixture()["proposal"].clone()).unwrap();
        let q: AcceptRequest = serde_json::from_value(fixture()["accept"].clone()).unwrap();
        self.client
            .propose_preparation(
                &q.original.identity,
                &p.original,
                &p.negotiation,
                boot_ns() + 500_000_000,
            )
            .await
            .unwrap()
    }
    fn request(&self) -> AcceptRequest {
        serde_json::from_value(fixture()["accept"].clone()).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn actual_accepted_delivery_outlasts_proposal_and_exact_retry_never_extends_it() {
    let s = Server::new().await;
    let p = s.propose().await;
    let q = s.request();
    let a = s.client.accept_preparation(&p, &q, None).await.unwrap();
    assert!(a.deadline_ns() > p.original_deadline_ns() + 20_000_000_000);
    assert_eq!(a.request_digest(), &q.digest().unwrap());
    assert_eq!(a.proposal_digest(), &p.proposal().digest().unwrap());
    assert!(a.received_ns() >= a.sent_ns());
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    let sends = s.peer.sends.load(Ordering::SeqCst);
    assert!(s.client.accept_preparation(&p, &q, None).await.is_err());
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), sends);
    let again = s.client.accept_preparation(&p, &q, Some(&a)).await.unwrap();
    assert!(again.deadline_ns() <= a.deadline_ns());
    assert_eq!(
        serde_json::to_value(&again.delivery().receipt).unwrap(),
        serde_json::to_value(&a.delivery().receipt).unwrap()
    );
    let sends = s.peer.sends.load(Ordering::SeqCst);
    let mut changed = q.clone();
    changed.original.operation_id = Uuid::new_v4();
    assert!(
        s.client
            .accept_preparation(&p, &changed, Some(&a))
            .await
            .is_err()
    );
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), sends);
}

#[tokio::test]
async fn malformed_wrong_nonce_changed_receipt_late_and_oversized_responses_refuse() {
    let s = Server::new().await;
    let p = s.propose().await;
    let q = s.request();
    let a = s.client.accept_preparation(&p, &q, None).await.unwrap();
    for mode in [1, 2, 3, 4, 5, 6, 7] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client.accept_preparation(&p, &q, Some(&a)).await.is_err(),
            "mode {mode}"
        );
    }
}

#[tokio::test]
async fn expired_original_scope_and_bad_negotiation_refuse_before_send() {
    let s = Server::new().await;
    let p: Proposal = serde_json::from_value(fixture()["proposal"].clone()).unwrap();
    let q = s.request();
    assert!(
        s.client
            .propose_preparation(
                &q.original.identity,
                &p.original,
                &p.negotiation,
                boot_ns() - 1
            )
            .await
            .is_err()
    );
    let mut identity = q.original.identity.clone();
    identity.boot_id = Uuid::new_v4();
    assert!(
        s.client
            .propose_preparation(
                &identity,
                &p.original,
                &p.negotiation,
                boot_ns() + 500_000_000
            )
            .await
            .is_err()
    );
    let mut bad = p.negotiation.clone();
    bad.features.clear();
    assert!(
        s.client
            .propose_preparation(
                &q.original.identity,
                &p.original,
                &bad,
                boot_ns() + 500_000_000
            )
            .await
            .is_err()
    );
    assert_eq!(s.peer.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changed_or_unexpected_proposal_cannot_refresh_historical_offer() {
    let s = Server::new().await;
    let p: Proposal = serde_json::from_value(fixture()["proposal"].clone()).unwrap();
    let q = s.request();
    for mode in [2, 3, 6, 7, 9] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client
                .propose_preparation(
                    &q.original.identity,
                    &p.original,
                    &p.negotiation,
                    boot_ns() + 500_000_000
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn cancelling_a_waiter_releases_the_bounded_control_slot() {
    let s = Server::new().await;
    let p = Arc::new(s.propose().await);
    let q = s.request();
    s.peer.mode.store(8, Ordering::SeqCst);
    let client = s.client.clone();
    let pp = p.clone();
    let qq = q.clone();
    let task = tokio::spawn(async move { client.accept_preparation(&pp, &qq, None).await });
    s.peer.entered.acquire().await.unwrap().forget();
    task.abort();
    assert!(matches!(task.await,Err(e) if e.is_cancelled()));
    s.peer.mode.store(0, Ordering::SeqCst);
    s.peer.release.add_permits(1);
    assert!(s.client.accept_preparation(&p, &q, None).await.is_ok());
}

#[tokio::test]
async fn delayed_proposal_does_not_resample_the_original_clock() {
    let s = Server::new().await;
    let p: Proposal = serde_json::from_value(fixture()["proposal"].clone()).unwrap();
    let q = s.request();
    s.peer.mode.store(8, Ordering::SeqCst);
    let client = s.client.clone();
    let task = tokio::spawn(async move {
        client
            .propose_preparation(
                &q.original.identity,
                &p.original,
                &p.negotiation,
                boot_ns() + 500_000_000,
            )
            .await
    });
    s.peer.entered.acquire().await.unwrap().forget();
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    s.peer.release.add_permits(1);
    assert!(task.await.unwrap().is_err());
}
