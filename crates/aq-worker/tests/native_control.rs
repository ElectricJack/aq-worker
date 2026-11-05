//! Actual loopback transport over published fixtures, not daemon authorization.
use aq_admission::{startup::NativeOffer, transport::*};
use aq_worker::{
    config::WorkerConfig,
    native::http::{
        NativeClient,
        control::{Granted, Preparation},
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::post,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

fn fixture() -> Value {
    serde_json::from_slice(include_bytes!(
        "../../../vendor/aq-admission-0.1.7/artifacts/control.fixture.json"
    ))
    .unwrap()
}
fn request<T: DeserializeOwned>(key: &str) -> T {
    serde_json::from_value(fixture()["requests"][key].clone()).unwrap()
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
    fixture: Value,
    mode: AtomicUsize,
    nonces: Mutex<BTreeSet<Uuid>>,
    stall: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
}
async fn handler(
    State(peer): State<Arc<Peer>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(headers["authorization"], "Bearer synthetic-machine");
    if path == "artifact" {
        peer.entered.add_permits(1);
        peer.release.acquire().await.unwrap().forget();
        return Json(json!({}));
    }
    let nonce = Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
    assert!(!nonce.is_nil());
    assert!(
        peer.nonces.lock().unwrap().insert(nonce),
        "each physical send needs its own nonce"
    );
    assert_eq!(
        body, peer.fixture["requests"][&path],
        "immutable operation bytes must not acquire transmission fields"
    );
    if path == "poll" && peer.stall.load(Ordering::SeqCst) {
        peer.entered.add_permits(1);
        peer.release.acquire().await.unwrap().forget();
    }
    let index = match path.as_str() {
        "poll" => 0,
        "accept" => 1,
        "prepare" => 2,
        "renew" => 3,
        "reconcile" => 4,
        _ => panic!("unexpected path"),
    };
    let mut reply = peer.fixture["replies"][index].clone();
    if path == "reconcile" {
        reply["body"]["delivery_nonce"] = json!(nonce);
    } else if path != "poll" {
        reply["body"]["delivery"]["nonce"] = json!(nonce);
    }
    match peer.mode.load(Ordering::SeqCst) {
        1 if path != "poll" => {
            if path == "reconcile" {
                reply["body"]["delivery_nonce"] = json!(Uuid::new_v4());
            } else {
                reply["body"]["delivery"]["nonce"] = json!(Uuid::new_v4());
            }
        }
        2 => reply = fixture()["replies"][0].clone(),
        3 => {
            reply["body"]["unrecognized"] = json!(true);
        }
        4 if path == "renew" => {
            reply["body"]["versions"] = fixture()["replies"][2]["body"]["versions"].clone()
        }
        5 if path == "renew" => {
            reply["body"]["delivery"]["clock"] =
                serde_json::to_value(DeliveryClock::new(2199, 2200).unwrap()).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        6 if path == "accept" => {
            reply["body"]["original"]["materials"][0]["material_digest"] = json!("c".repeat(64));
        }
        7 if path == "reconcile" => {
            reply["body"]["attempts"] = json!([]);
        }
        _ => {}
    }
    Json(reply)
}
struct Server {
    peer: Arc<Peer>,
    client: NativeClient,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new() -> Self {
        Self::with_fixture(fixture()).await
    }
    async fn with_fixture(fixture: Value) -> Self {
        let peer = Arc::new(Peer {
            fixture,
            mode: AtomicUsize::new(0),
            nonces: Mutex::new(BTreeSet::new()),
            stall: AtomicBool::new(false),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = WorkerConfig {
            server_url: format!("http://{}", listener.local_addr().unwrap()),
            node_id: request::<Poll>("poll").identity.node_id,
            credential: "synthetic-machine".into(),
            cf_client_id: None,
            cf_client_secret: None,
        };
        let router = Router::new()
            .route("/v1/worker/profiled/{path}", post(handler))
            .with_state(peer.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            peer,
            client: NativeClient::from_config(&config).unwrap(),
            task,
        }
    }
    async fn prepared(&self) -> Preparation {
        self.client
            .accept(&request("accept"), boot_ns() + 1_000_000_000, None)
            .await
            .unwrap()
    }
    async fn granted(&self) -> Granted {
        let plan = self.prepared().await;
        let offer: NativeOffer = serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
        self.client
            .prepare(&request("prepare"), &plan, &offer, None)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn hard_execution_deadline_can_exceed_two_minute_delivery_ttl() {
    let mut data = fixture();
    let old = data["native_offer"]["offered"]["offer"]["binding"]["launch_manifest_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    data["native_offer"]["offered"]["offer"]["manifest"]["resources"]["wall_time_ms"] =
        json!(300_000);
    let manifest: aq_session_protocol::LaunchManifest =
        serde_json::from_value(data["native_offer"]["offered"]["offer"]["manifest"].clone())
            .unwrap();
    let new = aq_session_protocol::canonical_digest(&manifest).unwrap();
    fn replace(v: &mut Value, old: &str, new: &str) {
        match v {
            Value::String(s) if s == old => *s = new.into(),
            Value::Object(m) => m.values_mut().for_each(|v| replace(v, old, new)),
            Value::Array(a) => a.iter_mut().for_each(|v| replace(v, old, new)),
            _ => {}
        }
    }
    replace(&mut data, &old, new.as_str());
    data["replies"][2]["body"]["execution_deadline_ms"] = json!(250_000);
    let offer: NativeOffer = serde_json::from_value(data["native_offer"].clone()).unwrap();
    offer.validate().unwrap();
    let accept: Accept = serde_json::from_value(data["requests"]["accept"].clone()).unwrap();
    let prepare: Prepare = serde_json::from_value(data["requests"]["prepare"].clone()).unwrap();
    let s = Server::with_fixture(data).await;
    let plan = s
        .client
        .accept(&accept, boot_ns() + 1_000_000_000, None)
        .await
        .unwrap();
    let grant = s
        .client
        .prepare(&prepare, &plan, &offer, None)
        .await
        .unwrap();
    assert!(grant.hard_deadline_ns() - boot_ns() > 120_000_000_000);
    assert!(grant.deadline_ns() - boot_ns() < 1_000_000_000);
}

#[tokio::test]
async fn changed_initial_operation_refuses_before_a_second_prepare_send() {
    let s = Server::new().await;
    let plan = s.prepared().await;
    let offer = serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
    let mut req: Prepare = request("prepare");
    let grant = s.client.prepare(&req, &plan, &offer, None).await.unwrap();
    let sends = s.peer.nonces.lock().unwrap().len();
    req.operation_id = Uuid::new_v4();
    assert!(
        s.client
            .prepare(&req, &plan, &offer, Some(&grant))
            .await
            .is_err()
    );
    assert_eq!(s.peer.nonces.lock().unwrap().len(), sends);
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn exact_control_roundtrips_and_retries_preserve_deadlines_and_owner_bytes() {
    let s = Server::new().await;
    let poll = s.client.poll(&request("poll")).await.unwrap();
    assert_eq!(poll.page.offers.len(), poll.offer_deadlines_ns.len());
    assert!(poll.offer_deadlines_ns[0] > boot_ns());
    let reservation = boot_ns() + 1_000_000_000;
    let plan = s
        .client
        .accept(&request("accept"), reservation, None)
        .await
        .unwrap();
    let replay = s
        .client
        .accept(&request("accept"), reservation, Some(&plan))
        .await
        .unwrap();
    assert!(replay.deadline_ns() <= plan.deadline_ns());
    let offer = serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
    let grant = s
        .client
        .prepare(&request("prepare"), &replay, &offer, None)
        .await
        .unwrap();
    let repeated = s
        .client
        .prepare(&request("prepare"), &replay, &offer, Some(&grant))
        .await
        .unwrap();
    assert!(repeated.deadline_ns() <= grant.deadline_ns());
    assert!(repeated.hard_deadline_ns() <= grant.hard_deadline_ns());
    let renewed = s.client.renew(&request("renew"), &repeated).await.unwrap();
    assert!(renewed.deadline_ns() > repeated.deadline_ns());
    assert!(renewed.deadline_ns() <= repeated.hard_deadline_ns());
    let retry = s.client.renew(&request("renew"), &renewed).await.unwrap();
    assert!(retry.deadline_ns() <= renewed.deadline_ns());
    assert_eq!(retry.hard_deadline_ns(), renewed.hard_deadline_ns());
    assert!(
        s.client
            .prepare(&request("prepare"), &replay, &offer, Some(&renewed))
            .await
            .is_err()
    );
    let current = s.client.reconcile(&request("reconcile")).await.unwrap();
    assert_eq!(current.attempts.len(), 1);
    assert_eq!(s.peer.nonces.lock().unwrap().len(), 8);
}

#[tokio::test]
async fn nonce_kind_shape_and_changed_original_replays_refuse() {
    let s = Server::new().await;
    let plan = s.prepared().await;
    for mode in [1, 2, 3, 6] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client
                .accept(&request("accept"), boot_ns() + 1_000_000_000, Some(&plan))
                .await
                .is_err(),
            "mode{mode}"
        );
    }
    for mode in [1, 2, 3, 7] {
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client.reconcile(&request("reconcile")).await.is_err(),
            "mode{mode}"
        );
    }
}

#[tokio::test]
async fn renewal_revision_and_transit_expiry_refuse_without_reanchoring() {
    for mode in [1, 2, 3, 4, 5] {
        let s = Server::new().await;
        let grant = s.granted().await;
        s.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            s.client.renew(&request("renew"), &grant).await.is_err(),
            "mode{mode}"
        );
    }
    let s = Server::new().await;
    let grant = s.granted().await;
    let remaining = grant.deadline_ns().saturating_sub(boot_ns());
    tokio::time::sleep(Duration::from_nanos(remaining + 1_000_000)).await;
    let sends = s.peer.nonces.lock().unwrap().len();
    assert!(s.client.renew(&request("renew"), &grant).await.is_err());
    assert_eq!(
        s.peer.nonces.lock().unwrap().len(),
        sends,
        "expired permit must not send renewal"
    );
    assert!(s.client.accept(&request("accept"), 0, None).await.is_err());
}

#[tokio::test]
async fn occupied_control_and_bulk_lanes_leave_the_renewal_lane_available() {
    let s = Server::new().await;
    let grant = s.granted().await;
    s.peer.stall.store(true, Ordering::SeqCst);
    let mut held = Vec::new();
    for _ in 0..2 {
        let c = s.client.clone();
        held.push(tokio::spawn(async move {
            let _ = c.poll(&request("poll")).await;
        }));
        let c = s.client.clone();
        held.push(tokio::spawn(async move {
            let offer: NativeOffer =
                serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
            let expected = offer.offered.artifacts[0].clone();
            let r = ArtifactFetch {
                schema_version: 1,
                operation_id: Uuid::new_v4(),
                identity: request::<Poll>("poll").identity,
                binding: offer.offered.offer.binding.clone(),
                artifact: expected.artifact.clone(),
                offset: 0,
                length: 1,
            };
            let _ = c.artifact(&r, &expected).await;
        }));
    }
    tokio::time::timeout(Duration::from_secs(1), s.peer.entered.acquire_many(4))
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(s.client.poll(&request("poll")).await.is_err());
    let renewed = s.client.renew(&request("renew"), &grant).await.unwrap();
    assert!(renewed.deadline_ns() > boot_ns());
    for h in held {
        h.abort();
    }
}

/// I0 run 13 offer_window_race: server poll processing that precedes its reply
/// clock stamp must not also be charged against the stated offer window.
#[tokio::test]
async fn slow_poll_reply_keeps_the_stated_offer_window_after_receipt() {
    let s = Server::new().await;
    s.peer.stall.store(true, Ordering::SeqCst);
    let c = s.client.clone();
    let poll = tokio::spawn(async move { c.poll(&request("poll")).await });
    tokio::time::timeout(Duration::from_secs(1), s.peer.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    // Fixture window is 500 ms; a send-anchored deadline would leave <= 200 ms.
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.peer.release.add_permits(1);
    let poll = poll.await.unwrap().unwrap();
    let remaining = poll.offer_deadlines_ns[0].saturating_sub(boot_ns());
    assert!(remaining > 400_000_000, "remaining window {remaining} ns");
}
