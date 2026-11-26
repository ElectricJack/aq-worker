//! Local preparation and input-download routes reading the durable test journal.
use super::*;
use axum::extract::State;

#[derive(Clone)]
pub(super) struct ReadPeer {
    root: std::path::PathBuf,
    pub(super) calls: Arc<Mutex<Vec<serde_json::Value>>>,
    pub(super) mode: Arc<AtomicU8>,
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
}
impl ReadPeer {
    pub(super) fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            calls: Default::default(),
            mode: Default::default(),
            entered: Default::default(),
            release: Default::default(),
        }
    }
    fn snapshot(&self, headers: &HeaderMap) -> Snapshot {
        assert_eq!(headers["authorization"], "Bearer synthetic-node-secret");
        serde_json::from_slice(&fs::read(self.root.join("journal.json")).unwrap()).unwrap()
    }
    fn record(&self, request: &impl serde::Serialize) {
        self.calls
            .lock()
            .unwrap()
            .push(serde_json::to_value(request).unwrap());
    }
    pub(super) fn router(
        self,
        delivery: PreparationDelivery,
        descriptor: aq_admission::RetainedArtifact,
        bytes: Vec<u8>,
    ) -> Router {
        let material = delivery.original.materials[0].clone();
        Router::new()
            .route(
                "/v1/worker/profiled/accept",
                post(accept).with_state(delivery),
            )
            .route(
                "/v1/worker/profiled/artifact",
                post(artifact).with_state((self.clone(), descriptor, bytes)),
            )
            .route(
                "/v1/worker/profiled/material",
                post(material_response).with_state((self, material)),
            )
    }
}
async fn accept(
    State(mut delivery): State<PreparationDelivery>,
    headers: HeaderMap,
    Json(request): Json<Accept>,
) -> Json<ControlReply> {
    delivery.delivery.nonce =
        Uuid::parse_str(headers["aq-delivery-nonce"].to_str().unwrap()).unwrap();
    delivery.original.operation_id = request.operation_id;
    delivery.original.reservation = request.reservation;
    delivery.original.guardian_id = request.guardian_id;
    delivery.original.launch_id = request.launch_id;
    Json(ControlReply::Preparation(delivery))
}
fn lost_read() -> Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "synthetic lost read",
    )
        .into_response()
}
fn payload(bytes: Vec<u8>, name: &'static str, metadata: &impl serde::Serialize) -> Response {
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    headers.insert("cache-control", "no-store".parse().unwrap());
    headers.insert(
        name,
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(metadata).unwrap())
            .parse()
            .unwrap(),
    );
    response
}
async fn artifact(
    State((peer, descriptor, bytes)): State<(ReadPeer, aq_admission::RetainedArtifact, Vec<u8>)>,
    headers: HeaderMap,
    Json(request): Json<ArtifactFetch>,
) -> Response {
    let snapshot = peer.snapshot(&headers);
    assert_eq!(snapshot.digest, canonical_digest(&snapshot.state).unwrap());
    assert!(
        snapshot.state.attempts[&request.binding.attempt_id]
            .downloads
            .values()
            .any(|p| p.requests.iter().any(|r| same(r, &request).unwrap()))
    );
    peer.record(&request);
    if peer.mode.load(Ordering::SeqCst) == 1 {
        return lost_read();
    }
    if peer.mode.load(Ordering::SeqCst) == 2 {
        peer.entered.notify_one();
        peer.release.notified().await;
    }
    let start = request.offset as usize;
    let end = start + request.length as usize;
    let mut response = payload(
        bytes[start..end].to_vec(),
        "aq-artifact-retention",
        &descriptor,
    );
    response.headers_mut().insert(
        "aq-artifact-offset",
        request.offset.to_string().parse().unwrap(),
    );
    response
}
async fn material_response(
    State((peer, lease)): State<(ReadPeer, aq_admission::MaterialLease)>,
    headers: HeaderMap,
    Json(request): Json<MaterialFetch>,
) -> Response {
    let snapshot = peer.snapshot(&headers);
    let saved =
        &snapshot.state.attempts[&request.binding.attempt_id].material_downloads[&request.lease_id];
    assert!(same(&saved.request, &request).unwrap());
    peer.record(&request);
    if peer.mode.load(Ordering::SeqCst) == 1 {
        return lost_read();
    }
    let delivery = MaterialDelivery {
        clock: DeliveryClock::new(1600, lease.expires_at_ms).unwrap(),
        lease,
    };
    payload(SECRET.to_vec(), "aq-material-delivery", &delivery)
}
