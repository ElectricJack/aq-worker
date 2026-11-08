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

/// R1 fixtures: a journal holding one attempt accepted and prepared against a loopback
/// server whose artifact route answers `overloaded` 503s (`Retry-After: 0`) for its first
/// `refusals` reads, then the startup payload.
#[cfg(feature = "test-support")]
mod preparation_failure {
    use super::*;
    use aq_admission::{
        RetainedArtifact,
        control::{ControlReply, PreparationDelivery},
    };
    use aq_session_protocol::{ArtifactRef, Digest, Text};
    use aq_worker::native::{
        journal::{Journal, Limits, RuntimeIdentity},
        materials::MaterialStore,
        preparation::InputPipeline,
        runner::NativeConfig,
    };
    use axum::response::IntoResponse;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::os::unix::fs::PermissionsExt;

    pub struct Prepared {
        pub dir: tempfile::TempDir,
        pub pipeline: Option<Arc<InputPipeline>>,
        pub preparation: Arc<Preparation>,
        pub id: Uuid,
        pub payload: ArtifactRef,
        pub reads: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Prepared {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    fn reference(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    pub fn limits() -> Limits {
        Limits {
            slots: 1,
            memory_bytes: u64::MAX,
            cpu_millis: u64::MAX,
            disk_bytes: 4096,
        }
    }
    pub fn identity() -> WorkerIdentity {
        request::<Accept>("accept").identity
    }
    pub fn payload_bytes() -> Vec<u8> {
        let startup: Value = serde_json::from_slice(include_bytes!(
            "../../../vendor/aq-admission-0.1.6/artifacts/startup.fixture.json"
        ))
        .unwrap();
        startup["payload_utf8"]
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec()
    }
    pub async fn prepared(refusals: usize) -> Prepared {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = dir.path().join("journal");
        let mut journal =
            Journal::create_with_identity_for_test(&root, identity(), limits()).unwrap();
        let mut offer: NativeOffer =
            serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
        offer.offered.reservation_ttl_ms = 5000;
        let payload = offer.startup.payload.clone();
        let descriptor: RetainedArtifact = (offer.offered.artifacts.iter())
            .find(|d| d.artifact == payload)
            .unwrap()
            .clone();
        let runtime = RuntimeIdentity {
            runtime: reference("synthetic.runtime"),
            qualification: reference("synthetic.qualification"),
        };
        let id = journal
            .reserve(offer, runtime, boot_ns() + 4_500_000_000, 1024)
            .unwrap();
        let caps = request::<Accept>("accept")
            .acceptance
            .advertised_capabilities;
        let accept = journal.begin_accept(id, caps).unwrap();
        let cap = journal.state().attempts[&id].reservation.deadline_ns;
        let mut delivery: PreparationDelivery =
            serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
        delivery.original.clock = DeliveryClock::new(1500, 5500).unwrap();
        delivery.delivery.clock = DeliveryClock::new(1600, 5500).unwrap();
        delivery.original.materials[0].expires_at_ms = 8000;
        let reads = Arc::new(AtomicUsize::new(0));
        let seen = reads.clone();
        let accept_route = post(move |headers: HeaderMap, Json(r): Json<Accept>| {
            let mut d = delivery.clone();
            async move {
                let nonce = headers["aq-delivery-nonce"].to_str().unwrap();
                d.delivery.nonce = Uuid::parse_str(nonce).unwrap();
                d.original.operation_id = r.operation_id;
                d.original.reservation = r.reservation;
                (d.original.guardian_id, d.original.launch_id) = (r.guardian_id, r.launch_id);
                Json(ControlReply::Preparation(d))
            }
        });
        let artifact_route = post(move |Json(r): Json<ArtifactFetch>| {
            let (descriptor, reads) = (descriptor.clone(), seen.clone());
            async move {
                if reads.fetch_add(1, Ordering::SeqCst) < refusals {
                    let overloaded = axum::http::StatusCode::SERVICE_UNAVAILABLE;
                    let body = r#"{"error":"overloaded","message":"request capacity exhausted; retry the same operation"}"#;
                    return (overloaded, [("retry-after", "0")], body).into_response();
                }
                let mut response = payload_bytes().into_response();
                let headers = response.headers_mut();
                headers.insert("content-type", "application/octet-stream".parse().unwrap());
                headers.insert("cache-control", "no-store".parse().unwrap());
                let retention = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&descriptor).unwrap());
                headers.insert("aq-artifact-retention", retention.parse().unwrap());
                headers.insert("aq-artifact-offset", r.offset.to_string().parse().unwrap());
                response
            }
        });
        let router = Router::new()
            .route("/v1/worker/profiled/accept", accept_route)
            .route("/v1/worker/profiled/artifact", artifact_route);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = NativeClient::from_config(&worker(&url)).unwrap();
        let preparation = Arc::new(client.accept(&accept, cap, None).await.unwrap());
        journal.record_preparation(id, &preparation).unwrap();
        let materials = dir.path().join("materials");
        std::fs::create_dir(&materials).unwrap();
        std::fs::set_permissions(&materials, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = MaterialStore::open(&materials, identity(), 8).unwrap();
        let pipeline = Some(Arc::new(InputPipeline::new(journal, store, client)));
        Prepared {
            dir,
            pipeline,
            preparation,
            id,
            payload,
            reads,
            task,
        }
    }
    pub fn worker(url: &str) -> WorkerConfig {
        WorkerConfig {
            server_url: url.into(),
            node_id: identity().node_id,
            credential: "synthetic-machine".into(),
            cf_client_id: None,
            cf_client_secret: None,
        }
    }
    /// The slot's native configuration over the attempt's state directory.
    pub fn native_config(state: &std::path::Path) -> NativeConfig {
        use aq_admission::preparation_v2::{Negotiation, contract_artifact, required_features};
        let state_dir = state.canonicalize().unwrap();
        NativeConfig {
            schema_version: 1,
            git_root: state_dir.join("git"),
            git_socket: state_dir.join("git.sock"),
            capacity: limits(),
            runtime: RuntimeIdentity {
                runtime: reference("runtime"),
                qualification: reference("qualification"),
            },
            adapter: reference("adapter"),
            renderer: reference("renderer"),
            adapter_manifest: state_dir.join("not-read-manifest.json"),
            adapter_manifest_digest: Digest::of(b"not read by recovery"),
            capabilities: vec![],
            features: vec![],
            negotiation: Negotiation {
                contract: contract_artifact().unwrap(),
                features: required_features(),
            }
            .into(),
            source_profile: reference("source"),
            maximum_preparation_ms: 1000,
            inventory_definition: state_dir.join("not-read-inventory-definition"),
            inventory_reference: reference("inventory"),
            inventory_adapter: reference("inventory-adapter"),
            inventory_evidence: reference("inventory-evidence"),
            harness_qualifications: vec![reference("harness-qualification")],
            inventory_ttl_ms: 1000,
            executor: None,
            state_dir,
        }
    }
}

/// R1 (live rerun 2026-11-08): two `503 overloaded` answers to the preparation's artifact
/// download are retried with the same request, and the download completes.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn overloaded_artifact_download_is_retried_and_preparation_continues() {
    let p = preparation_failure::prepared(2).await;
    let pipeline = p.pipeline.as_ref().unwrap();
    let verified = pipeline
        .artifact(p.id, p.preparation.clone(), p.payload.clone())
        .await
        .unwrap();
    assert_eq!(verified.bytes(), preparation_failure::payload_bytes());
    assert_eq!(p.reads.load(Ordering::SeqCst), 3, "503, 503, then 200");
    let a = pipeline.snapshot().await.unwrap().attempts[&p.id].clone();
    assert!(
        a.downloads.values().all(|d| d.receipt.is_some()),
        "completed"
    );
}

/// R1: a persistent 503 fails preparation after `FETCH_TRIES` reads. The attempt then holds
/// no execution history (only its reservation, acceptance, preparation and a pending
/// download), which recovery used to read as "missing or mixed exact phase execution" on
/// every retry. It now settles as never started: no server call (it is down), no journal
/// write, and the attempt is still the journal's only one for the service to archive.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn a_preparation_failed_by_overload_settles_as_never_started() {
    use aq_worker::native::runner::{SalvageDisposition, retained_attempts, salvage_interrupted};
    let mut p = preparation_failure::prepared(usize::MAX).await;
    let pipeline = p.pipeline.as_ref().unwrap();
    let failed = pipeline
        .artifact(p.id, p.preparation.clone(), p.payload.clone())
        .await;
    let e = failed.err().unwrap();
    assert_eq!(
        e.to_string(),
        "native endpoint returned 503 Service Unavailable"
    );
    let tries = aq_worker::native::http::FETCH_TRIES as usize;
    assert_eq!(p.reads.load(Ordering::SeqCst), tries, "bounded retries");
    let a = pipeline.snapshot().await.unwrap().attempts[&p.id].clone();
    assert!(a.acceptance.is_some() && a.preparation.is_some());
    assert!(!a.downloads.is_empty() && a.downloads.values().all(|d| d.receipt.is_none()));
    assert!(a.phase_execution.is_none() && a.refresh_execution.is_none());
    assert!(a.native_host.is_none() && a.result.is_none() && a.initial_grant.is_none());
    drop(p.pipeline.take());

    let config = preparation_failure::native_config(p.dir.path());
    let worker = preparation_failure::worker("http://127.0.0.1:9");
    // The debug-build recovery futures outgrow the 2 MiB test thread: run them on a larger one.
    let salvage = || {
        let run = || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            rt.unwrap()
                .block_on(salvage_interrupted(&worker, &config, p.id))
        };
        std::thread::scope(|s| {
            let big = std::thread::Builder::new().stack_size(16 << 20);
            big.spawn_scoped(s, run).unwrap().join().unwrap()
        })
    };
    let journal = p.dir.path().join("journal/journal.json");
    let before = std::fs::read(&journal).unwrap();
    assert_eq!(salvage().unwrap(), SalvageDisposition::NotStarted);
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        before,
        "recovery wrote nothing"
    );
    assert_eq!(retained_attempts(&config, worker.node_id).unwrap(), [p.id]);
}
