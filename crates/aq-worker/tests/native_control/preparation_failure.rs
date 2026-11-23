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
    pub task: tokio::task::JoinHandle<()>,
    /// The loopback server's routes and address, to bring it back after an outage.
    pub router: Router,
    pub address: std::net::SocketAddr,
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
        "../../../../vendor/aq-admission-0.1.6/artifacts/startup.fixture.json"
    ))
    .unwrap();
    startup["payload_utf8"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec()
}
fn artifact_route(
    descriptor: RetainedArtifact,
    seen: Arc<AtomicUsize>,
    refusals: usize,
) -> axum::routing::MethodRouter {
    post(move |Json(r): Json<ArtifactFetch>| {
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
    })
}
fn accept_route(delivery: PreparationDelivery) -> axum::routing::MethodRouter {
    post(move |headers: HeaderMap, Json(r): Json<Accept>| {
        let mut d = delivery.clone();
        async move {
            let nonce = headers["aq-delivery-nonce"].to_str().unwrap();
            d.delivery.nonce = Uuid::parse_str(nonce).unwrap();
            d.original.operation_id = r.operation_id;
            d.original.reservation = r.reservation;
            (d.original.guardian_id, d.original.launch_id) = (r.guardian_id, r.launch_id);
            Json(ControlReply::Preparation(d))
        }
    })
}
fn delivery_in_window(window_ms: u64) -> PreparationDelivery {
    let mut delivery: PreparationDelivery =
        serde_json::from_value(fixture()["replies"][1]["body"].clone()).unwrap();
    delivery.original.clock = DeliveryClock::new(1500, 1500 + window_ms).unwrap();
    delivery.delivery.clock = DeliveryClock::new(1600, 1500 + window_ms).unwrap();
    delivery.original.materials[0].expires_at_ms = window_ms + 6500;
    delivery
}
pub async fn prepared(refusals: usize, window_ms: u64) -> Prepared {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = dir.path().join("journal");
    let mut journal = Journal::create_identity(&root, identity(), limits()).unwrap();
    let mut offer: NativeOffer = serde_json::from_value(fixture()["native_offer"].clone()).unwrap();
    offer.offered.reservation_ttl_ms = window_ms + 1000;
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
        .reserve(
            offer,
            runtime,
            boot_ns() + (window_ms + 500) * 1_000_000,
            1024,
        )
        .unwrap();
    let caps = request::<Accept>("accept")
        .acceptance
        .advertised_capabilities;
    let accept = journal.begin_accept(id, caps).unwrap();
    let cap = journal.state().attempts[&id].reservation.deadline_ns;
    let delivery = delivery_in_window(window_ms);
    let reads = Arc::new(AtomicUsize::new(0));
    let seen = reads.clone();
    let accept_route = accept_route(delivery);
    let artifact_route = artifact_route(descriptor, seen, refusals);
    let router = Router::new()
        .route("/v1/worker/profiled/accept", accept_route)
        .route("/v1/worker/profiled/artifact", artifact_route);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = router.clone();
    let task = tokio::spawn(async move { axum::serve(listener, served).await.unwrap() });
    let client = NativeClient::from_config(&worker(&format!("http://{address}"))).unwrap();
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
        router,
        address,
    }
}
/// Runs `f` on a tokio worker with a 1 MiB stack, from a thread outside any runtime. R4: the
/// recovery below the slot task must leave half of tokio's 2 MiB default to the supervisor
/// and service layers above it (with them, the base needed 2.17 MiB).
pub fn on_worker_stack<T: Send + 'static>(
    f: impl std::future::Future<Output = T> + Send + 'static,
) -> T {
    let run = move || {
        let mut rt = tokio::runtime::Builder::new_multi_thread();
        let rt = rt.worker_threads(1).thread_stack_size(1 << 20);
        let rt = rt.enable_all().build().unwrap();
        rt.block_on(rt.spawn(f)).unwrap()
    };
    std::thread::spawn(run).join().unwrap()
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
        node_git: None,
        state_dir,
    }
}
